//! Local chunking pass: walk a directory, split each file into
//! fixed-size pieces hashed by SHA-256, zstd-compress unique chunks, and
//! emit a manifest.
//!
//! The hot path streams chunk-by-chunk so memory usage stays bounded at
//! `chunk_size` regardless of input tree size — required because game
//! clients can run multi-GB across tens of thousands of files and we'd
//! OOM otherwise.

use crate::manifest::{ChunkEntry, FileEntry, Manifest};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, create_dir_all, write};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;
use walkdir::WalkDir;

pub struct ChunkOpts {
    pub input: PathBuf,
    pub output: PathBuf,
    pub version: String,
    pub game: String,
    pub platform: String,
    pub chunk_size: u64,
    pub zstd_level: i32,
}

pub fn chunk(opts: &ChunkOpts) -> Result<()> {
    let chunks_dir = opts.output.join("chunks");
    create_dir_all(&chunks_dir)
        .with_context(|| format!("create output chunks dir at {}", chunks_dir.display()))?;

    println!(
        "chunker: input={} output={} chunk_size={} MiB zstd={}",
        opts.input.display(),
        opts.output.display(),
        opts.chunk_size / (1024 * 1024),
        opts.zstd_level
    );

    let files = collect_files(&opts.input)?;
    println!("found {} files", files.len());

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    pb.enable_steady_tick(Duration::from_millis(200));

    let mut total_size: u64 = 0;
    let mut file_entries: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut chunk_entries: BTreeMap<String, ChunkEntry> = BTreeMap::new();
    let mut compressed_total: u64 = 0;
    let mut buffer = vec![0u8; opts.chunk_size as usize];
    let mut written: HashSet<String> = HashSet::new();

    for path in &files {
        let rel_path = path
            .strip_prefix(&opts.input)
            .with_context(|| format!("strip_prefix on {}", path.display()))?
            .to_str()
            .with_context(|| format!("non-UTF-8 path: {}", path.display()))?
            .replace('\\', "/");
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("metadata for {}", path.display()))?;
        let file_size = metadata.len();
        total_size += file_size;

        let mut chunk_hashes: Vec<String> = Vec::new();
        let mut remaining = file_size;
        while remaining > 0 {
            let to_read = std::cmp::min(remaining as usize, opts.chunk_size as usize);
            let buf = &mut buffer[..to_read];
            file.read_exact(buf)
                .with_context(|| format!("read chunk from {}", path.display()))?;

            let hash = hex::encode(Sha256::digest(&*buf));
            chunk_hashes.push(hash.clone());

            if written.insert(hash.clone()) {
                let compressed = zstd::encode_all(&buf[..], opts.zstd_level)
                    .with_context(|| format!("zstd-compress chunk {hash}"))?;
                let chunk_path = chunks_dir.join(format!("{hash}.zst"));
                write(&chunk_path, &compressed)
                    .with_context(|| format!("write chunk {}", chunk_path.display()))?;
                compressed_total += compressed.len() as u64;
                chunk_entries.insert(
                    hash.clone(),
                    ChunkEntry {
                        size: to_read as u64,
                        compressed_size: compressed.len() as u64,
                        url: format!("../../chunks/{hash}.zst"),
                    },
                );
            }
            remaining -= to_read as u64;
        }

        file_entries.insert(
            rel_path,
            FileEntry {
                size: file_size,
                chunks: chunk_hashes,
            },
        );
        pb.inc(1);
    }
    pb.finish_with_message("chunked");

    let manifest = Manifest {
        version: opts.version.clone(),
        game_id: opts.game.clone(),
        platform: opts.platform.clone(),
        generated_at: now_iso8601(),
        chunk_size: opts.chunk_size,
        total_size,
        files: file_entries,
        chunks: chunk_entries,
    };

    let manifest_path = opts.output.join("manifest.json");
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    write(&manifest_path, manifest_json.as_bytes())
        .with_context(|| format!("write manifest to {}", manifest_path.display()))?;

    let ratio = if total_size > 0 {
        (compressed_total as f64) / (total_size as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "wrote {} unique chunks ({:.1}% ratio); manifest at {}",
        manifest.chunks.len(),
        ratio,
        manifest_path.display()
    );
    Ok(())
}

/// Walk the input tree depth-first, collecting regular files in sorted
/// order. Sorting up front keeps the chunking deterministic: same input,
/// same chunk write order, same progress-bar narrative across runs.
///
/// Junk files are filtered: macOS AppleDouble shadows (`._*`),
/// `.DS_Store`, `Thumbs.db`. Without this, a directory tar'd on macOS
/// without `COPYFILE_DISABLE=1` doubles every entry with an `._*` shadow,
/// bloating the manifest and shipping useless bytes to every consumer.
/// Junk filtering belongs in the tool so a single Mac-tarred input does
/// not poison every downstream consumer's manifest.
fn collect_files(root: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| !is_junk_file(entry.file_name()))
        .map(|entry| entry.into_path())
        .collect();
    paths.sort();
    Ok(paths)
}

/// File-name-only matcher (no path inspection): detects OS metadata
/// detritus that should never be part of a content release.
fn is_junk_file(name: &std::ffi::OsStr) -> bool {
    let Some(s) = name.to_str() else { return false };
    // macOS AppleDouble shadow files for any underlying file `X` are
    // named `._X`. They carry resource forks / extended attributes and
    // are useless on any non-HFS+ consumer.
    if s.starts_with("._") {
        return true;
    }
    matches!(s, ".DS_Store" | "Thumbs.db" | "desktop.ini")
}

/// ISO 8601 UTC, millisecond precision, `Z` suffix — matches the JS
/// `Date.prototype.toISOString` output the launcher's downloader expects.
fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

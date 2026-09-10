//! Split regular files into SHA-256-addressed zstd chunks.
use crate::validation;
use anyhow::{Context, Result, ensure};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, create_dir_all, write};
use std::io::Read;
use std::path::{Path, PathBuf};
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

/// Read one chunk at a time and call `on_chunk` with its hash and raw bytes.
/// Return the original file size and ordered chunk hashes. Changed files fail.
pub fn for_each_chunk<F>(
    path: &Path,
    chunk_size: u64,
    mut on_chunk: F,
) -> Result<(u64, Vec<String>)>
where
    F: FnMut(&str, &[u8]) -> Result<()>,
{
    validation::chunk_size(chunk_size)?;
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("metadata for {}", path.display()))?;
    ensure!(metadata.is_file(), "chunk input must be a regular file");
    let file_size = metadata.len();

    let mut buffer = vec![0u8; chunk_size as usize];
    let mut hashes: Vec<String> = Vec::new();
    let mut remaining = file_size;
    while remaining > 0 {
        let to_read = remaining.min(chunk_size) as usize;
        let buf = &mut buffer[..to_read];
        file.read_exact(buf)
            .with_context(|| format!("read chunk from {}", path.display()))?;

        let hash = hex::encode(Sha256::digest(&*buf));
        on_chunk(&hash, buf)?;
        hashes.push(hash);
        remaining -= to_read as u64;
    }
    let after = file.metadata()?;
    let mut extra = [0];
    ensure!(
        file.read(&mut extra)? == 0
            && after.len() == file_size
            && after.modified()? == metadata.modified()?,
        "input changed while chunking: {}",
        path.display()
    );
    Ok((file_size, hashes))
}

pub fn chunk(opts: &ChunkOpts) -> Result<()> {
    validation::chunk_size(opts.chunk_size)?;
    validation::identifier(&opts.version)?;
    validation::identifier(&opts.game)?;
    validation::identifier(&opts.platform)?;
    let input = opts
        .input
        .canonicalize()
        .context("resolve input directory")?;
    ensure!(input.is_dir(), "input must be a directory");
    create_dir_all(&opts.output)?;
    let output = opts.output.canonicalize()?;
    ensure!(
        !output.starts_with(&input),
        "output must be outside the input tree"
    );
    let chunks_dir = output.join("chunks");
    create_dir_all(&chunks_dir)
        .with_context(|| format!("create output chunks dir at {}", chunks_dir.display()))?;

    println!(
        "chunker: input={} output={} chunk_size={} MiB zstd={}",
        opts.input.display(),
        opts.output.display(),
        opts.chunk_size / (1024 * 1024),
        opts.zstd_level
    );

    let files = collect_files(&input)?;
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
    let mut written: HashSet<String> = HashSet::new();

    for path in &files {
        let rel_path = path
            .strip_prefix(&input)?
            .components()
            .map(|part| part.as_os_str().to_str().context("non-UTF-8 input path"))
            .collect::<Result<Vec<_>>>()?
            .join("/");
        validation::relative_path(&rel_path)?;

        let (file_size, chunk_hashes) =
            for_each_chunk(path, opts.chunk_size, |hash, uncompressed| {
                if written.insert(hash.to_string()) {
                    let compressed = zstd::encode_all(uncompressed, opts.zstd_level)
                        .with_context(|| format!("zstd-compress chunk {hash}"))?;
                    let chunk_path = chunks_dir.join(format!("{hash}.zst"));
                    write(&chunk_path, &compressed)
                        .with_context(|| format!("write chunk {}", chunk_path.display()))?;
                    compressed_total += compressed.len() as u64;
                    chunk_entries.insert(
                        hash.to_string(),
                        ChunkEntry {
                            size: uncompressed.len() as u64,
                            compressed_size: compressed.len() as u64,
                            url: format!("../../chunks/{hash}.zst"),
                        },
                    );
                }
                Ok(())
            })?;

        validation::relative_path(&rel_path)?;
        total_size = total_size
            .checked_add(file_size)
            .context("input total size overflow")?;
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

    validation::manifest(&manifest)?;
    let manifest_path = output.join("manifest.json");
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

/// Sort regular files for deterministic manifests. Propagate traversal errors.
fn collect_files(root: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry.context("read input tree")?;
        if is_junk_file(entry.file_name()) {
            continue;
        }
        ensure!(
            !entry.file_type().is_symlink(),
            "input contains a symlink: {}",
            entry.path().display()
        );
        if entry.file_type().is_file() {
            paths.push(entry.into_path());
        } else {
            ensure!(
                entry.file_type().is_dir(),
                "unsupported input file: {}",
                entry.path().display()
            );
        }
    }
    paths.sort();
    Ok(paths)
}

fn is_junk_file(name: &std::ffi::OsStr) -> bool {
    let Some(s) = name.to_str() else { return false };
    if s.starts_with("._") {
        return true;
    }
    matches!(s, ".DS_Store" | "Thumbs.db" | "desktop.ini")
}

/// UTC timestamp with millisecond precision.
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

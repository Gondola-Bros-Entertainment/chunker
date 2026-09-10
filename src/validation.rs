//! Publisher-side format and content validation. Consumers must still verify downloads.
use crate::manifest::{ChunkEntry, Manifest};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

pub const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

pub fn chunk_size(size: u64) -> Result<()> {
    ensure!(
        (1..=MAX_CHUNK_SIZE).contains(&size),
        "chunk size must be between 1 and {MAX_CHUNK_SIZE} bytes"
    );
    Ok(())
}

pub fn concurrency(value: usize) -> Result<()> {
    ensure!(
        (1..=64).contains(&value),
        "concurrency must be between 1 and 64"
    );
    Ok(())
}

pub fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid version/game/platform identifier: {value:?}"
    );
    Ok(())
}

pub fn relative_path(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "empty relative path");
    for part in value.split('/') {
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part
                    .chars()
                    .any(|c| c.is_control() || "\\<>:\"|?*".contains(c)),
            "unsafe relative path: {value:?}"
        );
        let base = part.split('.').next().unwrap().to_ascii_lowercase();
        let device = matches!(base.as_str(), "con" | "prn" | "aux" | "nul")
            || ((base.starts_with("com") || base.starts_with("lpt"))
                && base.len() == 4
                && base.as_bytes()[3].is_ascii_digit());
        ensure!(!device, "reserved path: {value:?}");
    }
    Ok(())
}

pub fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let stat = fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        stat.is_file() && stat.len() <= limit,
        "not a bounded regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "file grew past its limit: {}",
        path.display()
    );
    Ok(bytes)
}

pub fn manifest(value: &Manifest) -> Result<()> {
    identifier(&value.version)?;
    identifier(&value.game_id)?;
    identifier(&value.platform)?;
    chunk_size(value.chunk_size)?;
    let mut used = HashSet::new();
    let mut paths = HashSet::new();
    let mut directories = BTreeMap::new();
    for (hash, chunk) in &value.chunks {
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid chunk hash"
        );
        ensure!(
            chunk.size > 0 && chunk.size <= value.chunk_size,
            "invalid chunk size for {hash}"
        );
        ensure!(
            chunk.compressed_size > 0
                && chunk.compressed_size
                    <= zstd::zstd_safe::compress_bound(chunk.size as usize) as u64,
            "invalid compressed size for {hash}"
        );
        ensure!(
            chunk.url == format!("../../chunks/{hash}.zst"),
            "invalid chunk URL for {hash}"
        );
    }
    let mut total = 0_u64;
    for (name, file) in &value.files {
        relative_path(name)?;
        let folded = name.to_lowercase();
        ensure!(
            !paths.contains(&folded) && !directories.contains_key(&folded),
            "colliding file path: {name}"
        );
        let components: Vec<_> = name.split('/').collect();
        for count in 1..components.len() {
            let parent = components[..count].join("/");
            let key = parent.to_lowercase();
            ensure!(
                !paths.contains(&key)
                    && directories
                        .get(&key)
                        .is_none_or(|original| original == &parent),
                "colliding directory path: {name}"
            );
            directories.insert(key, parent);
        }
        paths.insert(folded);
        let expected_count =
            file.size / value.chunk_size + u64::from(file.size % value.chunk_size != 0);
        ensure!(
            file.chunks.len() as u64 == expected_count,
            "wrong number of chunks in {name}"
        );
        let mut remaining = file.size;
        for hash in &file.chunks {
            let chunk = value
                .chunks
                .get(hash)
                .with_context(|| format!("missing chunk {hash} in {name}"))?;
            let size = remaining.min(value.chunk_size);
            ensure!(chunk.size == size, "wrong chunk boundary in {name}");
            remaining -= size;
            used.insert(hash);
        }
        total = total
            .checked_add(file.size)
            .context("manifest total size overflow")?;
    }
    ensure!(
        total == value.total_size,
        "manifest total does not match its files"
    );
    ensure!(
        used.len() == value.chunks.len(),
        "manifest contains unreferenced chunks"
    );
    Ok(())
}

pub fn compressed(hash: &str, chunk: &ChunkEntry, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 == chunk.compressed_size,
        "compressed size mismatch for {hash}"
    );
    chunk_size(chunk.size)?;
    let mut decoder = zstd::stream::read::Decoder::new(bytes)?;
    decoder.window_log_max(26)?;
    let mut raw = Vec::new();
    decoder.take(chunk.size + 1).read_to_end(&mut raw)?;
    ensure!(
        raw.len() as u64 == chunk.size && hex::encode(Sha256::digest(&raw)) == hash,
        "chunk content/hash mismatch for {hash}"
    );
    Ok(())
}

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("manifest exceeds size limit");
    }
    let value: Manifest = serde_json::from_slice(bytes).context("parse manifest")?;
    manifest(&value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::FileEntry;
    fn fixture() -> Manifest {
        let raw = b"abcdefgh";
        let hash = hex::encode(Sha256::digest(raw));
        let compressed = zstd::encode_all(&raw[..], 12).unwrap();
        Manifest {
            version: "v1".into(),
            game_id: "game".into(),
            platform: "win".into(),
            generated_at: "now".into(),
            chunk_size: 8,
            total_size: 8,
            files: BTreeMap::from([(
                "data".into(),
                FileEntry {
                    size: 8,
                    chunks: vec![hash.clone()],
                },
            )]),
            chunks: BTreeMap::from([(
                hash.clone(),
                ChunkEntry {
                    size: 8,
                    compressed_size: compressed.len() as u64,
                    url: format!("../../chunks/{hash}.zst"),
                },
            )]),
        }
    }
    #[test]
    fn portable_paths() {
        for name in [
            "",
            "../escape",
            "/root",
            "a//b",
            "a/./b",
            "C:/x",
            "a\\b",
            "con.txt",
            "x.",
            "x ",
            "x\n",
        ] {
            assert!(relative_path(name).is_err(), "{name:?}");
        }
        for name in [
            "assets/Hello world.bin",
            "日本語/é.bin",
            "Orivella.app/Contents/MacOS/Orivella",
        ] {
            relative_path(name).unwrap();
        }
    }
    #[test]
    fn manifest_boundaries_and_collisions() {
        manifest(&fixture()).unwrap();
        let mut bad = fixture();
        bad.total_size += 1;
        assert!(manifest(&bad).is_err());
        let mut bad = fixture();
        bad.files.get_mut("data").unwrap().chunks.clear();
        assert!(manifest(&bad).is_err());
        let mut bad = fixture();
        bad.files.insert(
            "DATA".into(),
            FileEntry {
                size: 0,
                chunks: vec![],
            },
        );
        assert!(manifest(&bad).is_err());
        let mut bad = fixture();
        bad.files.insert(
            "data/child".into(),
            FileEntry {
                size: 0,
                chunks: vec![],
            },
        );
        assert!(manifest(&bad).is_err());
        let mut bad = fixture();
        bad.files.insert(
            "A/x".into(),
            FileEntry {
                size: 0,
                chunks: vec![],
            },
        );
        bad.files.insert(
            "a/y".into(),
            FileEntry {
                size: 0,
                chunks: vec![],
            },
        );
        assert!(manifest(&bad).is_err());
        let mut bad = fixture();
        bad.chunks.values_mut().next().unwrap().url = "https://other.invalid/x".into();
        assert!(manifest(&bad).is_err());
    }
    #[test]
    fn compressed_data_is_bounded_and_authenticated() {
        let f = fixture();
        let (hash, entry) = f.chunks.iter().next().unwrap();
        let valid = zstd::encode_all(&b"abcdefgh"[..], 12).unwrap();
        compressed(hash, entry, &valid).unwrap();
        let corrupt = zstd::encode_all(&b"abcdefgi"[..], 12).unwrap();
        assert!(compressed(hash, entry, &corrupt).is_err());
        assert!(compressed(hash, entry, &valid[..valid.len() - 1]).is_err());
        let expanded = zstd::encode_all(&vec![0; 65536][..], 12).unwrap();
        let entry = ChunkEntry {
            size: 8,
            compressed_size: expanded.len() as u64,
            url: String::new(),
        };
        assert!(compressed(hash, &entry, &expanded).is_err());
    }
}

//! Manifest schema. JSON shape is stable: field names use the exact
//! camelCase a JavaScript downloader would expect (`gameId`,
//! `generatedAt`, `chunkSize`, `totalSize`, `compressedSize`), so any
//! consumer parsing it can stay generic.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(rename = "gameId")]
    pub game_id: String,
    pub platform: String,
    /// ISO 8601 UTC, millisecond precision, `Z` suffix (matches JS
    /// `Date.prototype.toISOString`).
    #[serde(rename = "generatedAt")]
    pub generated_at: String,
    #[serde(rename = "chunkSize")]
    pub chunk_size: u64,
    #[serde(rename = "totalSize")]
    pub total_size: u64,
    /// Relative path → file metadata. `BTreeMap` gives sorted-by-path
    /// determinism across runs, which keeps version-diff readability sane.
    pub files: BTreeMap<String, FileEntry>,
    /// SHA-256 hash → chunk metadata. Sorted-by-hash for the same reason.
    pub chunks: BTreeMap<String, ChunkEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub size: u64,
    /// Ordered list of chunk hashes. Concatenating the decompressed
    /// chunks in this order reconstructs the original file byte-for-byte.
    pub chunks: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// Uncompressed size in bytes.
    pub size: u64,
    /// On-disk (zstd-compressed) size in bytes.
    #[serde(rename = "compressedSize")]
    pub compressed_size: u64,
    /// Relative URL from the manifest's location, pointing at the chunk
    /// in the shared pool. From `<prefix>/versions/<v>/manifest.json`,
    /// `../../chunks/<hash>.zst` resolves to `<prefix>/chunks/<hash>.zst`.
    pub url: String,
}

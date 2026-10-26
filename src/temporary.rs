//! Working directories that are removed when dropped.
use anyhow::{Context, Result};
use tempfile::TempDir;

pub fn directory() -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix("chunker-")
        .tempdir()
        .context("create chunker temporary directory")
}

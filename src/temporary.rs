use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub struct Directory(PathBuf);
impl Directory {
    pub fn new() -> Result<Self> {
        for attempt in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "chunker-{}-{}-{attempt}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("create chunker temporary directory"),
            }
        }
        bail!("could not create unique temporary directory")
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

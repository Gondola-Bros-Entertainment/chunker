//! Incremental publish from a validated base. Only replacement files are read locally.
use crate::{
    chunk::{for_each_chunk, now_iso8601},
    manifest::{ChunkEntry, FileEntry, Manifest},
    publish::{self, Target},
    r2::{self, Condition},
    temporary, validation,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Override {
    pub path: String,
    pub local: PathBuf,
}
pub struct PatchOpts {
    pub bucket: String,
    pub prefix: String,
    pub base_version: Option<String>,
    pub new_version: String,
    pub overrides: Vec<Override>,
    pub removes: Vec<String>,
    pub chunk_size: u64,
    pub zstd_level: i32,
    pub concurrency: usize,
    pub force: bool,
    pub allow_stale_base: bool,
}

fn preflight(opts: &PatchOpts) -> Result<()> {
    ensure!(
        !opts.force,
        "overwriting version manifests is no longer supported; choose a new version"
    );
    validation::identifier(&opts.new_version)?;
    if let Some(base) = &opts.base_version {
        validation::identifier(base)?;
    }
    validation::relative_path(&opts.prefix)?;
    validation::chunk_size(opts.chunk_size)?;
    validation::concurrency(opts.concurrency)?;
    ensure!(
        !opts.overrides.is_empty() || !opts.removes.is_empty(),
        "patch needs an override or removal"
    );
    let mut names = HashSet::new();
    for replacement in &opts.overrides {
        validation::relative_path(&replacement.path)?;
        ensure!(
            names.insert(replacement.path.to_lowercase()),
            "duplicate/conflicting patch path: {}",
            replacement.path
        );
        ensure!(
            std::fs::symlink_metadata(&replacement.local)?.is_file(),
            "override must be a regular file"
        );
    }
    for name in &opts.removes {
        validation::relative_path(name)?;
        ensure!(
            names.insert(name.to_lowercase()),
            "duplicate/conflicting patch path: {name}"
        );
    }
    Ok(())
}

pub async fn patch(opts: &PatchOpts) -> Result<()> {
    preflight(opts)?;
    let client = r2::build_client().await?;
    let prior = r2::get(
        &client,
        &opts.bucket,
        &format!("{}/latest.txt", opts.prefix),
        1024,
    )
    .await?
    .context("no published base pointer")?;
    let current = std::str::from_utf8(&prior.bytes)?.trim();
    validation::identifier(current)?;
    let base = opts.base_version.as_deref().unwrap_or(current);
    ensure!(
        opts.allow_stale_base || base == current,
        "base version differs from latest; use --allow-stale-base for an intentional fork"
    );
    ensure!(
        opts.new_version != current && opts.new_version != base,
        "patch target must be a new version"
    );
    let object = r2::get(
        &client,
        &opts.bucket,
        &format!("{}/versions/{base}/manifest.json", opts.prefix),
        validation::MAX_MANIFEST_BYTES,
    )
    .await?
    .context("base manifest missing")?;
    let manifest = validation::parse_manifest(&object.bytes)?;
    ensure!(
        manifest.version == base,
        "base manifest version does not match its location"
    );
    ensure!(
        manifest.chunk_size == opts.chunk_size,
        "chunk size does not match base manifest"
    );
    for name in &opts.removes {
        ensure!(
            manifest.files.contains_key(name),
            "removal is not present in base: {name}"
        );
    }
    let target_key = format!(
        "{}/versions/{}/manifest.json",
        opts.prefix, opts.new_version
    );
    ensure!(
        r2::head(&client, &opts.bucket, &target_key)
            .await?
            .is_none(),
        "version manifest already exists; choose a new version"
    );
    let condition = Condition::Absent;
    let staging = temporary::Directory::new()?;
    let (manifest, pending) = build_manifest(opts, manifest, staging.path())?;
    publish::publish_manifest(
        &client,
        &Target {
            bucket: &opts.bucket,
            prefix: &opts.prefix,
            concurrency: opts.concurrency,
            manifest_condition: condition,
        },
        manifest,
        pending,
        Some(&prior),
    )
    .await
}

fn build_manifest(
    opts: &PatchOpts,
    mut base: Manifest,
    staging: &Path,
) -> Result<(Manifest, BTreeMap<String, PathBuf>)> {
    let mut pending = BTreeMap::new();
    for replacement in &opts.overrides {
        let (size, hashes) = for_each_chunk(&replacement.local, opts.chunk_size, |hash, raw| {
            if !base.chunks.contains_key(hash) {
                let bytes = zstd::encode_all(raw, opts.zstd_level)?;
                let file = staging.join(format!("{hash}.zst"));
                std::fs::write(&file, &bytes)?;
                base.chunks.insert(
                    hash.to_owned(),
                    ChunkEntry {
                        size: raw.len() as u64,
                        compressed_size: bytes.len() as u64,
                        url: format!("../../chunks/{hash}.zst"),
                    },
                );
                pending.insert(hash.to_owned(), file);
            }
            Ok(())
        })?;
        base.files.insert(
            replacement.path.clone(),
            FileEntry {
                size,
                chunks: hashes,
            },
        );
    }
    for name in &opts.removes {
        base.files.remove(name);
    }
    let used: HashSet<_> = base.files.values().flat_map(|f| f.chunks.iter()).collect();
    base.chunks.retain(|hash, _| used.contains(hash));
    pending.retain(|hash, _| used.contains(hash));
    base.total_size = base.files.values().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size)
            .context("patch total size overflow")
    })?;
    base.version = opts.new_version.clone();
    base.generated_at = now_iso8601();
    validation::manifest(&base)?;
    Ok((base, pending))
}

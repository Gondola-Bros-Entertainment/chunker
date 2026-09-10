//! Validate local content first, then publish immutable objects and compare-and-swap the pointer.
use crate::{
    manifest::{ChunkEntry, Manifest},
    r2::{self, Condition, IMMUTABLE_CACHE, NO_CACHE, Object},
    validation,
};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::Client;
use futures::stream::{self, StreamExt, TryStreamExt};
use std::{collections::BTreeMap, path::PathBuf};

pub struct PublishOpts {
    pub chunks_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub version: String,
    pub bucket: String,
    pub prefix: String,
    pub concurrency: usize,
}
pub struct Target<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub concurrency: usize,
    pub manifest_condition: Condition,
}

pub async fn publish(opts: &PublishOpts) -> Result<()> {
    validation::identifier(&opts.version)?;
    validation::relative_path(&opts.prefix)?;
    validation::concurrency(opts.concurrency)?;
    let manifest = validation::parse_manifest(&validation::read_regular(
        &opts.manifest_path,
        validation::MAX_MANIFEST_BYTES,
    )?)?;
    ensure!(
        manifest.version == opts.version,
        "--version does not match manifest version"
    );
    let mut files = BTreeMap::new();
    for (hash, entry) in &manifest.chunks {
        let file = opts.chunks_dir.join(format!("{hash}.zst"));
        validation::compressed(
            hash,
            entry,
            &validation::read_regular(&file, entry.compressed_size)?,
        )?;
        files.insert(hash.clone(), file);
    }
    // No network writes are possible before every source chunk has passed validation.
    let client = r2::build_client().await?;
    let previous = r2::get(
        &client,
        &opts.bucket,
        &format!("{}/latest.txt", opts.prefix),
        1024,
    )
    .await?;
    let key = format!("{}/versions/{}/manifest.json", opts.prefix, opts.version);
    ensure!(
        r2::head(&client, &opts.bucket, &key).await?.is_none(),
        "version manifest already exists; choose a new version"
    );
    publish_manifest(
        &client,
        &Target {
            bucket: &opts.bucket,
            prefix: &opts.prefix,
            concurrency: opts.concurrency,
            manifest_condition: Condition::Absent,
        },
        manifest,
        files,
        previous.as_ref(),
    )
    .await
}

async fn ensure_chunk(
    client: &Client,
    bucket: &str,
    key: &str,
    hash: &str,
    entry: &ChunkEntry,
    local: &std::path::Path,
) -> Result<u64> {
    // Validate again while reading the exact bytes used by PUT, in case local files changed.
    let bytes = validation::read_regular(local, entry.compressed_size)?;
    validation::compressed(hash, entry, &bytes)?;
    if r2::head(client, bucket, key).await?.is_none()
        && r2::put(
            client,
            bucket,
            key,
            bytes,
            ("application/zstd", IMMUTABLE_CACHE),
            &Condition::Absent,
        )
        .await?
    {
        return Ok(entry.compressed_size);
    }
    // Never overwrite the canonical compressed representation of an existing raw hash.
    // Different zstd levels/builds may encode the same bytes differently.
    let object = r2::get(
        client,
        bucket,
        key,
        zstd::zstd_safe::compress_bound(entry.size as usize) as u64,
    )
    .await?
    .context("concurrent chunk disappeared")?;
    let mut canonical = entry.clone();
    canonical.compressed_size = object.bytes.len() as u64;
    validation::compressed(hash, &canonical, &object.bytes)?;
    Ok(canonical.compressed_size)
}

pub async fn publish_manifest(
    client: &Client,
    target: &Target<'_>,
    mut manifest: Manifest,
    files: BTreeMap<String, PathBuf>,
    previous: Option<&Object>,
) -> Result<()> {
    validation::manifest(&manifest)?;
    if let Some(old) = previous {
        ensure!(
            std::str::from_utf8(&old.bytes)?.trim() != manifest.version,
            "cannot replace the active version; choose a new version"
        );
    }
    let chunks = &manifest.chunks;
    let uploaded: Vec<(String, u64)> = stream::iter(files)
        .map(|(hash, file)| async move {
            let entry = chunks
                .get(&hash)
                .context("pending chunk missing from manifest")?;
            let size = ensure_chunk(
                client,
                target.bucket,
                &format!("{}/chunks/{hash}.zst", target.prefix),
                &hash,
                entry,
                &file,
            )
            .await?;
            Ok::<_, anyhow::Error>((hash, size))
        })
        .buffer_unordered(target.concurrency)
        .try_collect()
        .await?;
    for (hash, size) in uploaded {
        manifest.chunks.get_mut(&hash).unwrap().compressed_size = size;
    }
    // Inherited patch chunks are trusted base content, but every reference must still exist.
    // Clients independently verify hashes on download; patching does not download the old tree.
    for (hash, entry) in &manifest.chunks {
        let key = format!("{}/chunks/{hash}.zst", target.prefix);
        let remote = r2::head(client, target.bucket, &key)
            .await?
            .with_context(|| format!("required chunk missing: {hash}"))?;
        ensure!(
            remote.size == entry.compressed_size,
            "required chunk size changed: {hash}"
        );
    }
    validation::manifest(&manifest)?;
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    ensure!(
        bytes.len() as u64 <= validation::MAX_MANIFEST_BYTES,
        "manifest exceeds size limit"
    );
    let key = format!(
        "{}/versions/{}/manifest.json",
        target.prefix, manifest.version
    );
    r2::required_put(
        client,
        target.bucket,
        &key,
        bytes.clone(),
        ("application/json", IMMUTABLE_CACHE),
        &target.manifest_condition,
    )
    .await?;
    let visible = r2::get(client, target.bucket, &key, validation::MAX_MANIFEST_BYTES)
        .await?
        .context("published manifest is not observable")?;
    ensure!(
        visible.bytes == bytes,
        "published manifest differs from validated bytes"
    );
    r2::required_put(
        client,
        target.bucket,
        &format!("{}/latest.txt", target.prefix),
        format!("{}\n", manifest.version).into_bytes(),
        ("text/plain", NO_CACHE),
        &Condition::previous(previous),
    )
    .await?;
    println!("publish complete: version {}", manifest.version);
    Ok(())
}

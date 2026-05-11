//! Incremental publish: patch a previously-published version with a
//! small set of file changes, without re-walking the full content tree.
//!
//! The standard `release` flow chunks an entire local tree, uploads its
//! delta vs. the shared chunk pool, and flips the version pointer. That
//! works but requires the full tree on disk wherever the chunker runs —
//! a 5+ GB tarball pull for every single-row data edit. For repos with
//! frequent small edits (a single data file, a single map binary, a
//! script tweak), the cost is wildly disproportionate to the change.
//!
//! `patch` starts from a previously-published manifest instead. The
//! caller provides only the changed files; their chunks are produced
//! locally, deltas uploaded against the shared pool, and a new version
//! manifest is composed by overlaying the changes onto the base. The
//! full tree never has to exist locally.
//!
//! Output is byte-equivalent to what a full re-chunk would have produced
//! had its input tree contained the same overlaid state. Same chunk
//! pool, same manifest schema, same launcher behavior.
//!
//! ## Safety discipline
//!
//! Matches [`crate::publish`]: chunks land first, manifest second,
//! `latest.txt` flips last and only after the manifest is HEAD-visible.
//! Adds:
//!
//! - **Race detection** against `latest.txt`: if the explicit
//!   `--base-version` doesn't match the current pointer, refuse to
//!   proceed unless the caller acknowledges the staleness.
//! - **Overwrite guard**: refuse to publish over an existing version
//!   manifest unless `--force` is set.
//! - **Local pre-flight**: validate override paths, removal paths, and
//!   chunk-size compatibility before any network calls.

use crate::chunk::{for_each_chunk, now_iso8601};
use crate::manifest::{ChunkEntry, FileEntry, Manifest};
use crate::r2::{
    IMMUTABLE_CACHE, NO_CACHE, build_client, get_object_bytes, object_exists, put_bytes, put_string,
};
use anyhow::{Context, Result, anyhow};
use aws_sdk_s3::Client;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// One file replacement applied to the base manifest. `path` is the
/// manifest-relative key (forward slashes, exactly as it appears in
/// `manifest.files`); `local` is where the replacement bytes live.
#[derive(Clone)]
pub struct Override {
    pub path: String,
    pub local: PathBuf,
}

pub struct PatchOpts {
    pub bucket: String,
    pub prefix: String,
    /// Explicit base version. When `None`, resolves to the current
    /// `latest.txt` pointer.
    pub base_version: Option<String>,
    pub new_version: String,
    pub overrides: Vec<Override>,
    /// Manifest-relative paths to drop from the new version. Must
    /// already be present in the base manifest.
    pub removes: Vec<String>,
    pub chunk_size: u64,
    pub zstd_level: i32,
    pub concurrency: usize,
    /// Overwrite a target version manifest that already exists in R2.
    /// Without this, a duplicate version aborts the patch.
    pub force: bool,
    /// Proceed even when an explicit `--base-version` disagrees with
    /// the current `latest.txt`. Default is to refuse, on the
    /// assumption that the disagreement is a concurrent-publish race
    /// the caller didn't intend.
    pub allow_stale_base: bool,
}

pub async fn patch(opts: &PatchOpts) -> Result<()> {
    preflight(opts)?;

    let client = Arc::new(build_client().await?);
    let latest_key = format!("{}/latest.txt", opts.prefix);
    let current_latest = read_latest(&client, &opts.bucket, &latest_key).await?;

    let base_version = resolve_base_version(opts, &current_latest)?;

    let base_manifest_key = format!("{}/versions/{}/manifest.json", opts.prefix, base_version);
    println!(
        "patch: base={base_version} → new={} (pulling {}/{base_manifest_key})",
        opts.new_version, opts.bucket
    );
    let base_manifest = fetch_manifest(&client, &opts.bucket, &base_manifest_key).await?;

    validate_chunk_size(&base_manifest, opts.chunk_size, &base_version)?;
    validate_removes(&base_manifest, &opts.removes, &base_version)?;

    let new_manifest_key = format!(
        "{}/versions/{}/manifest.json",
        opts.prefix, opts.new_version
    );
    if !opts.force && object_exists(&client, &opts.bucket, &new_manifest_key).await? {
        return Err(anyhow!(
            "version manifest {new_manifest_key} already exists in {}; pass --force to overwrite",
            opts.bucket
        ));
    }

    let (new_manifest, pending_uploads) = build_new_manifest(opts, base_manifest)?;

    let new_chunk_count = pending_uploads.len();
    upload_new_chunks(
        &client,
        &opts.bucket,
        &opts.prefix,
        pending_uploads,
        opts.concurrency,
    )
    .await?;

    let manifest_json =
        serde_json::to_vec_pretty(&new_manifest).context("serialize new manifest to JSON")?;
    println!("uploading manifest → {}/{new_manifest_key}", opts.bucket);
    put_bytes(
        &client,
        &opts.bucket,
        manifest_json,
        &new_manifest_key,
        "application/json",
        IMMUTABLE_CACHE,
    )
    .await?;

    if !object_exists(&client, &opts.bucket, &new_manifest_key).await? {
        return Err(anyhow!(
            "manifest upload reported success but HEAD {new_manifest_key} returned 404; refusing to flip latest.txt"
        ));
    }

    println!(
        "flipping {}/{latest_key} → {}",
        opts.bucket, opts.new_version
    );
    put_string(
        &client,
        &opts.bucket,
        &format!("{}\n", opts.new_version),
        &latest_key,
        "text/plain",
        NO_CACHE,
    )
    .await?;

    println!(
        "patch complete: {} → {} ({} override{}, {} remove{}, {} new chunk{} uploaded)",
        base_version,
        opts.new_version,
        opts.overrides.len(),
        plural(opts.overrides.len()),
        opts.removes.len(),
        plural(opts.removes.len()),
        new_chunk_count,
        plural(new_chunk_count),
    );
    Ok(())
}

/// Local-only validation. No network, no S3 client. Failing here costs
/// nothing; failing later costs an in-flight publish.
fn preflight(opts: &PatchOpts) -> Result<()> {
    if opts.overrides.is_empty() && opts.removes.is_empty() {
        return Err(anyhow!(
            "patch with no --override and no --remove is a no-op; nothing to publish"
        ));
    }
    for o in &opts.overrides {
        if !o.local.is_file() {
            return Err(anyhow!(
                "override local file does not exist or is not a regular file: {}",
                o.local.display()
            ));
        }
        if o.path.contains('\\') || o.path.starts_with('/') {
            return Err(anyhow!(
                "override path must be manifest-style (forward slashes, no leading slash): {}",
                o.path
            ));
        }
    }
    if opts.chunk_size == 0 {
        return Err(anyhow!("--chunk-size must be positive"));
    }
    Ok(())
}

async fn read_latest(client: &Arc<Client>, bucket: &str, key: &str) -> Result<String> {
    let bytes = get_object_bytes(client, bucket, key)
        .await
        .with_context(|| {
            format!("read current pointer {key} (does the prefix exist? has any version been published?)")
        })?;
    let s = std::str::from_utf8(&bytes).with_context(|| format!("decode {key} as UTF-8"))?;
    Ok(s.trim().to_string())
}

fn resolve_base_version(opts: &PatchOpts, current_latest: &str) -> Result<String> {
    match &opts.base_version {
        None => Ok(current_latest.to_string()),
        Some(explicit) => {
            if !opts.allow_stale_base && explicit != current_latest {
                return Err(anyhow!(
                    "race: --base-version {explicit} but current latest.txt is {current_latest}. \
                     Pass --allow-stale-base to publish a fork-from-stale-base anyway."
                ));
            }
            Ok(explicit.clone())
        }
    }
}

async fn fetch_manifest(client: &Arc<Client>, bucket: &str, key: &str) -> Result<Manifest> {
    let bytes = get_object_bytes(client, bucket, key).await?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).with_context(|| format!("parse manifest at {key}"))?;
    Ok(manifest)
}

fn validate_chunk_size(base: &Manifest, requested: u64, base_version: &str) -> Result<()> {
    if base.chunk_size != requested {
        return Err(anyhow!(
            "chunk_size mismatch: base manifest v{base_version} has {} but --chunk-size is {}. \
             Different chunk sizes produce different chunk boundaries — chunks would not align with \
             the shared pool. Use `chunker release` for a full re-chunk instead.",
            base.chunk_size,
            requested
        ));
    }
    Ok(())
}

fn validate_removes(base: &Manifest, removes: &[String], base_version: &str) -> Result<()> {
    for rm in removes {
        if !base.files.contains_key(rm) {
            return Err(anyhow!(
                "--remove path {rm} not present in base manifest v{base_version}"
            ));
        }
    }
    Ok(())
}

/// Apply overrides + removes to a clone of the base manifest. Returns
/// the new manifest plus the list of new chunks that need uploading
/// (deduplicated by hash across all overrides). New chunks land in the
/// manifest.chunks index up front; orphan chunks (no longer referenced
/// by any file) are pruned at the end.
fn build_new_manifest(opts: &PatchOpts, base: Manifest) -> Result<(Manifest, Vec<PendingChunk>)> {
    let mut files: BTreeMap<String, FileEntry> = base.files;
    let mut chunks: BTreeMap<String, ChunkEntry> = base.chunks;
    let mut pending: Vec<PendingChunk> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut total_size: i64 = base.total_size as i64;

    for o in &opts.overrides {
        let prior_size = files.get(&o.path).map(|f| f.size as i64).unwrap_or(0);

        let (file_size, hashes) =
            for_each_chunk(&o.local, opts.chunk_size, |hash, uncompressed| {
                if seen.insert(hash.to_string()) && !chunks.contains_key(hash) {
                    let compressed = zstd::encode_all(uncompressed, opts.zstd_level)
                        .with_context(|| format!("zstd-compress chunk {hash}"))?;
                    chunks.insert(
                        hash.to_string(),
                        ChunkEntry {
                            size: uncompressed.len() as u64,
                            compressed_size: compressed.len() as u64,
                            url: format!("../../chunks/{hash}.zst"),
                        },
                    );
                    pending.push(PendingChunk {
                        hash: hash.to_string(),
                        compressed,
                    });
                }
                Ok(())
            })
            .with_context(|| format!("chunk override file {}", o.local.display()))?;

        files.insert(
            o.path.clone(),
            FileEntry {
                size: file_size,
                chunks: hashes,
            },
        );
        total_size += file_size as i64 - prior_size;
    }

    for rm in &opts.removes {
        if let Some(removed) = files.remove(rm) {
            total_size -= removed.size as i64;
        }
    }

    // Prune chunk entries no longer referenced by any file. The blobs
    // themselves remain in the shared R2 pool (other versions may still
    // reference them), but their metadata leaves this version's
    // manifest so the launcher's cache map doesn't track ghosts.
    let referenced: HashSet<String> = files.values().flat_map(|f| f.chunks.clone()).collect();
    chunks.retain(|hash, _| referenced.contains(hash));

    if total_size < 0 {
        return Err(anyhow!(
            "internal error: total_size went negative ({total_size}) — likely a manifest accounting bug"
        ));
    }

    let manifest = Manifest {
        version: opts.new_version.clone(),
        game_id: base.game_id,
        platform: base.platform,
        generated_at: now_iso8601(),
        chunk_size: base.chunk_size,
        total_size: total_size as u64,
        files,
        chunks,
    };
    Ok((manifest, pending))
}

struct PendingChunk {
    hash: String,
    compressed: Vec<u8>,
}

/// Concurrent upload of every pending chunk. Each upload does a HEAD
/// first — chunks are content-addressed so a hash collision means the
/// content is identical, and re-uploading the same bytes wastes
/// bandwidth + R2 PUT quota.
async fn upload_new_chunks(
    client: &Arc<Client>,
    bucket: &str,
    prefix: &str,
    pending: Vec<PendingChunk>,
    concurrency: usize,
) -> Result<()> {
    if pending.is_empty() {
        println!("no new chunks to upload");
        return Ok(());
    }

    let pb = ProgressBar::new(pending.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} new chunks ({eta})",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    pb.enable_steady_tick(Duration::from_millis(200));
    let pb_arc = Arc::new(pb);

    let prefix_arc: Arc<str> = Arc::from(prefix);
    let bucket_arc: Arc<str> = Arc::from(bucket);

    let results = stream::iter(pending)
        .map(|chunk| {
            let client = client.clone();
            let bucket = bucket_arc.clone();
            let prefix = prefix_arc.clone();
            let pb = pb_arc.clone();
            async move {
                let key = format!("{prefix}/chunks/{}.zst", chunk.hash);
                let r = upload_one(&client, &bucket, chunk, &key).await;
                pb.inc(1);
                r
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<Result<()>>>()
        .await;

    pb_arc.finish_with_message("uploaded new chunks");

    for r in results {
        r?;
    }
    Ok(())
}

async fn upload_one(
    client: &Arc<Client>,
    bucket: &str,
    chunk: PendingChunk,
    key: &str,
) -> Result<()> {
    if object_exists(client, bucket, key).await? {
        return Ok(());
    }
    put_bytes(
        client,
        bucket,
        chunk.compressed,
        key,
        "application/zstd",
        IMMUTABLE_CACHE,
    )
    .await
    .with_context(|| format!("upload chunk {} → {key}", chunk.hash))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

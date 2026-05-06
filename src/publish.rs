//! Publish a previously-chunked output dir to S3-compatible storage.
//!
//! Order is: chunks (concurrent), then per-version manifest, then
//! verify-manifest-landed via HEAD, then flip `latest.txt`. Any failure
//! before the flip leaves the previous `latest.txt` value intact, so
//! readers always see a self-consistent (chunks + manifest + pointer)
//! tuple — never a pointer to a manifest or chunks that don't exist.

use crate::r2::{NO_CACHE, build_client, object_exists, put_file, put_string};
use anyhow::{Context, Result, anyhow};
use aws_sdk_s3::Client;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use walkdir::WalkDir;

pub struct PublishOpts {
    pub chunks_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub version: String,
    pub bucket: String,
    pub prefix: String,
    pub concurrency: usize,
}

pub async fn publish(opts: &PublishOpts) -> Result<()> {
    if !opts.chunks_dir.exists() {
        return Err(anyhow!(
            "chunks dir does not exist: {}",
            opts.chunks_dir.display()
        ));
    }
    if !opts.manifest_path.exists() {
        return Err(anyhow!(
            "manifest does not exist: {}",
            opts.manifest_path.display()
        ));
    }

    let client = Arc::new(build_client().await?);

    upload_chunks(
        &client,
        &opts.bucket,
        &opts.chunks_dir,
        &format!("{}/chunks", opts.prefix),
        opts.concurrency,
    )
    .await?;

    let manifest_key = format!("{}/versions/{}/manifest.json", opts.prefix, opts.version);
    println!("uploading manifest → {}/{manifest_key}", opts.bucket);
    put_file(&client, &opts.bucket, &opts.manifest_path, &manifest_key).await?;

    // Verify manifest is observable via HEAD before flipping the pointer.
    // Belt-and-suspenders: PUT returned success, but if a transparent
    // retry / replication anomaly meant readers can't see it yet, flipping
    // latest.txt now would point at a 404. Refuse to flip in that case.
    if !object_exists(&client, &opts.bucket, &manifest_key).await? {
        return Err(anyhow!(
            "manifest upload reported success but HEAD {manifest_key} returned 404; refusing to flip latest.txt"
        ));
    }

    let latest_key = format!("{}/latest.txt", opts.prefix);
    println!("flipping {}/{latest_key} → {}", opts.bucket, opts.version);
    put_string(
        &client,
        &opts.bucket,
        &format!("{}\n", opts.version),
        &latest_key,
        "text/plain",
        NO_CACHE,
    )
    .await?;

    println!("publish complete: version {}", opts.version);
    Ok(())
}

/// Concurrent upload of every file under `chunks_dir`, naming each
/// object as `<key_prefix>/<filename>`.
async fn upload_chunks(
    client: &Arc<Client>,
    bucket: &str,
    chunks_dir: &Path,
    key_prefix: &str,
    concurrency: usize,
) -> Result<()> {
    let jobs = collect_jobs(chunks_dir, key_prefix)?;
    let total = jobs.len();
    if total == 0 {
        println!("no chunks to upload");
        return Ok(());
    }

    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} chunks ({eta})",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    pb.enable_steady_tick(Duration::from_millis(200));

    let pb_arc = Arc::new(pb);
    let results = stream::iter(jobs)
        .map(|job| {
            let client = client.clone();
            let bucket = bucket.to_string();
            let pb = pb_arc.clone();
            async move {
                let result = put_file(&client, &bucket, &job.local, &job.key)
                    .await
                    .with_context(|| format!("upload {}", job.key));
                pb.inc(1);
                result
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<Result<()>>>()
        .await;

    pb_arc.finish_with_message("uploaded");

    for r in results {
        r?;
    }
    Ok(())
}

struct UploadJob {
    local: PathBuf,
    key: String,
}

fn collect_jobs(dir: &Path, key_prefix: &str) -> Result<Vec<UploadJob>> {
    let mut jobs = Vec::new();
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let local = entry.path().to_path_buf();
            let name = local
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("non-UTF-8 chunk filename: {}", local.display()))?;
            jobs.push(UploadJob {
                local: local.clone(),
                key: format!("{key_prefix}/{name}"),
            });
        }
    }
    Ok(jobs)
}

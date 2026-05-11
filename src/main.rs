//! `chunker` — cross-platform CDN content chunker. CLI entry point.
//!
//! Four subcommands:
//!
//! - `chunk`   — produce content-addressed chunks + manifest from a directory
//! - `publish` — upload a chunk dir + manifest, verify the manifest is
//!   observable, then flip `latest.txt`
//! - `release` — one-shot of `chunk` + `publish`
//! - `patch`   — incrementally republish from a base version with a small
//!   set of file overrides or removes; no full tree required locally
//!
//! Chunk hashes are SHA-256 of the **uncompressed** bytes; the on-disk
//! `<hash>.zst` is the zstd-level-12 compression of those same bytes. The
//! launcher decompresses and re-hashes on download to detect corruption,
//! so it doesn't matter that compressed bytes can drift between zstd
//! library versions — only the uncompressed hash is the contract.

mod chunk;
mod manifest;
mod patch;
mod publish;
mod r2;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_ZSTD_LEVEL: i32 = 12;
const DEFAULT_CONCURRENCY: usize = 16;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Chunk a directory into content-addressed pieces + manifest.
    Chunk(ChunkArgs),
    /// Publish a previously-chunked output dir to S3-compatible storage.
    Publish(PublishArgs),
    /// Chunk + publish in one pass.
    Release(ReleaseArgs),
    /// Patch a published version with a small set of overrides /
    /// removals. Starts from a base manifest in R2 instead of walking
    /// a local tree, so single-file edits do not require the full
    /// content tree on the runner.
    Patch(PatchArgs),
}

#[derive(clap::Args)]
struct ChunkArgs {
    /// Source directory to chunk.
    #[arg(long)]
    input: PathBuf,
    /// Output directory (will be created). Receives `manifest.json` and
    /// `chunks/<sha256>.zst`.
    #[arg(long)]
    output: PathBuf,
    /// Version string recorded in the manifest.
    #[arg(long)]
    version: String,
    /// Game / app identifier recorded in the manifest.
    #[arg(long)]
    game: String,
    /// Target platform recorded in the manifest (e.g. `win`, `mac`,
    /// `linux`).
    #[arg(long)]
    platform: String,
    /// Chunk size in bytes (default 4 MiB).
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: u64,
    /// zstd compression level (default 12 — speed/ratio sweet spot;
    /// level 19 is ~5% smaller but ~10× slower).
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    zstd_level: i32,
}

#[derive(clap::Args)]
struct PublishArgs {
    /// Directory containing `manifest.json` and `chunks/<sha256>.zst`
    /// (the output of a prior `chunker chunk`).
    #[arg(long)]
    chunks: PathBuf,
    /// Version string. Must match the version inside the manifest;
    /// determines the `<prefix>/versions/<v>/` upload path and the
    /// content of `latest.txt`.
    #[arg(long)]
    version: String,
    /// Target bucket name.
    #[arg(long)]
    bucket: String,
    /// Top-level prefix inside the bucket (e.g. `client`).
    #[arg(long)]
    prefix: String,
    /// Concurrent chunk uploads (default 16).
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,
}

#[derive(clap::Args)]
struct ReleaseArgs {
    /// Source directory to chunk + publish.
    #[arg(long)]
    input: PathBuf,
    /// Version string for both the manifest and the `latest.txt` flip.
    #[arg(long)]
    version: String,
    /// Game / app identifier.
    #[arg(long)]
    game: String,
    /// Target platform.
    #[arg(long)]
    platform: String,
    /// Target bucket.
    #[arg(long)]
    bucket: String,
    /// Top-level prefix.
    #[arg(long)]
    prefix: String,
    /// Chunk size in bytes (default 4 MiB).
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: u64,
    /// zstd compression level (default 12).
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    zstd_level: i32,
    /// Concurrent chunk uploads (default 16).
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,
    /// Optional working directory for chunk output (default: a temp dir
    /// that's removed on success).
    #[arg(long)]
    work_dir: Option<PathBuf>,
}

#[derive(clap::Args)]
struct PatchArgs {
    /// Target bucket (must contain the prior version's manifest + the
    /// shared chunk pool).
    #[arg(long)]
    bucket: String,
    /// Top-level prefix inside the bucket (e.g. `client`).
    #[arg(long)]
    prefix: String,
    /// Explicit base version to patch from. When omitted, the current
    /// `<prefix>/latest.txt` value is used.
    #[arg(long)]
    base_version: Option<String>,
    /// Version string written into the new manifest and into
    /// `latest.txt` after the publish succeeds.
    #[arg(long)]
    version: String,
    /// File replacement: `<manifest-path>=<local-file>`. Repeatable.
    /// The local file is chunked at the same `--chunk-size` as the base
    /// manifest; new chunks land in the shared pool, existing chunks
    /// are skipped via HEAD check. The manifest path uses forward
    /// slashes and is relative to the content root (the same form
    /// stored in `manifest.files`).
    #[arg(long = "override", value_parser = parse_override)]
    overrides: Vec<patch::Override>,
    /// Manifest-relative path to remove from the new version.
    /// Repeatable. Must already be present in the base manifest.
    #[arg(long = "remove")]
    removes: Vec<String>,
    /// Chunk size in bytes (default 4 MiB). Must match the base
    /// manifest's chunk size — different sizes produce different chunk
    /// boundaries and would not share the chunk pool.
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: u64,
    /// zstd compression level (default 12).
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    zstd_level: i32,
    /// Concurrent chunk uploads (default 16).
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,
    /// Overwrite the target version manifest if it already exists in
    /// R2. Without this flag, a duplicate version is a fail-loud abort.
    #[arg(long, default_value_t = false)]
    force: bool,
    /// Proceed even when `--base-version` disagrees with the current
    /// `latest.txt`. Default refuses to publish in that case, on the
    /// assumption that the disagreement is a race the caller did not
    /// intend (someone else published since you fetched the base).
    #[arg(long, default_value_t = false)]
    allow_stale_base: bool,
}

fn parse_override(spec: &str) -> Result<patch::Override, String> {
    let Some((path, local)) = spec.split_once('=') else {
        return Err(format!(
            "expected MANIFEST_PATH=LOCAL_FILE (no `=` found): {spec}"
        ));
    };
    let path = path.trim();
    let local = local.trim();
    if path.is_empty() || local.is_empty() {
        return Err(format!(
            "MANIFEST_PATH and LOCAL_FILE must both be non-empty: {spec}"
        ));
    }
    Ok(patch::Override {
        path: path.to_string(),
        local: PathBuf::from(local),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Chunk(args) => run_chunk(args),
        Cmd::Publish(args) => run_publish(args).await,
        Cmd::Release(args) => run_release(args).await,
        Cmd::Patch(args) => run_patch(args).await,
    }
}

fn run_chunk(args: ChunkArgs) -> anyhow::Result<()> {
    chunk::chunk(&chunk::ChunkOpts {
        input: args.input,
        output: args.output,
        version: args.version,
        game: args.game,
        platform: args.platform,
        chunk_size: args.chunk_size,
        zstd_level: args.zstd_level,
    })?;
    Ok(())
}

async fn run_publish(args: PublishArgs) -> anyhow::Result<()> {
    publish::publish(&publish::PublishOpts {
        chunks_dir: args.chunks.join("chunks"),
        manifest_path: args.chunks.join("manifest.json"),
        version: args.version,
        bucket: args.bucket,
        prefix: args.prefix,
        concurrency: args.concurrency,
    })
    .await
}

async fn run_release(args: ReleaseArgs) -> anyhow::Result<()> {
    let work_dir = match args.work_dir {
        Some(p) => p,
        None => std::env::temp_dir().join(format!(
            "chunker-{}-{}",
            args.game,
            chrono::Utc::now().timestamp()
        )),
    };

    chunk::chunk(&chunk::ChunkOpts {
        input: args.input,
        output: work_dir.clone(),
        version: args.version.clone(),
        game: args.game,
        platform: args.platform,
        chunk_size: args.chunk_size,
        zstd_level: args.zstd_level,
    })?;

    publish::publish(&publish::PublishOpts {
        chunks_dir: work_dir.join("chunks"),
        manifest_path: work_dir.join("manifest.json"),
        version: args.version,
        bucket: args.bucket,
        prefix: args.prefix,
        concurrency: args.concurrency,
    })
    .await
}

async fn run_patch(args: PatchArgs) -> anyhow::Result<()> {
    patch::patch(&patch::PatchOpts {
        bucket: args.bucket,
        prefix: args.prefix,
        base_version: args.base_version,
        new_version: args.version,
        overrides: args.overrides,
        removes: args.removes,
        chunk_size: args.chunk_size,
        zstd_level: args.zstd_level,
        concurrency: args.concurrency,
        force: args.force,
        allow_stale_base: args.allow_stale_base,
    })
    .await
}

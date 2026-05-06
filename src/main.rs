//! `chunker` — cross-platform CDN content chunker. CLI entry point.
//!
//! Three subcommands:
//!
//! - `chunk`   — produce content-addressed chunks + manifest from a directory
//! - `publish` — upload a chunk dir + manifest to S3-compatible storage,
//!   then atomically flip `latest.txt`
//! - `release` — one-shot of `chunk` + `publish`
//!
//! Chunk hashes are SHA-256 of the **uncompressed** bytes; the on-disk
//! `<hash>.zst` is the zstd-level-12 compression of those same bytes. The
//! launcher decompresses and re-hashes on download to detect corruption,
//! so it doesn't matter that compressed bytes can drift between zstd
//! library versions — only the uncompressed hash is the contract.

mod chunk;
mod manifest;
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
    /// zstd compression level (default 12 — sweet spot per upstream JS
    /// chunker; level 19 was overkill, builds took forever for ~5%
    /// smaller).
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Chunk(args) => run_chunk(args),
        Cmd::Publish(args) => run_publish(args).await,
        Cmd::Release(args) => run_release(args).await,
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

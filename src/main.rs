//! Command-line entry point for chunking, publishing and incremental patches.

mod chunk;
mod manifest;
mod patch;
mod publish;
mod r2;
mod temporary;
mod validation;

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
    /// Split a directory into content-addressed chunks and a manifest.
    Chunk(ChunkArgs),
    /// Publish existing chunks and a manifest to S3-compatible storage.
    Publish(PublishArgs),
    /// Chunk and publish a directory in one command.
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
    /// zstd compression level (default 12).
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
    /// Source directory to chunk and publish.
    #[arg(long)]
    input: PathBuf,
    /// Version recorded in the manifest and `latest.txt`.
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
    /// Optional working directory for chunk output. The default temporary
    /// directory is removed when the command finishes.
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
    /// manifest. Existing encodings are verified and reused. Manifest paths
    /// use forward slashes and are relative to the content root.
    #[arg(long = "override", value_parser = parse_override)]
    overrides: Vec<patch::Override>,
    /// Manifest-relative path to remove from the new version.
    /// Repeatable. Must already be present in the base manifest.
    #[arg(long = "remove")]
    removes: Vec<String>,
    /// Chunk size in bytes (default 4 MiB). Must match the base
    /// manifest's chunk size to preserve chunk boundaries and reuse content.
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: u64,
    /// zstd compression level (default 12).
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    zstd_level: i32,
    /// Concurrent chunk uploads (default 16).
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,
    /// Retired: published versions cannot be overwritten. Use a new version.
    #[arg(long, hide = true, default_value_t = false)]
    force: bool,
    /// Proceed even when `--base-version` disagrees with the current
    /// `latest.txt`. The final pointer update still rejects concurrent changes.
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
    validation::identifier(&args.version)?;
    validation::relative_path(&args.prefix)?;
    validation::concurrency(args.concurrency)?;
    let temporary = if args.work_dir.is_none() {
        Some(temporary::Directory::new()?)
    } else {
        None
    };
    let work_dir = args
        .work_dir
        .unwrap_or_else(|| temporary.as_ref().unwrap().path().to_path_buf());

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

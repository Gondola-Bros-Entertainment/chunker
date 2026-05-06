//! S3-compatible client setup and small helpers. Defaults to Cloudflare
//! R2 when `R2_ACCOUNT_ID` is set; otherwise falls back to the standard
//! AWS env (`AWS_ENDPOINT_URL`, `AWS_REGION`, etc.) so the same binary
//! can publish to any S3-compatible target.

use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use std::path::Path;

/// Build an S3 client wired for the configured endpoint.
///
/// Credential precedence:
/// 1. `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` (explicit R2 envs)
/// 2. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (standard AWS envs)
///
/// Endpoint:
/// - `R2_ACCOUNT_ID` set → `https://<account>.r2.cloudflarestorage.com`
/// - otherwise → `AWS_ENDPOINT_URL` (or standard region resolution)
pub async fn build_client() -> Result<Client> {
    let creds = resolve_credentials()?;
    let region = aws_sdk_s3::config::Region::new(env_or("AWS_REGION", "auto"));

    let mut builder = aws_sdk_s3::config::Builder::new()
        .region(region)
        .credentials_provider(creds)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());

    if let Ok(account_id) = std::env::var("R2_ACCOUNT_ID") {
        builder = builder.endpoint_url(format!("https://{account_id}.r2.cloudflarestorage.com"));
    } else if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
        builder = builder.endpoint_url(endpoint);
    }

    Ok(Client::from_conf(builder.build()))
}

fn resolve_credentials() -> Result<Credentials> {
    let access = first_non_empty(&["R2_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"])
        .context("missing access key id (R2_ACCESS_KEY_ID or AWS_ACCESS_KEY_ID)")?;
    let secret = first_non_empty(&["R2_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"])
        .context("missing secret access key (R2_SECRET_ACCESS_KEY or AWS_SECRET_ACCESS_KEY)")?;
    Ok(Credentials::new(access, secret, None, None, "chunker-env"))
}

fn first_non_empty(names: &[&str]) -> Option<String> {
    for name in names {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => return Some(v),
            _ => continue,
        }
    }
    None
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Upload a file with a known content type and a long-lived immutable
/// cache directive (the default for content-addressed chunks + version
/// manifests).
pub async fn put_file(client: &Client, bucket: &str, local: &Path, key: &str) -> Result<()> {
    let body = ByteStream::from_path(local)
        .await
        .with_context(|| format!("read {}", local.display()))?;
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .content_type(content_type_for(local))
        .cache_control(IMMUTABLE_CACHE)
        .send()
        .await
        .with_context(|| format!("PUT {key}"))?;
    Ok(())
}

/// Upload a small in-memory string. Used for `latest.txt`, which gets a
/// no-cache directive so launchers see the flip immediately.
pub async fn put_string(
    client: &Client,
    bucket: &str,
    body: &str,
    key: &str,
    content_type: &str,
    cache_control: &str,
) -> Result<()> {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body.as_bytes().to_vec()))
        .content_type(content_type)
        .cache_control(cache_control)
        .send()
        .await
        .with_context(|| format!("PUT {key}"))?;
    Ok(())
}

const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
pub const NO_CACHE: &str = "public, max-age=0, must-revalidate";

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => "application/json",
        Some("yml") | Some("yaml") => "text/yaml",
        Some("txt") => "text/plain",
        Some("zst") => "application/zstd",
        Some("exe") | Some("bin") => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

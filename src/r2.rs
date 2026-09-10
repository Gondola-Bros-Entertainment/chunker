//! Bounded S3 reads and conditional writes. Credentials remain process-local.
use anyhow::{Context, Result, bail, ensure};
use aws_sdk_s3::{Client, config::Credentials, primitives::ByteStream};

pub const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
pub const NO_CACHE: &str = "public, max-age=0, must-revalidate";

pub async fn build_client() -> Result<Client> {
    let first = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    };
    let credentials = Credentials::new(
        first(&["R2_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"]).context("missing S3 access key")?,
        first(&["R2_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"])
            .context("missing S3 secret key")?,
        None,
        None,
        "chunker-env",
    );
    let mut builder = aws_sdk_s3::config::Builder::new()
        .region(aws_sdk_s3::config::Region::new(
            std::env::var("AWS_REGION").unwrap_or_else(|_| "auto".into()),
        ))
        .credentials_provider(credentials)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());
    if let Ok(account) = std::env::var("R2_ACCOUNT_ID") {
        ensure!(
            account.len() == 32 && account.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid R2 account ID"
        );
        builder = builder.endpoint_url(format!("https://{account}.r2.cloudflarestorage.com"));
    } else if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
        builder = builder.endpoint_url(endpoint);
    }
    Ok(Client::from_conf(builder.build()))
}

pub struct Object {
    pub bytes: Vec<u8>,
    pub etag: String,
}
pub struct Metadata {
    pub size: u64,
}

pub async fn get(client: &Client, bucket: &str, key: &str, limit: u64) -> Result<Option<Object>> {
    let response = match client.get_object().bucket(bucket).key(key).send().await {
        Ok(r) => r,
        Err(e) if e.raw_response().is_some_and(|r| r.status().as_u16() == 404) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("GET {key}")),
    };
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n >= 0 && n as u64 <= limit),
        "object exceeds limit: {key}"
    );
    let etag = response
        .e_tag()
        .context("S3 response has no ETag")?
        .to_owned();
    let mut body = response.body;
    let mut bytes = Vec::new();
    while let Some(part) = body.next().await {
        let part = part?;
        ensure!(
            part.len() as u64 <= limit - bytes.len() as u64,
            "object exceeds limit: {key}"
        );
        bytes.extend_from_slice(&part);
    }
    Ok(Some(Object { bytes, etag }))
}

pub async fn head(client: &Client, bucket: &str, key: &str) -> Result<Option<Metadata>> {
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(r) => Ok(Some(Metadata {
            size: r
                .content_length()
                .filter(|&n| n >= 0)
                .context("S3 response has no size")? as u64,
        })),
        Err(e) if e.raw_response().is_some_and(|r| r.status().as_u16() == 404) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("HEAD {key}")),
    }
}

#[derive(Clone)]
pub enum Condition {
    Absent,
    Match(String),
}
impl Condition {
    pub fn previous(value: Option<&Object>) -> Self {
        value.map_or(Self::Absent, |v| Self::Match(v.etag.clone()))
    }
}

// false means another writer changed the object. Never retry unconditionally.
pub async fn put(
    client: &Client,
    bucket: &str,
    key: &str,
    bytes: Vec<u8>,
    metadata: (&str, &str),
    condition: &Condition,
) -> Result<bool> {
    let request = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(bytes))
        .content_type(metadata.0)
        .cache_control(metadata.1);
    let request = match condition {
        Condition::Absent => request.if_none_match("*"),
        Condition::Match(etag) => request.if_match(etag),
    };
    match request.send().await {
        Ok(_) => Ok(true),
        Err(e)
            if e.raw_response()
                .is_some_and(|r| matches!(r.status().as_u16(), 409 | 412)) =>
        {
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| format!("conditional PUT {key}")),
    }
}

pub async fn required_put(
    client: &Client,
    bucket: &str,
    key: &str,
    bytes: Vec<u8>,
    metadata: (&str, &str),
    condition: &Condition,
) -> Result<()> {
    if !put(client, bucket, key, bytes, metadata, condition).await? {
        bail!("concurrent publication changed {key}; refusing to overwrite it");
    }
    Ok(())
}

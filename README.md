# chunker

Cross-platform CDN content chunker. Splits a directory of files into
content-addressed, zstd-compressed chunks; emits a JSON manifest; publishes
chunks + manifest to S3-compatible storage with an atomic-ish version flip.

Designed for game/app content distribution where:

- Releases are large (multi-GB) but each release changes only a few files.
- Downloaders fetch only the changed chunks via a content-addressed pool.
- Per-version manifests are immutable; a single `latest.txt` pointer flips
  atomically when a release is ready.

## Install

Pre-built binaries on each release (Linux x86_64, Windows x86_64). On
macOS: `cargo build --release` locally — Apple Silicon does it in ~60s
and CI macOS minutes are a 10× billing multiplier not worth burning for
this. From CI runners:

```bash
curl -L https://github.com/Gondola-Bros-Entertainment/chunker/releases/download/v0.1.0/chunker-linux-x64 -o chunker
chmod +x chunker
```

Or build from source:

```bash
cargo install --path .
```

## Usage

Three subcommands. Each one composable; `release` is a one-shot of `chunk`
+ `publish`.

```bash
# Chunk a directory locally.
chunker chunk \
  --input ./client-build \
  --output ./out \
  --version 1.0.2 \
  --game my-game \
  --platform win

# Publish a previously-chunked output dir to S3-compatible storage.
chunker publish \
  --chunks ./out \
  --version 1.0.2 \
  --bucket my-content-bucket \
  --prefix client

# One-shot: chunk + publish.
chunker release \
  --input ./client-build \
  --version 1.0.2 \
  --game my-game \
  --platform win \
  --bucket my-content-bucket \
  --prefix client
```

## Environment

`publish` and `release` read S3 credentials from environment:

- `R2_ACCOUNT_ID` — Cloudflare account id (32 hex chars). For non-R2 S3
  endpoints, leave unset and use `AWS_ENDPOINT_URL`.
- `R2_ACCESS_KEY_ID`
- `R2_SECRET_ACCESS_KEY`

Equivalent `AWS_*` envs are also accepted; if both are present, `R2_*`
wins.

## Output layout

After `chunker chunk --output ./out`:

```
out/
├── manifest.json
└── chunks/
    └── <sha256>.zst
```

After `chunker publish` to bucket `B` with prefix `P`:

```
B/
├── P/versions/<version>/manifest.json   (immutable per release)
├── P/chunks/<sha256>.zst                (shared content-addressed pool)
└── P/latest.txt                         (rewritten last — atomic-ish flip)
```

## Manifest format

```json
{
  "version": "1.0.2",
  "gameId": "my-game",
  "platform": "win",
  "generatedAt": "2026-05-06T14:30:00.000Z",
  "chunkSize": 4194304,
  "totalSize": 5644321870,
  "files": {
    "assets/data/example.bin": {
      "size": 5702864,
      "chunks": ["a1b2…", "c3d4…"]
    }
  },
  "chunks": {
    "a1b2…": {
      "size": 4194304,
      "compressedSize": 2103456,
      "url": "../../chunks/a1b2….zst"
    }
  }
}
```

## License

MIT

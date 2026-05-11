# chunker

Cross-platform CDN content chunker. Splits a directory of files into
content-addressed, zstd-compressed chunks; emits a JSON manifest; publishes
chunks + manifest to S3-compatible storage and flips a version pointer
**only after** verifying the manifest is observable.

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

Four subcommands. `chunk` / `publish` / `release` walk a local content
tree; `patch` re-publishes from a base manifest in R2 without touching
the full tree.

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

# Patch a published version with a small set of file changes — no
# local tree required, only the changed files themselves.
chunker patch \
  --bucket my-content-bucket \
  --prefix client \
  --version 1.0.3 \
  --override resmap/field/Hub/Hub.shbd=/tmp/Hub.shbd \
  --override 9Data/Shine/ClassName.shn=/tmp/ClassName.shn \
  --remove resmap/field/Old/Old.shbd
```

### When to use `patch`

`release` re-hashes the entire content tree on every publish, which
requires the full tree (multi-GB for game content) on the runner. For
the common case of a tiny edit — one row in a data file, one map
binary, one script — the cost is wildly disproportionate to the change.

`patch` starts from the previously-published manifest in R2 instead.
Override files are chunked locally, deltas land in the shared pool,
and a new manifest is composed by overlaying the changes onto the
base. The full tree never has to exist locally; the runner only needs
the override files themselves.

Output is byte-equivalent to what a full re-chunk would have produced
had its input tree contained the same overlaid state.

#### Safety discipline

- `latest.txt` is read at start and compared to `--base-version` (or
  used as the implicit base). A mismatch aborts the patch — pass
  `--allow-stale-base` to override if you genuinely want a stale-base
  fork.
- Refuses to publish over an existing target-version manifest unless
  `--force` is set.
- `--chunk-size` is validated against the base manifest's chunk size;
  patches across different chunk sizes are rejected (they would not
  share chunk boundaries with the existing pool).
- Override paths and removal paths are validated locally before any
  network call. Local file existence is checked up front. Failing
  before opening the S3 client keeps half-applied state off the table.
- Chunks are uploaded first, then the new manifest, then `latest.txt`
  flips last — and only after a HEAD confirms the manifest is
  observable. Any failure before the flip leaves the previous
  `latest.txt` value intact.

#### `--override` syntax

`--override MANIFEST_PATH=LOCAL_FILE`, repeatable. The manifest path
is exactly the key as it appears in `manifest.files` — forward
slashes, no leading slash, relative to the content root. Example:
`resmap/field/Hub/Hub.shbd=/tmp/Hub.shbd` replaces the chunks for
`resmap/field/Hub/Hub.shbd` in the new manifest with the chunks of
`/tmp/Hub.shbd`.

Adding an entirely new file (no entry in the base) works the same
way — the path is inserted rather than replaced.

#### `--remove` syntax

`--remove MANIFEST_PATH`, repeatable. The path must already exist in
the base manifest (otherwise the patch aborts before any network
calls). Removed paths are dropped from the new manifest's `files`
map; their chunks stay in the shared pool but are pruned from this
manifest's `chunks` index if no other file references them.

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
└── P/latest.txt                         (flipped last, after HEAD-verifying the manifest landed)
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

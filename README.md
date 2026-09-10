# Chunker

Chunker splits a directory into SHA-256-addressed, zstd-compressed chunks and
publishes a versioned manifest to S3-compatible storage, including Cloudflare R2.
A patch reads the previous manifest and processes only replacement files.

## Install

Download an executable and its SHA-256 checksum from
[Releases](https://github.com/Gondola-Bros-Entertainment/chunker/releases/latest):

| Platform | Executable |
| --- | --- |
| Linux x86-64 | `chunker-linux-x64` |
| Windows x86-64 | `chunker-windows-x64.exe` |
| macOS Apple Silicon | `chunker-macos-arm64` |

On Linux/macOS, verify the checksum, rename the executable to `chunker`, and make
it executable with `chmod +x chunker`. The macOS executable is ad hoc signed,
not notarized. Intel Macs can build from source.

To build from source, install stable Rust and run:

```sh
cargo install --path . --locked
```

## Usage

```sh
# Write chunks and a manifest locally. Keep output outside the input tree.
chunker chunk --input ./client --output ./out \
  --version 1.0.0 --game my-game --platform win

# Publish that exact version.
chunker publish --chunks ./out --version 1.0.0 \
  --bucket builds --prefix my-game/win

# Or chunk and publish in one command.
chunker release --input ./client --version 1.0.1 --game my-game --platform win \
  --bucket builds --prefix my-game/win

# Replace one file and remove another without downloading the old tree.
chunker patch --bucket builds --prefix my-game/win \
  --base-version 1.0.1 --version 1.0.2 \
  --override assets/world.json=./world.json --remove assets/old.json
```

`--override MANIFEST_PATH=LOCAL_FILE` and `--remove MANIFEST_PATH` can be repeated.
An override can add a new file. A removal must name a file in the base manifest.
Duplicate paths and overlapping overrides/removals are rejected.

Chunk size defaults to 4 MiB; zstd level defaults to 12. `--chunk-size` accepts
1 byte through 64 MiB. Patches must use the base manifest's chunk size. Upload
concurrency defaults to 16 and accepts 1 to 64. New patch chunks are staged in a
temporary directory, so their compressed contents do not all stay in memory.
`release` removes its automatically created working directory on success or
failure; a supplied `--work-dir` is retained.

Run `chunker <command> --help` for the full option list.

## Credentials

Publishing uses `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`, falling back to
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. Set `R2_ACCOUNT_ID` for R2, or
`AWS_ENDPOINT_URL` for another S3 endpoint. `AWS_REGION` defaults to `auto`.
Credentials are read from the environment and are not written to output files.

The storage service must support conditional `PutObject` requests with
`If-Match` and `If-None-Match`. [Cloudflare R2 supports both](https://developers.cloudflare.com/r2/api/s3/api/).

## Validation and publication

Chunker rejects missing or unreadable input, symlinks, unsafe paths, path
collisions, invalid manifests, missing chunks, corrupt compressed data, and
version mismatches. Paths use `/`, cannot escape the content root, and must be
compatible with Windows filenames. OS metadata files such as `.DS_Store` and
AppleDouble files are excluded.

Before publication, local chunks are decompressed with a size limit and checked
against their SHA-256 hashes. Existing chunks are never overwritten. If another
zstd configuration produced the same raw content, Chunker verifies and reuses the
stored encoding and records its compressed size in the new manifest.

Chunks are published first. Every referenced chunk, including inherited patch
chunks, must exist with the expected size. Chunker then writes a new version
manifest, reads it back, and compares its bytes before updating `latest.txt`.
That final update uses the ETag read at the start. If another publisher changed
the pointer, the update fails and preserves the other publisher's value.
An interrupted or competing publication may leave unused chunks or an inactive
version manifest; retry with a new version identifier.

Published version manifests cannot be overwritten. The old `--force` option is
rejected. `--allow-stale-base` permits an intentional fork from an older base,
but still checks for a competing pointer update during publication.

Patches validate the base manifest and check inherited object sizes without
redownloading the old content. The bucket and base publication are trusted.
Chunker does not sign releases, authenticate users, or install files. Publishers
own signing and release policy; installers must verify that authority and the
bytes they download. SHA-256 checks alone do not authenticate a publisher.

## Storage format

```text
<prefix>/chunks/<sha256>.zst
<prefix>/versions/<version>/manifest.json
<prefix>/latest.txt
```

The local `chunk` output contains `chunks/` and `manifest.json`. Each manifest
stores `version`, `gameId`, `platform`, `generatedAt`, `chunkSize`, `totalSize`,
`files` and `chunks`. File entries contain a byte `size` and an ordered list of
chunk hashes; chunk entries contain `size`, `compressedSize` and the relative
URL `../../chunks/<sha256>.zst`. Empty files have size zero and no chunks.
Concatenating a file's decompressed chunks reconstructs its original bytes.
The manifest format is unchanged in 0.2.1.

## Development and releases

Tests require Python 3.11+ in addition to Rust. They use synthetic files and a
loopback S3 fixture with fake credentials; no R2 account is needed.

```sh
bash scripts/check.sh
```

This runs rustfmt, Clippy, a release build, Rust unit tests, and CLI regressions.
CI runs the same checks on Linux, Windows and macOS. Dependencies are resolved
from `Cargo.lock`, and workflow actions are pinned to commits.

To release, update `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md`, merge the change,
then push the matching `vX.Y.Z` tag. The release workflow creates a draft, tests
and uploads all three executables and their checksums, then publishes only when
all platforms pass. A failed run leaves the draft unpublished. A manual retry
must select the same version tag; already public releases are not overwritten.

## License

MIT. See [LICENSE](LICENSE).

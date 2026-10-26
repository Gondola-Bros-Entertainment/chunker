# Chunker

Chunker splits a directory into SHA-256-addressed, zstd-compressed chunks and
publishes a versioned manifest to S3-compatible storage, including Cloudflare R2.
Every version under a prefix shares one chunk pool, so a new version uploads only
chunks the pool lacks, and a downloader can fetch only the chunks it does not
have. A patch reads the base manifest from storage and processes only
replacement files.

## Install

Download an executable and its `.sha256` file from
[Releases](https://github.com/Gondola-Bros-Entertainment/chunker/releases/latest):

| Platform | Executable |
| --- | --- |
| Linux x86-64 | `chunker-linux-x64` |
| Windows x86-64 | `chunker-windows-x64.exe` |
| macOS Apple Silicon | `chunker-macos-arm64` |

Check the download with
`sha256sum -c chunker-linux-x64.sha256` on Linux or
`shasum -a 256 -c chunker-macos-arm64.sha256` on macOS, then rename the
executable to `chunker` and make it executable with `chmod +x chunker`. On
Windows, compare the output of `Get-FileHash chunker-windows-x64.exe` with the
hash in its `.sha256` file. The macOS executable is ad hoc signed, not
notarized; if macOS blocks it, run `xattr -d com.apple.quarantine chunker`.
Intel Macs can build from source.

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

`--override MANIFEST_PATH=LOCAL_FILE` and `--remove MANIFEST_PATH` can be repeated,
and a patch needs at least one of them. An override can add a new file. A removal
must name a file in the base manifest. Duplicate paths and overlapping
overrides/removals are rejected, ignoring case. Without `--base-version`, a patch
builds on the version that `latest.txt` names. Any other `--base-version` fails
unless you pass `--allow-stale-base`, which publishes a fork of that version as
the new latest.

Chunk size defaults to 4 MiB; zstd level defaults to 12. `--chunk-size` accepts
1 byte through 64 MiB. Patches reuse the base manifest's chunk size. Upload
concurrency defaults to 16 and accepts 1 to 64. `release` writes the compressed
tree to a new directory under the system temporary directory and removes it when
it finishes, even on failure. If that location lacks space, pass `--work-dir` to
use another directory, which is kept.

Run `chunker <command> --help` for the full option list.

## Credentials

Publishing uses `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`, falling back to
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. `AWS_SESSION_TOKEN` is not read,
so temporary credentials do not work. For R2, set `R2_ACCOUNT_ID` to your
32-character hexadecimal account ID. For another S3 endpoint, set
`AWS_ENDPOINT_URL`; it is ignored when `R2_ACCOUNT_ID` is set. `AWS_REGION`
defaults to `auto`. Credentials are read from the environment and are not written
to output files.

The storage service must support conditional `PutObject` requests with
`If-Match` and `If-None-Match`. [Cloudflare R2 supports both](https://developers.cloudflare.com/r2/api/s3/api/).

## Validation and publication

Chunker rejects missing or unreadable input, input that changes while it is
read, symlinks, unsafe paths, path collisions, invalid manifests, missing chunks,
corrupt compressed data, and version mismatches. Paths use `/`, cannot escape the
content root, and must be compatible with Windows filenames. `.DS_Store`,
`Thumbs.db`, `desktop.ini` and AppleDouble (`._*`) files are left out.

Existing chunks are never overwritten. When a chunk to upload is already in the
pool, Chunker downloads the stored object, verifies it against its hash and keeps
its encoding. `publish` and `release` treat every chunk in the manifest as one to
upload, so republishing a mostly unchanged tree downloads every chunk it reuses.
`patch` uploads only chunks that are not in the base. It trusts the base's chunks
and checks only their sizes.

Chunks are uploaded first, then the version manifest, then `latest.txt`, so
`latest.txt` only names a complete version. Published version manifests cannot
be overwritten. If another publisher changes `latest.txt` during the run, the
update fails and preserves the other publisher's value. An interrupted or
competing publication may leave unused chunks or an inactive version manifest;
retry with a new version identifier.

Chunker does not sign manifests. Downloaders must verify each chunk against its
SHA-256 hash and get the manifest from a source they trust.

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
`latest.txt` holds the current version followed by a newline.

Chunks and version manifests are uploaded with
`Cache-Control: public, max-age=31536000, immutable`, and `latest.txt` with
`Cache-Control: public, max-age=0, must-revalidate`.

## Development and releases

Tests need Rust and Python 3 but no storage account. To run the checks that CI
runs on Linux, Windows and macOS:

```sh
bash scripts/check.sh
```

The CLI tests run with `python3`; set `PYTHON` to use another interpreter.

To release, update the version in `Cargo.toml` and `Cargo.lock`, rename
`## Unreleased` in `CHANGELOG.md` to exactly `## X.Y.Z` (that section becomes the
release notes), merge the change, then push the matching `vX.Y.Z` tag. The
release workflow publishes the release only when every platform and the notices
pass; until then it stays a draft. To retry, run the workflow manually on the
same tag. A release that is already public is not overwritten.

To check a dependency change before tagging, install cargo-about with the command
in `.github/workflows/release.yml`, then generate `THIRD-PARTY-NOTICES` as the
release does:

```sh
python3 scripts/notices.py THIRD-PARTY-NOTICES
```

It fails when `about.toml` does not accept a dependency's license, or when a
dependency under a license other than Apache-2.0 has no license file of its own.

## License

MIT. See [LICENSE](LICENSE). The executables also include third-party software,
listed with its licenses in each release's `THIRD-PARTY-NOTICES`.

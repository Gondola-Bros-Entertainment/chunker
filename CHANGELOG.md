# Changelog

## Unreleased

- Chunk `patch` overrides at the base manifest's chunk size. `patch` no longer
  accepts `--chunk-size`.
- Remove the hidden `--force` option from `patch`. Passing it now fails as an
  unknown argument with exit code 2 instead of 1.
- Publish `THIRD-PARTY-NOTICES` and its checksum with the executables. It
  carries the license and copyright notices of the crates and bundled C
  libraries built into them, and the notices that the Rust toolchain ships for
  its standard library.

## 0.2.1

- Validate paths, manifests and local chunk contents before publication. Reject
  missing input, traversal errors, symlinks, invalid sizes and version mismatches.
- Preserve concurrent publications with conditional object writes and an ETag
  check when updating `latest.txt`. Verify the manifest by reading it back.
- Keep published versions immutable; reject the retired `--force` option.
- Reuse existing compressed chunk encodings without overwriting shared objects.
  Check every inherited patch chunk's existence and compressed size.
- Stage patch chunks on disk and clean up automatically created working folders.
- Add unit and CLI regression tests, including publication failures and races.
- Add a macOS ARM64 executable, checksums, and a draft release that becomes public
  only after Linux, Windows and macOS pass.

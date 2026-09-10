#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo clippy --release --locked --all-targets -- -D warnings
cargo build --release --locked
cargo test --release --locked
binary=target/release/chunker
if [[ "${OS:-}" == Windows_NT ]]; then binary+=.exe; fi
python tests/cli.py "$binary"

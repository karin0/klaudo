#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"

shellcheck -S style check.sh
# The binary embeds the page, so it is built first.
pnpm -C web install --frozen-lockfile
pnpm -C web check
pnpm -C web build
cargo fmt --check
cargo clippy --all-targets
cargo test
cargo build --release

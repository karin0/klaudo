#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"

shellcheck -S style check.sh
cargo fmt --check
cargo clippy --all-targets
cargo test
cargo build --release

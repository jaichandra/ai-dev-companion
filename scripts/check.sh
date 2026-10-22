#!/bin/sh
# The gate before every commit: formatting-free, warning-free, tests green.
# Usage: scripts/check.sh && git commit ...
set -e
cd "$(dirname "$0")/.."
cargo clippy -q --all-targets -- -D warnings
cargo test -q

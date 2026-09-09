#!/usr/bin/env bash
# Project checks: formatting, tests, whole-tree compile.
# Run from the repository root:  ./scripts/check.sh
set -euo pipefail

cd "$(dirname "$0")/.."

# The sandbox keeps cargo's global registry out of reach; use the in-repo one.
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo-home}"

echo "==> cargo fmt --check"
cargo fmt --all --check

echo "==> cargo test --workspace"
cargo test --workspace

echo "==> cargo check --workspace"
cargo check --workspace

echo "OK: fmt + tests + check are green."

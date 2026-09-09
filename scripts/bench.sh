#!/usr/bin/env bash
# Engine ingest benchmark (release profile with thin-LTO).
# Generates a CSV and times preview / staging / folder staging.
# Run from the repository root:  ./scripts/bench.sh
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo-home}"

echo "==> cargo run --release -p strata-core --example ingest_bench"
cargo run --release -p strata-core --example ingest_bench

#!/usr/bin/env bash
set -euo pipefail

compiler_version="$(rustc --version)"
cargo_version="$(cargo --version)"
rustc --version --verbose
printf '%s\n' "$cargo_version"

if [[ "$compiler_version" != 'rustc 1.99.0 ('* || "$cargo_version" != 'cargo 1.99.0 ('* ]]; then
    printf '%s\n' 'BitEngine requires the exact Rust/Cargo 1.99.0 toolchain for its quality gate and releases.' >&2
    exit 1
fi

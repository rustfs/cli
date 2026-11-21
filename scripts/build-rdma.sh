#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
toolchain=()
if [[ -n "${RUSTFS_RDMA_TOOLCHAIN:-}" ]]; then
    toolchain=("+$RUSTFS_RDMA_TOOLCHAIN")
fi
exec cargo "${toolchain[@]}" build --locked --manifest-path "$root_dir/nixl/Cargo.toml" "$@"

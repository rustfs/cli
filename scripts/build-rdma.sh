#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
private="$ROOT_DIR/vendor/rustfs-ucx"
if [[ ! -f "$private/Cargo.toml" || ! -f "$private/integrations/cli/src/main.rs" || ! -f "$private/integrations/cli/src/lib.rs" || ! -f "$private/scripts/build-env.sh" ]]; then
    printf '%s\n' 'RDMA requires the authorized private submodule vendor/rustfs-ucx.' >&2
    printf '%s\n' 'Run: git submodule update --init vendor/rustfs-ucx' >&2
    printf '%s\n' 'Authenticate with an account that can read rustfs/rustfs-ucx.' >&2
    exit 2
fi

source "$private/scripts/build-env.sh"

exec cargo "+${RUSTFS_RDMA_TOOLCHAIN:-1.98.1}" build \
    --manifest-path "$ROOT_DIR/rdma/Cargo.toml" --features rdma \
    -j "${CARGO_BUILD_JOBS:-2}" "$@"

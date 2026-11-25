#!/usr/bin/env bash
# Build and link the combined client against NVIDIA's x86_64 SDK without a GPU.
set -euo pipefail
: "${NIXL_PREFIX:?NIXL SDK prefix is required}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
packages="${RUNNER_TEMP:-$work}/cuobject-packages"
mkdir -p "$packages"
repo=https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64
while read -r digest package; do
    archive="$packages/$package"
    if ! echo "$digest  $archive" | sha256sum --check --status 2>/dev/null; then
        curl --fail --silent --show-error --location --retry 3 --retry-all-errors \
            --connect-timeout 20 --max-time 300 "$repo/$package" -o "$archive"
        echo "$digest  $archive" | sha256sum --check
    fi
    dpkg-deb -x "$archive" "$work/sdk"
done <<'PACKAGES'
e06a0f89898ec9a7df3124e4d6ad3a1ebf92a0b3ea42c3f81c6fff5ef92a5126 libcuobjclient-13-5_1.4.0.107-1_amd64.deb
ce6188558d94a15593c4ae82cb578828ce7686ea09e2658a201962f56e66fe7c libcuobjclient-dev-13-5_1.4.0.107-1_amd64.deb
d02925a4ab1706b6ba578ff6d7db953c5bcae3291d3abdbd7e4311de8d3d4c11 libcufile-13-5_1.20.0.107-1_amd64.deb
b8e2c20dc796ca09b3ec55ce5fd72f2bdc4c6efea857b907a9cec5b356256072 libcufile-dev-13-5_1.20.0.107-1_amd64.deb
e079c64a1ab86d69a43a7dd8029ddcacb7de8c1953a4c095e485429bd90e8636 cuda-cudart-dev-13-5_13.5.37-1_amd64.deb
PACKAGES
export CUDA_HOME="$work/sdk/usr/local/cuda-13.5"
export CUOBJCLIENT_ROOT="$CUDA_HOME/targets/x86_64-linux"
export LD_LIBRARY_PATH="$CUOBJCLIENT_ROOT/lib:$NIXL_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export LIBRARY_PATH="$CUOBJCLIENT_ROOT/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
export NIXL_PLUGIN_DIR="$NIXL_PREFIX/lib/plugins"
./scripts/build-rdma.sh --features cuobject
cargo test --locked --manifest-path nixl/Cargo.toml --features cuobject
cargo test --locked --manifest-path nixl/Cargo.toml -p rustfs-rdma-client --no-default-features --features cuobject

#!/usr/bin/env bash
# Build a CPU-only UCX runtime for the native CLI test job.
set -euo pipefail

: "${NIXL_PREFIX:?set the installation prefix}"
build_dir=$(mktemp -d)
trap 'rm -rf "$build_dir"' EXIT
jobs=${CARGO_BUILD_JOBS:-4}
manifest=$(cargo metadata --locked --manifest-path nixl/Cargo.toml --format-version 1 --filter-platform x86_64-unknown-linux-gnu |
    jq -er '.packages[] | select(.name == "nixl-sys") | .manifest_path')
source_dir=$(git -C "$(dirname "$manifest")" rev-parse --show-toplevel)

cd "$build_dir"
curl --fail --location --retry 3 --output ucx.tar.gz https://github.com/openucx/ucx/releases/download/v1.22.0/ucx-1.22.0.tar.gz
echo '258941cddd14ca60d38c0d31b9b09ec1052c901086841011a498da8b55a3cb24  ucx.tar.gz' | sha256sum --check
tar -xzf ucx.tar.gz
(
    cd ucx-1.22.0
    ./configure --prefix="$NIXL_PREFIX" --enable-mt --without-cuda --without-rocm --without-java --without-go --disable-numa --disable-examples
    make -j"$jobs"
    make install
)

# Meson can populate subprojects. Leave Cargo's reviewed dependency checkout intact.
git -C "$source_dir" archive HEAD | tar -x -C "$build_dir"
meson setup "$build_dir/build" "$build_dir" --prefix "$NIXL_PREFIX" --libdir lib \
    -Ducx_path="$NIXL_PREFIX" -Denable_plugins=UCX -Dinstall_headers=true \
    -Dbuild_tests=false -Dbuild_examples=false -Dbuild_nixl_ep=false -Dwith_trace=false
meson compile -C "$build_dir/build" -j "$jobs"
meson install -C "$build_dir/build"

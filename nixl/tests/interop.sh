#!/usr/bin/env bash
# Run against disposable buckets on two verified HTTPS aliases. The second
# endpoint serves ordinary S3. RC_RDMA_BACKEND selects the native protocol.
set -euo pipefail
: "${RC:?path to the native rc executable}"
RDMA_BUCKET=${RDMA_BUCKET:-${NIXL_BUCKET:-}}
: "${RDMA_BUCKET:?alias/bucket on the native endpoint}"
: "${HTTP_BUCKET:?alias/bucket on the HTTP-only endpoint}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
prefix="rc-interop-$(basename "$tmp")"
nr() { "$RC" --data-transport rdma "$@"; }
plain() { "$RC" --data-transport http "$@"; }
automatic() { "$RC" --data-transport auto "$@"; }

# Cross both upload-part and download-window boundaries with distinct data.
dd if=/dev/urandom of="$tmp/model" bs=1M count=129 status=none
printf 'NIXL part receipt\n' > "$tmp/small"
: > "$tmp/empty"
for name in model small empty; do
    object="$RDMA_BUCKET/$prefix/$name"
    nr cp "$tmp/$name" "$object"
    nr cp "$object" "$tmp/native-$name"
    plain cp "$object" "$tmp/http-$name"
    cmp "$tmp/$name" "$tmp/native-$name"
    cmp "$tmp/$name" "$tmp/http-$name"
done
nr object head --bytes 19 "$RDMA_BUCKET/$prefix/model" > "$tmp/range"
head -c 19 "$tmp/model" | cmp - "$tmp/range"

plain cp "$tmp/small" "$RDMA_BUCKET/$prefix/http-source"
nr cp "$RDMA_BUCKET/$prefix/http-source" "$tmp/from-http"
cmp "$tmp/small" "$tmp/from-http"

automatic cp "$tmp/small" "$RDMA_BUCKET/$prefix/auto"
nr cp "$RDMA_BUCKET/$prefix/auto" "$tmp/auto"
cmp "$tmp/small" "$tmp/auto"

object="$HTTP_BUCKET/$prefix/declined"
plain cp "$tmp/small" "$object"
if nr cp --overwrite=true "$tmp/model" "$object"; then
    echo "Expected native upload to be declined" >&2
    exit 1
fi
plain cp "$object" "$tmp/preserved"
cmp "$tmp/small" "$tmp/preserved"
automatic cp --overwrite=true "$tmp/model" "$object"
automatic cp "$object" "$tmp/fallback"
cmp "$tmp/model" "$tmp/fallback"

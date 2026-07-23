#!/usr/bin/env bash

set -euo pipefail

VERSION="${1:?usage: probe-rustfs-compatibility.sh VERSION OUTPUT IMAGE_ID}"
OUTPUT="${2:?usage: probe-rustfs-compatibility.sh VERSION OUTPUT IMAGE_ID}"
IMAGE_ID="${3:?usage: probe-rustfs-compatibility.sh VERSION OUTPUT IMAGE_ID}"
ENDPOINT="${TEST_S3_ENDPOINT:-http://localhost:9000}"
ACCESS_KEY="${TEST_S3_ACCESS_KEY:?TEST_S3_ACCESS_KEY is required}"
SECRET_KEY="${TEST_S3_SECRET_KEY:?TEST_S3_SECRET_KEY is required}"
REGION="${TEST_S3_REGION:-us-east-1}"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROBES="$(mktemp)"
RESPONSE="$(mktemp)"
trap 'rm -f "$PROBES" "$RESPONSE"' EXIT

record() {
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$PROBES"
}

signed_curl() {
  curl --silent --show-error --globoff \
    --aws-sigv4 "aws:amz:${REGION}:s3" \
    --user "${ACCESS_KEY}:${SECRET_KEY}" \
    "$@"
}

# These probes are run only after the corresponding rc integration tests pass.
record "s3-data-plane" "passed" "rc S3 smoke tests passed"
record "admin-v3" "passed" "rc admin v3 cluster info test passed"

if [[ "$VERSION" == "1.0.0-beta.10" ]]; then
  runtime_status="$(signed_curl --output "$RESPONSE" --write-out '%{http_code}' \
    "${ENDPOINT}/rustfs/admin/v4/runtime/capabilities")"
  test "$runtime_status" = "200"
  test -s "$RESPONSE"
  record "admin-v4-runtime-capabilities" "passed" "HTTP 200 with a non-empty response"

  set +e
  signed_curl --max-time 3 --output "$RESPONSE" \
    "${ENDPOINT}/?events=s3:ObjectCreated:*&ping=1"
  stream_exit=$?
  set -e
  if [[ "$stream_exit" -ne 0 && "$stream_exit" -ne 28 ]]; then
    echo "listen notification probe failed with curl exit ${stream_exit}" >&2
    exit "$stream_exit"
  fi
  grep -q '"Records":\[\]' "$RESPONSE"
  record "listen-notification-stream" "passed" "Received a streaming keepalive record"

  batch_status="$(signed_curl --request POST \
    --header 'Content-Type: application/octet-stream' \
    --data-binary $'replicate:\n  apiVersion: v1' \
    --output "$RESPONSE" --write-out '%{http_code}' \
    "${ENDPOINT}/rustfs/admin/v3/start-job")"
  test "$batch_status" = "501"
  grep -q 'NotImplemented' "$RESPONSE"
  record "batch-jobs" "passed" "HTTP 501 NotImplemented; stub was not reported as supported"
else
  record "admin-v4-runtime-capabilities" "not-run" "Version-dependent on the supported floor"
  record "listen-notification-stream" "not-run" "Version-dependent on the supported floor"
  record "batch-jobs" "not-run" "Version-dependent on the supported floor"
fi

python3 "$PROJECT_ROOT/scripts/rustfs_compatibility.py" \
  --manifest "$PROJECT_ROOT/.github/rustfs-compatibility.json" \
  --report "$VERSION" \
  --probes "$PROBES" \
  --image-id "$IMAGE_ID" \
  --output "$OUTPUT"

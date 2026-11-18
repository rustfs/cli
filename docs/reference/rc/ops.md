# rc operational utilities

## Purpose

`rc ping`, `rc ready`, and `rc du` provide lightweight service checks and storage-usage reporting. The health commands use public RustFS endpoints. `rc du` lists S3 objects by default and supports explicit RustFS usage snapshots.

## Syntax

```bash
rc [GLOBAL OPTIONS] ping <ALIAS> [--timeout <SECONDS>]
rc [GLOBAL OPTIONS] ready <ALIAS> [--timeout <SECONDS>]
rc [GLOBAL OPTIONS] du <ALIAS[/BUCKET[/PREFIX]]> [OPTIONS]
```

## Health commands

`rc ping` sends an unsigned request to the documented RustFS liveness endpoint, `GET /health`. It reports the configured endpoint identity, server-provided service and version fields when present, and round-trip latency.

`rc ready` sends an unsigned request to `GET /health/ready`. RustFS 1.0.0-beta.10 returns HTTP 200 when required dependencies are ready, HTTP 503 when readiness is degraded, and may return HTTP 429 with `Retry-After` when the compatibility busy check is enabled. Both 429 and 503 produce retryable network exit code 3 rather than a false success.

Both commands default to a five-second timeout. `--timeout` accepts an integer from 1 through 300 seconds. Connection, response-body, and total request time remain bounded by the configured alias transport settings and the command timeout.

Examples:

```bash
rc ping local
rc ready local --timeout 2
rc ready local --json
```

## Usage command

`rc du` recursively lists current objects through S3 and sums their sizes. An alias scans its visible buckets. A bucket or prefix scans only that target. The default requires S3 list permissions and sends no RustFS admin requests. Large scans can take many requests, and concurrent writes or deletes can affect what later pages see.

Bucket-relative targets select a directory. Both `rc du local/photos/logs` and `rc du local/photos/logs/` scan keys under `logs/`, so a sibling such as `logs-old/a` is excluded. Without a trailing slash, `rc` first lists at most one current key under the directory. A single object or a missing directory returns the `not_found` exit code (5). An explicit trailing slash also allows empty prefixes and directories containing only historical versions or incomplete uploads. This boundary applies to all scan modes.

Use `--snapshot` for the last cluster or bucket totals from the RustFS background scanner. It uses `GET /rustfs/admin/v3/datausageinfo` when the server advertises `admin.data-usage`. Human and JSON output identify the source as `client_scan` or `server_snapshot`. For timestamped snapshots, JSON includes `snapshot_at` and `snapshot_age_seconds`. A snapshot older than one hour is marked `stale` and returned.

### Options

| Option | Description |
| --- | --- |
| `--snapshot` | Read the RustFS cluster or bucket snapshot. Prefixes and incomplete uploads require a scan and return unsupported-feature exit code 7 in snapshot-only mode. |
| `--fallback` | Keep the previous snapshot-first behavior. Use an S3 scan if the snapshot is unavailable or lacks the requested bucket or dimensions. May also be combined with `--snapshot`. |
| `--versions` | On a scan, list every version and count delete markers separately. A snapshot already includes the version counts reported by RustFS. |
| `--incomplete` | Include incomplete multipart uploads and the bytes in their uploaded parts. |

### Migrating from snapshot-first usage

Scripts that relied on a fast snapshot or have only RustFS admin permissions should add `--snapshot`. For a versioned bucket, the default scan sums current object sizes. Use `--versions` to sum bytes across versions. Existing invocations with `--fallback` retain their snapshot-first selection. Scripts that used a partial name such as `logs` to match several directories must now specify each directory separately. Both sources use the existing v3 usage output.

```bash
# Scan current objects in one bucket
rc du local/photos

# Scan a prefix including every version and delete marker
rc du local/photos/2026/ --versions

# Include uploaded bytes from incomplete multipart uploads
rc du local/photos --incomplete

# Read the cluster snapshot without initiating a scan
rc du local --snapshot
```

## Client-scan semantics

The scan uses these S3 operations:

- `ListBuckets` determines cluster scope;
- `ListObjectsV2` counts current objects and bytes by default;
- `ListObjectVersions` replaces current-object byte enumeration when `--versions` is selected;
- `ListMultipartUploads` and `ListParts` supply incomplete-upload counts and bytes when `--incomplete` is selected.

With `--versions`, `object_count` counts current non-delete objects, `version_count` counts non-delete versions, and `delete_marker_count` counts delete markers. Scans exclude zero-byte directory markers (keys ending in `/`) from object and version counts. Ordinary zero-byte files and objects with a nonzero payload count normally. Incomplete uploaded-part bytes are included in `total_bytes` when `--incomplete` is selected and are also exposed separately as `incomplete_upload_bytes`.

Directory-scoped `--incomplete` relies on the backend honoring a directory prefix in `ListMultipartUploads`. In a test with MinIO RELEASE.2025-09-06T17-38-46Z, listing the bucket returned an open upload for `logs/open`, but requesting the prefix `logs/` returned an empty list. Directory totals on that release can therefore omit uploaded parts.

For an alias-wide scan, a bucket-specific failure does not discard successfully collected buckets. Output is marked `partial`, includes sorted failure records, and the process exits with retryable network code 3. A bucket- or prefix-scoped failure returns its mapped authentication, not-found, or network exit directly.

## JSON output

All three commands use [output schema v3](output.md). Health records use the `health` family; usage records use the `usage` family. Human output escapes control and bidirectional formatting characters from server-controlled identity, bucket, and failure strings.

## Compatibility

| Backend | `ping` / `ready` | Default `du` | `du --snapshot` |
| --- | --- | --- | --- |
| RustFS 1.0.0-beta.10 | `/health`, `/health/ready` | S3 listing | Supported |
| Other RustFS versions | Supported when documented health routes exist | S3 listing | Requires the advertised data-usage capability |
| AWS S3, MinIO, other S3-compatible services | RustFS health routes are not assumed | Subject to the service's list/version/multipart support and permissions | Requires RustFS admin APIs |

Prometheus scraping and control of the RustFS background scanner are intentionally outside these commands.

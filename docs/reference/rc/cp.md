# rc cp

## Purpose

`rc cp` copies files and objects between local paths and S3-compatible remote paths. It is a legacy-compatible command; prefer `rc object copy` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] cp [OPTIONS] <SOURCE>... <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | One or more local files/directories or remote objects/prefixes. |
| `TARGET` | Local directory or remote prefix for multiple sources; local or remote path for one source. |
| `-r, --recursive` | Recursively copy a directory or prefix. |
| `--overwrite` | Overwrite destination data where supported. |
| `--dry-run` | Show planned copies without copying data. |
| `--preserve` | Preserve applicable metadata. |
| `--content-type` | Set object content type for uploads. |
| `--storage-class` | Set destination storage class for uploads where supported. |
| `--enc-s3 <TARGET>` | Apply `SSE-S3` to the named remote destination write. |
| `--enc-kms <TARGET>=<KMS_KEY_ID>` | Apply `SSE-KMS` to the named remote destination write. |
| `--include <GLOB>` | Include source-relative paths matching the glob. Repeatable. |
| `--exclude <GLOB>` | Exclude matching paths after includes are evaluated. Repeatable. |
| `--newer-than <AGE>` | Select sources strictly newer than an age such as `1h` or `7d`. |
| `--older-than <AGE>` | Select sources strictly older than an age such as `1h` or `7d`. |
| `--rewind <TIME>` | Select source metadata at or before a UTC timestamp or age. |
| `--concurrency <COUNT>` | Bound all in-flight leaf transfers across the command. Defaults to `4`. |
| `--rate-limit <RATE>` | Pace aggregate transfer starts by expected bytes, for example `10MiB/s`. |
| `--retry-attempts <COUNT>` | Maximum attempts for classified transient failures. Defaults to `3`. |
| `--retry-initial-backoff-ms <MS>` | Initial retry backoff. Defaults to `100`. |
| `--retry-max-backoff-ms <MS>` | Maximum retry backoff. Defaults to `10000`. |
| `--continue-on-error` | Continue eligible work after an item fails; the final exit code remains non-zero. |
| `--fail-empty` | Return the not-found exit code when no source passes selection. |
| `--summary` | Print deterministic aggregate counters in human output; bulk and recursive copies summarize automatically. |
| `--portable-names` | When downloading to a local filesystem, reject keys that cannot be created on Windows. Unix destinations accept characters such as `:` by default. |

## Examples

Upload a file:

```bash
rc cp ./report.json local/reports/report.json
```

Upload a directory recursively:

```bash
rc object copy ./reports/ local/reports/ --recursive
```

Copy between buckets on the same alias:

```bash
rc cp local/reports/summary.json local/archive/summary.json
```

Copy multiple files with command-wide controls:

```bash
rc cp ./january.csv ./february.csv local/reports/ --concurrency 8 --rate-limit 10MiB/s --summary
```

Filter a recursive upload using source-relative paths and UTC metadata:

```bash
rc cp ./reports/ local/archive/ --recursive --include '*.csv' --exclude 'private-*' --newer-than 7d
```

Upload with explicit destination encryption:

```bash
rc cp ./report.json local/archive/report.json --enc-s3 local/archive/report.json
```

Recursively upload a directory and apply one KMS key to the remote target prefix:

```bash
rc cp ./reports/ local/archive/ --recursive --enc-kms local/archive/=alias/archive-key
```

## Behavior

The last path is always the target. Multiple sources require a local directory or remote prefix target, and ambiguous targets fail before any transfer starts. Sources can mix local and remote paths only where the command can infer a valid copy direction. S3-to-S3 copies are limited to paths under the same alias in the current implementation; use `rc mirror` for remote-to-remote synchronization across aliases. Recursive S3-to-S3 copy remains unsupported. Use trailing slashes consistently when copying directory-like prefixes.

Include rules restrict the candidate set when present. Exclude rules are evaluated afterwards and always win, regardless of flag order. `--newer-than` and `--older-than` use strict UTC comparisons; `--rewind` includes its boundary. Candidates without required source timestamps are skipped. Empty selections succeed unless `--fail-empty` is passed.

Concurrency, rate pacing, and retry budgets are global to the command rather than per source. Only classified transient failures are retried. `--continue-on-error` preserves individual failures and continues remaining work, but the aggregate exit code is still non-zero. Summaries count planned, skipped, successful, failed, cancelled, and transferred bytes. Planned bulk JSON output is intentionally unavailable until the versioned output contract supports it; combining `--json` with multi-source, recursive, filtered, rate-limited, or summary planning fails explicitly instead of silently changing JSON output. Legacy single-object JSON output remains unchanged.

Destination encryption flags apply only to remote writes. On `rc cp`, the selector in `--enc-s3` or `--enc-kms` must match the command destination exactly:

- For a single-object write, use the full remote object path.
- For a recursive upload or remote-to-remote copy, use the same remote prefix passed as `TARGET`.

The current implementation supports `SSE-S3` and `SSE-KMS`. It does not support `SSE-C`, repeated encryption selectors, or MinIO `mc`-style prefix fan-out matching beyond the exact destination argument for the current command. For shared encryption rules across commands, see [Encryption workflows](encryption.md).

When the server returns a source or destination object version ID, JSON copy output uses the output v3 `versioned_objects` envelope with `data.operation` set to `copy`. `data.source_version_id` identifies the copied source version and `data.version_id` identifies the created destination version. Copies for which the backend reports no version information retain the legacy JSON shape.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

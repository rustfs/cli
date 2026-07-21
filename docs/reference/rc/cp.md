# rc cp

## Purpose

`rc cp` copies files and objects between local paths and S3-compatible remote paths. It is a legacy-compatible command; prefer `rc object copy` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] cp [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local file, local directory, or remote object/prefix path. This version accepts one source and one target. |
| `TARGET` | Local or remote destination path. |
| `-r, --recursive` | Recursively copy a directory or prefix. |
| `--overwrite` | Overwrite destination data where supported. |
| `--dry-run` | Show planned copies without copying data. |
| `--preserve` | Preserve applicable metadata. |
| `--content-type` | Set object content type for uploads. |
| `--storage-class` | Set destination storage class for uploads where supported. |
| `--enc-s3 <TARGET>` | Apply `SSE-S3` to the named remote destination write. |
| `--enc-kms <TARGET>=<KMS_KEY_ID>` | Apply `SSE-KMS` to the named remote destination write. |

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

Upload with explicit destination encryption:

```bash
rc cp ./report.json local/archive/report.json --enc-s3 local/archive/report.json
```

Recursively upload a directory and apply one KMS key to the remote target prefix:

```bash
rc cp ./reports/ local/archive/ --recursive --enc-kms local/archive/=alias/archive-key
```

## Behavior

The last path is the target. Sources can mix local and remote paths only where the command can infer a valid copy direction. S3-to-S3 copies are limited to paths under the same alias in the current implementation; use `rc mirror` for remote-to-remote synchronization across aliases. Use trailing slashes consistently when copying directory-like prefixes.

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

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

## Behavior

The last path is the target. Sources can mix local and remote paths only where the command can infer a valid copy direction. S3-to-S3 copies are limited to paths under the same alias in the current implementation; use `rc mirror` for remote-to-remote synchronization across aliases. Use trailing slashes consistently when copying directory-like prefixes.

Destination encryption flags apply only to remote writes and only when the flag target matches the command target exactly. The current implementation supports `SSE-S3` and `SSE-KMS`. For shared encryption rules across commands, see [`rc encryption`](encryption.md).

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

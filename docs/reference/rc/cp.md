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

## Examples

Upload a file:

```bash
rc cp ./report.json local/reports/report.json
```

Upload a directory recursively:

```bash
rc object copy ./reports/ local/reports/ --recursive
```

Copy between buckets:

```bash
rc cp local/reports/summary.json backup/reports/summary.json
```

## Behavior

The last path is the target. Sources can mix local and remote paths only where the command can infer a valid copy direction. Use trailing slashes consistently when copying directory-like prefixes.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

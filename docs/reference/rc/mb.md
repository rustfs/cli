# rc mb

## Purpose

`rc mb` creates a bucket. It is a legacy-compatible command; prefer `rc bucket create` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] mb [OPTIONS] <ALIAS/BUCKET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Target bucket path to create. |
| `-p, --ignore-existing` | Treat an existing bucket as success. |
| `--region` | Override the alias default region for bucket creation. |
| `--with-lock` | Enable object locking on the new bucket. |
| `--with-versioning` | Enable bucket versioning after creation. |

## Examples

```bash
rc mb local/reports
rc bucket create local/archive --with-versioning --with-lock
rc mb local/reports --ignore-existing
```

## Behavior

Bucket creation uses the endpoint and credentials from the alias. Object locking must be enabled when the bucket is created.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

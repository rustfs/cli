# rc tag

## Purpose

`rc tag` manages bucket and object tags.

## Syntax

```bash
rc [GLOBAL OPTIONS] tag <COMMAND>
rc tag list [OPTIONS] <PATH>
rc tag set [OPTIONS] <PATH> --tags KEY=VALUE [--tags KEY=VALUE ...]
rc tag remove [OPTIONS] <PATH>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | Bucket path or object path whose tags are managed. |
| `-t, --tags KEY=VALUE` | Tag key-value pair to set. May be repeated. |
| `--version-id` | Target a specific object version where supported. |
| `--force` | Force operation even if capability detection fails. |

## Examples

```bash
rc tag set local/reports/summary.json --tags env=prod --tags owner=analytics
rc tag list local/reports/summary.json
rc tag remove local/reports/summary.json
```

## Behavior

Tag operations use S3-compatible tagging APIs. Object-version targeting depends on backend support.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

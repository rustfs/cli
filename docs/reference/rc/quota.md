# rc quota

## Purpose

`rc quota` manages bucket quota. It is a legacy-compatible command; prefer `rc bucket quota` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] quota <COMMAND>
rc quota set <ALIAS/BUCKET> <SIZE>
rc quota info <ALIAS/BUCKET>
rc quota clear <ALIAS/BUCKET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose quota is managed. |
| `SIZE` | Quota value in bytes or units such as `10G`, `500M`, or `10KB`. |

## Examples

```bash
rc bucket quota set local/reports 100G
rc quota info local/reports
rc bucket quota clear local/reports
```

## Behavior

Quota commands require backend support for bucket quota APIs. Clearing quota removes the configured bucket quota.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

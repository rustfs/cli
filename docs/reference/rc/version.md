# rc version

## Purpose

`rc version` manages bucket versioning and lists object versions. It is a legacy-compatible command; prefer `rc bucket version` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] version <COMMAND>
rc version enable <ALIAS/BUCKET>
rc version suspend <ALIAS/BUCKET>
rc version info <ALIAS/BUCKET>
rc version list [OPTIONS] <ALIAS/BUCKET[/PREFIX]>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose versioning state is managed. |
| `ALIAS/BUCKET[/PREFIX]` | Bucket or prefix whose versions are listed. |
| `-n, --max` | Maximum number of versions to list. Defaults to `100`. |
| `--force` | Force operation even if capability detection fails. |

## Examples

```bash
rc bucket version enable local/reports
rc version info local/reports
rc bucket version list local/reports --max 200
rc version suspend local/reports
```

## Behavior

Versioning changes apply at the bucket level. Listing versions requires backend support for S3 versioning APIs.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

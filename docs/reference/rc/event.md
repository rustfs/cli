# rc event

## Purpose

`rc event` manages bucket notification rules. It is a legacy-compatible command; prefer `rc bucket event` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] event <COMMAND>
rc event add [OPTIONS] <ALIAS/BUCKET> <ARN>
rc event list [OPTIONS] <ALIAS/BUCKET>
rc event remove [OPTIONS] <ALIAS/BUCKET> <ARN>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose notification configuration is managed. |
| `ARN` | Notification target ARN. |
| `--event EVENT` | Event filter. Can be supplied more than once. |
| `--force` | Force operation even if capability detection fails. |

## Examples

```bash
rc bucket event add local/reports arn:aws:sns:us-east-1:123456789012:topic --event s3:ObjectCreated:*
rc event list local/reports
rc bucket event remove local/reports arn:aws:sns:us-east-1:123456789012:topic
```

## Behavior

Adding a rule configures notifications for the specified ARN. Removing a rule targets the matching ARN.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

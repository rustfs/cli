# rc rb

## Purpose

`rc rb` removes a bucket. It is a legacy-compatible command; prefer `rc bucket remove` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] rb [OPTIONS] <ALIAS/BUCKET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket path to remove. |
| `--force` | Delete objects before removing a non-empty bucket. |
| `--dangerous` | Remove the bucket even when incomplete multipart uploads exist. |

## Examples

```bash
rc rb local/empty-bucket
rc bucket remove local/old-bucket --force
```

## Behavior

Without `--force`, the backend must allow deletion of the bucket as-is. Use destructive options carefully; they can delete object data before removing the bucket.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

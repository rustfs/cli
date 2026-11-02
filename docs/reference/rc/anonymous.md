# rc anonymous

## Purpose

`rc anonymous` manages anonymous bucket or prefix access. It is a legacy-compatible command; prefer `rc bucket anonymous` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] anonymous <COMMAND>
rc anonymous set <private|public|download|upload> <PATH>
rc anonymous set-json <FILE> <PATH>
rc anonymous get <PATH>
rc anonymous get-json <PATH>
rc anonymous list <PATH>
rc anonymous links [-r|--recursive] <PATH>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | Bucket or prefix path, `ALIAS/BUCKET[/PREFIX]`. |
| `PERMISSION` | One of `private`, `public`, `download`, or `upload`. |
| `FILE` | Policy JSON file for `set-json`. |
| `-r, --recursive` | Recursively list public links. |

## Examples

```bash
rc bucket anonymous set download local/public/reports
rc anonymous get local/public/reports
rc anonymous links local/public/reports --recursive
rc anonymous set private local/public
```

## Behavior

Setting prefix-level anonymous access reads the existing policy and updates only recognizable rc-generated rules for that prefix. Other prefixes, custom statements, principals, conditions, Deny rules, and policy metadata are preserved. Repeating the same prefix update is idempotent. A failed policy read or malformed policy stops the update without writing.

Bucket-level `set` and `set-json` replace the policy. Setting `private` remains bucket-level because it clears the policy. Prefix updates are read-modify-write operations; coordinate concurrent policy writers because S3 bucket policies do not support an atomic merge.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

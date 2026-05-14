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

Setting prefix-level anonymous access replaces the bucket policy document with statements generated for the target prefix. Setting `private` is bucket-level because it clears the anonymous policy.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

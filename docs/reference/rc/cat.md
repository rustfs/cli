# rc cat

## Purpose

`rc cat` prints the full contents of an object to stdout. It is a legacy-compatible command; prefer `rc object show` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] cat [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Object path to read. |
| `--offset` | Start reading at a byte offset. |
| `--length` | Limit output to a byte length. |
| `--version-id` | Read a specific object version. |

## Examples

```bash
rc cat local/reports/summary.txt
rc object show local/reports/summary.txt --version-id VERSION_ID
```

## Behavior

Object bytes are written to stdout, so redirect output when reading binary objects.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

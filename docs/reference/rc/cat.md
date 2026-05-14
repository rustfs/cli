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
| `--enc-key` | Encrypt or decrypt with the given base64-encoded key. |
| `--rewind` | Read object state at a specific time where supported. |
| `--version-id` | Read a specific object version. |

## Examples

```bash
rc cat local/reports/summary.txt
rc object show local/reports/summary.txt --version-id VERSION_ID
rc cat local/reports/summary.txt --rewind 2026-05-01T00:00:00Z
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

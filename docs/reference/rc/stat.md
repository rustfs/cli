# rc stat

## Purpose

`rc stat` shows object metadata. It is a legacy-compatible command; prefer `rc object stat` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] stat [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Object path to inspect. |
| `--version-id` | Inspect a specific object version. |
| `--rewind` | Inspect object state at a specific time where supported. |

## Examples

```bash
rc stat local/reports/summary.json
rc object stat local/reports/summary.json --json
```

## Behavior

Metadata output includes object identity and storage metadata such as size, modified time, ETag, content type, and user metadata when available.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

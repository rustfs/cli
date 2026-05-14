# rc diff

## Purpose

`rc diff` compares two locations and reports objects that are missing, different, or identical depending on selected options.

## Syntax

```bash
rc [GLOBAL OPTIONS] diff [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local or remote source path. |
| `TARGET` | Local or remote target path. |
| `-r, --recursive` | Compare recursively. |
| `--diff-only` | Show only differences instead of all compared entries. |

## Examples

```bash
rc diff local/reports backup/reports --recursive
rc diff ./reports local/reports --recursive --json
```

## Behavior

Diff is read-only. Use it before copy, mirror, or remove workflows to inspect drift between two locations.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

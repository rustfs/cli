# rc tree

## Purpose

`rc tree` displays object keys in a tree-like view. It is a legacy-compatible command; prefer `rc object tree` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] tree [OPTIONS] <ALIAS/BUCKET[/PREFIX]>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | Bucket or prefix to display. |
| `-L, --level` | Maximum tree depth. Defaults to `3`. |
| `-d, --dirs-only` | Show only directory-like prefixes. |
| `-f, --full-path` | Show full path prefixes. |
| `-P, --pattern` | Include only keys matching a pattern. |
| `-s, --size` | Show object sizes. |

## Examples

```bash
rc tree local/reports -L 2
rc object tree local/reports --recursive --size
```

## Behavior

Tree output is optimized for human inspection of key organization. Use `rc object list --json` for machine-readable inventory.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

# rc mv

## Purpose

`rc mv` moves files or objects between local and S3-compatible locations. It is a legacy-compatible command; prefer `rc object move` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] mv [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local path or remote object/prefix path to move. |
| `TARGET` | Local or remote destination path. |
| `-r, --recursive` | Move a directory or prefix recursively. |
| `--dry-run` | Show planned changes without moving data. |

## Examples

```bash
rc mv local/inbox/report.json local/archive/report.json
rc object move ./incoming/ local/inbox/ --recursive
```

## Behavior

Move operations copy data to the target and remove the source after a successful copy. Review recursive moves with `--dry-run` before running destructive operations.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

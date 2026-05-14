# rc head

## Purpose

`rc head` prints the beginning of an object. It is a legacy-compatible command; prefer `rc object head` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] head [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Object path to preview. |
| `-n, --lines` | Number of lines to print. Defaults to `10`. |
| `-c, --bytes` | Number of bytes to print. |
| `--version-id` | Read a specific object version. |

## Examples

```bash
rc head local/logs/app.log --lines 20
rc object head local/data/archive.bin --bytes 512
```

## Behavior

Use `--bytes` for binary files or fixed-size previews. Use `--lines` for text objects.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

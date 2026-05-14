# rc mirror

## Purpose

`rc mirror` synchronizes objects from a source location to a target location.

## Syntax

```bash
rc [GLOBAL OPTIONS] mirror [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local or remote source directory, bucket, or prefix. |
| `TARGET` | Local or remote destination directory, bucket, or prefix. |
| `--overwrite` | Overwrite changed destination objects. |
| `--remove` | Delete target objects that no longer exist in the source. |
| `-n, --dry-run` | Show planned changes without applying them. |
| `-P, --parallel` | Number of parallel transfer workers. Defaults to `4`. |

## Examples

```bash
rc mirror ./site local/web/site --dry-run
rc mirror ./site local/web/site --overwrite --remove
rc mirror local/source backup/source --parallel 8
```

## Behavior

Mirror is intended for prefix-level synchronization. Use `--dry-run` first when combining `--overwrite` and `--remove`.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

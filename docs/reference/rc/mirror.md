# rc mirror

## Purpose

`rc mirror` synchronizes objects from one remote S3-compatible location to another remote S3-compatible location.

## Syntax

```bash
rc [GLOBAL OPTIONS] mirror [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Remote source bucket or prefix path, in `ALIAS/BUCKET[/PREFIX]` form. |
| `TARGET` | Remote destination bucket or prefix path, in `ALIAS/BUCKET[/PREFIX]` form. |
| `--overwrite` | Overwrite changed destination objects. |
| `--remove` | Delete target objects that no longer exist in the source. |
| `-n, --dry-run` | Show planned changes without applying them. |
| `-P, --parallel` | Number of parallel transfer workers. Defaults to `4`. |

## Examples

```bash
rc mirror local/source backup/source --parallel 8
rc mirror local/web/site backup/web/site --dry-run
rc mirror local/web/site backup/web/site --overwrite --remove
```

## Behavior

Mirror is intended for remote prefix-level synchronization. Local filesystem paths are not supported by the current implementation. Use `--dry-run` first when combining `--overwrite` and `--remove`.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

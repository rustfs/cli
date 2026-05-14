# rc find

## Purpose

`rc find` searches objects under a bucket path using name, path, size, time, and metadata filters. It is a legacy-compatible command; prefer `rc object find` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] find [OPTIONS] <ALIAS/BUCKET[/PREFIX]>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | Bucket or prefix to search. |
| `--name` | Match object name with a glob-like pattern. |
| `--older` | Match objects older than a duration such as `7d`. |
| `--newer` | Match objects newer than a duration such as `1h`. |
| `--larger` | Match objects larger than a size such as `10M`. |
| `--smaller` | Match objects smaller than a size such as `500K`. |
| `--maxdepth` | Limit search depth; `0` means unlimited. |
| `--count` | Print only the number of matches. |
| `--exec` | Execute a command for each match, using `{}` as the placeholder. |
| `--print` | Print full paths instead of paths relative to the search root. |

## Examples

```bash
rc find local/reports --name '*.json'
rc object find local/logs --newer 1d --larger 10M
```

## Behavior

Find evaluates filters against objects returned by the backend. Duration and size suffixes are parsed by `rc`, so use explicit units for scripts.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

# rc object

## Purpose

The `rc object` operation is the preferred noun-first entry point for object workflows. It groups listing, copy, move, remove, metadata, preview, search, tree, presigned URL, retention, and legal-hold operations.

## Syntax

```bash
rc [GLOBAL OPTIONS] object <COMMAND>
rc object list [OPTIONS] <ALIAS/BUCKET[/PREFIX]>
rc object copy [OPTIONS] <SOURCE> <TARGET>
rc object move [OPTIONS] <SOURCE> <TARGET>
rc object remove [OPTIONS] <PATH>...
rc object stat [OPTIONS] <ALIAS/BUCKET/KEY>
rc object show [OPTIONS] <ALIAS/BUCKET/KEY>
rc object head [OPTIONS] <ALIAS/BUCKET/KEY>
rc object find [OPTIONS] <ALIAS/BUCKET[/PREFIX]>
rc object tree [OPTIONS] <ALIAS/BUCKET[/PREFIX]>
rc object share [OPTIONS] <ALIAS/BUCKET/KEY>
rc object retention <info|set|clear> ...
rc object legalhold <info|set|clear> ...
```

## Commands

| Command | Description |
| --- | --- |
| `list` | List objects under a bucket path. |
| `copy` | Copy local files to S3, S3 objects to local paths, or S3 objects between remote paths. |
| `move` | Move objects or files by copying then deleting the source. |
| `remove` | Remove one or more objects. |
| `stat` | Show object metadata. |
| `show` | Print the full object body. |
| `head` | Print the first lines or bytes of an object. |
| `find` | Search object keys with filters. |
| `tree` | Display object keys as a tree. |
| `share` | Generate a presigned object URL. |
| `retention` | Inspect, set, or clear retention for an object version. |
| `legalhold` | Inspect legal-hold status or set it to ON/OFF for an object version. |

## Common Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local path or remote `ALIAS/BUCKET/KEY`. Copy accepts one source and one target in this version. |
| `TARGET` | Local destination or remote `ALIAS/BUCKET[/PREFIX]`. |
| `PATH` | Remote object or prefix path. |
| `--recursive` | Recurse into directories or prefixes for copy, list, remove, or tree operations. |
| `--dry-run` | Print planned changes without mutating data where supported. |
| `--versions` | Include versions where the backend supports object versioning. |
| `--content-type` | Set content type for upload or streaming commands. |

## Examples

Upload a file:

```bash
rc object copy ./report.json local/reports/report.json
```

Download an object:

```bash
rc object copy local/reports/report.json ./report.json
```

Remove a prefix after previewing it:

```bash
rc object remove local/reports/tmp/ --recursive --dry-run
rc object remove local/reports/tmp/ --recursive --force
```

Generate a one-day download URL:

```bash
rc object share local/reports/report.json --expire 1d
```

Inspect retention and legal hold on one exact version:

```bash
rc object retention info local/records/invoice.pdf --version-id v2
rc object legalhold info local/records/invoice.pdf --version-id v2
```

## Behavior

Prefer `rc object ...` for new scripts. Legacy commands such as `rc cp`, `rc mv`, `rc rm`, `rc cat`, `rc head`, `rc stat`, `rc find`, `rc tree`, and `rc share` remain available for compatibility.

The top-level [`rc retention`](retention.md) and [`rc legalhold`](legalhold.md) names provide `mc`-compatible entry points to the same implementations.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

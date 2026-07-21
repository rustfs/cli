# rc ls

## Purpose

`rc ls` lists buckets or objects. It is a legacy-compatible command; prefer `rc bucket list` for bucket workflows and `rc object list` for object workflows.

## Syntax

```bash
rc [GLOBAL OPTIONS] ls [OPTIONS] <PATH>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | `ALIAS/` to list buckets, or `ALIAS/BUCKET[/PREFIX]` to list objects. |
| `-r, --recursive` | Recursively list objects. |
| `--versions` | Show object versions where supported. |
| `--incomplete` | List incomplete multipart uploads for one exact object key. |
| `--summarize` | Show totals only. |

## Examples

```bash
rc ls local/
rc ls local/reports --recursive
rc bucket list local/reports --versions
rc ls local/reports/archive.tar --incomplete
```

## Behavior

When `PATH` contains only an alias, `rc ls` lists buckets. When it contains a bucket, it lists objects under the optional prefix.

`--incomplete` currently requires `ALIAS/BUCKET/KEY` and treats `KEY` as an exact object key.
Bucket-wide, prefix, and recursive multipart listing are rejected until the RustFS server-side
prefix bug tracked by `rustfs/backlog#1384` is fixed and can be detected safely.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

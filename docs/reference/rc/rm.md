# rc rm

## Purpose

`rc rm` removes objects. It is a legacy-compatible command; prefer `rc object remove` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] rm [OPTIONS] <PATH>...
```

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | One or more remote object or prefix paths. |
| `-r, --recursive` | Remove objects recursively under a prefix. |
| `-f, --force` | Run without interactive confirmation where required. |
| `--dry-run` | Show objects that would be removed. |
| `--incomplete` | Abort incomplete multipart uploads. |
| `--versions` | Remove object versions where supported. |
| `--version-id <VERSION_ID>` | Remove exactly one object version without deleting sibling versions. |
| `--purge` | Remove all versions and delete markers under the target where supported. |
| `--bypass` | Bypass governance retention if the backend and credentials allow it. |

## Examples

```bash
rc rm local/reports/tmp.json
rc rm local/reports/tmp.json --version-id VERSION_ID
rc object remove local/reports/tmp/ --recursive --dry-run
rc object remove local/reports/tmp/ --recursive --versions --dry-run --json
rc object remove local/reports/tmp/ --recursive --force
rc rm local/reports/archive.tar --incomplete --dry-run
rc rm local/reports/incoming/ --incomplete --recursive
```

## Behavior

Recursive and version-aware deletions can remove many objects. Use `--dry-run` to inspect the target set before running destructive commands.

JSON output for `--version-id` and `--versions` uses the output v3 `versioned_objects` envelope. A dry run reports entries under `data.planned` with `data.dry_run` set to `true`; it never reports them as removed. A partial failure uses an error envelope whose `data.removed` and `data.failed` arrays retain the version ID for every result. Unversioned removal retains its legacy JSON shape for compatibility.

`--incomplete` matches an exact object key unless `--recursive` is supplied. Bucket-wide and
prefix cleanup require `--recursive` as an explicit destructive-scope acknowledgement. The
command retrieves every listing page before aborting uploads, retries transient failures with
bounded concurrency, treats a repeated abort as idempotent, and reports one deterministic result
per upload. `--dry-run` performs no abort requests. An interrupt cancels outstanding cleanup and
returns the interrupted exit code, so the command can be rerun safely.

Multipart JSON uses the v3 `multipart_uploads` family. Mixed outcomes use `status: partial` and
preserve successful and failed upload IDs in `data.results`.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

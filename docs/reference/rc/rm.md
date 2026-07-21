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
| `--incomplete` | Abort incomplete multipart uploads for exact object keys. |
| `--versions` | Remove object versions where supported. |
| `--purge` | Remove all versions and delete markers under the target where supported. |
| `--bypass` | Bypass governance retention if the backend and credentials allow it. |

## Examples

```bash
rc rm local/reports/tmp.json
rc object remove local/reports/tmp/ --recursive --dry-run
rc object remove local/reports/tmp/ --recursive --force
rc rm local/reports/archive.tar --incomplete --dry-run
```

## Behavior

Recursive and version-aware deletions can remove many objects. Use `--dry-run` to inspect the target set before running destructive commands.

`--incomplete` is limited to exact object keys. Recursive and prefix cleanup are rejected until
the RustFS server-side prefix bug tracked by `rustfs/backlog#1384` is fixed and can be detected
safely. Exact-key cleanup remains idempotent and can be rerun after interruption.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

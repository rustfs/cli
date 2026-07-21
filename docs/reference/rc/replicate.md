# rc replicate

## Purpose

`rc replicate` manages bucket replication configuration and status. It is a legacy-compatible command; prefer `rc bucket replication` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] replicate <COMMAND>
rc replicate add [OPTIONS] --remote-bucket <TARGET_ALIAS/BUCKET> <SOURCE_ALIAS/BUCKET>
rc replicate update [OPTIONS] --id <ID> <SOURCE_ALIAS/BUCKET>
rc replicate list [OPTIONS] <SOURCE_ALIAS/BUCKET>
rc replicate status [OPTIONS] <SOURCE_ALIAS/BUCKET>
rc replicate diff [--prefix <PREFIX>] <SOURCE_ALIAS/BUCKET>
rc replicate remove [OPTIONS] <SOURCE_ALIAS/BUCKET>
rc replicate export [OPTIONS] <SOURCE_ALIAS/BUCKET>
rc replicate import [OPTIONS] <SOURCE_ALIAS/BUCKET> <FILE>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE_ALIAS/BUCKET` | Source bucket for replication. |
| `--remote-bucket TARGET_ALIAS/BUCKET` | Destination bucket for a new rule. |
| `--id` | Rule identifier to update or remove. |
| `--replicate` | Comma-separated flags: `delete`, `delete-marker`, `existing-objects`. |
| `--priority` | Rule priority. Defaults to `1` for new rules. |
| `--storage-class` | Destination storage class override. |
| `--bandwidth` | Bandwidth limit in bytes per second; `0` means unlimited. |
| `--sync` | Enable synchronous replication. |
| `--prefix` | Key prefix filter for rules or a replication diff scan. |
| `--healthcheck-seconds` | Health check interval. Defaults to `60` for new rules. |
| `--disable-proxy` | Disable replication proxy. |
| `--all` | Remove all rules. |
| `--force` | Force operation even if capability detection fails. |

## Examples

```bash
rc bucket version enable local/reports
rc bucket version enable backup/reports
rc bucket replication add local/reports --remote-bucket backup/reports --replicate delete,existing-objects
rc replicate status local/reports
rc bucket replication diff local/reports --prefix quarterly/2026/
rc replicate export local/reports > replication.json
```

## Behavior

Replication generally requires versioning on source and destination buckets. The target bucket is resolved through its own alias, so configure both source and destination aliases before adding rules.

`replication diff` calls the RustFS Admin API to scan object versions that are pending or failed replication. It reports object keys, version IDs, delete-marker state, sizes, replication statuses, and last-modified timestamps. The request has no body and may be narrowed with `--prefix`.

The command accepts at most 8 MiB for either a successful or error response. JSON output uses output schema v3 with the `replication` family and `diff` operation. Unknown server fields are preserved under `extensions`.

The server may stop after its bounded scan and return `truncated: true`. Such output is explicitly partial and non-resumable. Narrow `--prefix` and run the command again; do not interpret an empty truncated result as proof that no backlog exists. The command does not provide time-range, metrics, MRF, or resume-token behavior.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

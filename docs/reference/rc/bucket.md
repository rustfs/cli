# rc bucket

## Purpose

The `rc bucket` operation is the preferred noun-first entry point for bucket-oriented workflows. It groups bucket listing, creation, deletion, notification, CORS, encryption, versioning, quota, anonymous access, lifecycle, and replication operations under one command family.

## Syntax

```bash
rc [GLOBAL OPTIONS] bucket <COMMAND>
rc bucket list [OPTIONS] <PATH>
rc bucket create [OPTIONS] <ALIAS/BUCKET>
rc bucket remove [OPTIONS] <ALIAS/BUCKET>
rc bucket event <add|list|remove> ...
rc bucket cors <list|set|remove> ...
rc bucket encryption <set|info|clear> ...
rc bucket version <enable|suspend|info|list> ...
rc bucket quota <set|info|clear> ...
rc bucket anonymous <set|set-json|get|get-json|list|links> ...
rc bucket lifecycle <rule|tier|restore> ...
rc bucket replication <add|update|list|status|diff|remove|export|import|check|resync> ...
```

## Commands

| Command | Description |
| --- | --- |
| `list` | List buckets for an alias or list objects under a bucket path. |
| `create` | Create a bucket. |
| `remove` | Remove a bucket. |
| `event` | Manage bucket notification rules. |
| `cors` | Manage bucket CORS rules. |
| `encryption` | Manage bucket default encryption. |
| `version` | Manage bucket versioning and object versions. |
| `quota` | Manage bucket quota. |
| `anonymous` | Manage anonymous bucket or prefix access. |
| `lifecycle` | Manage lifecycle rules, remote tiers, and object restore requests. |
| `replication` | Manage bucket replication rules and inspect replication status or differences. |

## Parameters

| Parameter | Description |
| --- | --- |
| `PATH` | `ALIAS/`, `ALIAS/BUCKET`, or `ALIAS/BUCKET/PREFIX`. |
| `--recursive` | Recursively list objects for `bucket list`. |
| `--versions` | Include object versions when listing supported versioned buckets. |
| `--summarize` | Show totals only for list output. |
| `--ignore-existing` | Do not fail if the bucket already exists when creating it. |
| `--with-versioning` | Enable versioning when creating the bucket. |
| `--with-lock` | Enable object lock when creating the bucket. |
| `--force` | Force destructive or capability-gated operations where supported. |
| `--dangerous` | Remove a bucket even when incomplete multipart uploads exist. |

## Examples

List buckets for an alias:

```bash
rc bucket list local/
```

Create a versioned bucket with object lock:

```bash
rc bucket create local/archive --with-versioning --with-lock
```

List notification rules:

```bash
rc bucket event list local/archive
```

Set CORS from an XML or JSON file:

```bash
rc bucket cors set local/archive cors.xml
```

Inspect default bucket encryption:

```bash
rc bucket encryption info local/archive
```

Set a bucket default to `SSE-KMS`:

```bash
rc bucket encryption set local/archive --mode sse-kms --key-id alias/archive-key
```

Check replication status:

```bash
rc bucket replication status local/archive
```

Scan for pending or failed versions below a prefix:

```bash
rc bucket replication diff local/archive --prefix reports/2026/
```

Actively validate configured targets and start a confirmed existing-object resync:

```bash
rc bucket replication check local/archive --yes
rc bucket replication resync start local/archive --older-than 30d --yes
rc bucket replication resync status local/archive
```

## Behavior

Prefer `rc bucket ...` for new scripts. Legacy commands such as `rc mb`, `rc rb`, `rc event`, `rc cors`, `rc version`, `rc anonymous`, `rc quota`, `rc ilm`, and `rc replicate` remain available and delegate to the same implementations.

`rc bucket encryption set`, `info`, and `clear` manage only the bucket default for future writes. They do not rewrite, decrypt, or re-encrypt existing objects in the bucket. For object-level encryption flags and more detailed examples, see [Encryption workflows](encryption.md).

`rc bucket replication diff` performs a bounded, read-only scan. A truncated result is partial and cannot be resumed; narrow `--prefix` and run the command again. An empty truncated result does not prove that the bucket has no replication backlog.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

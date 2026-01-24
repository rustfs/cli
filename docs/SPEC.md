# rc CLI Specification v2

> **PROTECTED FILE**: Changes to this specification require the Breaking Change process.
> See AGENTS.md for the required workflow.

## Overview

`rc` is a command-line interface for S3-compatible object storage services. It is designed
to work with RustFS, AWS S3, and other S3-compatible backends.

## General Conventions

### Path Syntax

- **Remote path**: `<alias>/<bucket>[/<key>]`
  - `local/mybucket` - refers to a bucket
  - `local/mybucket/path/to/file.txt` - refers to an object
  - `local/mybucket/path/to/dir/` - trailing slash indicates directory semantics

- **Local path**: Standard filesystem path
  - `/home/user/file.txt` - absolute path
  - `./file.txt` - relative path
  - `../file.txt` - relative path

### Trailing Slash Semantics

The trailing `/` is significant:

| Path | Meaning |
|------|---------|
| `alias/bucket/dir` | Could be a file named "dir" or a directory |
| `alias/bucket/dir/` | Explicitly a directory (prefix) |

For `ls`: A path without trailing slash lists the item itself if it's a file, or contents if it's a directory.
For `cp`: A path with trailing slash preserves the source filename.

### Overwrite Strategy

Default behavior depends on the mode:

- **Interactive mode** (TTY detected): Prompt for confirmation
- **Non-interactive mode**: Error unless flag specified

| Flag | Behavior |
|------|----------|
| `--overwrite` | Force overwrite existing files |
| `--no-clobber` | Skip existing files silently |
| `--if-match <etag>` | Only overwrite if ETag matches (conditional write) |

### Output Modes

| Flag | Behavior |
|------|----------|
| (none) | Human-readable output with colors and progress bars |
| `--json` | Strict JSON output (no colors, no progress, no logs) |
| `--quiet` | Suppress non-error output |
| `--no-color` | Disable colored output |
| `--no-progress` | Disable progress bars |

### JSON Output Contract

When `--json` is specified:

1. Output is valid JSON following `schemas/output_v2.json`
2. No ANSI color codes or escape sequences
3. No progress bars or spinners
4. Timestamps are ISO8601 UTC: `2024-01-15T10:30:00Z`
5. Sizes include both `size_bytes` (integer) and `size_human` (string)
6. Paths use `/` as separator regardless of platform

---

## Exit Codes

| Code | Name | Description |
|------|------|-------------|
| 0 | SUCCESS | Operation completed successfully |
| 1 | GENERAL_ERROR | Unspecified error |
| 2 | USAGE_ERROR | Invalid arguments or path format |
| 3 | NETWORK_ERROR | Retryable network error (timeout, 503, etc.) |
| 4 | AUTH_ERROR | Authentication or permission failure |
| 5 | NOT_FOUND | Bucket or object does not exist |
| 6 | CONFLICT | Precondition failed, version conflict |
| 7 | UNSUPPORTED_FEATURE | Backend does not support this operation |
| 130 | INTERRUPTED | Operation interrupted (Ctrl+C) |

---

## Commands

### alias - Manage Storage Aliases

#### alias set

Add or update a storage alias.

```
rc alias set <NAME> <ENDPOINT> <ACCESS_KEY> <SECRET_KEY> [OPTIONS]
```

**Arguments:**
| Argument | Description |
|----------|-------------|
| NAME | Unique alias name (alphanumeric, underscore, hyphen) |
| ENDPOINT | S3 endpoint URL |
| ACCESS_KEY | Access key ID |
| SECRET_KEY | Secret access key |

**Options:**
| Option | Default | Description |
|--------|---------|-------------|
| --region | us-east-1 | AWS region |
| --signature | v4 | Signature version: v4, v2 |
| --bucket-lookup | auto | Bucket lookup: auto, path, dns |
| --insecure | false | Allow insecure TLS |

**Exit Codes:** 0 (success), 2 (invalid input)

**Example:**
```bash
rc alias set local http://localhost:9000 accesskey secretkey
rc alias set s3 https://s3.amazonaws.com AKIAIOSFODNN7EXAMPLE wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
```

#### alias list

List configured aliases.

```
rc alias list [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| -l, --long | Show full details including endpoints |

**Output (human):**
```
local    http://localhost:9000
s3       https://s3.amazonaws.com
```

**Output (--json):**
```json
{
  "aliases": [
    {"name": "local", "endpoint": "http://localhost:9000", "region": "us-east-1"},
    {"name": "s3", "endpoint": "https://s3.amazonaws.com", "region": "us-east-1"}
  ]
}
```

**Exit Codes:** 0 (success)

#### alias remove

Remove an alias.

```
rc alias remove <NAME>
```

**Exit Codes:** 0 (success), 5 (alias not found)

---

### admin - Administrative Operations

Administrative commands for cluster management.

#### admin info

Display cluster, server, or disk information.

```
rc admin info cluster <ALIAS>
rc admin info server <ALIAS>
rc admin info disk <ALIAS> [OPTIONS]
```

**Arguments:**
| Argument | Description |
|----------|-------------|
| ALIAS | Alias name of the server |

**Options (disk):**
| Option | Description |
|--------|-------------|
| --offline | Show only offline disks |
| --healing | Show only healing disks |

**Output (--json):**
- `admin info cluster`: See `schemas/output_v2.json#admin-info-cluster`
- `admin info server`: See `schemas/output_v2.json#admin-info-server`
- `admin info disk`: See `schemas/output_v2.json#admin-info-disk`

**Exit Codes:** 0, 4 (auth error), 5 (alias not found)

#### admin heal

Manage cluster healing operations.

```
rc admin heal status <ALIAS>
rc admin heal start <ALIAS> [OPTIONS]
rc admin heal stop <ALIAS>
```

**Arguments:**
| Argument | Description |
|----------|-------------|
| ALIAS | Alias name of the server |

**Options (start):**
| Option | Default | Description |
|--------|---------|-------------|
| -b, --bucket | (all) | Specific bucket to heal |
| -p, --prefix | (none) | Object prefix to heal |
| --scan-mode | normal | Scan mode: normal, deep |
| --remove | false | Remove dangling objects/parts |
| --recreate | false | Recreate missing data |
| --dry-run | false | Show what would be healed without healing |

**Output (--json):**
- `admin heal status`: See `schemas/output_v2.json#admin-heal-status`
- `admin heal start/stop`: See `schemas/output_v2.json#admin-heal-operation`

**Exit Codes:** 0, 2 (invalid input), 4 (auth error), 5 (alias not found)

---

### ls - List Objects

List buckets or objects.

```
rc ls [OPTIONS] <PATH>
```

**Arguments:**
| Argument | Description |
|----------|-------------|
| PATH | Remote path: `alias/` (list buckets) or `alias/bucket[/prefix]` |

**Options:**
| Option | Default | Description |
|--------|---------|-------------|
| -l, --long | false | Show detailed information |
| -r, --recursive | false | List recursively |
| --max-keys | 1000 | Maximum keys per request |

**Output (human):**
```
[2024-01-15 10:30:00]     0B dir/
[2024-01-15 10:30:00] 1.2MiB file.txt
```

**Output (--json):** See `schemas/output_v2.json#ls`

**Exit Codes:** 0, 2 (invalid path), 4 (auth error), 5 (bucket not found)

---

### mb - Make Bucket

Create a new bucket.

```
rc mb <PATH>
```

**Arguments:**
| Argument | Description |
|----------|-------------|
| PATH | Remote path: `alias/bucket` |

**Exit Codes:** 0, 2 (invalid path), 4 (auth error), 6 (bucket exists)

---

### rb - Remove Bucket

Delete a bucket.

```
rc rb [OPTIONS] <PATH>
```

**Options:**
| Option | Description |
|--------|-------------|
| --force | Delete bucket even if not empty (deletes all objects first) |

**Exit Codes:** 0, 4 (auth error), 5 (bucket not found), 6 (bucket not empty)

---

### cat - Display Object Contents

Output object contents to stdout.

```
rc cat <PATH>
```

**Exit Codes:** 0, 4 (auth error), 5 (object not found)

---

### head - Display First Lines

Output first N lines of an object.

```
rc head [OPTIONS] <PATH>
```

**Options:**
| Option | Default | Description |
|--------|---------|-------------|
| -n, --lines | 10 | Number of lines to display |

**Exit Codes:** 0, 4 (auth error), 5 (object not found)

---

### stat - Show Object Metadata

Display object or bucket metadata.

```
rc stat <PATH>
```

**Output (human):**
```
Name      : file.txt
Size      : 1.2 MiB (1258291 bytes)
Type      : application/octet-stream
ETag      : d41d8cd98f00b204e9800998ecf8427e
Modified  : 2024-01-15T10:30:00Z
```

**Output (--json):** See `schemas/output_v2.json#stat`

**Exit Codes:** 0, 4 (auth error), 5 (not found)

---

### cp - Copy Objects

Copy objects between locations.

```
rc cp [OPTIONS] <SOURCE> <TARGET>
```

**Options:**
| Option | Description |
|--------|-------------|
| -r, --recursive | Copy directories recursively |
| --overwrite | Overwrite existing objects |
| --no-clobber | Skip existing objects |

**Supported Transfers:**
- Local → Remote: `rc cp ./file.txt local/bucket/`
- Remote → Local: `rc cp local/bucket/file.txt ./`
- Remote → Remote: `rc cp local/bucket1/file.txt local/bucket2/`

**Exit Codes:** 0, 2 (invalid path), 4 (auth error), 5 (source not found)

---

### mv - Move Objects

Move (rename) objects.

```
rc mv [OPTIONS] <SOURCE> <TARGET>
```

Same options and behavior as `cp`, but deletes source after successful copy.

---

### rm - Remove Objects

Delete objects.

```
rc rm [OPTIONS] <PATH>
```

**Options:**
| Option | Description |
|--------|-------------|
| -r, --recursive | Delete recursively |
| --force | Don't prompt for confirmation |

**Exit Codes:** 0, 4 (auth error), 5 (not found)

---

### find - Find Objects

Search for objects matching criteria.

```
rc find [OPTIONS] <PATH>
```

**Options:**
| Option | Description |
|--------|-------------|
| --name | Glob pattern for object names |
| --larger | Minimum size (e.g., "10MB") |
| --smaller | Maximum size |
| --newer-than | Modified after (e.g., "1d", "2024-01-01") |
| --older-than | Modified before |

---

### diff - Compare Locations

Show differences between two locations.

```
rc diff <PATH1> <PATH2>
```

---

### mirror - Synchronize Locations

Mirror objects from source to target.

```
rc mirror [OPTIONS] <SOURCE> <TARGET>
```

**Options:**
| Option | Description |
|--------|-------------|
| --delete | Delete objects in target not in source |
| --dry-run | Show what would be done |

---

### tree - Display Tree Structure

Display objects in tree format.

```
rc tree [OPTIONS] <PATH>
```

---

### share - Generate Presigned URL

Generate a presigned URL for temporary access.

```
rc share download <PATH> [OPTIONS]
```

**Options:**
| Option | Default | Description |
|--------|---------|-------------|
| --expire | 7d | URL expiration time |

---

## Optional Commands (Capability-Dependent)

These commands require specific backend support. If the backend doesn't support
the feature, the command returns exit code 7 (UNSUPPORTED_FEATURE).

Use `--force` to attempt the operation anyway.

### version - Bucket Versioning

```
rc version enable <PATH>
rc version suspend <PATH>
rc version list <PATH>
```

### retention - Object Retention

```
rc retention set [OPTIONS] <PATH>
rc retention clear <PATH>
```

### tag - Object Tags

```
rc tag set <PATH> <TAGS>
rc tag list <PATH>
rc tag remove <PATH>
```

### watch - Event Notifications

```
rc watch <PATH>
```

### sql - S3 Select Queries

```
rc sql [OPTIONS] <PATH>
```

---

## Configuration

Configuration is stored in `~/.config/rc/config.toml`.

See the plan document for full configuration schema.

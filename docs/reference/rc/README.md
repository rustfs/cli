# rc Command Reference

This reference documents the operations exposed by `rc`, the RustFS S3-compatible command-line client. The structure follows the command-reference style used by MinIO `mc`: each operation describes its purpose, syntax, parameters, examples, and behavior, while keeping the examples specific to `rc`.

`rc` supports both noun-first command groups and legacy command names. Prefer the noun-first groups for new scripts:

| Operation | Preferred form | Legacy-compatible form |
| --- | --- | --- |
| Configure targets | [`rc alias`](alias.md) | none |
| Bucket workflows | [`rc bucket`](bucket.md) | `rc ls`, `rc mb`, `rc rb`, `rc event`, `rc cors`, `rc version`, `rc anonymous`, `rc quota`, `rc ilm`, `rc replicate` |
| Object workflows | [`rc object`](object.md) | `rc ls`, `rc cp`, `rc mv`, `rc rm`, `rc cat`, `rc head`, `rc stat`, `rc find`, `rc tree`, `rc share` |
| Administrative workflows | [`rc admin`](admin.md) | none |
| Streaming upload | [`rc pipe`](pipe.md) | none |
| Difference reports | [`rc diff`](diff.md) | none |
| Mirroring | [`rc mirror`](mirror.md) | none |
| S3 Select | [`rc sql`](sql.md) | none |
| Tags | [`rc tag`](tag.md) | none |
| Shell completions | [`rc completions`](completions.md) | none |

Legacy command pages remain documented for users migrating from MinIO `mc`-style workflows:

- [`rc ls`](ls.md)
- [`rc mb`](mb.md)
- [`rc rb`](rb.md)
- [`rc cat`](cat.md)
- [`rc head`](head.md)
- [`rc stat`](stat.md)
- [`rc cp`](cp.md)
- [`rc mv`](mv.md)
- [`rc rm`](rm.md)
- [`rc find`](find.md)
- [`rc event`](event.md)
- [`rc cors`](cors.md)
- [`rc tree`](tree.md)
- [`rc share`](share.md)
- [`rc version`](version.md)
- [`rc anonymous`](anonymous.md)
- [`rc quota`](quota.md)
- [`rc ilm`](ilm.md)
- [`rc replicate`](replicate.md)

## Path Format

Remote paths use `ALIAS/BUCKET/KEY` form. An alias-only path such as `local/` refers to a configured S3-compatible service. A bucket path such as `local/photos` refers to a bucket. An object path such as `local/photos/2026/image.jpg` refers to a specific object key.

## Output Modes

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

## Credentials

Credentials are stored through aliases. Configure an alias before running remote operations:

```bash
rc alias set local http://localhost:9000 ACCESS_KEY SECRET_KEY
```

Do not put production credentials in examples, logs, issue descriptions, or screenshots.

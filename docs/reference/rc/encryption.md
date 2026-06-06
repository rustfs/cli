# Encryption Workflows

## Purpose

This guide documents the bucket and object encryption workflows exposed by `rc`. It follows the same high-level split as MinIO `mc`: bucket default encryption is a bucket operation, while object write encryption is set per write command. In `rc`, bucket defaults are managed through the noun-first `rc bucket encryption` command family, and object write encryption is configured on `rc cp`, `rc mv`, and `rc pipe`.

## Syntax

Bucket default encryption:

```bash
rc bucket encryption set <ALIAS/BUCKET> --mode sse-s3
rc bucket encryption set <ALIAS/BUCKET> --mode sse-kms
rc bucket encryption set <ALIAS/BUCKET> --mode sse-kms --key-id <KMS_KEY_ID>
rc bucket encryption info <ALIAS/BUCKET>
rc bucket encryption clear <ALIAS/BUCKET>
```

Object write encryption:

```bash
rc cp <SOURCE> <TARGET> --enc-s3 <TARGET>
rc cp <SOURCE> <TARGET> --enc-kms <TARGET>=<KMS_KEY_ID>

rc mv <SOURCE> <TARGET> --enc-s3 <TARGET>
rc mv <SOURCE> <TARGET> --enc-kms <TARGET>=<KMS_KEY_ID>

rc pipe <ALIAS/BUCKET/KEY> --enc-s3
rc pipe <ALIAS/BUCKET/KEY> --enc-kms <KMS_KEY_ID>
```

## Modes

| Mode | Meaning |
| --- | --- |
| `sse-s3` | Use S3-managed keys (`AES256`). |
| `sse-kms` | Use KMS-managed keys with either the server default key or a provided key identifier. |

## Bucket Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose default encryption is managed. Object paths are invalid here. |
| `--mode` | Required for `set`. Accepts `sse-s3` or `sse-kms`. |
| `--key-id` | Optional with `--mode sse-kms`; when omitted, the server default KMS key is used. Invalid with `--mode sse-s3`. |

## Object Write Parameters

| Parameter | Description |
| --- | --- |
| `--enc-s3 <TARGET>` | Apply `SSE-S3` to the named remote destination write. |
| `--enc-kms <TARGET>=<KMS_KEY_ID>` | Apply `SSE-KMS` to the named remote destination write. |
| `--enc-s3` | On `rc pipe`, apply `SSE-S3` to the single upload target. |
| `--enc-kms <KMS_KEY_ID>` | On `rc pipe`, apply `SSE-KMS` to the single upload target. |

## Examples

Configure bucket default encryption:

```bash
rc bucket encryption set local/archive --mode sse-s3
rc bucket encryption info local/archive
rc bucket encryption clear local/archive
```

Configure bucket default encryption with KMS:

```bash
rc bucket encryption set local/archive --mode sse-kms
rc bucket encryption set local/archive --mode sse-kms --key-id alias/archive-key
```

Upload with explicit destination encryption:

```bash
rc cp ./report.json local/archive/report.json --enc-s3 local/archive/report.json
rc mv local/inbox/a.txt local/archive/a.txt --enc-kms local/archive/a.txt=alias/archive-key
printf 'hello\n' | rc pipe local/archive/hello.txt --enc-s3
```

Recursively copy to a remote prefix and encrypt the entire write target:

```bash
rc cp ./reports/ local/archive/ --recursive --enc-kms local/archive/=alias/archive-key
rc mv local/inbox/ local/archive/ --recursive --enc-s3 local/archive/
```

## Behavior

Bucket default encryption applies to new writes when no object-level encryption flag is supplied. Object-level encryption flags override the bucket default for that specific write.

Changing or clearing a bucket default does not rewrite existing objects. Objects already written with `SSE-S3` or `SSE-KMS` remain as stored until a later write replaces them.

For `rc cp` and `rc mv`, destination encryption is scoped to the current command target only. The selector in `--enc-s3` or `--enc-kms` must exactly match the destination path you passed on the command line:

- Use the full remote object path for one object.
- Use the exact remote prefix argument for recursive writes.

`rc` currently supports:

- `SSE-S3`
- `SSE-KMS`

## Compatibility Notes

The current implementation intentionally stays smaller than MinIO `mc`:

- No `SSE-C` support.
- No KMS encryption context or bucket key configuration.
- No repeated `--enc-s3` or `--enc-kms` selectors on a single command.
- No selector expansion beyond the exact destination argument of the current `rc cp` or `rc mv` invocation.

These limits are part of the current `rc` contract and are documented here so scripts do not assume broader `mc` compatibility than the implementation provides.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

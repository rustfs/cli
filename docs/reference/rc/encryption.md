# rc encryption

## Purpose

`rc encryption` documents the bucket and object encryption workflows exposed by `rc`. Bucket default encryption is managed through the noun-first `rc bucket encryption` command family. Object write encryption is configured per destination write on `rc cp`, `rc mv`, and `rc pipe`.

## Syntax

```bash
rc bucket encryption set <ALIAS/BUCKET> --mode sse-s3
rc bucket encryption set <ALIAS/BUCKET> --mode sse-kms --key-id <KMS_KEY_ID>
rc bucket encryption info <ALIAS/BUCKET>
rc bucket encryption clear <ALIAS/BUCKET>

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
| `sse-kms` | Use KMS-managed keys with the provided key identifier. |

## Bucket Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose default encryption is managed. Object paths are invalid here. |
| `--mode` | Required for `set`. Accepts `sse-s3` or `sse-kms`. |
| `--key-id` | Required with `--mode sse-kms`. Invalid with `--mode sse-s3`. |

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
rc bucket encryption set local/archive --mode sse-kms --key-id alias/archive-key
```

Upload with explicit destination encryption:

```bash
rc cp ./report.json local/archive/report.json --enc-s3 local/archive/report.json
rc mv local/inbox/a.txt local/archive/a.txt --enc-kms local/archive/a.txt=alias/archive-key
printf 'hello\n' | rc pipe local/archive/hello.txt --enc-s3
```

## Behavior

Bucket default encryption applies to new writes when no object-level encryption flag is supplied. Object-level encryption flags override the bucket default for that specific write.

`rc` currently supports:

- `SSE-S3`
- `SSE-KMS`

This reference does not document `SSE-C`, KMS encryption context, or bucket key configuration because those are not part of the current implementation.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

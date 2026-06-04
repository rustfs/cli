# rc pipe

## Purpose

`rc pipe` streams stdin directly into an object. Use it for generated data, shell pipelines, and backups that should not be written to a temporary local file first.

## Syntax

```bash
rc [GLOBAL OPTIONS] pipe [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Destination object path. |
| `--content-type` | Object content type. Defaults to `application/octet-stream`. |
| `--storage-class` | Storage class for the uploaded object. |
| `--enc-s3` | Apply `SSE-S3` to the upload target. |
| `--enc-kms <KMS_KEY_ID>` | Apply `SSE-KMS` to the upload target. |

## Examples

```bash
tar -czf - ./logs | rc pipe local/backups/logs.tar.gz --content-type application/gzip
printf 'hello\n' | rc pipe local/tmp/hello.txt --content-type text/plain
printf 'hello\n' | rc pipe local/archive/hello.txt --enc-s3
tar -czf - ./logs | rc pipe local/backups/logs.tar.gz --enc-kms alias/backup-key
```

## Behavior

The command reads from stdin until EOF and uploads that stream as the destination object.

`--enc-s3` and `--enc-kms` are mutually exclusive. When either flag is set, that object write uses the requested mode even if the bucket default encryption is different. The current implementation supports `SSE-S3` and `SSE-KMS` only. For shared encryption rules across commands, see [Encryption workflows](encryption.md).

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

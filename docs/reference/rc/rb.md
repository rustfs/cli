# rc rb

## Purpose

`rc rb` removes a bucket. It is a legacy-compatible command; prefer `rc bucket remove` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] rb [OPTIONS] <ALIAS/BUCKET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket path to remove. |
| `--force` | Run the staged client-side cleanup before ordinary bucket deletion. |
| `--dangerous` | Permit aborting the multipart uploads discovered before mutation. Requires `--force --yes`. |
| `--yes` | Confirm dangerous multipart cleanup. Valid only with `--force --dangerous`. |

## Examples

```bash
rc rb local/empty-bucket
rc bucket remove local/old-bucket --force
rc bucket remove local/old-bucket --force --dangerous --yes
```

## Behavior

Without `--force`, `rc` sends only the ordinary S3 `DeleteBucket` request.

With `--force`, `rc` first discovers all current objects, versions (including null
versions), delete markers, and incomplete multipart uploads. Discovery must complete
before any mutation. Multipart uploads cause a refusal unless
`--force --dangerous --yes` was supplied.

The precomputed object set is deleted in deterministic batches of at most 1000.
Any item failure stops later batches and prevents multipart cleanup and bucket
deletion. Object Lock legal hold, governance retention, and compliance retention are
reported as conflicts; this command does not bypass retention.

In dangerous mode, only the precomputed multipart set is aborted, with bounded
concurrency. `rc` then re-lists objects, versions, delete markers, and multipart
uploads. Any concurrent residue prevents bucket deletion. The final request is always
ordinary S3 `DeleteBucket`; `rc bucket remove` never sends
`x-rustfs-force-delete`.

JSON mode emits the output-v3 `bucket_remove` envelope with completed stages,
the failed stage, discovery totals, and per-item success or failure records. Cleanup
is not transactional and no rollback is attempted.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

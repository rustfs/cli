# rc mb

## Purpose

`rc mb` creates a bucket. It is a legacy-compatible command; prefer `rc bucket create` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] mb [OPTIONS] <ALIAS/BUCKET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Target bucket path to create. |
| `-p, --ignore-existing` | Treat an existing bucket as success. |
| `--region REGION` | Send an explicit S3 location constraint and verify the location reported by the service. |
| `--with-lock` | Enable Object Lock at bucket creation time. This also enables versioning. |
| `--with-versioning` | Enable bucket versioning after creation. |

## Examples

```bash
rc mb local/reports
rc bucket create local/archive --region us-east-1 --with-lock
rc mb local/reports --ignore-existing
```

## Behavior

Bucket creation uses the endpoint and credentials from the alias. `--with-lock` implies
`--with-versioning` because S3 Object Lock requires versioning. Object Lock is sent in the
create request and cannot be added to an existing unlocked bucket.

`--with-versioning` is a post-create operation when Object Lock is not requested. `rc` verifies
the effective versioning and Object Lock states before reporting success. If creation succeeds
but a later enable or verification stage fails, `rc` reports a partial result and does not remove
the bucket. The JSON result lists `completed_stages` and `failed_stage` so retry logic can inspect
the durable state.

With `--ignore-existing`, a matching existing bucket is successful. Versioning may be enabled and
verified on an existing bucket. An Object Lock request succeeds only if the existing bucket is
already locked; `rc` never attempts a retroactive Object Lock mutation. An explicit region must
match the location reported by the service.

RustFS beta.10 treats the region as server-global. The create request still carries an explicit
`--region`, but RustFS does not persist a distinct per-bucket region. Human and JSON output label
the effective value as the service-reported location; a mismatch is a conflict rather than a
claim that the requested value was stored per bucket.

## JSON compatibility (BREAKING)

`rc mb` and `rc bucket create` now emit the output v3 `bucket_operations` family in JSON mode.
This replaces the legacy root-level `{status, bucket, message}` shape for these commands. Scripts
must dispatch on `schema_version: 3`, read the requested and effective state from `data`, and read
failures from `error`. A failure may also contain `data` when a create workflow completed one or
more durable stages.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

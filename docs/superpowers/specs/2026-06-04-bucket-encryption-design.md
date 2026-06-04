# Bucket Encryption and SSE/KMS Design

## Summary

This design adds encryption management to `rc` with a command surface that follows the existing noun-first CLI structure while aligning behavior with the primary MinIO `mc` encryption workflows.

The first implementation target is:

- Bucket default encryption management through `rc bucket encryption`
- Object write encryption for `cp`, `mv`, and `pipe`
- Support for `SSE-S3` and `SSE-KMS`
- No support for `SSE-C` in this phase
- Best-effort compatibility for generic S3 targets, with RustFS/MinIO as the primary compatibility target

## Goals

- Add bucket default encryption commands that map to S3 bucket encryption APIs
- Add object-level encryption flags for write and rewrite operations
- Keep all encryption behavior behind `rc_core` abstractions
- Match common `mc` encryption behavior where practical
- Preserve current CLI conventions and exit code behavior

## Non-Goals

- `SSE-C`
- Encryption flags for `mirror`
- Encryption metadata display in `stat` or `head`
- KMS encryption context support
- Bucket key support
- Changes to protected CLI contract files unless later required by implementation

## User-Facing Command Surface

### Bucket Default Encryption

Commands:

```bash
rc bucket encryption set <PATH> --mode sse-s3
rc bucket encryption set <PATH> --mode sse-kms --key-id <KMS_KEY_ID>
rc bucket encryption info <PATH>
rc bucket encryption clear <PATH>
```

Rules:

- `<PATH>` must be a bucket path in `alias/bucket` form
- `--mode` is required for `set`
- `--key-id` is only valid when `--mode sse-kms` is selected
- `info` returns success when encryption is not configured and reports `Not configured`
- `clear` removes the bucket encryption configuration

### Object Write Encryption

Commands:

```bash
rc cp <SOURCE> <TARGET> --enc-s3 <TARGET>
rc cp <SOURCE> <TARGET> --enc-kms <TARGET>=<KMS_KEY_ID>

rc mv <SOURCE> <TARGET> --enc-s3 <TARGET>
rc mv <SOURCE> <TARGET> --enc-kms <TARGET>=<KMS_KEY_ID>

rc pipe <TARGET> --enc-s3
rc pipe <TARGET> --enc-kms <KMS_KEY_ID>
```

Rules:

- `cp` and `mv` encryption flags apply only to the destination write
- `pipe` encryption flags apply to the single remote write target
- `--enc-s3` and `--enc-kms` are mutually exclusive for the same destination
- Bucket default encryption remains in effect when no object-level encryption is supplied
- Object-level encryption overrides the bucket default for that specific write

## Alignment with `mc`

This design intentionally aligns with the `mc` primary workflow rather than cloning its full command layout.

Aligned behavior:

- Bucket default encryption can be set, inspected, and cleared
- Object writes can explicitly request `SSE-S3` or `SSE-KMS`
- `SSE-C` is excluded from the first phase
- Object-level encryption takes precedence over bucket default encryption for a specific write

Intentional differences:

- `rc` uses `rc bucket encryption` instead of top-level `mc encrypt`
- `rc` keeps bucket operations under noun-first command groups
- The first phase does not include the broader long-tail encryption feature set

## Architecture

### Layering

- `crates/cli`
  - Parse command-line arguments
  - Validate user input
  - Format output
  - Map domain errors to exit codes
- `crates/core`
  - Define encryption domain models
  - Extend `ObjectStore` with bucket encryption operations
  - Define object write encryption request types
- `crates/s3`
  - Map domain models to S3 SDK calls, request fields, and response parsing

This keeps encryption out of the `cli` crate implementation details and preserves the current dependency boundaries.

### Core Domain Types

Add a new encryption domain module in `crates/core`, re-exported through `rc_core`.

Suggested types:

```rust
pub enum BucketEncryption {
    SseS3,
    SseKms { key_id: String },
}

pub enum ObjectEncryptionRequest {
    SseS3,
    SseKms { key_id: String },
}
```

Representation details:

- `Option<BucketEncryption>` is used to represent an unconfigured bucket encryption state
- `ObjectEncryptionRequest` is only used for write operations
- The first phase should reject unsupported or partially parsed encryption states rather than silently downgrading behavior

### `ObjectStore` Extensions

Add bucket encryption methods:

```rust
async fn get_bucket_encryption(&self, bucket: &str) -> Result<Option<BucketEncryption>>;
async fn set_bucket_encryption(&self, bucket: &str, encryption: BucketEncryption) -> Result<()>;
async fn delete_bucket_encryption(&self, bucket: &str) -> Result<()>;
```

Extend write operations to accept optional encryption:

```rust
async fn put_object(
    &self,
    path: &RemotePath,
    data: Vec<u8>,
    content_type: Option<&str>,
    encryption: Option<&ObjectEncryptionRequest>,
) -> Result<ObjectInfo>;

async fn copy_object(
    &self,
    src: &RemotePath,
    dst: &RemotePath,
    encryption: Option<&ObjectEncryptionRequest>,
) -> Result<ObjectInfo>;
```

Implementation notes:

- Existing call sites without explicit encryption should pass `None`
- The design should avoid duplicating S3 header logic in each command

## S3 Backend Mapping

### Bucket Default Encryption

Use these S3 APIs:

- `GetBucketEncryption`
- `PutBucketEncryption`
- `DeleteBucketEncryption`

Expected mappings:

- `SSE-S3` maps to `AES256`
- `SSE-KMS` maps to `aws:kms` with a KMS key ID
- Missing bucket encryption configuration is treated as `Ok(None)`

Error handling:

- Missing bucket encryption configuration should not be treated as a command failure
- Unsupported or unknown server-side encryption rules should produce a clear error indicating unsupported encryption configuration

### Object Write Encryption

For uploads and remote copies:

- `SSE-S3` maps to `x-amz-server-side-encryption: AES256`
- `SSE-KMS` maps to:
  - `x-amz-server-side-encryption: aws:kms`
  - `x-amz-server-side-encryption-aws-kms-key-id: <KMS_KEY_ID>`

Applies to:

- `put_object`
- `copy_object`
- Any command path layered on top of those APIs, including `mv` when implemented as copy-then-delete

## CLI Design

### New Bucket Command Group

Add a new command module:

- `crates/cli/src/commands/encryption.rs`

Expose it through:

- `rc bucket encryption ...`
- Bucket command help output and dispatch

Suggested subcommands:

- `Set`
- `Info`
- `Clear`

Suggested output model:

```rust
struct BucketEncryptionOutput {
    bucket: String,
    status: String,
    mode: Option<String>,
    kms_key_id: Option<String>,
}
```

Human-readable examples:

- `Bucket: my-bucket`
- `Encryption: Not configured`
- `Encryption: SSE-S3`
- `Encryption: SSE-KMS`
- `KMS Key ID: my-key`

### Existing Command Extensions

Extend these command modules:

- `crates/cli/src/commands/cp.rs`
- `crates/cli/src/commands/mv.rs`
- `crates/cli/src/commands/pipe.rs`

Add argument parsing for:

- `--enc-s3`
- `--enc-kms`

Behavior:

- `cp` and `mv` parse destination-bound encryption requests
- `pipe` parses a single write-target encryption request
- Parsing should occur before any remote operation begins so invalid combinations fail early

## Validation Rules

### Bucket Encryption Validation

- Empty path is invalid
- `alias/bucket/object` is invalid for bucket encryption commands
- `--mode sse-s3` with `--key-id` is invalid
- `--mode sse-kms` without `--key-id` is invalid
- Unknown mode values are invalid

### Object Encryption Validation

- `--enc-s3` and `--enc-kms` cannot target the same destination
- `--enc-kms <TARGET>=<KMS_KEY_ID>` must contain both a destination and a non-empty key ID
- Local destinations are invalid for encryption flags
- Source paths must never be treated as write encryption targets
- `pipe` cannot accept both `--enc-s3` and `--enc-kms`

## Error Handling and Exit Codes

Use current command conventions and existing exit code mappings.

- Invalid flags or path shape: `UsageError`
- Unsupported backend capability: `UnsupportedFeature`
- Permission or credentials failure: `AuthError`
- Missing bucket: `NotFound`
- Network and retryable transport failures: existing network mapping
- Unrecognized backend encryption configuration: `GeneralError` with an explicit error message

Important behavior:

- `info` for an unconfigured bucket must return success
- The human-readable text for unsupported configurations should state that the server returned an unsupported encryption configuration

## Testing Strategy

### Help Contract Tests

Update `crates/cli/tests/help_contract.rs` to cover:

- `rc bucket encryption`
- `rc bucket encryption set`
- `rc bucket encryption info`
- `rc bucket encryption clear`
- `rc cp --help` includes `--enc-s3` and `--enc-kms`
- `rc mv --help` includes `--enc-s3` and `--enc-kms`
- `rc pipe --help` includes `--enc-s3` and `--enc-kms`

### CLI Unit Tests

Add unit tests for:

- Bucket path validation failures
- `sse-kms` without `--key-id`
- `sse-s3` with `--key-id`
- Invalid `--enc-kms` format
- `--enc-s3` and `--enc-kms` conflicts
- `pipe` encryption flag conflicts

Each new command path should include at least two exit code scenarios.

### S3 Adapter Tests

Add focused tests in `crates/s3/src/client.rs` for:

- Bucket encryption response parsing
- Missing bucket encryption configuration detection
- Bucket encryption request construction
- `put_object` with `SSE-S3`
- `put_object` with `SSE-KMS`
- `copy_object` with `SSE-S3`
- `copy_object` with `SSE-KMS`

### Integration Tests

Add integration coverage for:

- `bucket encryption set -> info -> clear`
- `cp --enc-s3`
- `cp --enc-kms`
- `mv` with at least one encryption scenario
- `pipe --enc-s3`
- `pipe --enc-kms`

For KMS integration coverage:

- Prefer a real supported backend path when available
- If the current integration environment does not provide usable KMS support, tests may skip with a clear reason
- Skipping must be explicit and visible, not silently treated as success

## Files Expected to Change During Implementation

Expected core files:

- `crates/core/src/lib.rs`
- `crates/core/src/traits.rs`
- `crates/core/src/encryption.rs` (new)

Expected CLI files:

- `crates/cli/src/commands/mod.rs`
- `crates/cli/src/commands/bucket.rs`
- `crates/cli/src/commands/encryption.rs` (new)
- `crates/cli/src/commands/cp.rs`
- `crates/cli/src/commands/mv.rs`
- `crates/cli/src/commands/pipe.rs`
- `crates/cli/tests/help_contract.rs`
- `crates/cli/tests/integration.rs`

Expected S3 files:

- `crates/s3/src/client.rs`

Expected documentation files:

- `docs/reference/rc/encryption.md` (new)
- `docs/reference/rc/bucket.md`
- `docs/reference/rc/cp.md`
- `docs/reference/rc/mv.md`
- `docs/reference/rc/pipe.md`

## Open Compatibility Notes

- AWS S3 and MinIO-compatible systems do not always expose identical error shapes for missing bucket encryption configuration
- The implementation should follow the same pragmatic pattern already used for missing CORS or replication configuration detection
- RustFS/MinIO is the primary support target for advanced-path behavior and integration testing

## Delivery Boundary

The first implementation should be considered complete when all of the following are true:

- `rc bucket encryption set|info|clear` is implemented
- `cp`, `mv`, and `pipe` support `SSE-S3` and `SSE-KMS` destination encryption
- Help contract coverage is added
- Unit and adapter tests cover parser and backend behavior
- Integration tests cover the main supported flows
- Documentation is added for the new command surface

The first implementation should not expand into adjacent encryption features unless a concrete backend or test gap requires it.

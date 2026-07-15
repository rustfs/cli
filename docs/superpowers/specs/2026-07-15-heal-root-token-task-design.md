# Root Heal Token Task Support Design

## Objective

Allow operators to inspect and stop a manual root recursive heal by supplying the client token returned by `rc admin heal start <ALIAS>` without also supplying a bucket. Preserve the existing tokenless global background-status behavior and all bucket/prefix task workflows.

## Problem

The RustFS server supports task-scoped requests against the root heal route:

```text
POST /rustfs/admin/v3/heal/?clientToken=<TOKEN>
POST /rustfs/admin/v3/heal/?clientToken=<TOKEN>&forceStop=true
```

The CLI can start a root recursive heal and receives a client token, but its validation currently rejects `--client-token` unless `--bucket` is also present. The S3 adapter independently rejects an empty task bucket. As a result, the CLI cannot query or stop the root task it started.

## Scope

The change covers both root task status and root task stop so the two operations remain symmetric.

In scope:

- Accept `--client-token` without `--bucket` for `heal status` and `heal stop`.
- Map a token-scoped request with an empty bucket and no prefix to `/heal/`.
- Preserve bucket and prefix task behavior.
- Preserve tokenless global status and stop behavior.
- Add CLI validation tests and S3 request-contract tests.
- Update the README and the protected admin command reference.

Out of scope:

- Server changes.
- JSON output schema changes.
- Exit code changes.
- Configuration or schema-version changes.
- Historical task persistence beyond the server's existing retention behavior.

## Command Behavior

The CLI will use the following parameter matrix for both status and stop:

| Bucket | Prefix | Client token | Result |
| --- | --- | --- | --- |
| Absent | Absent | Absent | Use the existing global operation. |
| Present | Optional | Present | Use the existing bucket/prefix task operation. |
| Absent | Absent | Present | Use the root token-scoped task operation. |
| Present | Optional | Absent | Return `UsageError`. |
| Absent | Present | Any | Return `UsageError`. |

Examples:

```bash
rc admin heal start rustfs
rc admin heal status rustfs --client-token <TOKEN_FROM_START>
rc admin heal stop rustfs --client-token <TOKEN_FROM_START>
```

The existing command remains unchanged:

```bash
rc admin heal status rustfs
```

It continues to query `/background-heal/status` and report aggregate runtime state rather than a specific manual task.

## Architecture

### CLI request construction

`heal_task_request` remains the shared validator for status and stop. A token-only invocation will produce a `HealTaskRequest` with an empty `bucket`, no `prefix`, and the supplied token. Keeping `bucket: String` avoids changing the public core type or its serialization.

The validator will continue to reject a prefix without a bucket and a bucket without a token.

### S3 task path construction

`rustfs_heal_task_path` will support three valid mappings:

```text
bucket="",  prefix=None      -> /heal/
bucket="b", prefix=None      -> /heal/b
bucket="b", prefix=Some("p") -> /heal/b/p
```

An empty bucket with a non-empty prefix remains invalid as a defensive boundary check even though CLI validation rejects it first.

The existing `heal_task_status` and `heal_task_stop` methods will continue adding `clientToken` and `forceStop` query parameters. No `AdminApi` trait change is required.

### Error handling

- Missing token for a bucket-scoped task remains `UsageError`.
- Prefix without bucket remains `UsageError`.
- Root token requests use existing admin API error mapping for authentication, transport, and server failures.
- Server task summaries such as `notFound`, `finished`, and `stopped` retain their current output and exit-code behavior.

## Testing Strategy

Implementation will follow test-driven development.

CLI unit tests will first assert that:

- Token-only input creates a root `HealTaskRequest`.
- Bucket plus token remains valid.
- Bucket without token remains invalid.
- Prefix without bucket remains invalid.

S3 unit and request-contract tests will first assert that:

- An empty bucket without a prefix maps to `/heal/`.
- An empty bucket with a prefix is rejected.
- Root task status sends `POST /rustfs/admin/v3/heal/?clientToken=<TOKEN>`.
- Root task stop sends `POST /rustfs/admin/v3/heal/?clientToken=<TOKEN>&forceStop=true`.
- Status and stop responses preserve the client token and existing status mapping.

After targeted tests pass, the full required validation is:

```bash
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

## Documentation

`README.md` will show the complete root manual-heal workflow and distinguish task-scoped status from tokenless aggregate status.

`docs/reference/rc/admin.md` will document token-only root task status and stop commands, update the Heal Workflow table, and state that every manual heal is token-scoped. Because this reference path is protected by `AGENTS.md`, the pull request title or description must include `BREAKING`. The change is additive and does not alter configuration or JSON output, so no schema version bump or migration is applicable.

## Files

- Modify `crates/cli/src/commands/admin/heal.rs` for validation and CLI unit tests.
- Modify `crates/s3/src/admin.rs` for root path mapping and request-contract tests.
- Modify `README.md` for the root task workflow.
- Modify `docs/reference/rc/admin.md` for the formal command reference.

## Acceptance Criteria

- A root recursive heal token can be used with both `heal status` and `heal stop` without a bucket.
- Tokenless global status behavior is unchanged.
- Existing bucket and prefix task behavior is unchanged.
- Invalid bucket/token and prefix/bucket combinations still return usage errors.
- Exact root status and stop HTTP request contracts are covered by tests.
- README and admin reference documentation describe the new supported workflow.
- Formatting, clippy, and workspace tests pass before any implementation commit.

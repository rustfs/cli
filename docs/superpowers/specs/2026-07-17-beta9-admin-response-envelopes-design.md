# Beta.9 Admin Response Envelopes Design

## Context

RustFS 1.0.0-beta.9 changed two existing admin v3 responses from flat payloads to envelopes that also advertise v4 discovery routes:

- `GET /rustfs/admin/v3/info` now returns the cluster information under `info`.
- `GET /rustfs/admin/v3/pools/status` now returns the requested pool under `pool`.

The CLI currently deserializes both response bodies directly into `ClusterInfo` and `PoolStatus`. Those models default every field and ignore unknown fields, so the beta.9 envelopes deserialize successfully as empty values. The user then sees `standalone`, `unknown`, zero servers and disks, or an empty pool instead of an error.

## Goal

Decode the RustFS 1.0.0-beta.9 response envelopes for cluster information and targeted pool status while preserving the existing `AdminApi` interface and CLI output contracts.

## Scope

The change covers:

- `rc admin info cluster`
- `rc admin info server`
- `rc admin info disk`
- `rc admin pool status <ALIAS> <POOL> [--by-id]`

The change intentionally supports the beta.9 envelope format only. A beta.8 flat response must fail deserialization instead of silently producing a default value.

The following cluster operations were audited and do not use the changed response envelopes, so they remain unchanged:

- pool listing and pool status without a target
- decommission start, status, cancel, and clear
- rebalance and expand start, status, and stop
- heal start, status, and stop
- site replication add, info, status, and remove

## Architecture

Keep wire-format knowledge inside `crates/s3`. Add two private deserialization-only types in `crates/s3/src/admin.rs`:

```rust
#[derive(Debug, Deserialize)]
struct ServerInfoResponse {
    info: ClusterInfo,
}

#[derive(Debug, Deserialize)]
struct PoolStatusResponse {
    pool: PoolStatus,
}
```

`AdminClient::cluster_info()` requests `ServerInfoResponse` and returns its `info` field. `AdminClient::pool_status()` requests `PoolStatusResponse` and returns its `pool` field. The server's `admin_discovery` field is intentionally ignored because the current CLI does not consume v4 discovery routes.

This keeps `rc_core::admin::AdminApi`, `ClusterInfo`, `PoolStatus`, and all CLI presentation code unchanged. It also avoids teaching the generic HTTP request helper about endpoint-specific envelope names.

## Error Handling

The envelope fields are required and do not use `serde(default)`. Responses that omit `info` or `pool`, including beta.8 flat responses, return the existing `Error::Json` path. No default object is synthesized.

HTTP status handling, authentication, request signing, and CLI exit-code behavior remain unchanged.

## Testing

Follow test-driven development:

1. Change the S3 admin client fixtures for cluster information and targeted pool status to beta.9 envelopes and verify the existing implementation fails.
2. Add or expand CLI integration tests using beta.9 envelopes:
   - cluster output includes deployment, server, and disk data from `info`;
   - server and disk subcommands receive the same unwrapped cluster model;
   - targeted pool status includes values from `pool`.
3. Add negative transport tests proving flat beta.8 payloads fail instead of yielding default objects.
4. Verify unaffected cluster-operation tests still pass.
5. Run the required repository checks before committing code:
   - `cargo fmt --all --check`
   - `cargo clippy --workspace -- -D warnings`
   - `cargo test --workspace`

## Compatibility and Contracts

This is a client-side protocol-alignment fix. It does not modify protected CLI reference files, JSON output schemas, exit codes, or configuration schemas. User-facing text and JSON output remain the same once the beta.9 payload is unwrapped.

## Delivery

Commit implementation changes only after all required checks pass. Push the feature branch and open a draft pull request that links `rustfs/rustfs#4927`, explains both affected beta.9 envelopes, and records the validation commands.

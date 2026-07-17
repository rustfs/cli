# Beta.9 Admin Response Envelopes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode the strict RustFS 1.0.0-beta.9 envelopes returned by cluster info and targeted pool status endpoints without changing core or CLI output contracts.

**Architecture:** Define two private deserialization-only envelope types in the S3 admin adapter. Unwrap `info` and `pool` inside the corresponding `AdminApi` methods so all existing callers continue receiving `ClusterInfo` and `PoolStatus`; required envelope fields make legacy flat responses fail explicitly.

**Tech Stack:** Rust 2024, Serde, Tokio, reqwest, Cargo integration tests

---

## File Map

- Modify `crates/s3/src/admin.rs`: define beta.9 envelope types, unwrap them in `cluster_info` and `pool_status`, and add transport-level regression tests.
- Modify `crates/cli/tests/admin_info.rs`: serve beta.9 `info` envelopes and verify cluster, server, and disk commands receive the nested data.
- Modify `crates/cli/tests/admin_pool.rs`: serve the beta.9 `pool` envelope for targeted pool status while leaving pool-list fixtures flat.
- Do not modify `crates/core`, protected CLI reference documentation, output schemas, exit codes, or configuration schemas.

### Task 1: Add failing S3 transport tests for beta.9 envelopes

**Files:**
- Modify: `crates/s3/src/admin.rs:1259-2145`

- [ ] **Step 1: Add a beta.9 cluster-info transport test**

Add this test beside the other admin client route tests:

```rust
#[tokio::test]
async fn test_cluster_info_unwraps_beta9_info_response() {
    let (endpoint, receiver, handle) = start_admin_test_server(
        "200 OK",
        r#"{"info":{"mode":"distributed","deploymentID":"deployment-123","servers":[{"endpoint":"http://node1:9000","state":"online","drives":[]}]},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#,
    );
    let client = admin_client_for_endpoint(&endpoint);

    let info = client.cluster_info().await.expect("cluster info request");

    assert_eq!(info.mode.as_deref(), Some("distributed"));
    assert_eq!(info.deployment_id.as_deref(), Some("deployment-123"));
    assert_eq!(info.servers.as_ref().map(Vec::len), Some(1));

    let request = receiver.recv().expect("captured request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/info");
    handle.join().expect("server thread should finish");
}
```

- [ ] **Step 2: Change the targeted pool-status fixture to beta.9**

In `test_pool_status_uses_pool_status_route_with_by_id_query`, replace the flat body with:

```rust
r#"{"pool":{"id":1,"cmdline":"/data/pool1/disk{1...4}","lastUpdate":"2026-05-06T00:00:00Z","decommissionInfo":null},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#
```

Keep the existing assertions for `status.id` and `status.cmd_line`.

- [ ] **Step 3: Add strict legacy-response rejection tests**

Add the following tests to document the approved beta.9-only behavior:

```rust
#[tokio::test]
async fn test_cluster_info_rejects_flat_beta8_response() {
    let (endpoint, _receiver, handle) = start_admin_test_server(
        "200 OK",
        r#"{"mode":"distributed","deploymentID":"legacy"}"#,
    );
    let client = admin_client_for_endpoint(&endpoint);

    let error = client
        .cluster_info()
        .await
        .expect_err("flat beta.8 cluster info should be rejected");

    assert!(error.to_string().contains("missing field `info`"));
    handle.join().expect("server thread should finish");
}

#[tokio::test]
async fn test_pool_status_rejects_flat_beta8_response() {
    let (endpoint, _receiver, handle) = start_admin_test_server(
        "200 OK",
        r#"{"id":1,"cmdline":"/data/pool1/disk{1...4}"}"#,
    );
    let client = admin_client_for_endpoint(&endpoint);

    let error = client
        .pool_status(PoolTarget {
            pool: "1".to_string(),
            by_id: true,
        })
        .await
        .expect_err("flat beta.8 pool status should be rejected");

    assert!(error.to_string().contains("missing field `pool`"));
    handle.join().expect("server thread should finish");
}
```

- [ ] **Step 4: Run the focused S3 tests and verify red state**

Run:

```bash
cargo test -p rc-s3 admin::tests::test_cluster_info_unwraps_beta9_info_response -- --exact
cargo test -p rc-s3 admin::tests::test_pool_status_uses_pool_status_route_with_by_id_query -- --exact
```

Expected: both fail because the current code deserializes the envelope directly into default-valued domain models.

### Task 2: Add failing CLI regression tests

**Files:**
- Modify: `crates/cli/tests/admin_info.rs`
- Modify: `crates/cli/tests/admin_pool.rs`

- [ ] **Step 1: Convert the existing cluster fixture to the beta.9 envelope**

Wrap the existing body in `admin_info.rs` as follows:

```rust
r#"{"info":{"mode":"distributed","deploymentID":"deployment-123","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.9","drives":[{"endpoint":"http://node1:9000/data1","path":"/data1","state":"ok","totalspace":100,"usedspace":40,"availspace":60,"pool_index":1,"set_index":2,"disk_index":3}]}]},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#
```

Keep the disk location assertion and add:

```rust
assert!(stdout.contains("Deployment ID: deployment-123"), "stdout: {stdout}");
assert!(stdout.contains("Servers:       1"), "stdout: {stdout}");
assert!(stdout.contains("Disks:         1 (1 online)"), "stdout: {stdout}");
```

- [ ] **Step 2: Add server and disk subcommand tests using beta.9 envelopes**

Add one test for `admin info server` that asserts `http://node1:9000` and `1.0.0-beta.9` appear, and one test for `admin info disk` that asserts `/data1`, `ok`, and the endpoint appear. Each test must assert its request is `GET /rustfs/admin/v3/info` and join its server thread.

- [ ] **Step 3: Convert only targeted pool status to the beta.9 envelope**

In `pool_status_dispatches_by_id_pool_json`, replace the flat fixture with:

```rust
r#"{"pool":{"id":1,"cmdline":"/data/pool1/disk{1...4}","lastUpdate":"2026-05-10T00:00:00Z","status":"active","decommissionStatus":"none","rebalanceStatus":"failed","decommissionInfo":null},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#
```

Do not change `pool_list_dispatches_to_pool_list_json` or `pool_status_without_target_dispatches_to_pool_list_json`; beta.9 still returns a flat array from `/pools/list`.

- [ ] **Step 4: Run the focused CLI tests and verify red state**

Run:

```bash
cargo test -p rustfs-cli --test admin_info
cargo test -p rustfs-cli --test admin_pool pool_status_dispatches_by_id_pool_json -- --exact
```

Expected: assertions fail because cluster and pool data remain at their default values.

### Task 3: Implement strict beta.9 envelope decoding

**Files:**
- Modify: `crates/s3/src/admin.rs:680-755`

- [ ] **Step 1: Add private wire response types**

Immediately before the `impl AdminApi for AdminClient` block, add:

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

Do not add `serde(default)` to either required field and do not model `admin_discovery`.

- [ ] **Step 2: Unwrap cluster information**

Replace `cluster_info` with:

```rust
async fn cluster_info(&self) -> Result<ClusterInfo> {
    let response: ServerInfoResponse = self.request(Method::GET, "/info", None, None).await?;
    Ok(response.info)
}
```

- [ ] **Step 3: Unwrap targeted pool status**

Replace `pool_status` with:

```rust
async fn pool_status(&self, target: PoolTarget) -> Result<PoolStatus> {
    let query = pool_target_query(&target);
    let response: PoolStatusResponse = self
        .request(Method::GET, "/pools/status", Some(&query), None)
        .await?;
    Ok(response.pool)
}
```

- [ ] **Step 4: Run focused S3 and CLI tests and verify green state**

Run:

```bash
cargo test -p rc-s3 admin::tests::test_cluster_info_unwraps_beta9_info_response -- --exact
cargo test -p rc-s3 admin::tests::test_cluster_info_rejects_flat_beta8_response -- --exact
cargo test -p rc-s3 admin::tests::test_pool_status_uses_pool_status_route_with_by_id_query -- --exact
cargo test -p rc-s3 admin::tests::test_pool_status_rejects_flat_beta8_response -- --exact
cargo test -p rustfs-cli --test admin_info
cargo test -p rustfs-cli --test admin_pool pool_status_dispatches_by_id_pool_json -- --exact
```

Expected: all selected tests pass.

### Task 4: Verify unaffected cluster operations and repository quality gates

**Files:**
- Verify only; no planned file changes

- [ ] **Step 1: Run cluster-operation integration tests**

Run:

```bash
cargo test -p rustfs-cli --test admin_pool
cargo test -p rustfs-cli --test admin_decommission
cargo test -p rustfs-cli --test admin_rebalance
cargo test -p rustfs-cli --test admin_expand
cargo test -p rustfs-cli --test admin_replicate
```

Expected: all tests pass, confirming the targeted envelope change does not alter other cluster workflows.

- [ ] **Step 2: Run formatting**

Run:

```bash
cargo fmt --all --check
```

Expected: exit status 0 with no diff.

- [ ] **Step 3: Run Clippy**

Run:

```bash
cargo clippy --workspace -- -D warnings
```

Expected: exit status 0 with zero warnings.

- [ ] **Step 4: Run the full workspace test suite**

Run:

```bash
cargo test --workspace
```

Expected: every workspace test passes.

- [ ] **Step 5: Inspect the final diff**

Run:

```bash
git diff --check
git status --short
git diff --stat origin/main
git diff origin/main -- crates/s3/src/admin.rs crates/cli/tests/admin_info.rs crates/cli/tests/admin_pool.rs
```

Expected: only the approved design/plan documents, S3 adapter, and regression tests are changed; no protected file is modified.

### Task 5: Commit and create a draft pull request

**Files:**
- Stage the implementation and test files after all checks pass

- [ ] **Step 1: Commit the verified implementation**

Run:

```bash
git add crates/s3/src/admin.rs crates/cli/tests/admin_info.rs crates/cli/tests/admin_pool.rs
git commit -m "fix(admin): decode beta.9 response envelopes"
```

Expected: commit succeeds without bypassing hooks.

- [ ] **Step 2: Push the feature branch**

Run:

```bash
git push -u origin cxymds/fix-beta9-admin-response-envelopes
```

Expected: the branch is available on `rustfs/cli`.

- [ ] **Step 3: Open a draft pull request**

Create a draft PR targeting `main` with title:

```text
fix(admin): decode beta.9 response envelopes
```

The description must include:

```markdown
## Related issue

- Resolves rustfs/rustfs#4927

## Background

RustFS 1.0.0-beta.9 wraps `/v3/info` in `info` and targeted `/v3/pools/status` in `pool`. The CLI previously deserialized the envelopes directly into default-valued domain models, producing empty but successful output.

## Solution

- decode the beta.9 `info` envelope before returning `ClusterInfo`
- decode the beta.9 `pool` envelope before returning targeted `PoolStatus`
- reject legacy flat beta.8 payloads instead of silently returning defaults
- cover cluster, server, disk, and targeted pool status output
- verify other cluster workflows remain unchanged

## Validation

- `cargo fmt --all --check`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
```

Expected: a draft PR is created against `rustfs/cli:main`.

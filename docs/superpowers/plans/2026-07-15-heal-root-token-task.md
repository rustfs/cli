# Root Heal Token Task Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow `rc admin heal status` and `rc admin heal stop` to operate on a root recursive manual heal using only its client token while preserving tokenless global behavior.

**Architecture:** Keep `HealTaskRequest.bucket` unchanged and represent a root task with an empty bucket. Relax the shared CLI validator for token-only requests, map the empty task target to `/heal/` in the S3 adapter, and leave the existing status/stop transport methods and output mapping intact.

**Tech Stack:** Rust, clap, async-trait, reqwest, Tokio tests, repository TCP request-capture test helper, Markdown documentation.

---

## File Map

- Modify `crates/cli/src/commands/admin/heal.rs`: shared task-argument validation and unit tests.
- Modify `crates/s3/src/admin.rs`: root task path mapping and exact HTTP request-contract tests.
- Modify `README.md`: concise root manual-heal workflow.
- Modify `docs/reference/rc/admin.md`: formal root token status/stop contract and examples.
- Reference `docs/superpowers/specs/2026-07-15-heal-root-token-task-design.md`: approved design and acceptance criteria.

### Task 1: Accept token-only root task arguments in the CLI

**Files:**
- Modify: `crates/cli/src/commands/admin/heal.rs:474-507`
- Test: `crates/cli/src/commands/admin/heal.rs:614-620`

- [ ] **Step 1: Replace the rejection test with a failing root-request test**

Replace `test_heal_task_request_rejects_token_without_bucket` with:

```rust
#[test]
fn test_heal_task_request_accepts_token_without_bucket() {
    let formatter = Formatter::default();

    let request = heal_task_request(None, None, Some("root-token".to_string()), &formatter)
        .expect("root token request should be valid")
        .expect("root token request should be task scoped");

    assert!(request.bucket.is_empty());
    assert!(request.prefix.is_none());
    assert_eq!(request.client_token, "root-token");
}
```

- [ ] **Step 2: Add regression tests for invalid partial targets**

Add:

```rust
#[test]
fn test_heal_task_request_rejects_bucket_without_token() {
    let formatter = Formatter::default();

    let result = heal_task_request(Some("logs".to_string()), None, None, &formatter);

    assert!(matches!(result, Err(ExitCode::UsageError)));
}

#[test]
fn test_heal_task_request_rejects_prefix_without_bucket() {
    let formatter = Formatter::default();

    let result = heal_task_request(
        None,
        Some("2026/".to_string()),
        Some("root-token".to_string()),
        &formatter,
    );

    assert!(matches!(result, Err(ExitCode::UsageError)));
}
```

- [ ] **Step 3: Run the focused tests and verify the new root test fails**

Run:

```bash
cargo test -p rustfs-cli heal_task_request --lib
```

Expected: `test_heal_task_request_accepts_token_without_bucket` fails because the current validator returns `UsageError`; the two regression tests pass.

- [ ] **Step 4: Implement the minimum validator change**

Change the match in `heal_task_request` to:

```rust
match (has_target, has_token) {
    (false, false) => Ok(None),
    (_, true) => Ok(Some(HealTaskRequest {
        bucket: bucket.unwrap_or_default(),
        prefix,
        client_token: client_token.expect("client token is present"),
    })),
    (true, false) => {
        formatter.error("Heal task request requires --client-token when --bucket is set.");
        Err(ExitCode::UsageError)
    }
}
```

The existing prefix-before-bucket validation remains above this match and continues rejecting root prefixes.

- [ ] **Step 5: Run the focused tests and verify they pass**

Run:

```bash
cargo test -p rustfs-cli heal_task_request --lib
```

Expected: all `heal_task_request` tests pass.

### Task 2: Map root task requests to the RustFS root heal route

**Files:**
- Modify: `crates/s3/src/admin.rs:575-595`
- Test: `crates/s3/src/admin.rs` in the existing heal path and request-contract test module

- [ ] **Step 1: Add failing unit tests for root task path construction**

Add near `test_rustfs_heal_path_matches_admin_routes`:

```rust
#[test]
fn test_rustfs_heal_task_path_supports_root_target() {
    let request = HealTaskRequest {
        bucket: String::new(),
        prefix: None,
        client_token: "root-token".to_string(),
    };

    assert_eq!(
        rustfs_heal_task_path(&request).expect("root task path"),
        "/heal/"
    );
}

#[test]
fn test_rustfs_heal_task_path_rejects_root_prefix() {
    let request = HealTaskRequest {
        bucket: String::new(),
        prefix: Some("2026/".to_string()),
        client_token: "root-token".to_string(),
    };

    assert!(matches!(
        rustfs_heal_task_path(&request),
        Err(Error::InvalidPath(_))
    ));
}
```

- [ ] **Step 2: Run the focused path tests and verify the root test fails**

Run:

```bash
cargo test -p rc-s3 rustfs_heal_task_path --lib
```

Expected: the root-target test fails with `heal task status requires a bucket target`; the root-prefix test passes.

- [ ] **Step 3: Implement root-aware task path construction**

Replace `rustfs_heal_task_path` with:

```rust
fn rustfs_heal_task_path(request: &HealTaskRequest) -> Result<String> {
    let bucket = (!request.bucket.is_empty()).then_some(request.bucket.as_str());
    let prefix = request
        .prefix
        .as_deref()
        .filter(|prefix| !prefix.is_empty());

    match (bucket, prefix) {
        (None, None) => Ok("/heal/".to_string()),
        (Some(bucket), None) => Ok(format!("/heal/{}", urlencoding::encode(bucket))),
        (Some(bucket), Some(prefix)) => Ok(format!(
            "/heal/{}/{}",
            urlencoding::encode(bucket),
            urlencoding::encode(prefix)
        )),
        (None, Some(_)) => Err(Error::InvalidPath(
            "heal task prefix requires a bucket target".to_string(),
        )),
    }
}
```

- [ ] **Step 4: Run the focused path tests and verify they pass**

Run:

```bash
cargo test -p rc-s3 rustfs_heal_task_path --lib
```

Expected: both root path tests pass.

- [ ] **Step 5: Add a root task status request-contract test**

Add beside the bucket status contract test:

```rust
#[tokio::test]
async fn test_heal_task_status_queries_root_route_with_client_token() {
    let (endpoint, receiver, handle) = start_admin_test_server(
        "200 OK",
        r#"{"summary":"running","detail":"","startTime":"2026-07-15T00:38:07Z","settings":{"recursive":true,"dryRun":false,"remove":false,"recreate":true,"scanMode":1,"updateParity":false,"nolock":false},"items":[]}"#,
    );
    let client = admin_client_for_endpoint(&endpoint);

    let status = client
        .heal_task_status(HealTaskRequest {
            bucket: String::new(),
            prefix: None,
            client_token: "root-token".to_string(),
        })
        .await
        .expect("root heal task status request");

    assert_eq!(status.heal_id, "root-token");
    assert!(status.healing);
    assert!(status.bucket.is_empty());
    assert!(status.object.is_empty());

    let request = receiver.recv().expect("captured request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/heal/?clientToken=root-token"
    );
    assert!(request.body.is_empty());
    handle.join().expect("server thread should finish");
}
```

- [ ] **Step 6: Add a root task stop request-contract test**

Add beside the bucket task stop contract test:

```rust
#[tokio::test]
async fn test_heal_task_stop_posts_root_force_stop_with_client_token() {
    let (endpoint, receiver, handle) = start_admin_test_server(
        "200 OK",
        r#"{"summary":"stopped","detail":"heal task cancelled","startTime":"2026-07-15T00:38:07Z","settings":{"recursive":true,"dryRun":false,"remove":false,"recreate":true,"scanMode":1,"updateParity":false,"nolock":false},"items":[]}"#,
    );
    let client = admin_client_for_endpoint(&endpoint);

    let status = client
        .heal_task_stop(HealTaskRequest {
            bucket: String::new(),
            prefix: None,
            client_token: "root-token".to_string(),
        })
        .await
        .expect("root heal task stop request");

    assert_eq!(status.heal_id, "root-token");
    assert!(!status.healing);
    assert!(status.bucket.is_empty());

    let request = receiver.recv().expect("captured request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/heal/?clientToken=root-token&forceStop=true"
    );
    assert!(request.body.is_empty());
    handle.join().expect("server thread should finish");
}
```

- [ ] **Step 7: Run all S3 heal tests**

Run:

```bash
cargo test -p rc-s3 heal --lib
```

Expected: all heal path, status, start, and stop tests pass, including the existing bucket and prefix contracts.

### Task 3: Document the root manual-heal workflow

**Files:**
- Modify: `README.md:259-265`
- Modify: `docs/reference/rc/admin.md:55-76,139-164`

- [ ] **Step 1: Update the README heal examples**

Replace the Heal operations example block with commands grouped by meaning:

```bash
# Aggregate background heal status
rc admin heal status local

# Root recursive manual heal
rc admin heal start local --scan-mode deep
rc admin heal status local --client-token <TOKEN_FROM_START>
rc admin heal stop local --client-token <TOKEN_FROM_START>

# Bucket manual heal
rc admin heal start local --bucket mybucket --scan-mode deep
rc admin heal status local --bucket mybucket --client-token <TOKEN_FROM_START>
rc admin heal stop local --bucket mybucket --client-token <TOKEN_FROM_START>

# Global force stop
rc admin heal stop local
```

- [ ] **Step 2: Add the root task example to the admin reference**

After the global background status example, add:

````markdown
Start and inspect a root recursive manual heal:

```bash
rc admin heal start local
rc admin heal status local --client-token <TOKEN_FROM_START>
rc admin heal stop local --client-token <TOKEN_FROM_START>
```
````

- [ ] **Step 3: Correct the behavior contract**

Replace the behavior paragraph with:

```markdown
`rc admin heal status <ALIAS>` reports aggregate background heal status. Manual heals started with `rc admin heal start` are token-scoped tasks; the start output includes a client token. Root recursive tasks are inspected or stopped with `--client-token`, while bucket or prefix tasks additionally pass `--bucket` and optional `--prefix`.
```

- [ ] **Step 4: Extend the Heal Workflow table**

Add these rows while retaining the existing aggregate, bucket/prefix, start, and global-stop rows:

```markdown
| `rc admin heal status <ALIAS> --client-token <TOKEN>` | Show a token-scoped root recursive manual heal task. |
| `rc admin heal stop <ALIAS> --client-token <TOKEN>` | Stop a token-scoped root recursive manual heal task. |
```

Replace the final manual-heal note with:

```markdown
All manual heals are token-scoped. Save the `clientToken` returned by `heal start`; the token is required to inspect or stop the task. Root recursive tasks use the token alone, while bucket and prefix tasks also require their original target options.
```

- [ ] **Step 5: Review the protected contract requirements**

Confirm that the diff does not modify `schemas/output_v1.json`, `schemas/output_v2.json`, `crates/cli/src/exit_code.rs`, or `crates/core/src/config.rs`. Record that the pull request title or description must include `BREAKING` because `docs/reference/rc/admin.md` is protected, while no config migration or output schema bump applies to this additive command support.

### Task 4: Format, verify, and commit the implementation

**Files:**
- Verify all modified source and documentation files.

- [ ] **Step 1: Format the workspace**

Run:

```bash
cargo fmt --all
cargo fmt --all --check
```

Expected: both commands exit successfully and `--check` produces no diff.

- [ ] **Step 2: Run static analysis with warnings denied**

Run:

```bash
cargo clippy --workspace -- -D warnings
```

Expected: exit code 0 with zero warnings.

- [ ] **Step 3: Run the complete workspace test suite**

Run:

```bash
cargo test --workspace
```

Expected: every workspace unit, integration, and golden test passes.

- [ ] **Step 4: Inspect the final diff and protected-file scope**

Run:

```bash
git diff --check
git status --short
git diff --stat
git diff --stat origin/main
git diff -- crates/cli/src/commands/admin/heal.rs crates/s3/src/admin.rs README.md docs/reference/rc/admin.md
```

Expected: only the planned root token support and documentation changes are present; no credential, output-schema, exit-code, or config changes appear.

- [ ] **Step 5: Commit only after all checks pass**

Run:

```bash
git add crates/cli/src/commands/admin/heal.rs crates/s3/src/admin.rs README.md docs/reference/rc/admin.md
git commit -m "feat(phase-2): support root heal task status and stop"
```

Expected: the commit succeeds without bypassing hooks. If any required check fails, do not commit; diagnose and fix the failure first.

- [ ] **Step 6: Confirm the branch state**

Run:

```bash
git status --short --branch
git log -3 --oneline --decorate
```

Expected: the working tree is clean and the branch contains the design, plan, and verified implementation commits.

# Site Replication Resync Snapshots

`rc` exposes the RustFS site-replication resync handler through three bounded commands:

```text
rc admin replicate resync start ALIAS --site SITE --yes
rc admin replicate resync status ALIAS --site SITE
rc admin replicate resync cancel ALIAS --site SITE --yes
```

`SITE` must be an exact deployment ID or a unique exact display name. Deployment IDs take precedence. `start` and `cancel` require confirmation before alias loading or any network request. The commands reject an obvious self-target before mutation, while RustFS remains authoritative and may reject additional invalid target states.

## Snapshot semantics

The current RustFS `status` operation does not query live resync workers. It returns the persisted response from the last successful `start` or `cancel` handler invocation for the selected peer. `rc` therefore reports:

- `snapshot_kind: persisted_last_operation` for `status`;
- `snapshot_kind: mutation_response` for `start` and `cancel`;
- `lifecycle_state: unknown` in every response;
- the server operation and server status as snapshot fields, not as evidence that work is still running or has completed.

The output uses the output-v3 `admin_operations` envelope. It retains the operation ID, every returned bucket status and error, the top-level error detail, and an allowlist of safe future lifecycle metadata. Credential-like and unknown extension fields are not projected.

A non-empty error detail, an overall `failed` status, or any failed/error-bearing bucket makes the command exit with General `1`, even if the server's overall status string is `success`. The JSON result is still emitted so automation can inspect all bucket failures.

## Current RustFS limitations

These limitations apply to the current `/rustfs/admin/v3/site-replication/resync/op` implementation:

- Every `start` creates a new UUID and can overlap an existing resync job.
- Repeated `cancel` is not idempotent and can replace the previous snapshot with failed or partial results.
- Bucket side effects occur before the site snapshot is saved. A crash or snapshot-save failure is not atomic and can leave effects without the corresponding snapshot.
- Successful mutation snapshots survive fresh clients and server restarts, but `status` still does not query the live resync pool.
- `status` lists all buckets before reading the snapshot, so an unrelated list-buckets failure can block the read.
- Later runtime progress and failures are not aggregated into the saved snapshot.
- `start` and `cancel` iterate all buckets synchronously. Responses are unpaginated and are accepted only up to the client response limit.

Do not repeatedly invoke `start` or `cancel` as a recovery strategy. After a timeout or connection loss, the mutation outcome is unknown because a signed request may have reached RustFS. Inspect the persisted snapshot and the storage state before deciding whether another mutation is safe.

## Bounds and transport behavior

- Serialized peer request: 1 MiB maximum.
- Successful response: 8 MiB maximum.
- Error response: 64 KiB maximum.
- Redirects are not followed.
- Signed PUT requests are sent once and are never automatically retried.
- A transport failure during `start` or `cancel` exits with Network `3` and reports an unknown outcome.

Other exits are Usage `2`, Auth `4`, NotFound `5`, Conflict `6`, Unsupported `7`, and General `1` for malformed, oversized, partial, or all-bucket-failure responses.

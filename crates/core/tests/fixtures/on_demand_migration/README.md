# On-Demand Migration wire fixtures

Vendored verbatim from `crates/madmin/fixtures/on_demand_migration/*.json` in
[rustfs/rustfs](https://github.com/rustfs/rustfs) at commit
`1a88870809896c989465540b519afc74731d7f33` (2026-09-06, "fix(odm): preserve
cursor compatibility and native source semantics (#7238)").

These files pin the admin API contract for
`/rustfs/admin/v3/on-demand-migration/{bucket}`. The parse tests in
`crates/core/src/admin/on_demand_migration.rs` and the transport tests in
`crates/s3/src/admin/on_demand_migration.rs` read them instead of hand-written
literals, so a server-side contract change shows up as a fixture diff rather
than a silently drifting test.

| File | Route |
| --- | --- |
| `set_request.json` | Plaintext `PUT` body (the only place a secret appears) |
| `set_response.json` | `PUT` response with the redacted config and probe summary |
| `get_response.json` | `GET` response with the redacted config |
| `status.json` | `GET .../status` without a backfill job |
| `status_with_backfill.json` | `GET .../status` with a backfill summary |
| `backfill_job.json` | `POST`/`GET .../backfill` checkpoint document |

Refresh by copying the upstream files again and updating the commit above.

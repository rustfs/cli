# RustFS table catalog commands

`rc table` manages the Iceberg catalog. `rc admin table` manages catalog enablement, maintenance, recovery and backing migration. These commands target the RustFS server contract at `22bff27aee9cfcb8054c9c2ad5b8b91a7376430a`; operations can still be restricted by permissions, configuration and catalog backing. They do not implement AIStor-specific APIs.

## Connection and resource names

Configure the usual alias with `rc alias set`. The catalog uses that alias's endpoint, credentials, region, TLS trust and client certificate settings. Requests use SigV4 service `s3` and append `/iceberg/v1` to the endpoint. Catalog access does not request or print vended credentials. There is no separate persistent catalog profile.

Resources are `alias/warehouse`, `alias/warehouse/namespace`, and `alias/warehouse/namespace/table`. Namespace components are dot-separated at the command line, for example `local/analytics/sales.eu/orders`; the transport encodes the ordered components using the REST unit separator. Names are not object keys. Warehouse names are existing bucket names. Each namespace, table and ref component accepts up to 64 lowercase ASCII letters, digits, hyphens or underscores, with alphanumeric boundaries; namespace length is at most 512 bytes. Views use the same resource shape as tables.

## Basic workflow

```sh
rc bucket create local/analytics
rc admin table warehouse enable local/analytics
rc table warehouse show local/analytics
rc table config local/analytics
rc table namespace create local/analytics/sales --property owner=analytics
rc table create local/analytics/sales/orders --schema-file schema.json
rc table list local/analytics/sales
rc table show local/analytics/sales/orders --json
rc table metadata show local/analytics/sales/orders
rc table rename local/analytics/sales/orders local/analytics/sales/orders_v2
rc table remove local/analytics/sales/orders_v2
rc table namespace remove local/analytics/sales
```

Example `schema.json`:

```json
{"type":"struct","fields":[{"id":1,"name":"id","type":"long","required":true}]}
```

`create` also accepts `--partition-spec-file`, `--sort-order-file`, `--location`, repeatable `--property key=value`, and `--format-version 1|2`. Locations must pass the server's warehouse validation. `register TARGET --metadata-location LOCATION` registers existing metadata without overwrite. Rename must stay within the same alias and warehouse. Drop preserves data files; there is no `--purge` or recursive namespace removal. Create a normal bucket before enabling catalog support; enablement does not call CreateBucket.

Use a compatible engine such as PyIceberg, DuckDB or Spark to write and query rows. `rc sql` continues to mean S3 Select on an object.

## Read and namespace operations

- `namespace list WAREHOUSE`, `show NAMESPACE`, `exists NAMESPACE`, `remove NAMESPACE`.
- `namespace update NAMESPACE --set key=value --remove key` changes properties; the same key cannot occur in both sets.
- `table list NAMESPACE`, `show TABLE --snapshots all|refs`, `exists TABLE`.
- `snapshot list TABLE --snapshots all|refs` and `snapshot show TABLE SNAPSHOT_ID` project LoadTable metadata; they are not a separate server scan or SQL query.
- `metadata show TABLE` returns metadata location, version token and generation for pointer operations.
- `exists` succeeds when present and returns exit 5 for an absent resource.

Namespace, table and view lists retrieve all server pages by default. `--page-size` accepts 1 through 1000. `--no-paginate` returns one page with its opaque `next-page-token`; pass that unchanged through `--page-token` to resume. A later page failure fails the whole command before printing partial results. Each response and accumulated list is limited to 64 MiB; use single-page mode for larger listings. Snapshot projections do not have server-side pagination.

## Commits and refs

There are two distinct commit contracts. The CLI never automatically retries a mutation or refreshes stale conditions to force it through.

### Standard metadata updates

```sh
rc table commit local/analytics/sales/orders --file update.json --commit-id property-edit-1
```

```json
{
  "requirements": [{"type":"assert-ref-snapshot-id","ref":"main","snapshot-id":123}],
  "updates": [{"action":"set-properties","updates":{"owner":"analytics"}}]
}
```

Supply Iceberg requirements that protect the state used to prepare the update. The command requires a nonempty requirements array. The current RustFS standard update path uses these requirements and its internal publication CAS; it does not enforce the optional external expected-version/location fields. Therefore the CLI rejects those fields or flags in this mode. A requirements failure is a conflict; review the current state before preparing a new operation.

### Existing metadata pointer commit

```sh
rc table commit local/analytics/sales/orders --file pointer.json \
  --expected-version-token CURRENT_TOKEN \
  --expected-metadata-location CURRENT_LOCATION \
  --commit-id pointer-edit-1
```

```json
{"new-metadata-location":"s3://analytics/PATH/TO/VALIDATED/METADATA.json"}
```

Use metadata that already exists and satisfies the server's metadata-directory, schema, object-reference and warehouse checks. This mode requires a nonempty version token and old metadata location, either in the file or flags; conflicting values and nonempty metadata updates are rejected. Reuse the same commit ID and exact request after an uncertain outcome. This is not a promise of idempotency for every catalog API.

```sh
rc table ref list local/analytics/sales/orders
rc table ref set local/analytics/sales/orders release --type tag \
  --snapshot-id 123 --expected-snapshot-id null --commit-id release-create-1
rc table ref remove local/analytics/sales/orders release \
  --expected-snapshot-id 123 --commit-id release-remove-1
```

`ref set` accepts optional `--min-snapshots-to-keep`, `--max-snapshot-age-ms`, `--max-ref-age-ms`. `null` requires the ref to be absent: the transport sends an explicit standard Iceberg requirement, because the dedicated RustFS ref endpoint treats JSON null as an omitted optional field. Numeric expectations use the ref endpoint. `ref remove --force` allows a retained ref to be removed but does not bypass the server's protection of `main`.

## Views

`view create TARGET --file view.json`, `view list NAMESPACE`, `view show TARGET`, `view exists TARGET`, `view replace TARGET --file update.json`, and `view remove TARGET` are available. The create target supplies the name. Example create body:

```json
{
  "schema":{"type":"struct","schema-id":0,"fields":[{"id":1,"name":"id","type":"long","required":true}]},
  "view-version":{"version-id":1,"schema-id":0,"timestamp-ms":1788652800000,"summary":{},"representations":[{"type":"sql","sql":"SELECT id FROM sales.orders","dialect":"spark"}],"default-namespace":["sales"]},
  "properties":{}
}
```

Replace takes the server's view `requirements`/`updates` body and requires `expected-metadata-location` in that file. Read the current view response before preparing it; LoadView does not expose a version token. The server checks the expected metadata location during conditional publication. View rename/register are not exposed because the referenced RustFS server has no corresponding route.

## Administrative operations

Administrative mutations below require `--yes`; they still enforce server authorization and backing-specific checks. `--file` reads a JSON object, or stdin with `--file -`, bounded to 8 MiB. Server JSON fields use their original spelling and are not converted from CLI flags.

| Command below `rc admin table` | Request body / behavior |
| --- | --- |
| `maintenance plan TABLE --file FILE` | Metadata-maintenance request; forcibly sets `delete`, `commit-snapshot-expiration`, `commit-compaction` to false. |
| `maintenance run TABLE --file FILE --yes` | Sends the requested maintenance actions; only explicitly true flags apply changes. |
| `maintenance config show TABLE` | Loads current configuration, which may be absent. |
| `maintenance config set TABLE --file FILE --yes` | `TableMaintenanceConfig` object; inspect current configuration first. |
| `maintenance scheduler show TABLE` | Reads scheduler/job state. |
| `maintenance scheduler run TABLE --yes` | Runs one scheduling pass using the server's default scheduler identity. |
| `maintenance worker run TABLE --file FILE --yes` | `{}` or `{"worker-id":"operator-worker"}`; runs one worker pass. |
| `maintenance job show TABLE JOB` | Loads one report. |
| `maintenance job heartbeat TABLE JOB --file FILE --yes` | `{"lease-id":"CURRENT_LEASE","worker-id":"CURRENT_WORKER"}`. |
| `maintenance job quarantine TABLE JOB --file FILE --yes` | Server quarantine action and optional reason. |
| `catalog diagnostics TABLE` | Reads recovery/consistency state. |
| `catalog export TABLE` | Returns the server export document for inspection/backup. |
| `catalog import TABLE --file FILE --yes` | `{"metadata-location":"...","properties":{}}`; this is a metadata import request, not direct replay of the export envelope. |
| `catalog recover TABLE --yes` | Repairs commit finalization; no input file. |
| `catalog rollback TABLE --file FILE --yes` | `metadata-location`, `version-token`, optional stable `commit-id`; only forward-safe rollback is supported by the server. |
| `catalog metadata-update TABLE --file FILE --yes` | `metadata-location`, `version-token`, optional stable `commit-id`. |
| `catalog external show TABLE` | Reads external bridge configuration. |
| `catalog external set TABLE --file FILE --yes` | Server bridge configuration, including catalog, external-namespace and external-table. |
| `catalog external sync TABLE --file FILE --yes` | Server sync request including metadata-location and any required expected state; no vendor polling. |
| `migration status WAREHOUSE` | Reads migration readiness and blockers. |
| `migration start WAREHOUSE --yes` | Materializes the server-managed migration target. |
| `migration cancel WAREHOUSE --yes` | Requests cancellation; server can reject an advanced target. |

Minimal maintenance preview input:

```json
{"retain-recent-metadata-files":5,"snapshot-expiration":{"min-snapshots-to-keep":2,"max-snapshot-age-ms":604800000}}
```

Minimal maintenance config input:

```json
{"version":1,"retain-recent-metadata-files":5,"delete-enabled":false,"background-enabled":false}
```

The `background-enabled` server configuration does not make this CLI a daemon or supply a built-in periodic server scheduler. Refer to the [pinned RustFS request definitions](https://github.com/rustfs/rustfs/blob/22bff27aee9cfcb8054c9c2ad5b8b91a7376430a/rustfs/src/admin/handlers/table_catalog/mod.rs) and [maintenance models](https://github.com/rustfs/rustfs/blob/22bff27aee9cfcb8054c9c2ad5b8b91a7376430a/rustfs/src/table_catalog/model.rs) for advanced request fields. Unsupported actions fail explicitly; the CLI does not emulate them by modifying reserved S3 objects.

## Output and errors

`--json` uses the existing output v3 envelope with `type: table_catalog`. Success is written to stdout and errors to stderr. Success `data` contains `operation` and the server `result` (or snapshot projection). Use `.data.result` when consuming JSON. Human output prints readable JSON for nested catalog documents. Sensitive credential fields are removed, including unexpected credential bundles in LoadTable responses.

Exit codes: 2 invalid arguments/request, 3 network/transient service error, 4 authentication/authorization, 5 not found, 6 conflict/precondition failure, 7 unsupported operation. Mutations with a lost response have an unknown outcome and are not retried automatically. Error envelopes conservatively report `retryable: false`; callers must decide whether a read retry or an exact commit replay is safe. No automatic cross-warehouse rename, purge, overwrite registration, staged create, v3, multi-table commit, table replication, table encryption policy or Delta Sharing is offered.

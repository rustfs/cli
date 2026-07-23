# rc admin

## Purpose

The `rc admin` operation manages the RustFS Admin API, including scanner and storage diagnostics, bounded realtime metrics, KMS inspection and key lifecycle management, cluster information, healing, pools, expansion, decommissioning, rebalance workflows, IAM users, policies, groups, service accounts, site replication, and service control.

`rc admin` does not implement the MinIO Admin API. MinIO aliases remain available to S3 data commands, but MinIO administrative operations require a MinIO-compatible admin client.

## Syntax

```bash
rc [GLOBAL OPTIONS] admin <COMMAND>
rc admin diagnostics <health|cluster|extensions> <ALIAS>
rc admin info <cluster|server|disk|storage> <ALIAS> [OPTIONS]
rc admin scanner status <ALIAS>
rc admin metrics <ALIAS> [OPTIONS]
rc admin kms status <ALIAS>
rc admin kms configure <ALIAS> <--config-file PATH|--stdin>
rc admin kms reconfigure <ALIAS> <--config-file PATH|--stdin>
rc admin kms start <ALIAS>
rc admin kms restart <ALIAS> --yes
rc admin kms stop <ALIAS> --yes
rc admin kms roundtrip <ALIAS> <BUCKET> [--key-id KEY_ID] --yes
rc admin kms key list <ALIAS> [--limit N] [--marker TOKEN]
rc admin kms key status <ALIAS> [KEY_ID]
rc admin kms key create <ALIAS> [--name NAME] [--description TEXT] [--tag KEY=VALUE]...
rc admin kms key delete <ALIAS> <KEY_ID> [--pending-window-days 7..30] --yes
rc admin kms key delete <ALIAS> <KEY_ID> --immediate --yes --confirm-immediate
rc admin kms key cancel-deletion <ALIAS> <KEY_ID>
rc admin heal <status|start|stop> <ALIAS> [OPTIONS]
rc admin pool <list|status> <ALIAS> [POOL] [OPTIONS]
rc admin expand <start|status|stop> <ALIAS>
rc admin decommission <start|cancel|clear> <ALIAS> <POOL> [OPTIONS]
rc admin decommission status <ALIAS> [POOL] [OPTIONS]
rc admin rebalance <start|status|stop> <ALIAS>
rc admin user <ls|add|info|rm|enable|disable> ...
rc admin policy <ls|create|info|rm|attach> ...
rc admin group <ls|add|info|rm|enable|disable|add-members|rm-members> ...
rc admin service-account <ls|create|info|rm> ...
rc admin service <restart|stop|freeze|unfreeze> <ALIAS>
rc admin replicate add <ALIAS> <ALIAS> [<ALIAS>...]
rc admin replicate <info|status> <ALIAS> [OPTIONS]
rc admin replicate edit <ALIAS> --site <DEPLOYMENT_ID|NAME> [EDIT OPTIONS] --yes
rc admin replicate resync <start|cancel> <ALIAS> --site <DEPLOYMENT_ID|NAME> --yes
rc admin replicate resync status <ALIAS> --site <DEPLOYMENT_ID|NAME>
rc admin replicate remove <ALIAS> <--all|--site <NAME>>
```

## Commands

| Command | Description |
| --- | --- |
| `diagnostics` | Read bounded authenticated health, cluster, and extension snapshots. |
| `info` | Display cluster, server, or disk information. |
| `scanner` | Inspect scanner health, freshness, and cycle state. |
| `metrics` | Query bounded realtime metrics as normalized JSON Lines or raw server records. |
| `kms` | Inspect KMS state and manage safe native key lifecycle operations. |
| `heal` | Start, stop, or inspect healing operations. |
| `pool` | List pools and inspect pool status. |
| `expand` | Manage post-expansion data rebalancing. Alias: `scale`. |
| `decommission` | Manage server pool decommissioning. Alias: `decom`. |
| `rebalance` | Manage post-expansion rebalancing. |
| `user` | Manage IAM users. |
| `policy` | Manage IAM policies and attachments. |
| `group` | Manage IAM groups and group membership. |
| `service-account` | Manage service accounts. |
| `service` | Control the server process: restart, stop, freeze, unfreeze. |
| `replicate` | Manage site replication across clusters. |

## Examples

Show cluster information:

```bash
rc admin info cluster local
```

Read detailed authenticated health observations:

```bash
rc admin diagnostics health local
```

Read the cluster snapshot and extension catalog:

```bash
rc admin diagnostics cluster local
rc admin diagnostics extensions local
```

Inspect scanner health and storage topology:

```bash
rc admin scanner status local
rc admin info storage local
```

Collect two scanner and disk metric snapshots:

```bash
rc --json admin metrics local --scope scanner,disk --samples 2 --interval 3s --by-host --by-disk
```

Inspect KMS state and manage a key lifecycle:

```bash
rc admin kms status local
rc admin kms key list local --limit 100
rc admin kms key status local
rc admin kms configure local --config-file /secure/kms.json
rc admin kms start local
rc admin kms key create local --name archive --description "Archive key" --tag environment=prod
rc admin kms key delete local <KEY_ID> --pending-window-days 7 --yes
rc admin kms key cancel-deletion local <KEY_ID>
```

Start a deep heal for a prefix:

```bash
rc admin heal start local --bucket logs --prefix 2026/ --scan-mode deep
```

Check global background heal status:

```bash
rc admin heal status local
```

Start and inspect a root recursive manual heal:

```bash
rc admin heal start local
rc admin heal status local --client-token <TOKEN_FROM_START>
rc admin heal stop local --client-token <TOKEN_FROM_START>
```

Check a manual bucket heal task using the client token returned by `start`:

```bash
rc admin heal status local --bucket logs --prefix 2026/ --client-token <TOKEN_FROM_START>
```

Stop a manual bucket heal task:

```bash
rc admin heal stop local --bucket logs --client-token <TOKEN_FROM_START>
```

Start decommissioning a pool:

```bash
rc admin decommission start local '/data/pool1/disk{1...4}'
```

Check decommissioning status for all pools:

```bash
rc admin decommission status local
```

Check, cancel, or clear decommissioning by pool ID:

```bash
rc admin decommission status local 1 --by-id
rc admin decommission cancel local 1 --by-id
rc admin decommission clear local 1 --by-id
```

Start and inspect post-expansion rebalancing:

```bash
rc admin rebalance start local
rc admin rebalance status local
rc admin rebalance stop local
```

Create a user and attach a policy:

```bash
rc admin user add local analyst STRONG_PASSWORD
rc admin policy attach local readonly --user analyst
```

Create a service account with a policy file:

```bash
rc admin service-account create local SA_ACCESS_KEY SA_SECRET_KEY --policy ./policy.json
```

Link two sites for site replication and check the result:

```bash
rc admin replicate add site1 site2
rc admin replicate info site1
rc admin replicate edit site1 --site <DEPLOYMENT_ID> --name edge-eu --yes
rc admin replicate resync start site1 --site edge-eu --yes
rc admin replicate resync status site1 --site edge-eu
rc admin replicate status site1
```

Gracefully stop or restart a server:

```bash
rc admin service stop local
rc admin service restart local
```

## Behavior

Admin operations use the configured alias to create a RustFS admin client. The credentials behind the alias must have permissions for the requested administrative API. The command accepts aliases with or without a trailing slash.

`rc admin diagnostics` performs bounded read-only requests. Each JSON response is limited to 8 MiB. The health command reads the authenticated RustFS health snapshot and is separate from public liveness or readiness probes. Its drive throughput and latency fields are live observations, not active benchmarks, and the command preserves the server's `unsupported_probes` list instead of claiming `mc support diag` parity. Cluster output represents `snapshot: null` as `initializing_or_unavailable`. Extension diagnostics read schemas and runtime capability summaries only; they never request extension instance configuration.

The diagnostic commands require capability discovery to classify the corresponding route as available. Authentication failures, unsupported routes, malformed JSON, and transport failures retain distinct exit codes.

## Observability Workflow

The read-only observability commands target RustFS Admin API v3 routes introduced with the beta.10 diagnostics surface.

| Command | Description |
| --- | --- |
| `rc admin scanner status <ALIAS>` | Classify scanner state as `healthy`, `stale`, `empty`, `partial`, or `disabled` and retain current server diagnostic fields. |
| `rc admin info storage <ALIAS>` | Show backend topology, disk health, and aggregate capacity. |
| `rc admin metrics <ALIAS> [OPTIONS]` | Stream bounded realtime metric snapshots. |

`admin metrics` accepts these query and output options:

| Option | Description |
| --- | --- |
| `--scope <SCOPES>` | Comma-separated scopes: `scanner`, `disk`, `os`, `batch-jobs`, `site-resync`, `network`, `memory`, `cpu`, `rpc`, or `all`. |
| `--samples <1..120>` | Limit the number of server snapshots. Defaults to `1`. |
| `--interval <DURATION>` | Set the server sampling interval, for example `3s`. |
| `--host <HOST>` / `--disk <PATH>` | Restrict metrics to selected hosts or disks. Each option may be repeated. |
| `--by-host` / `--by-disk` | Request grouped host or disk metrics. |
| `--job-id <ID>` / `--deployment-id <ID>` | Restrict batch-job or site-resync metrics. |
| `--metrics-format normalized\|raw` | Emit v3 normalized JSON Lines or bounded raw server JSON records. |

Normalized metrics always use one compact v3 JSON object per line, including numeric samples, labels, per-sample timestamps, errors, partial/final markers, and the retained raw snapshot. Raw mode intentionally omits the v3 wrapper. The client rejects responses above 16 MiB, individual records above 1 MiB, and records beyond the requested sample count.

Permission failures return the authentication exit code. A missing observability route returns `unsupported_feature`, allowing automation to distinguish an older RustFS server from missing credentials. Malformed and oversized responses fail without emitting partial normalized records.

## KMS Key Lifecycle Workflow

The KMS commands target the native RustFS beta.10 Admin API. They do not implement the MinIO KMS admin protocol.

| Command | Description |
| --- | --- |
| `rc admin kms status <ALIAS>` | Show `not-configured`, `configured`, `running`, `error`, or `unknown` service state plus a non-secret configuration summary. |
| `rc admin kms configure <ALIAS> <--config-file PATH\|--stdin>` | Validate and install an initial Local, Vault KV2, or Vault Transit JSON configuration. |
| `rc admin kms reconfigure <ALIAS> <--config-file PATH\|--stdin>` | Replace configuration through RustFS's native stop, reconfigure, persist, and restart workflow. |
| `rc admin kms start <ALIAS>` | Start a configured KMS service. |
| `rc admin kms restart <ALIAS> --yes` | Force a KMS service restart after explicit confirmation. |
| `rc admin kms stop <ALIAS> --yes` | Stop KMS after explicit confirmation. |
| `rc admin kms roundtrip <ALIAS> <BUCKET> [--key-id KEY_ID] --yes` | Verify a real SSE-KMS object write/read cycle in an explicit existing bucket, using the configured default key when `--key-id` is omitted. |
| `rc admin kms key list <ALIAS> [--limit N] [--marker TOKEN]` | List native RustFS KMS keys with pagination. The limit range is `1..=1000`. |
| `rc admin kms key status <ALIAS> [KEY_ID]` | Show key metadata and lifecycle state. When `KEY_ID` is omitted, use the configured default key ID. |
| `rc admin kms key create <ALIAS> [--name NAME] [--description TEXT] [--tag KEY=VALUE]...` | Create a key. Names are sent through RustFS's reserved `name` tag. Tags reject malformed, duplicate, reserved-name, and control-character input. |
| `rc admin kms key delete <ALIAS> <KEY_ID> [--pending-window-days 7..30] --yes` | Schedule deletion. The pending window defaults to seven days and `--yes` is mandatory. |
| `rc admin kms key delete <ALIAS> <KEY_ID> --immediate --yes --confirm-immediate` | Permanently delete a key with two explicit non-interactive acknowledgements. Immediate deletion cannot be cancelled. |
| `rc admin kms key cancel-deletion <ALIAS> <KEY_ID>` | Cancel a previously scheduled deletion. |

An unconfigured KMS service is a successful status result with `state=not-configured`; it is not treated as a network failure. Permission failures return the authentication exit code. Missing status or list routes return `unsupported_feature`, while a missing explicitly requested key returns `not_found`.

Human output includes service state, backend family, health, default key ID, cache state, and key metadata. JSON output uses the v3 `kms` family. Configuration responses are normalized instead of passed through: Vault tokens, AppRole secret IDs, local master keys, plaintext data keys, and ciphertext blobs are never part of the KMS inspection output.

Mutation responses use `key_create`, `key_delete`, and `key_cancel_deletion` operations in the v3 `kms` family. Server error messages are classified into stable permission, missing-key, conflict, unavailable, rejected-request, and malformed-response failures without echoing response bodies. Create and cancellation results deserialize only lifecycle metadata; unknown key-material fields are ignored. These commands do not configure, start, or stop the KMS service and never request or export data keys.

Configuration is never accepted through field-specific command-line flags or positional JSON. Use exactly one of `--config-file PATH` or `--stdin`. Input is limited to 1 MiB and must match one strict `backend_type` request shape: `Local`, `VaultKV2`, or `VaultTransit`. Unknown fields, missing required fields, invalid URLs, relative Local key directories, zero timeout/retry/cache values, insecure production Vault transport, and unsafe Local key-file modes are rejected locally before network access. Vault addresses cannot contain URL credentials, query parameters, or fragments, which prevents hidden secrets from bypassing owned-buffer zeroization. Reconfiguration may use an empty Vault Token value to retain server-stored credentials because RustFS beta.10 defines that sentinel for an existing token; initial configuration may not. AppRole has no equivalent sentinel in beta.10, so both `role_id` and `secret_id` remain mandatory and partial credentials are always rejected.

On Unix, `--config-file` accepts only a regular non-symlink file with no group or other permission bits; mode `0600` is recommended and modes such as `0640` or `0644` are rejected. Standard input has no filesystem permission check and should come from a protected pipe or secret manager. The CLI stores raw input, typed secret fields, serialized request bytes, and the HTTP request body in zeroizing containers. Client errors are static and server response bodies are never copied into diagnostics, so Local master keys, Vault tokens, AppRole IDs, and AppRole secrets are not emitted in debug, human, JSON, or error output.

The v3 lifecycle success operations are `configure`, `reconfigure`, `start`, `restart`, and `stop`, each with the resulting service `state`. Unconfigured start is `not_found`; permission denial is `auth_error`; unavailable service/storage is `network_error`; malformed or rejected responses are `general_error`; missing lifecycle routes are `unsupported_feature`. Restart and stop refuse to contact the server unless `--yes` is present.

`kms roundtrip` refuses to run without `--yes`. It generates exactly 4 KiB of random test content internally, writes one randomly named temporary object with explicit SSE-KMS headers, reads and compares the decrypted bytes without creating or reporting a digest, and always attempts permanent deletion even when write, read, or verification fails. The read is bounded to 4 KiB. Application-owned plaintext buffers are zeroized; the temporary object name, plaintext, ciphertext, digest, and generated key material never appear in debug, human, JSON, or error output. A successful v3 result reports only `bucket`, `key_id`, `passed`, `cleanup_passed`, and write/read/cleanup/total milliseconds. Cleanup failure is a distinct error, and a primary failure explicitly reports when cleanup also failed.

`kms key status` describes native RustFS key lifecycle metadata. It does not claim compatibility with the `mc admin kms key status` encryption/decryption probe. The round-trip diagnostic uses only the S3 object API and does not call an Admin API key-generation route. RustFS beta.10 has no direct decrypt-test Admin API or KMS-specific metrics route/selector contract, so `rc` does not offer KMS-specific metrics and intentionally does not expose the legacy `generate-data-key` response because that response contains plaintext data-key material.

`rc admin heal status <ALIAS>` reports aggregate background heal status. Manual heals started with `rc admin heal start` are token-scoped tasks; the start output includes a client token. Root recursive tasks are inspected or stopped with `--client-token`, while bucket or prefix tasks additionally pass `--bucket` and optional `--prefix`.

## Heal Workflow

`rc admin heal` manages cluster healing operations.

| Command | Description |
| --- | --- |
| `rc admin heal status <ALIAS>` | Show aggregate background heal status. |
| `rc admin heal status <ALIAS> --client-token <TOKEN>` | Show a token-scoped root recursive manual heal task. |
| `rc admin heal status <ALIAS> --bucket <BUCKET> [--prefix <PREFIX>] --client-token <TOKEN>` | Show a token-scoped manual heal task. |
| `rc admin heal start <ALIAS> [OPTIONS]` | Start a manual heal operation. |
| `rc admin heal stop <ALIAS>` | Stop the global background heal operation. |
| `rc admin heal stop <ALIAS> --client-token <TOKEN>` | Stop a token-scoped root recursive manual heal task. |
| `rc admin heal stop <ALIAS> --bucket <BUCKET> [--prefix <PREFIX>] --client-token <TOKEN>` | Stop a token-scoped manual heal task. |

`heal start` accepts these operation options:

| Option | Description |
| --- | --- |
| `-b, --bucket <BUCKET>` | Heal a single bucket. Omit this option to recursively heal all buckets. |
| `-p, --prefix <PREFIX>` | Limit a bucket heal to an object prefix. |
| `--scan-mode normal\|deep` | Select the scan mode. Defaults to `normal`. |
| `--remove` | Remove dangling objects or parts found by the heal scan. |
| `--recreate` | Recreate missing data. |
| `--dry-run` | Report what would be healed without applying changes. |

All manual heals are token-scoped. Save the `clientToken` returned by `heal start`; the token is required to inspect or stop the task. Root recursive tasks use the token alone, while bucket and prefix tasks also require their original target options.

## Decommission Workflow

`rc admin decommission` retires server pools from a cluster. The `POOL` argument can be a pool command line, comma-separated pool command lines, or a zero-based pool ID when `--by-id` is set.

| Command | Description |
| --- | --- |
| `rc admin decommission start <ALIAS> <POOL> [--by-id]` | Start decommissioning one or more pools. |
| `rc admin decommission status <ALIAS> [POOL] [--by-id]` | Show decommissioning status for all pools or one pool. |
| `rc admin decommission cancel <ALIAS> <POOL> [--by-id]` | Cancel decommissioning for a pool. |
| `rc admin decommission clear <ALIAS> <POOL> [--by-id]` | Clear failed or canceled decommissioning metadata for a pool. |

Use `rc admin pool list <ALIAS>` or `rc admin pool status <ALIAS>` to find pool IDs and pool command lines before starting a decommission.

## Rebalance Workflow

`rc admin rebalance` manages post-expansion data movement after server pools are added to a deployment.

| Command | Description |
| --- | --- |
| `rc admin rebalance start <ALIAS>` | Start a cluster rebalance operation. |
| `rc admin rebalance status <ALIAS>` | Show cluster-wide and per-pool rebalance status. |
| `rc admin rebalance stop <ALIAS>` | Stop a running rebalance operation. |

`rc admin expand` is an alias-oriented workflow for the same post-expansion rebalance step. The `expand` command is also available as `scale`.

## Service Control Workflow

`rc admin service` controls the server process behind an alias.

| Command | Description |
| --- | --- |
| `rc admin service restart <ALIAS>` | Request a graceful shutdown for restart. The supervising process manager (systemd, Kubernetes) is responsible for relaunching the binary. |
| `rc admin service stop <ALIAS>` | Request a graceful shutdown. |
| `rc admin service freeze <ALIAS>` | Set the service freeze flag. Currently advisory: the server records the flag but does not yet gate request admission on it. |
| `rc admin service unfreeze <ALIAS>` | Clear the service freeze flag. |

The server response reports whether the action was `accepted` and whether it is `effective` on the current build. RustFS has no in-process supervisor, so `restart` and `stop` both perform a graceful stop; `restart` relies on the process manager to bring the server back up.

## Site Replication Workflow

`rc admin replicate` manages multi-cluster site replication. Peer sites are given as configured alias names; their endpoints and credentials are resolved from the local alias store, so every participating site needs an alias with root credentials before running `add`.

| Command | Description |
| --- | --- |
| `rc admin replicate add <ALIAS> <ALIAS> [<ALIAS>...]` | Link two or more sites into a site replication cluster. The first alias receives the request. |
| `rc admin replicate info <ALIAS>` | Show the current site replication configuration. |
| `rc admin replicate edit <ALIAS> --site <DEPLOYMENT_ID\|NAME> [EDIT OPTIONS] --yes` | Read the complete peer document, select one exact peer, apply a bounded edit, and write the peer document back. |
| `rc admin replicate resync start <ALIAS> --site <DEPLOYMENT_ID\|NAME> --yes` | Request a site resync and return the mutation response snapshot. |
| `rc admin replicate resync status <ALIAS> --site <DEPLOYMENT_ID\|NAME>` | Return the last persisted start or cancel snapshot. This is not live worker status. |
| `rc admin replicate resync cancel <ALIAS> --site <DEPLOYMENT_ID\|NAME> --yes` | Request cancellation and return the mutation response snapshot. |
| `rc admin replicate status <ALIAS> [OPTIONS]` | Show replication status. Without flags the buckets, users, groups, and policies summaries are requested. |
| `rc admin replicate remove <ALIAS> --all` | Dissolve the entire site replication cluster. |
| `rc admin replicate remove <ALIAS> --site <NAME>` | Remove one or more named sites. Repeat `--site` per name. |

`status` accepts these section flags:

| Option | Description |
| --- | --- |
| `--buckets` | Include the bucket replication summary. |
| `--users` | Include the IAM user replication summary. |
| `--groups` | Include the IAM group replication summary. |
| `--policies` | Include the IAM policy replication summary. |
| `--metrics` | Include replication metrics. |

`edit` accepts these options:

| Option | Description |
| --- | --- |
| `--site <DEPLOYMENT_ID\|NAME>` | Select an exact deployment ID first, otherwise a unique exact site name. Partial matching is never used. |
| `--endpoint <URL>` | Replace the peer endpoint. The value must be an HTTP or HTTPS origin without user information, path, query, or fragment. |
| `--name <NAME>` | Rename the selected peer, including the local deployment when it is selected by deployment ID. |
| `--skip-tls-verify` | Set `skipTlsVerify=true` and clear the custom CA. Conflicts with `--verify-tls` and `--ca-cert`. |
| `--verify-tls` | Set `skipTlsVerify=false`. |
| `--ca-cert <FILE>` | Set a certificate-only PEM CA bundle and enable TLS verification. The file is read with a 256 KiB bound. |
| `--clear-ca-cert` | Set the custom CA to an empty value. Conflicts with `--ca-cert`. |
| `--yes` | Confirm the mutating read-modify-write operation. This is required before alias lookup or network access. |

At least one edit option must produce an effective semantic change. Endpoint origins are canonicalized for comparison, while an omitted `skipTlsVerify` is treated as `false` and an omitted `caCertPem` is treated as empty. The command rejects a final HTTP peer state when `skipTlsVerify=true` or a non-empty custom CA remains. This permits an atomic HTTPS-to-HTTP conversion only when the same command clears the active TLS values.

The complete selected peer object is retained privately and sent back with opaque future fields unchanged during the read-modify-write operation, but those fields are never printed. `info` and `edit` output use explicit safe projections: service-account access keys, CA contents, opaque future fields, and arbitrary server status strings are never printed. Successful JSON output uses output schema v3 with the `admin_operations` family. Mutating network failures are reported as non-retryable in JSON because the server outcome may be unknown; inspect `info` before deciding whether to retry.

`resync start` and `resync cancel` require `--yes` before alias lookup or network access. All three resync commands select a deployment ID first, otherwise a unique exact site name. Their output retains the operation ID, ordered bucket snapshots, and error details. A failed bucket or non-empty error detail produces General exit `1` while still emitting the complete result. A missing persisted snapshot produces Conflict exit `6`.

The current RustFS `resync status` endpoint returns the persisted result of the last successful start or cancel handler invocation; it does not inspect live workers. Every output therefore reports an unknown lifecycle state. Start operations can overlap, cancel is not idempotent, and bucket side effects are not atomic with snapshot persistence. A mutation timeout, malformed success response, or oversized success response has an unknown outcome and must not be retried blindly. See [Site Replication Resync Snapshots](../../site-replication-resync.md) for response bounds and the complete server limitations.

### BREAKING resync contract migration

The resync subcommands are additive and do not change existing command invocations. They use the existing output-v3 `admin_operations` envelope, so no JSON schema-version migration is required. The protected behavior contract is updated to make snapshot-only semantics explicit: automation must treat `result.lifecycle_state` as `unknown`, use `result.server_operation` only as the persisted operation type, and must not interpret `status` output as live progress. This PR must be marked `BREAKING` because it changes the protected CLI behavior contract.

### BREAKING output migration

`rc admin replicate info --json` previously emitted the RustFS server response directly. It now emits an output-v3 `admin_operations` envelope with `changed=false`; the safe site configuration is under `data.operations[0].result`. Scripts must update field access accordingly. `serviceAccountAccessKey` and `caCertPem` are intentionally absent, with CA presence represented by `hasCustomCA`. This PR must be marked `BREAKING` because the protected CLI behavior contract changes.

Site replication requires bucket versioning support on every site and replicates buckets, objects, IAM users, groups, policies, and service accounts across all linked sites. The server rejects loopback peer endpoints unless the deployment explicitly allows them (`RUSTFS_REPLICATION_ALLOW_LOOPBACK_TARGET=true`), which is intended for local testing only.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

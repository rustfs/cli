# rc admin

## Purpose

The `rc admin` operation manages the RustFS Admin API, including cluster information, healing, pools, expansion, decommissioning, rebalance workflows, IAM users, policies, groups, service accounts, site replication, and service control.

`rc admin` does not implement the MinIO Admin API. MinIO aliases remain available to S3 data commands, but MinIO administrative operations require a MinIO-compatible admin client.

## Syntax

```bash
rc [GLOBAL OPTIONS] admin <COMMAND>
rc admin info <cluster|server|disk> <ALIAS> [OPTIONS]
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
rc admin config <get|set|delete|help|history|restore|export|import> ...
rc admin config module-switch <get|set> ...
rc admin replicate add <ALIAS> <ALIAS> [<ALIAS>...]
rc admin replicate <info|status> <ALIAS> [OPTIONS]
rc admin replicate remove <ALIAS> <--all|--site <NAME>>
```

## Commands

| Command | Description |
| --- | --- |
| `info` | Display cluster, server, or disk information. |
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
| `config` | Inspect, plan, export, and mutate RustFS server configuration. |
| `replicate` | Manage site replication across clusters. |

## Examples

Show cluster information:

```bash
rc admin info cluster local
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
rc admin replicate status site1
```

Gracefully stop or restart a server:

```bash
rc admin service stop local
rc admin service restart local
```

## Behavior

Admin operations use the configured alias to create a RustFS admin client. The credentials behind the alias must have permissions for the requested administrative API. The command accepts aliases with or without a trailing slash.

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

## Server Configuration Workflow

`rc admin config` manages the RustFS server configuration behind an alias. It does not modify the local `rc` alias configuration.

| Command | Description |
| --- | --- |
| `rc admin config get <ALIAS> <SUBSYSTEM[:TARGET]>` | Read a subsystem or named target. Secret-bearing fields are redacted again by the client before output. |
| `rc admin config set <ALIAS> <SUBSYSTEM[:TARGET]> <KEY=VALUE>... [--dry-run]` | Validate keys with server help, read the current state, calculate a redacted diff, and apply the directive unless `--dry-run` is set. |
| `rc admin config delete <ALIAS> <SUBSYSTEM[:TARGET]> [KEY...] [--dry-run]` | Delete selected keys or the complete target after a redacted preflight diff. |
| `rc admin config help <ALIAS> [SUBSYSTEM] [KEY] [--env]` | Show server-provided subsystem, key, or environment-variable help. |
| `rc admin config history <ALIAS> [--count <N>]` | List recent history. History data is classified and redacted locally because server records can contain submitted values. |
| `rc admin config restore <ALIAS> <RESTORE_ID> [--dry-run] --yes` | Preview or apply a history entry as a complete configuration replacement. |
| `rc admin config export <ALIAS> --file <PATH>` | Create a new, owner-private, redacted configuration file. Existing files are never overwritten. |
| `rc admin config import <ALIAS> --file <PATH> [--dry-run] --yes` | Validate and preview a complete configuration replacement from a file. Redacted secret placeholders are rejected. |
| `rc admin config module-switch get <ALIAS>` | Show effective and persisted notification/audit module switches and their sources. |
| `rc admin config module-switch set <ALIAS> [--notify on\|off] [--audit on\|off] [--dry-run]` | Update one or both persisted switches. Omitted switches retain their persisted value even when an environment override changes the effective value. |

All set, delete, import, restore, and module-switch dry runs are client-side and do not send a mutation request. RustFS beta.10 does not expose a revision, ETag, or other optimistic concurrency token for these routes, so `rc` does not claim atomic compare-and-set protection.

Full imports and restores require `--yes`. RustFS beta.10 history entries contain the submitted directive rather than a complete pre-change snapshot. The server currently rebuilds its configuration from that directive during restore, which can remove unrelated settings. `rc` shows this as a complete replacement and warns about the unsafe server behavior tracked in [rustfs/backlog#1398](https://github.com/rustfs/backlog/issues/1398); a history restore must not be treated as preservation-safe rollback.

Exports are always redacted because secret values are not a portable client output contract. Replace any required secret values through an approved secret-management workflow before importing; `rc` rejects the `*redacted*` placeholder for secret-bearing fields.

## Site Replication Workflow

`rc admin replicate` manages multi-cluster site replication. Peer sites are given as configured alias names; their endpoints and credentials are resolved from the local alias store, so every participating site needs an alias with root credentials before running `add`.

| Command | Description |
| --- | --- |
| `rc admin replicate add <ALIAS> <ALIAS> [<ALIAS>...]` | Link two or more sites into a site replication cluster. The first alias receives the request. |
| `rc admin replicate info <ALIAS>` | Show the current site replication configuration. |
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

# rc admin

## Purpose

The `rc admin` operation manages RustFS or MinIO-compatible administrative APIs, including cluster information, healing, pools, expansion, decommissioning, rebalance workflows, IAM users, policies, groups, and service accounts.

## Syntax

```bash
rc [GLOBAL OPTIONS] admin <COMMAND>
rc admin info <cluster|server|disk> <ALIAS> [OPTIONS]
rc admin heal <status|start|stop> <ALIAS> [OPTIONS]
rc admin pool <list|status> <ALIAS> [POOL] [OPTIONS]
rc admin expand <start|status|stop> <ALIAS>
rc admin decommission <start|status|cancel> <ALIAS> <POOL> [OPTIONS]
rc admin rebalance <start|status|stop> <ALIAS>
rc admin user <ls|add|info|rm|enable|disable> ...
rc admin policy <ls|create|info|rm|attach> ...
rc admin group <ls|add|info|rm|enable|disable|add-members|rm-members> ...
rc admin service-account <ls|create|info|rm> ...
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

Check a manual bucket heal task using the client token returned by `start`:

```bash
rc admin heal status local --bucket logs --prefix 2026/ --client-token <TOKEN_FROM_START>
```

Stop a manual bucket heal task:

```bash
rc admin heal stop local --bucket logs --client-token <TOKEN_FROM_START>
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

## Behavior

Admin operations use the configured alias to create an admin client. The credentials behind the alias must have permissions for the requested administrative API. The command accepts aliases with or without a trailing slash.

`rc admin heal status <ALIAS>` reports aggregate background heal status. Manual bucket or prefix heals started with `rc admin heal start` are token-scoped tasks; the start output includes a client token, and subsequent task status or stop requests must pass that token with `--bucket`, optional `--prefix`, and `--client-token`.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

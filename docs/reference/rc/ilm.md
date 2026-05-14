# rc ilm

## Purpose

`rc ilm` manages bucket lifecycle configuration, remote storage tiers, and restore requests for transitioned objects. It is a legacy-compatible command; prefer `rc bucket lifecycle` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] ilm <COMMAND>
rc ilm rule <add|edit|list|remove|export|import> ...
rc ilm tier <add|edit|list|info|remove> ...
rc ilm restore [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Rule Operations

| Command | Description |
| --- | --- |
| `rule add` | Add an expiration or transition rule. |
| `rule edit` | Edit an existing lifecycle rule. |
| `rule list` | List lifecycle rules on a bucket. |
| `rule remove` | Remove one rule by ID or all rules. |
| `rule export` | Export lifecycle configuration as JSON. |
| `rule import` | Import lifecycle configuration from JSON. |

## Tier Operations

| Command | Description |
| --- | --- |
| `tier add` | Add a remote storage tier. |
| `tier edit` | Edit tier credentials. |
| `tier list` | List configured tiers. |
| `tier info` | Show tier statistics. |
| `tier remove` | Remove a tier. |

## Common Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose lifecycle configuration is managed. |
| `--id` | Lifecycle rule identifier. |
| `--prefix` | Restrict a rule to a key prefix. |
| `--tags` | Restrict a rule to tag filters. |
| `--expiry-days` | Expire current objects after the specified number of days. |
| `--transition-days` | Transition objects after the specified number of days. |
| `--storage-class` | Remote tier or storage class for transition rules. |
| `--all` | Remove all lifecycle rules where supported. |
| `--days` | Number of days to keep a restored copy. Defaults to `1`. |
| `--force` | Force operation even if capability detection fails. |

## Examples

```bash
rc bucket lifecycle rule add local/reports --expiry-days 30 --prefix logs/
rc ilm rule list local/reports
rc ilm rule export local/reports > lifecycle.json
rc ilm tier add rustfs WARM local --endpoint http://remote:9000 --access-key ACCESS_KEY --secret-key SECRET_KEY --bucket warm
rc ilm restore local/reports/archive/object.dat --days 7
```

## Behavior

Lifecycle behavior is applied by the storage service. Rule support, tier support, and restore support depend on backend capabilities.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

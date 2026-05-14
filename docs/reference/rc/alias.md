# rc alias

## Purpose

The `rc alias` operation manages named S3-compatible service endpoints in the local `rc` configuration. Use aliases to avoid repeating endpoint URLs and credentials in every command.

## Syntax

```bash
rc [GLOBAL OPTIONS] alias <COMMAND>
rc [GLOBAL OPTIONS] alias set [OPTIONS] <NAME> <ENDPOINT> <ACCESS_KEY> <SECRET_KEY>
rc [GLOBAL OPTIONS] alias list [-l|--long]
rc [GLOBAL OPTIONS] alias remove <NAME>
```

## Commands

| Command | Description |
| --- | --- |
| `set` | Add a new alias or replace an existing alias. |
| `list` | List configured aliases. |
| `remove` | Remove an alias from local configuration. |

## Parameters

| Parameter | Description |
| --- | --- |
| `NAME` | Local alias name, such as `local`, `s3`, or `rustfs`. |
| `ENDPOINT` | S3-compatible endpoint URL, such as `http://localhost:9000`. |
| `ACCESS_KEY` | Access key ID for the endpoint. |
| `SECRET_KEY` | Secret access key for the endpoint. |
| `--region` | AWS region to associate with the alias. Defaults to `us-east-1`. |
| `--signature` | Signature version, `v4` or `v2`. Defaults to `v4`. |
| `--bucket-lookup` | Bucket lookup style: `auto`, `path`, or `dns`. Defaults to `auto`. |
| `--insecure` | Allow insecure TLS connections for this alias. |
| `-l, --long` | Show full alias details when listing aliases. |

## Examples

Configure a local RustFS or MinIO-compatible service:

```bash
rc alias set local http://localhost:9000 ACCESS_KEY SECRET_KEY
```

Configure AWS S3 with an explicit region:

```bash
rc alias set s3 https://s3.amazonaws.com ACCESS_KEY SECRET_KEY --region us-east-1
```

List aliases with endpoint details:

```bash
rc alias list --long
```

Remove an alias:

```bash
rc alias remove old-local
```

## Behavior

`alias set` overwrites an existing alias with the same name. `alias list` does not print secret keys. Commands that need a remote service resolve the alias name before creating an S3 or admin client.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

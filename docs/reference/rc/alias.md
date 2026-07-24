# rc alias

## Purpose

The `rc alias` operation manages named S3-compatible service endpoints in the local `rc` configuration. Use aliases to avoid repeating endpoint URLs and credentials in every command.

## Syntax

```bash
rc [GLOBAL OPTIONS] alias <COMMAND>
rc [GLOBAL OPTIONS] alias set [OPTIONS] <NAME> <ENDPOINT> <ACCESS_KEY> <SECRET_KEY>
rc [GLOBAL OPTIONS] alias list [-l|--long]
rc [GLOBAL OPTIONS] alias remove <NAME>
rc [GLOBAL OPTIONS] alias export [NAMES]... [--output <FILE>] [--include-credentials --acknowledge-credentials]
rc [GLOBAL OPTIONS] alias import <FILE> [--replace]
```

## Commands

| Command | Description |
| --- | --- |
| `set` | Add a new alias or replace an existing alias. |
| `list` | List configured aliases. |
| `remove` | Remove an alias from local configuration. |
| `export` | Write selected aliases, or every alias, as portable JSON. |
| `import` | Validate and import a portable alias JSON document. |

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
| `-o, --output <FILE>` | Write an alias export to a file instead of stdout. |
| `--include-credentials` | Include plaintext access and secret keys in an export. |
| `--acknowledge-credentials` | Explicitly acknowledge plaintext credential export. |
| `--force` | Replace an existing export file. |
| `--replace` | Replace conflicting local aliases during import. |

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

Export connection settings without credentials:

```bash
rc alias export local s3 --output aliases.json
```

Export credentials after explicit acknowledgement:

```bash
rc alias export local \
  --include-credentials \
  --acknowledge-credentials \
  --output aliases-with-credentials.json
```

Import aliases, rejecting every conflict before changing the configuration:

```bash
rc alias import aliases.json
```

Replace conflicting aliases:

```bash
rc alias import aliases.json --replace
```

## Behavior

`alias set` overwrites an existing alias with the same name. `alias list` does
not print secret keys. Commands that need a remote service resolve the alias
name before creating an S3 or admin client.

Exports use a versioned JSON schema and are sorted by alias name for
deterministic output. Credentials are absent by default. Importing a redacted
authenticated alias preserves its endpoint and options with empty credentials;
run `rc alias set` to supply credentials before using it. Credential-bearing
exports require both explicit flags and files are created with owner-only
permissions on Unix.

Import validates the entire document, including schema version, duplicate
names, endpoint URLs, signature mode, bucket lookup mode, and mTLS path pairs,
before one configuration write. Existing aliases cause exit code 6 unless
`--replace` is supplied. Malformed documents cause exit code 2.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

# rc cors

## Purpose

`rc cors` manages bucket CORS configuration. It is a legacy-compatible command; prefer `rc bucket cors` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] cors <COMMAND>
rc cors list <ALIAS/BUCKET>
rc cors get <ALIAS/BUCKET>
rc cors set [OPTIONS] <ALIAS/BUCKET> [SOURCE]
rc cors remove <ALIAS/BUCKET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET` | Bucket whose CORS rules are managed. |
| `SOURCE` | JSON or XML CORS file. Use `-` to read from stdin. |
| `--file` | Legacy flag for the CORS file; mutually exclusive with positional `SOURCE`. |
| `--force` | Force operation even if capability detection fails. |

## Examples

```bash
rc bucket cors list local/web
rc bucket cors set local/web cors.xml
cat cors.json | rc cors set local/web -
rc cors remove local/web
```

## Behavior

`list` and `get` read the current CORS rules. `set` replaces the bucket CORS configuration with the supplied JSON or XML document. `remove` clears the CORS configuration.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

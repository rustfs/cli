# rc share

## Purpose

`rc share` generates presigned URLs for object sharing. It is a legacy-compatible command; prefer `rc object share` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] share [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Object to share. |
| `-e, --expire` | URL expiration duration. Defaults to `7d`. |
| `--upload` | Generate an upload URL where supported. |
| `--content-type` | Content type for upload-style URLs. |

## Examples

```bash
rc share local/reports/summary.pdf --expire 24h
rc object share local/uploads/inbox.bin --upload --expire 1h
```

## Behavior

Presigned URLs inherit the permissions and endpoint behavior of the alias credentials. Treat generated URLs as temporary credentials until they expire.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

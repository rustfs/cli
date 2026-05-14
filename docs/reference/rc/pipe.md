# rc pipe

## Purpose

`rc pipe` streams stdin directly into an object. Use it for generated data, shell pipelines, and backups that should not be written to a temporary local file first.

## Syntax

```bash
rc [GLOBAL OPTIONS] pipe [OPTIONS] <ALIAS/BUCKET/KEY>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Destination object path. |
| `--content-type` | Object content type. Defaults to `application/octet-stream`. |
| `--storage-class` | Storage class for the uploaded object. |

## Examples

```bash
tar -czf - ./logs | rc pipe local/backups/logs.tar.gz --content-type application/gzip
printf 'hello\n' | rc pipe local/tmp/hello.txt --content-type text/plain
```

## Behavior

The command reads from stdin until EOF and uploads that stream as the destination object.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

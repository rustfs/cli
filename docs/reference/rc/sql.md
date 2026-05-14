# rc sql

## Purpose

`rc sql` runs an S3 Select SQL expression against an object and streams query results to stdout.

## Syntax

```bash
rc [GLOBAL OPTIONS] sql [OPTIONS] <ALIAS/BUCKET/KEY> --query <SQL>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `ALIAS/BUCKET/KEY` | Object to query. |
| `--query` | SQL expression to run. |
| `--input-format` | Input format: `csv`, `json`, or `parquet` where supported. Defaults to `csv`. |
| `--output-format` | Output format: `csv` or `json`. Defaults to `csv`. |
| `--compression` | Compression type: `none`, `gzip`, or `bzip2`. Defaults to `none`. |

## Examples

```bash
rc sql local/data/report.csv --query 'SELECT s._1 FROM S3Object s'
rc sql local/data/events.jsonl --query 'SELECT * FROM S3Object' --input-format json --output-format json --compression gzip
```

## Behavior

The backend must support S3 Select for the target object format. Query results are written to stdout, so redirect output when storing results.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

# rc get

## Purpose

`rc get` downloads one remote object to the local filesystem. It is an
`mc get`-compatible, direction-safe entry point to the same planner, transfer
execution, progress, retry, error, and output path used by `rc cp`.

## Syntax

```bash
rc [GLOBAL OPTIONS] get [OPTIONS] <SOURCE> <TARGET>
```

`SOURCE` must be a remote `ALIAS/BUCKET/KEY` path and `TARGET` must be a local
filesystem path. Other directions fail with the usage exit code before a
transfer starts.

## Examples

```bash
rc get local/reports/report.json ./report.json
rc get local/archive/releases/app.tar.gz ./downloads/app.tar.gz
```

All applicable `rc cp` options remain available, including retry, progress,
checksum verification, version selection, and secure SSE-C source-key inputs.
See [`rc cp`](cp.md) for the shared transfer behavior and option reference.

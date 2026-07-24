# rc put

## Purpose

`rc put` uploads one or more local paths to remote object storage. It is an
`mc put`-compatible, direction-safe entry point to the same planner, transfer
execution, progress, retry, error, and output path used by `rc cp`.

## Syntax

```bash
rc [GLOBAL OPTIONS] put [OPTIONS] <SOURCE>... <TARGET>
```

Every `SOURCE` must be a local filesystem path and `TARGET` must be a remote
`ALIAS/BUCKET[/KEY]` path. Other directions fail with the usage exit code
before a transfer starts. Multiple sources require a remote prefix ending in
`/`, matching the canonical copy planner.

## Examples

```bash
rc put ./report.json local/reports/
rc put ./january.csv ./february.csv local/reports/ --concurrency 8 --summary
```

All applicable `rc cp` options remain available, including metadata, tags,
checksums, storage class, object-lock headers, secure encryption inputs, and
bulk transfer controls. See [`rc cp`](cp.md) for the shared transfer behavior
and option reference.

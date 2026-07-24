# Active replication target checks

`rc replicate check ALIAS/BUCKET` validates every replication target referenced
by the source bucket configuration. Despite using an HTTP GET extension, the
operation is active: RustFS writes a temporary probe object, creates a delete
marker, deletes the probe version, and attempts final cleanup on each target.

Interactive use asks for confirmation. Automation and JSON mode must pass
`--yes` explicitly:

```console
rc replicate check local/source --yes
rc --json replicate check local/source --yes
```

Current RustFS servers return the outcome of every target and each bucket,
versioning, object-lock, put, delete-marker, version-delete, and cleanup phase.
A failed cleanup is highlighted because a probe artifact may remain. The
command exits successfully only when the overall result is successful; a
completed check with failed targets still prints its structured JSON result and
returns a conflict exit status.

Older compatible servers return an empty success body. `rc` accepts that
response but reports it as a legacy result with no per-target or cleanup
evidence. Malformed structured responses are rejected instead of being treated
as legacy success.

Probe keys are allocated by the server below
`.rustfs.sys/replication-check/`. Output sanitizes control characters, and the
client rejects unbounded or structurally inconsistent error details. Do not
grant the invoking source credential more permissions than the replication
administration operation requires.

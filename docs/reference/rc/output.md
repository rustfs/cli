# JSON Output Contracts

`rc` versions structured output so scripts can opt into stable contracts. The schemas are stored in the repository under `schemas/`.

| Schema | Applies to | Compatibility status |
| --- | --- | --- |
| [`output_v1.json`](../../../schemas/output_v1.json) | Original S3 and alias command output | Preserved unchanged |
| [`output_v2.json`](../../../schemas/output_v2.json) | Existing cluster and administrative output | Preserved unchanged |
| [`output_v3.json`](../../../schemas/output_v3.json) | New capability, version, lock, multipart, watch, usage, metrics, and admin-operation families | Contract for new implementations |

Adding v3 does not silently change the JSON emitted by existing commands. Each command implementation must document when it adopts v3. Consumers should choose a parser from the command's documented output version instead of inferring a version from the installed `rc` release.

## Version 3 envelope

Every v3 record contains:

- `schema_version`, always the integer `3`;
- `type`, identifying the command family;
- `status`, normally `success` or `error`; multipart cleanup may use `partial` when
  successful aborts and per-upload failures must be returned together;
- `data` for successful and partial records, or `error` for failed records;
- optional `meta` with request and server context.

Byte counts are non-negative JSON integers and timestamps use RFC 3339 date-time strings. A field is nullable only when the schema explicitly permits `null`. Server-owned fields that are unavailable on a particular RustFS version are represented as `null`, not omitted, when the field is required by the family contract.

Objects allow additional properties so a newer RustFS server can expose extra diagnostic fields without breaking older clients. Required field names and their types remain stable within v3.

## Pagination and streaming

Paginated records use a `pagination` object with `truncated` and `continuation_token`. The token is `null` when there is no next page.

Streaming commands emit JSON Lines. Each non-empty line is one complete v3 record and validates independently against `output_v3.json`. A watch keepalive is a successful `watch_event` record with `data.event` set to `null` and `data.keepalive` set to `true`. Consumers must not parse an entire JSON Lines stream as one JSON document.

## Errors

Errors use the same family-specific `type` as successful output. An unsupported server capability has the typed error `unsupported_feature`, including the capability name and nullable server version. Other errors use the stable error kinds defined by the schema.

Multipart cleanup keeps every attempted upload under `data.results`. A `partial` record includes
both successful `aborted` entries and `failed` entries with typed per-upload errors, so retry logic
does not need to discard work that already completed. Dry runs use `success` with
`state: "would_abort"` and never mutate the server.

## Migrating from v1 or v2

Existing v1 and v2 consumers do not need to migrate until they adopt a new command family or a command explicitly documents v3 output.

When migrating:

1. Dispatch on `schema_version` before reading family data.
2. Read successful fields under `data` instead of from the root object.
3. Read failures under `error` and handle `unsupported_feature` separately.
4. Treat nullable server fields as unavailable data, not as zero values.
5. For watch output, validate and process each JSON Lines record independently.
6. Ignore unknown object properties while continuing to require documented fields and types.

The golden fixtures under `crates/cli/tests/fixtures/output_v3/` provide success, empty, and error examples for every v3 family.

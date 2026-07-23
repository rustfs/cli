# `rc watch`

Stream live RustFS object notifications for an alias or one bucket. The command uses the RustFS S3 `ListenNotification` extension and does not buffer the response until the connection closes.

## Syntax

```text
rc watch [GLOBAL OPTIONS] ALIAS[/BUCKET] [OPTIONS]
```

An alias-only path watches all buckets visible to the supplied credentials. A bucket path limits the server-side stream to that bucket. Object and prefix paths are not accepted; use `--prefix` and `--suffix` for key filtering.

## Options

| Option | Description | Default |
| --- | --- | --- |
| `-e, --event EVENT` | Select `put`, `delete`, `get`, or a full `s3:` event name. Repeat the option or use comma-separated values. | `put,delete,get` |
| `--prefix PREFIX` | Match decoded object keys beginning with this value. | none |
| `--suffix SUFFIX` | Match decoded object keys ending with this value. | none |
| `--ping SECONDS` | Ask RustFS to send a JSON keepalive at this interval. | `10` |
| `--reconnect-attempts COUNT` | Maximum reconnects after the initial connection. | `5` |
| `--reconnect-delay-ms MILLISECONDS` | Initial reconnect delay. | `500` |
| `--reconnect-max-delay-ms MILLISECONDS` | Upper bound for exponential reconnect delay. | `10000` |

The event shorthands map to these S3 patterns:

| Shorthand | S3 event pattern |
| --- | --- |
| `put` | `s3:ObjectCreated:*` |
| `delete` | `s3:ObjectRemoved:*` |
| `get` | `s3:ObjectAccessed:*` |

## Examples

Watch all default event groups across a RustFS service:

```bash
rc watch local/
```

Watch object creation and deletion under a bucket prefix:

```bash
rc watch local/photos --event put,delete --prefix incoming/
```

Watch JSON objects and emit v3 JSON Lines:

```bash
rc watch local/photos --event put --suffix .json --json
```

## Stream behavior

RustFS beta.10 serves this stream from `GET /` or `GET /BUCKET` with repeated `events` query parameters and optional `prefix`, `suffix`, and `ping` parameters. `rc` signs the request with the alias credentials and checks the `text/event-stream` response before decoding records.

Each event is decoded incrementally, including when an HTTP chunk splits a JSON line. Object keys are URL-decoded before display. Version IDs, delete-marker state, request or sequencer IDs, and source metadata are preserved when RustFS supplies them. Empty `Records` arrays and whitespace frames are transport keepalives and are not printed.

Unexpected disconnects and retryable HTTP failures use bounded exponential backoff. Authentication, unsupported-route, invalid-filter, and malformed-event errors are returned immediately instead of being hidden by reconnects. Pressing Ctrl-C cancels an in-flight connection, stream read, or reconnect delay and exits with status `130`.

## Capability handling

Capability discovery identifies RustFS `1.0.0-beta.10` `listen_notification` support explicitly. A server-declared unsupported, disabled, stubbed, version-gated, or permission-denied state stops the command before it opens the stream. If a future server does not declare this capability, `rc` does not infer either support or lack of support; the signed S3 route response is authoritative.

The credentials need the RustFS/S3 `s3:ListenNotification` permission for alias-wide streams and the corresponding bucket notification permission for bucket-scoped streams.

## Output contract and migration

Human output is one terminal-safe line per event. `--quiet` suppresses events but still reports errors.

`--json` and `--format json` emit one compact `watch_event` schema-v3 object per line. Each line validates independently against [`schemas/output_v3.json`](../../../schemas/output_v3.json). Transport keepalives do not produce output records. Existing schema-v1 and schema-v2 commands are unchanged, so existing consumers do not need to migrate. New watch consumers should dispatch on `schema_version`, process input line by line, and ignore optional source fields they do not need.

This reference adds a new command contract; it does not change an existing command's flags or output.

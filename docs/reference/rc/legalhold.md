# rc legalhold

## Purpose

`rc legalhold` reads and updates the legal-hold status of an object version. It is compatible with the `mc legalhold` command name. The preferred noun-first form is `rc object legalhold`.

A legal hold has no expiry date. While its status is ON, RustFS prevents the selected object version from being deleted until an authorized caller changes the status to OFF.

## Syntax

```bash
rc [GLOBAL OPTIONS] legalhold info [--version-id VERSION] <ALIAS/BUCKET/KEY>
rc [GLOBAL OPTIONS] legalhold set [--version-id VERSION] <ALIAS/BUCKET/KEY>
rc [GLOBAL OPTIONS] legalhold clear [--version-id VERSION] <ALIAS/BUCKET/KEY>

rc object legalhold info ...
rc object legalhold set ...
rc object legalhold clear ...
```

`rc object legal-hold` is also accepted as a spelling alias for the noun-first command.

## Options

| Option | Description |
| --- | --- |
| `--version-id VERSION`, `--vid VERSION` | Read or mutate one exact object version. An empty version ID is rejected before a request is sent. |

## Examples

Set legal hold to ON for the current version:

```bash
rc legalhold set local/records/invoice.pdf
```

Inspect one historical version:

```bash
rc legalhold info local/records/invoice.pdf --version-id v2
```

Set legal hold to OFF for that exact version:

```bash
rc object legalhold clear local/records/invoice.pdf --version-id v2
```

## Behavior

`set` sends legal-hold status ON and `clear` sends status OFF. Version selection is passed to RustFS as the signed S3 `versionId` query parameter. RustFS separately authorizes reads and mutations through its Object Lock policy actions.

Alias credentials and custom-header encryption material are redacted from command diagnostics and are never included in structured state output.

## JSON output

All legal-hold JSON uses the output v3 `locks` envelope. `data.operation` is `legal_hold_info`, `legal_hold_set`, or `legal_hold_clear`. Each item includes the bucket, key, nullable version ID, and `legal_hold` as a boolean. Errors are emitted as one v3 record on standard error.

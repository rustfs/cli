# rc retention

## Purpose

`rc retention` reads, sets, and clears Object Lock retention for an object version. It also provides the `--default` compatibility path for a bucket's default retention rule. The preferred noun-first forms are `rc object retention` and `rc bucket lock`.

Object retention requires Object Lock to have been enabled when the bucket was created, and Object Lock requires bucket versioning. Creating an Object Lock enabled bucket is outside this command family's scope.

## Syntax

```bash
rc [GLOBAL OPTIONS] retention info [--version-id VERSION] <ALIAS/BUCKET/KEY>
rc [GLOBAL OPTIONS] retention set [--version-id VERSION] [--bypass] <governance|compliance> <VALIDITY> <ALIAS/BUCKET/KEY>
rc [GLOBAL OPTIONS] retention clear [--version-id VERSION] [--bypass] <ALIAS/BUCKET/KEY>

rc [GLOBAL OPTIONS] retention info --default <ALIAS/BUCKET>
rc [GLOBAL OPTIONS] retention set --default <governance|compliance> <Nd|Ny> <ALIAS/BUCKET>
rc [GLOBAL OPTIONS] retention clear --default <ALIAS/BUCKET>
```

The equivalent noun-first object forms are:

```bash
rc object retention info ...
rc object retention set ...
rc object retention clear ...
```

## Options and arguments

| Option or argument | Description |
| --- | --- |
| `governance` | Protect the selected object while allowing an authorized, explicit governance bypass. |
| `compliance` | Protect the selected object without allowing the active retention period to be weakened. |
| `VALIDITY` | A positive count of days or years such as `30d` or `2y`, or an absolute RFC 3339 UTC timestamp ending in `Z` for object retention. |
| `--version-id VERSION`, `--vid VERSION` | Read or mutate one exact object version. An empty version ID is rejected. |
| `--bypass` | Explicitly request governance bypass. RustFS must also authorize `s3:BypassGovernanceRetention`. |
| `--default` | Operate on the bucket default rule. This cannot be combined with `--version-id` or `--bypass`. |

Bucket defaults accept only the unambiguous `Nd` and `Ny` forms. They never accept months or an absolute timestamp. The count must be positive and fit the S3 API's signed 32-bit field.

Changing or clearing the bucket default affects future object versions only. It does not rewrite retention already stored on existing versions, and clearing the default does not disable Object Lock.

## Examples

Set governance retention for one object version until an absolute UTC date:

```bash
rc retention set governance 2027-12-31T23:59:59Z local/records/invoice.pdf --version-id v2
```

Set retention using the `mc retention` compatible validity syntax:

```bash
rc retention set compliance 7y local/records/invoice.pdf
```

Inspect one historical version:

```bash
rc object retention info local/records/invoice.pdf --version-id v2
```

Shorten active governance retention with an explicitly authorized bypass:

```bash
rc retention set governance 30d local/records/invoice.pdf --version-id v2 --bypass
```

Set and inspect the bucket default:

```bash
rc retention set --default governance 30d local/records
rc retention info --default local/records
```

## Safety behavior

Before a mutation, `rc` validates the target date and reads the selected version's current retention. Invalid, past, or overflowing UTC dates fail before a PUT request is sent.

Active compliance retention can only be extended in compliance mode. `--bypass` does not permit clearing it, shortening it, or changing it to governance mode. Active governance retention can be extended without bypass; shortening, clearing, or changing its mode requires `--bypass`. Sending the flag adds the signed S3 bypass header and lets RustFS enforce the caller's bypass permission.

Alias credentials and custom-header encryption material are redacted from command diagnostics and are never included in structured state output.

## JSON output

All retention JSON uses the output v3 `locks` envelope:

```json
{
  "schema_version": 3,
  "type": "locks",
  "status": "success",
  "data": {
    "operation": "retention_info",
    "changed": false,
    "items": [
      {
        "bucket": "records",
        "key": "invoice.pdf",
        "version_id": "v2",
        "object_lock_enabled": true,
        "retention": {
          "mode": "governance",
          "retain_until": "2027-12-31T23:59:59Z"
        },
        "legal_hold": null,
        "default_retention": null
      }
    ]
  }
}
```

Errors are emitted as one v3 `locks` record on standard error. Existing output v1 and v2 records are unchanged.

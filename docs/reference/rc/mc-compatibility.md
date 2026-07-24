# MinIO mc compatibility

`rc` is a RustFS and S3 client, not a byte-for-byte reimplementation of
MinIO `mc`. This matrix separates stable `rc` commands, compatibility aliases,
runtime-gated operations, server-blocked work, and intentionally omitted
product-specific commands.

## Status meanings

| Status | Meaning |
| --- | --- |
| Canonical | A native `rc` command with its own documented contract. |
| Alias | An `mc`-style entry point that delegates to the same `rc` implementation as its canonical command. |
| Runtime gated | Implemented by `rc`, but executed only when the target RustFS version or capability supports the operation. |
| Server blocked | The client cannot implement truthful behavior until the linked RustFS server work lands. |
| Intentionally unsupported | Outside the RustFS CLI product scope; `rc` does not emulate it. |

Use `rc admin capabilities ALIAS` before automation that depends on an
administrative or diagnostic extension. A registered route alone is not
treated as proof that the operation works.

## Implemented and aliased commands

| Workflow | Canonical rc form | mc-compatible rc form | Status |
| --- | --- | --- | --- |
| Alias management | `rc alias` | `rc alias` | Canonical |
| Bucket workflows | `rc bucket ...` | `rc mb`, `rb`, `event`, `cors`, `version`, `anonymous`, `quota`, `ilm`, `replicate`, `retention --default` | Alias; some operations are runtime gated |
| Object listing and metadata | `rc object list/show/head/stat` | `rc ls`, `cat`, `head`, `stat` | Alias |
| Object transfer | `rc object copy/move` | `rc cp`, `get`, `put`, `mv`, `pipe`, `mirror` | Alias |
| Object deletion | `rc object remove` | `rc rm` | Alias |
| Search and presentation | `rc object find/tree/share` | `rc find`, `tree`, `share` | Alias |
| Retention | `rc object retention` and `rc bucket lock` | `rc retention` | Alias |
| Legal hold | `rc object legal-hold` | `rc legalhold` | Alias |
| Tags | `rc bucket tag` and `rc object tag` | `rc tag` | Alias; runtime gated |
| Administrative workflows | `rc admin ...` | selected `mc admin`-compatible forms | Canonical, partial by documented subcommand |
| Health and usage | `rc ping`, `ready`, `du` | equivalent top-level names | Canonical; `du` fast path is runtime gated |
| Events | `rc watch` | `mc watch`-style filters | Canonical; runtime gated |
| Other data workflows | `rc diff`, `sql`, `undo` | equivalent top-level names | Canonical |

The retention and legal-hold compatibility entry points do not contain separate
execution logic. They parse `mc`-style syntax and call the same canonical lock
and object APIs, so exit codes and safety checks remain aligned.

## Runtime-gated behavior

The following client implementations exist but must not be assumed available
on every S3-compatible target:

| Family | Gate |
| --- | --- |
| Bucket notifications, CORS, versioning, tags, lifecycle, replication, and watch | RustFS runtime capability or authoritative S3 route response |
| Admin diagnostics, scanner, storage observations, KMS, OIDC, replication, and configuration | RustFS admin capability and bounded typed response |
| `du` server snapshot | `admin.data-usage`; client scan requires explicit `--fallback` |
| Advanced transfer fidelity | Exact S3 headers and behavior supported by the target release |

An unavailable gate returns exit code `7` (`unsupported_feature`) rather than
silently claiming success. Authentication, network, not-found, and conflict
failures retain their distinct exit codes.

## Active server blockers

| mc behavior not yet available through rc | RustFS/backlog blocker |
| --- | --- |
| Prefix/recursive incomplete multipart listing and abort | [#1384](https://github.com/rustfs/backlog/issues/1384), client completion [#1366](https://github.com/rustfs/backlog/issues/1366) |
| Forced non-empty bucket cleanup that includes incomplete uploads | [#1405](https://github.com/rustfs/backlog/issues/1405) |
| Real bounded support-inspect archive and truthful per-probe diagnostics | [#1401](https://github.com/rustfs/backlog/issues/1401), [#1402](https://github.com/rustfs/backlog/issues/1402) |
| Replication metrics and MRF cluster truth | [#1413](https://github.com/rustfs/backlog/issues/1413), client completion [#1414](https://github.com/rustfs/backlog/issues/1414) |
| Durable replication repair and resync error/status semantics | [#1408](https://github.com/rustfs/backlog/issues/1408), [#1415](https://github.com/rustfs/backlog/issues/1415), [#1416](https://github.com/rustfs/backlog/issues/1416), [#1417](https://github.com/rustfs/backlog/issues/1417) |
| Configuration restore/history safety | [#1398](https://github.com/rustfs/backlog/issues/1398), [#1399](https://github.com/rustfs/backlog/issues/1399) |

These rows describe blockers, not hidden best-effort modes. `rc` fails closed
when it cannot provide the requested semantics.

## Intentionally unsupported mc families

| mc family | rc position |
| --- | --- |
| `mc license` | MinIO commercial licensing is not a RustFS operation. |
| `mc update` | Binary delivery is handled by Cargo, Homebrew, Scoop, containers, and release packages. The CLI does not self-update. |
| `mc od` | The deprecated object-dump interface is not reproduced; use `rc cat`, `head`, `stat`, or `object show`. |
| `mc batch` | MinIO batch-job documents are not accepted without a matching RustFS job API and durable semantics. |
| Active drive/network speed-test aliases | `rc` reports explicitly labeled observations and bounded diagnostics; it does not disguise a mutating benchmark as a read-only command. |

Commands absent from this matrix are not implicitly supported. Check the
[command reference](README.md) and `rc --help`, and treat an unknown command as
unsupported rather than attempting to translate it heuristically.

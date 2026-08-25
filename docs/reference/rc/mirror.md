# rc mirror

## Purpose

`rc mirror` synchronizes a directory-like tree between the local filesystem and S3-compatible storage. It supports local-to-remote, remote-to-local, and remote-to-remote synchronization. Local-to-local synchronization is rejected; use a filesystem synchronization tool for that case.

## Syntax

```bash
rc [GLOBAL OPTIONS] mirror [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | A local directory or remote bucket/prefix in `ALIAS/BUCKET[/PREFIX]` form. Prefix ambiguous relative local paths with `./` or `../`. |
| `TARGET` | A local directory or remote bucket/prefix in `ALIAS/BUCKET[/PREFIX]` form. Prefix a new relative local directory with `./` or `../`. |
| `--overwrite` | Replace changed destination files or objects. Existing remote objects are replaced only if their planned ETag still matches. |
| `--remove` | Remove selected destination files or objects that are absent from the source. Removal starts only after every copy succeeds. |
| `--include <GLOB>` | Include source-relative paths matching the glob. Repeatable. When present, unmatched paths are skipped. |
| `--exclude <GLOB>` | Exclude source-relative paths matching the glob. Repeatable; exclusion always wins. |
| `--newer-than <AGE>` | Select entries modified more recently than the age, such as `1h` or `7d`. |
| `--older-than <AGE>` | Select entries modified less recently than the age, such as `1h` or `7d`. |
| `--continue-on-error`, `--skip-errors` | Continue independent copy operations after a per-entry failure. Failed copies still block all removals. |
| `-n, --dry-run` | Emit the deterministic plan without creating, replacing, or removing files or objects. |
| `-P, --concurrency <N>` | Maximum operations in flight across the command. Defaults to `4`; valid range is `1..=256`. `--parallel` remains a visible compatibility alias. |
| `--rate-limit <RATE>` | Apply aggregate transfer pacing, such as `10MiB/s`. |
| `--retry-attempts <N>` | Maximum attempts for transient failures. Defaults to `3`. |
| `--retry-initial-backoff-ms <MS>` | Initial transient retry backoff. Defaults to `100`. |
| `--retry-max-backoff-ms <MS>` | Maximum transient retry backoff. Defaults to `10000`. |
| `--summary` | Print deterministic aggregate counts and transferred bytes in human output. |
| `--compare <auto\|etag\|size>` | Choose how existing destination objects are compared before a copy is skipped. Defaults to `auto`. |
| `--quiet` | Suppress non-error command output. The global `--quiet` option has the same effect. |
| `--portable-names` | Reject keys that cannot be created on Windows filesystems. Unix destinations accept characters such as `:` by default. |

## Examples

```bash
rc mirror ./site/ rustfs/web/site/ --overwrite --summary
rc mirror rustfs/archive/ ./restore/ --remove --dry-run
rc mirror stage/data/ prod/data/ --include '**/*.json' --exclude '**/private/**'
rc mirror stage/data/ prod/data/ --overwrite --remove --concurrency 8 --rate-limit 20MiB/s
rc mirror stage/data/ prod/data/ --overwrite --compare auto
```

## Behavior

### Root and path mapping

Both operands are directory-like roots. Every selected source-relative path is appended to the destination root without flattening. Remote prefixes are normalized to one trailing `/`, so `alias/bucket/prefix` and `alias/bucket/prefix/` map the same tree. Remote listing is paginated and the final plan is sorted by normalized relative path.

Remote keys that are absolute, contain traversal, use backslashes, contain control characters, or collide after normalization are rejected. Windows filename rules (`:`, reserved device names, trailing dots or spaces) apply only when the destination is a local filesystem that needs them: always on Windows, and on other platforms only when `--portable-names` is set. Remote-to-remote copies keep the original object key, including characters such as `:`. A new relative local target should be written explicitly, for example `./restore/`, so it cannot be confused with `ALIAS/BUCKET` syntax.

### Comparison and restart behavior

Entries are compared by size and the strongest stable metadata available. `--compare` selects the skip rule:

| Mode | Skip a copy when |
| --- | --- |
| `auto` (default) | Destination and source ETags match, or sizes match and destination user metadata `x-amz-meta-rc-source-etag` records the source ETag from a previous `rc mirror` copy. |
| `etag` | Destination and source ETags are identical. Multipart re-uploads that change the stored ETag are treated as different. |
| `size` | Object sizes match, ignoring ETag differences. |

Remote-to-remote copies download through a temporary file and upload to the destination. Multipart completion often stores a different ETag than the source, so a second `auto` run would recopy every object if it compared ListObjects ETags alone. `rc mirror` therefore writes `x-amz-meta-rc-source-etag` on remote uploads that have a source ETag. ListObjects does not return user metadata, so `auto` issues HeadObject for same-size destinations whose listed ETags differ.

Downloads preserve the source modification time, and local-to-remote restart checks accept a same-size destination written no earlier than the source. A completed entry is skipped on a restarted command.

`--overwrite` authorizes replacing a changed destination; it does not disable concurrency checks. Mirror revalidates sources and compares local destination metadata again before persistence, so changes observed by those checks fail with the conflict exit code. New remote objects use `If-None-Match: *`, while existing remote objects and remote removals use the planned ETag as a condition; these service-side conditions also reject remote races after the final client-side check. Local replacement is atomic but is not a filesystem compare-and-swap, so a local writer racing after the final metadata check may be replaced.

### Local filesystem safety

Mirror does not follow symbolic links. Source symlinks and special files are skipped, and their destination-relative paths are protected from `--remove`. A destination symlink or special file is a conflict. Missing destination directories are created one component at a time with no-follow validation.

Remote downloads are written to a unique sibling staging file and atomically persisted only after the source is revalidated. Failed or interrupted transfers remove staging files and preserve the previous destination. Remote-to-remote transfers use a bounded-memory temporary file instead of buffering the complete object in memory.

### Failure, removal, and dry-run semantics

Copy and removal phases use the shared transfer controls for filtering, concurrency, rate pacing, retry, cancellation, and summaries. Removals are a separate second phase and are withheld if any copy fails or is cancelled, including when `--continue-on-error` is set. A failed run keeps successful copies so the same command can resume deterministically.

`--dry-run` may list remote prefixes and read local metadata to build the plan, but it performs no uploads, downloads, directory creation, replacements, or removals. Use it before combining `--overwrite` and `--remove`.

### Compatibility and migration

Existing remote-to-remote commands continue to work. `--parallel` is retained as an alias of the shared `--concurrency` option. Mirror no longer falls back to an unconditional byte copy when source metadata lookup fails, and missing remote ETags are no longer treated as proof of equality. Automation that depended on either unsafe fallback must handle explicit network or conflict exits and retry after re-planning.

### BREAKING object-key portability contract migration

`--portable-names` is additive. Unix destinations no longer apply Windows filename rules by default, so Loki-style keys containing `:` can be mirrored locally. Remote-to-remote copies keep the original object key. This PR must be marked `BREAKING` because `docs/reference/rc/mirror.md` is a protected CLI behavior contract. No JSON schema or config `schema_version` bump applies.

### BREAKING incremental identity contract migration

`--compare auto|etag|size` and destination metadata `x-amz-meta-rc-source-etag` are additive. Default skip-on-matching-ETag behavior is unchanged. Objects copied before this change still recopy once under `auto`, then skip. This PR must be marked `BREAKING` because `docs/reference/rc/mirror.md` is a protected CLI behavior contract. No JSON schema or config `schema_version` bump applies.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

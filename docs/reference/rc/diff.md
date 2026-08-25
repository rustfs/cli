# rc diff

## Purpose

`rc diff` compares two locations and reports objects that are missing, different, or identical depending on selected options.

## Syntax

```bash
rc [GLOBAL OPTIONS] diff [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local or remote source path. |
| `TARGET` | Local or remote target path. |
| `-r, --recursive` | Compare recursively. |
| `--diff-only` | Show only differences instead of all compared entries. |
| `--compare <auto\|etag\|size>` | Choose how two objects are judged identical. Defaults to `auto`. |

## Examples

```bash
rc diff local/reports backup/reports --recursive
rc diff ./reports local/reports --recursive --json
rc diff stage/data/ prod/data/ --recursive --compare etag
```

## Behavior

Diff is read-only. Use it before copy, mirror, or remove workflows to inspect drift between two locations.

### Comparison rules

`--compare` selects when two objects are reported as identical. Sizes must always match; an entry whose size is unknown on either side is always reported as different.

| Mode | Same when |
| --- | --- |
| `auto` (default) | ETags match, or sizes match and the target records the source ETag in `x-amz-meta-rc-source-etag` from a previous `rc mirror` or cross-alias `rc cp`. |
| `etag` | Both ETags are present and identical. |
| `size` | Object sizes match, ignoring ETag differences. |

These are the same rules as `rc mirror --compare`, so the two commands cannot disagree about whether a pair of objects is already synchronized. A remote-to-remote copy that streams through the client cannot preserve the source ETag, so comparing listed ETags alone would report a difference for data that is byte-identical.

ListObjects does not return user metadata, so `auto` issues HeadObject only for same-size targets whose listed ETags differ. An unchanged tree costs no extra requests, and `etag` and `size` never issue HeadObject.

### BREAKING comparison contract migration

`rc diff` previously required both ETags to be present and identical, which is the current `--compare etag` behavior. The default is now `auto`, so a target that records the source ETag is reported as `Same` instead of `Different`, and the command exits `0` where it previously exited `1`. Sizes that are unknown on either side are now always reported as different rather than being treated as equal. Pass `--compare etag` to keep the previous comparison. This PR must be marked `BREAKING` because `docs/reference/rc/diff.md` is a protected CLI behavior contract. No JSON schema or config `schema_version` bump applies.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

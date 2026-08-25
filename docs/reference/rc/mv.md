# rc mv

## Purpose

`rc mv` moves files or objects between local and S3-compatible locations. It is a legacy-compatible command; prefer `rc object move` for new scripts.

## Syntax

```bash
rc [GLOBAL OPTIONS] mv [OPTIONS] <SOURCE> <TARGET>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SOURCE` | Local path or remote object/prefix path to move. |
| `TARGET` | Local or remote destination path. Remote destinations may use a different alias than the source. |
| `-r, --recursive` | Move a directory or prefix recursively. |
| `--dry-run` | Show planned changes without moving data. |
| `--continue-on-error` | Continue recursive moves after per-object failures. |
| `--enc-s3 <TARGET>` | Apply `SSE-S3` to the named remote destination write. |
| `--enc-kms <TARGET>=<KMS_KEY_ID>` | Apply `SSE-KMS` to the named remote destination write. |

## Examples

```bash
rc mv local/inbox/report.json local/archive/report.json
rc object move ./incoming/ local/inbox/ --recursive
rc mv local/inbox/a.txt local/archive/a.txt --enc-s3 local/archive/a.txt
rc mv local/inbox/ local/archive/ --recursive --enc-kms local/archive/=alias/archive-key
```

Move between aliases:

```bash
rc mv stage/data/report.json prod/archive/report.json
rc mv stage/data/ prod/archive/ --recursive
```

## Behavior

Move operations copy data to the target and remove the source after a successful copy. The source is removed only after its own copy succeeds, so a failed or partial move never destroys data that did not arrive. Review recursive moves with `--dry-run` before running destructive operations.

Same-alias moves between remote paths use server-side CopyObject. Moves between different aliases cannot use CopyObject, so they stream through the client exactly like a cross-alias `rc cp`, then delete the source. Cross-alias moves therefore inherit the copy path's constraints: `SSE-C` is rejected, and `--storage-class` is rejected for objects large enough to require a multipart upload.

### BREAKING cross-alias move contract migration

Remote-to-remote moves between different aliases previously failed with the unsupported-feature exit code. They now succeed by copying through the client and deleting the source. Automation that relied on the old rejection to detect an unsupported operation must be updated. Same-alias moves are unchanged. This PR must be marked `BREAKING` because `docs/reference/rc/mv.md` is a protected CLI behavior contract. No JSON schema or config `schema_version` bump applies.

Destination encryption flags apply only to remote writes. On `rc mv`, the selector in `--enc-s3` or `--enc-kms` must match the command destination exactly:

- For a single-object move, use the full remote object path.
- For a recursive move, use the same remote prefix passed as `TARGET`.

The current implementation supports `SSE-S3` and `SSE-KMS`. It does not support `SSE-C`, repeated encryption selectors, or MinIO `mc`-style prefix fan-out matching beyond the exact destination argument for the current command. For shared encryption rules across commands, see [Encryption workflows](encryption.md).

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |

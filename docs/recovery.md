# MRF responsibility management and independent integrity jobs

These commands separate three facts: recovery work can be idle, a durable MRF responsibility can remain unverified, and a selected object's content can match a stated digest. None implies either of the others. Risk acceptance, an audit, and creation of a protected copy do not remove the source MRF responsibility.

The commands require the RustFS recovery Admin APIs and the corresponding server permissions. They do not run a scan when opened or queried. A missing, unreachable, or unsupported node remains unknown; its responsibility count is not zero. Existing `rc admin heal` and replication MRF commands keep their existing behavior.

## Observe and act on a responsibility

```bash
rc admin heal mrf status production
rc --json admin heal mrf list production --limit 64 > mrf-page.json
rc admin heal mrf get production --node-id "$NODE_ID" --responsibility-id "$RESPONSIBILITY_ID"
rc admin heal mrf refresh production --node-id "$NODE_ID" --responsibility-id "$RESPONSIBILITY_ID"
rc admin heal mrf recheck production --node-id "$NODE_ID" --responsibility-id "$RESPONSIBILITY_ID" --expected-bucket-incarnation "$INCARNATION"
```

Use the server-returned `nodeId` and `responsibilityId` together. `status` and `list` read exactly one bounded page; `list` accepts a page limit of 1–256. When `nextCursor` is non-null, pass it explicitly as `--cursor` to the same command. Do not automatically enumerate all pages or poll every row. Observation times and node coverage describe a sampled page, not a frozen cluster snapshot or a complete inventory of verified objects. Summaries sampled on different pages must not be added together. `get` can also observe an Active responsibility that no longer appears in the retained legacy list.

`refresh` updates identity observations without reading the object payload. `recheck` requires the expected bucket incarnation and submits work; the acknowledgment does not mean that work finished. Query the same node and responsibility afterward. `not_observed` means the current runtime did not return that responsibility; it is not evidence of successful recovery or verified content. A source with an unknown incarnation or a different historical incarnation cannot be certified by checking the current same-named object.

Accepting unverified risk is a separate, explicit operation:

```bash
rc admin heal mrf accept-risk production --node-id "$NODE_ID" --responsibility-id "$RESPONSIBILITY_ID" --expected-bucket-incarnation "$INCARNATION" --request-id "$REQUEST_ID" --reason "Independent historical digest unavailable" --reference "incident-3177" --acknowledge-unverified-data-risk
```

Generate a non-nil UUID for `REQUEST_ID` and retain the exact request. A reason of 1–1024 bytes and a reference of 1–256 bytes are required. If applicable, also acknowledge `--acknowledge-unknown-source-incarnation` or `--acknowledge-bucket-incarnation-mismatch`. Acceptance remains unverified and retains the durable record. A lost acknowledgment is reported as an unknown outcome: read the same node and responsibility and reconcile the audit record using the original request ID and identical fields. The CLI does not automatically retry mutations or clear responsibilities.

## Select objects independently

```bash
rc --json admin integrity inventory production documents --limit 100 > inventory.json
```

Inventory reads one page, with a limit of 1–100. Continue explicitly using `--key-marker` and, when present, `--version-marker`. An MRF row is not an inventory record. Inventory protection declarations and stored metadata are observations; neither certifies the payload. Preserve the JSON output and select items from it into a manifest:

```json
[
  {
    "key": "archive/report.bin",
    "version_id": "null",
    "expected_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "target_key": null
  }
]
```

Replace the illustrative digest with the digest from your evidence. SHA-256 accepts 64 hexadecimal characters or canonical Base64 encoding of exactly 32 bytes; the request and receipt use canonical Base64. An administrator-supplied digest requires `--digest-source-reference`. This reference is an external provenance claim retained in the local receipt; RustFS does not certify its origin. A digest freshly calculated from the current object is not independent historical evidence. Audit can omit `expected_sha256` to use a supported stored digest, with the correspondingly narrower result interpretation.

The JSON string `"null"` selects the null version slot; JSON `null` or an omitted `version_id` selects whichever version is current when execution selects the source. This differs from an MRF row's null version. A concrete non-null version is a non-nil UUID. The inventory's bucket incarnation and source fingerprint are prior observations, not execution preconditions. Results report the source actually observed by execution and must not be attributed automatically to an old MRF responsibility. The backend rejects unsupported object or bucket configurations and detects source changes within its execution fences.

## Create paused work and save the receipt

```bash
rc admin integrity audit production documents --inventory-file inventory.json --manifest manifest.json --receipt audit-receipt.json --bytes-per-second 10485760 --max-object-bytes 1073741824 --digest-source-reference "backup-catalog/2026-11-01/report.bin"
```

Jobs contain 1–64 explicit objects. The per-object processing budget is 1 byte–5 GiB; the per-stream rate is 64 KiB/s–1 GiB/s. Audit reads the full payload and validates its length and digest. Newer streaming audit implementations do not stage a complete payload file, but older servers using the same integrity v3 API can do so. API availability alone does not identify that capability. The CLI therefore reserves `max_object_bytes` as a conservative payload staging budget for both audit and migration. Job checkpoints still write metadata. A rate applies to a stream, not all EC, disk, metadata, or network traffic. Audit estimates count one stream pass; inventory sizes produce an estimate rather than an ETA, and current-version size remains unknown. Select a rate and batch size qualified for concurrent business traffic.

Creation returns a durable paused job. The CLI reserves a temporary file in the receipt directory before making the request, verifies the returned job and request, writes and syncs the receipt, then publishes it without overwriting an existing file. On Unix it also syncs the parent directory. The command never resumes automatically. If local publication or synchronization fails after creation, stdout includes the paused job ID and receipt data, stderr contains an error, and the exit is nonzero. Preserve that output and inspect the job before any further action. Do not blindly rerun creation.

The version 1 receipt records the endpoint without credentials, alias, full job request and identity, inventory observation, digest provenance, optional deployment qualification and readiness, workload estimate, and query/resume argument arrays. Keep it with the incident or qualification record. It contains object identities and operational context; avoid secrets in free-text references. A receipt authorizes no server operation by itself: every request still uses the current alias credentials and server authorization.

## Resume, query, and export after a client restart

```bash
rc admin integrity resume production documents "$JOB_ID" --receipt audit-receipt.json
rc --json admin integrity job production documents "$JOB_ID" --output audit-result.json
rc admin integrity pause production documents "$JOB_ID"
rc admin integrity cancel production documents "$JOB_ID"
```

Copy the receipt to a new client if needed, configure an alias for the same normalized endpoint, and use the same bucket and job ID. The alias name may change. `resume` checks the receipt's endpoint, job identity, bucket incarnation and request against the durable job before sending control. A changed endpoint, different job, or changed request is a conflict requiring review. The result export is a separate no-overwrite file; it is not a substitute for the creation receipt. Console versions supporting this contract can import the receipt or query an existing job by bucket and ID.

`running` is the last persisted state, not proof that a worker is currently alive. After coordinator restart, explicit resume uses the existing cluster worker lock; a busy/conflicting response is nonzero. Pause and cancel are cooperative. An acknowledgment may still show an incomplete state; inspect the job to confirm the requested terminal state. Cancellation retains targets already published. There is no automatic job recreation, polling, or business key switch.

If creation loses its response, a paused job may exist without a known ID. The CLI returns `unknown` and performs no automatic recreation or resume. The current API has no job-list or idempotent create lookup, so an unknown ID cannot be recovered through these commands. A later explicit creation may leave an older paused record. If the ID is known but the receipt was lost, query/export the original job and recover the saved creation response or receipt before resuming; the CLI does not fabricate missing provenance or deployment qualification from the current job.

Interpret each result using its `evidence_source`: `administrator_supplied_sha256` means content matched the supplied digest; `stored_sha256` means content matched the stored digest. Missing or unknown evidence remains unknown. A `complete` backend job only means all items have terminal outcomes. CLI success for `job` requires every item to be `verified` for audit or `migrated` for migration and to have a recognized evidence source. A mismatch, unsupported item, failure, unknown evidence, or unfinished item returns nonzero. The durable format has no actual check timestamp: `checked_at` stays null, and `retrieved_at` is the client retrieval time. The `source_responsibility_retained: true` result flag records that this job does not discharge MRF responsibilities; it is not a live query of a particular responsibility. Use `heal mrf get` for its current observed state.

## Create a protected copy only after deployment qualification

```bash
rc admin integrity readiness production
rc admin integrity migrate production documents --inventory-file inventory.json --manifest migration.json --receipt migration-receipt.json --bytes-per-second 10485760 --max-object-bytes 1073741824 --digest-source-reference "backup-catalog/2026-11-01" --deployment-id "production-a" --qualification-reference "qualification/candidate-and-layout" --acknowledge-qualified-deployment
```

For migration, each item requires an explicit digest and a distinct `target_key` that is neither another selected source nor another target. The workflow is create-only and preserves the source. It stages the exact bytes validated against the digest, publishes those bytes, and reads the target back. The estimate includes three stream passes: source read, target write, and target verification read, plus a per-object staging budget. It excludes EC, metadata, retries and device waits. A target conflict, source change or digest mismatch must be investigated rather than bypassed by overwriting or claiming another operation's target.

Migration also requires an external deployment qualification record and local readiness reporting that the reader and protected new writes are enabled. The receipt stores the deployment ID, qualification reference and operator attestation. Local readiness is not fleet certification: `fleet_capability_verified` remains false in the current server contract. The CLI does not enable protection flags. Reader/writer/repair compatibility, exclusion of incompatible old processes, actual deployment layout, fault recovery and performance must be qualified separately before enabling protected writes. Resume rechecks local activation and the receipt's qualification and digest provenance. Existing advanced server stored-digest migration modes do not acquire independent historical certification through this product workflow.

## JSON and exit status

Recovery commands add the `mrf_observation`, `integrity_readiness`, `integrity_inventory`, `integrity_create`, `integrity_job`, and `integrity_control` families to [output schema v3](../schemas/output_v3.json). Generic integrity failures use `integrity`. Existing output v1/v2 families and exit-code values remain unchanged. Consumers must inspect the semantic `status` and process exit, not look for a generic `success` string.

| Exit | Recovery meaning |
| --- | --- |
| 0 | The requested observation was obtained, the action was acknowledged, a paused receipt was saved, or all queried job items passed. The `type` and `status` distinguish these outcomes. |
| 1 | Partial coverage/pagination, not observed, unknown mutation outcome, incomplete or unverified job, or local/general failure. |
| 2 | Invalid selector, manifest, receipt or command input. |
| 3 | Read-only network failure. Mutation ambiguity is reported as exit 1 with `unknown`. |
| 4 | Authentication or permission failure. |
| 5 | Known bucket or integrity job not found. |
| 6 | Identity, target, job, or local no-overwrite conflict. |
| 7 | Unsupported recovery API, unsupported selection, or protection not activated for migration. |

Errors carry `message`, `exit_code`, and `automatic_retry: false`. A node-local failure inside an aggregate response can produce `partial` or an unconfirmed action instead of a whole-request HTTP error. Every HTTP request has a 35-second total deadline, including response body reads, and an 8 MiB response limit. A command containing readiness, job lookup and control can make several bounded requests. A timeout does not undo a mutation already accepted by the server; always reconcile by original identity before retrying it.

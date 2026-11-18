//! Explicit inventory selection, paused jobs, and receipts that survive a client restart.

use clap::{Args, Subcommand};
use rc_core::admin::recovery::*;
use rc_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use super::{get_admin_alias, get_admin_client, normalize_admin_alias, recovery};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
pub enum IntegrityCommands {
    /// Read local protection configuration; this is not fleet qualification
    Readiness(AliasArgs),
    /// Observe one inventory page for independent object/version selection
    Inventory(InventoryArgs),
    /// Create a paused audit from explicit inventory selections and save its receipt
    Audit(CreateArgs),
    /// Create a paused, create-only migration for an externally qualified deployment
    Migrate(MigrateArgs),
    /// Read or export a durable job; complete does not mean every item passed
    Job(JobArgs),
    /// Explicitly resume a saved job using its receipt; does not recreate missing jobs
    Resume(ResumeArgs),
    /// Request a cooperative pause (may not release resources immediately)
    Pause(JobTarget),
    /// Request cancellation; already-published targets are retained
    Cancel(JobTarget),
}

#[derive(Args, Debug)]
pub struct AliasArgs {
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct InventoryArgs {
    pub alias: String,
    pub bucket: String,
    #[arg(long, default_value = "")]
    pub prefix: String,
    #[arg(long)]
    pub key_marker: Option<String>,
    #[arg(long, requires = "key_marker")]
    pub version_marker: Option<String>,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=100))]
    pub limit: u16,
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    pub alias: String,
    pub bucket: String,
    /// JSON array of explicit items (key, version_id, expected_sha256, target_key)
    #[arg(long)]
    pub manifest: PathBuf,
    /// Saved JSON output of integrity inventory; MRF rows are not inventory
    #[arg(long)]
    pub inventory_file: PathBuf,
    /// New file for the durable paused-job receipt; never overwritten
    #[arg(long)]
    pub receipt: PathBuf,
    /// Per-stream rate, 65536..1073741824 bytes/s; not a total disk/network cap
    #[arg(long)]
    pub bytes_per_second: u64,
    /// Per-object processing and conservative payload staging budget, at most 5 GiB
    #[arg(long)]
    pub max_object_bytes: u64,
    /// External provenance for administrator-supplied digests; not certified by RustFS
    #[arg(long)]
    pub digest_source_reference: Option<String>,
}

#[derive(Args, Debug)]
pub struct MigrateArgs {
    #[command(flatten)]
    pub create: CreateArgs,
    /// Deployment identity recorded in the external qualification report
    #[arg(long)]
    pub deployment_id: String,
    /// External compatibility, failure-recovery and performance qualification record
    #[arg(long)]
    pub qualification_reference: String,
    /// Attest that the referenced report qualifies this deployment and candidate binary
    #[arg(long, required = true)]
    pub acknowledge_qualified_deployment: bool,
}

#[derive(Args, Debug)]
pub struct JobTarget {
    pub alias: String,
    pub bucket: String,
    pub job_id: String,
}

#[derive(Args, Debug)]
pub struct JobArgs {
    #[command(flatten)]
    pub target: JobTarget,
    /// Save the result and its retrieval time to a new local file
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ResumeArgs {
    #[command(flatten)]
    pub target: JobTarget,
    #[arg(long)]
    pub receipt: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InventorySelection {
    bucket: String,
    inventory: IntegrityInventory,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum InventoryDocument {
    Raw(InventorySelection),
    Output { data: InventorySelection },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Qualification {
    deployment_id: String,
    reference: String,
    operator_attested: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkEstimate {
    object_count: usize,
    inventory_selected_bytes: Option<u64>,
    per_stream_bytes_per_second: u64,
    stream_passes: u8,
    minimum_stream_seconds: Option<f64>,
    peak_payload_staging_budget: u64,
    note: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format_version: u32,
    alias: String,
    endpoint: String,
    job: IntegrityJob,
    inventory_observed_at: String,
    inventory_bucket_incarnation: String,
    digest_source_reference: Option<String>,
    qualification: Option<Qualification>,
    readiness: Option<Value>,
    estimate: WorkEstimate,
    query_command: Vec<String>,
    resume_command: Vec<String>,
}

#[derive(Serialize)]
struct JobView<'a> {
    job: &'a IntegrityJob,
    retrieved_at: String,
    /// The persisted format has no actual check timestamp.
    checked_at: Option<String>,
    source_responsibility_retained: bool,
}

impl IntegrityCommands {
    fn alias(&self) -> &str {
        match self {
            Self::Readiness(args) => &args.alias,
            Self::Inventory(args) => &args.alias,
            Self::Audit(args) => &args.alias,
            Self::Migrate(args) => &args.create.alias,
            Self::Job(args) => &args.target.alias,
            Self::Resume(args) => &args.target.alias,
            Self::Pause(args) | Self::Cancel(args) => &args.alias,
        }
    }
}

pub async fn execute(command: IntegrityCommands, formatter: &Formatter) -> ExitCode {
    let alias = match get_admin_alias(command.alias(), formatter) {
        Ok(alias) => alias,
        Err(code) => return code,
    };
    let endpoint = match safe_endpoint(&alias.endpoint) {
        Ok(endpoint) => endpoint,
        Err(error) => return recovery::error("integrity", &error, false, formatter),
    };
    let client = match get_admin_client(command.alias(), formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_with_api(&command, &endpoint, &client, formatter).await
}

fn safe_endpoint(endpoint: &str) -> Result<String> {
    let mut url = url::Url::parse(endpoint)?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn execute_with_api(
    command: &IntegrityCommands,
    endpoint: &str,
    api: &dyn RecoveryApi,
    formatter: &Formatter,
) -> ExitCode {
    let result = match command {
        IntegrityCommands::Readiness(_) => readiness(api, formatter).await,
        IntegrityCommands::Inventory(args) => inventory(args, api, formatter).await,
        IntegrityCommands::Audit(args) => create(args, None, endpoint, api, formatter).await,
        IntegrityCommands::Migrate(args) => {
            create(&args.create, Some(args), endpoint, api, formatter).await
        }
        IntegrityCommands::Job(args) => job(args, api, formatter).await,
        IntegrityCommands::Resume(args) => {
            control(
                &args.target,
                "resume",
                Some(&args.receipt),
                endpoint,
                api,
                formatter,
            )
            .await
        }
        IntegrityCommands::Pause(args) => {
            control(args, "pause", None, endpoint, api, formatter).await
        }
        IntegrityCommands::Cancel(args) => {
            control(args, "cancel", None, endpoint, api, formatter).await
        }
    };
    result.unwrap_or_else(|error| recovery::error("integrity", &error, false, formatter))
}

async fn readiness(api: &dyn RecoveryApi, formatter: &Formatter) -> Result<ExitCode> {
    let value = api.integrity_readiness().await?;
    recovery::output(
        "integrity_readiness",
        "observed",
        "Local configuration observed. Fleet qualification is external and cannot be inferred from these switches.",
        &value,
        formatter,
    );
    if !formatter.is_json() {
        formatter.println(&serde_json::to_string_pretty(&value)?);
    }
    Ok(ExitCode::Success)
}

async fn inventory(
    args: &InventoryArgs,
    api: &dyn RecoveryApi,
    formatter: &Formatter,
) -> Result<ExitCode> {
    let page = api
        .integrity_inventory(
            &args.bucket,
            &IntegrityInventoryQuery {
                prefix: args.prefix.clone(),
                key_marker: args.key_marker.clone(),
                version_marker: args.version_marker.clone(),
                limit: args.limit,
            },
        )
        .await?;
    let partial = page.is_truncated;
    let selection = InventorySelection {
        bucket: args.bucket.clone(),
        inventory: page,
    };
    recovery::output(
        "integrity_inventory",
        if partial { "partial" } else { "observed" },
        "Select objects independently from this inventory. Protection declarations do not verify payloads. A string \"null\" selects the null version slot; JSON null selects the current version at execution.",
        &selection,
        formatter,
    );
    if !formatter.is_json() {
        formatter.println("KEY  VERSION  SIZE  PROTECTION  UNSUPPORTED / OBSERVATION ERROR");
        for item in &selection.inventory.items {
            formatter.println(&format!(
                "{}  {}  {}  {}  {}",
                formatter.sanitize_text(&item.key),
                formatter.sanitize_text(&item.version_id),
                item.size,
                formatter.sanitize_text(&item.protection),
                formatter.sanitize_text(
                    item.observation_error
                        .as_deref()
                        .or(item.audit_unsupported.as_deref())
                        .unwrap_or("-")
                )
            ));
        }
        formatter.println(&format!(
            "More pages: {partial}; key marker: {}; version marker: {}",
            formatter.sanitize_text(
                selection
                    .inventory
                    .next_key_marker
                    .as_deref()
                    .unwrap_or("-")
            ),
            formatter.sanitize_text(
                selection
                    .inventory
                    .next_version_marker
                    .as_deref()
                    .unwrap_or("-")
            )
        ));
    }
    Ok(if partial {
        ExitCode::GeneralError
    } else {
        ExitCode::Success
    })
}

fn reference(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 2048 {
        return Err(Error::InvalidPath(
            "a nonempty reference of at most 2048 bytes is required".into(),
        ));
    }
    Ok(())
}

fn selection(
    args: &CreateArgs,
    mode: &str,
) -> Result<(IntegrityJobRequest, InventorySelection, WorkEstimate)> {
    let mut request = IntegrityJobRequest {
        mode: mode.into(),
        items: recovery::read_json(&args.manifest, 128 * 1024)?,
        bytes_per_second: args.bytes_per_second,
        max_object_bytes: args.max_object_bytes,
    };
    request.validate_and_normalize()?;
    if request
        .items
        .iter()
        .any(|item| item.expected_sha256.is_some())
    {
        reference(args.digest_source_reference.as_deref().ok_or_else(|| {
            Error::InvalidPath("--digest-source-reference is required for supplied digests".into())
        })?)?;
    }
    let inventory = match recovery::read_json(&args.inventory_file, 8 * 1024 * 1024)? {
        InventoryDocument::Raw(value) | InventoryDocument::Output { data: value } => value,
    };
    if inventory.bucket != args.bucket {
        return Err(Error::InvalidPath(
            "inventory belongs to a different bucket".into(),
        ));
    }
    validate_recovery_uuid(&inventory.inventory.bucket_incarnation)?;
    let mut total = Some(0u64);
    for item in &request.items {
        let candidates: Vec<_> = inventory
            .inventory
            .items
            .iter()
            .filter(|candidate| {
                candidate.key == item.key
                    && item
                        .version_id
                        .as_ref()
                        .is_none_or(|version| candidate.version_id == *version)
            })
            .collect();
        if candidates.is_empty() {
            return Err(Error::InvalidPath(
                "every manifest item must be explicitly selected from the saved inventory page"
                    .into(),
            ));
        }
        if item.version_id.is_none() {
            total = None;
        } else {
            let candidate = candidates[0];
            if candidate.observation_error.is_some()
                || candidate.audit_unsupported.is_some()
                || candidate.size < 0
            {
                return Err(Error::UnsupportedFeature("selected inventory item has an observation error or unsupported payload; inspect the inventory reason".into()));
            }
            let size = u64::try_from(candidate.size)
                .map_err(|_| Error::InvalidPath("negative inventory size".into()))?;
            if size > request.max_object_bytes {
                return Err(Error::InvalidPath(
                    "selected object exceeds --max-object-bytes".into(),
                ));
            }
            total = total.and_then(|total| total.checked_add(size));
        }
    }
    let passes = if mode == "migrate" { 3 } else { 1 };
    let estimate = WorkEstimate {
        object_count: request.items.len(), inventory_selected_bytes: total, per_stream_bytes_per_second: request.bytes_per_second,
        stream_passes: passes,
        minimum_stream_seconds: total.map(|bytes| bytes as f64 * f64::from(passes) / request.bytes_per_second as f64),
        peak_payload_staging_budget: request.max_object_bytes,
        note: "Inventory is a prior observation, not an execution precondition. Current-version size is unknown. Stream estimates exclude EC, metadata, retries and device waits; they are not an ETA. Payload staging is conservatively budgeted across server versions: older audits can stage the full payload, while streaming audits do not. API availability alone does not identify that behavior. Checkpoints still write metadata.".into(),
    };
    Ok((request, inventory, estimate))
}

fn ensure_activated(readiness: &Value) -> Result<()> {
    if readiness.get("local_reader_supported") != Some(&Value::Bool(true))
        || readiness.get("new_writes_enabled_here") != Some(&Value::Bool(true))
    {
        return Err(Error::UnsupportedFeature("protected writes are not enabled at this endpoint; complete deployment qualification and enable both protection switches first".into()));
    }
    Ok(())
}

async fn create(
    args: &CreateArgs,
    migrate: Option<&MigrateArgs>,
    endpoint: &str,
    api: &dyn RecoveryApi,
    formatter: &Formatter,
) -> Result<ExitCode> {
    let mode = if migrate.is_some() {
        "migrate"
    } else {
        "audit"
    };
    let (request, inventory, estimate) = selection(args, mode)?;
    let qualification = if let Some(migrate) = migrate {
        reference(&migrate.deployment_id)?;
        reference(&migrate.qualification_reference)?;
        if !migrate.acknowledge_qualified_deployment {
            return Err(Error::InvalidPath(
                "explicit deployment qualification attestation is required".into(),
            ));
        }
        Some(Qualification {
            deployment_id: migrate.deployment_id.clone(),
            reference: migrate.qualification_reference.clone(),
            operator_attested: true,
        })
    } else {
        None
    };
    let file = recovery::prepare_receipt(&args.receipt)?;
    let readiness = if migrate.is_some() {
        let readiness = api.integrity_readiness().await?;
        ensure_activated(&readiness)?;
        Some(readiness)
    } else {
        None
    };
    let created = match api.integrity_create(&args.bucket, &request).await {
        Ok(job) => job,
        Err(error) => {
            let unknown = !matches!(
                error,
                Error::Auth(_)
                    | Error::InvalidPath(_)
                    | Error::UnsupportedFeature(_)
                    | Error::Conflict(_)
            );
            if !unknown {
                return Err(error);
            }
            return Ok(recovery::error(
                "integrity_create",
                &Error::General(format!(
                    "{error}. Creation outcome is unknown and no job ID was confirmed. A paused job may exist. Do not automatically recreate or resume; a later explicit creation may leave an older paused record."
                )),
                true,
                formatter,
            ));
        }
    };
    if created.format_version != 1
        || created.state != "paused"
        || created.bucket != args.bucket
        || created.request != request
        || validate_recovery_uuid(&created.id).is_err()
    {
        return Ok(recovery::error("integrity_create", &Error::General("Creation outcome is unknown: the paused-job receipt did not match the request. No automatic resume or recreation was attempted.".into()), true, formatter));
    }
    let mut query = vec![
        "rc".into(),
        "admin".into(),
        "integrity".into(),
        "job".into(),
        normalize_admin_alias(&args.alias).into(),
        args.bucket.clone(),
        created.id.clone(),
    ];
    let query_command = query.clone();
    query[3] = "resume".into();
    query.extend([
        "--receipt".into(),
        args.receipt.to_string_lossy().into_owned(),
    ]);
    let receipt = Receipt {
        format_version: 1,
        alias: normalize_admin_alias(&args.alias).into(),
        endpoint: endpoint.into(),
        job: created,
        inventory_observed_at: inventory.inventory.observed_at,
        inventory_bucket_incarnation: inventory.inventory.bucket_incarnation,
        digest_source_reference: args.digest_source_reference.clone(),
        qualification,
        readiness,
        estimate,
        query_command,
        resume_command: query,
    };
    if let Err(error) = recovery::save_receipt(file, &args.receipt, &receipt) {
        recovery::output(
            "integrity_create",
            "receipt_not_saved",
            "A paused job was created, but its receipt could not be durably saved. Record this job ID; execution has not been requested.",
            &receipt,
            formatter,
        );
        if !formatter.is_json() {
            formatter.println(&format!("Job ID: {}", receipt.job.id));
        }
        return Ok(recovery::error(
            "integrity_create",
            &error,
            false,
            formatter,
        ));
    }
    recovery::output(
        "integrity_create",
        "paused_receipt_saved",
        "Paused job created and receipt saved. Review the selection and budgets, then explicitly resume. No historical MRF responsibility is verified or removed.",
        &receipt,
        formatter,
    );
    if !formatter.is_json() {
        formatter.println(&format!("Job ID: {}\nObjects: {}; observed bytes: {:?}; per-stream rate: {} B/s; stream passes: {}; payload staging budget: {} bytes", receipt.job.id, receipt.estimate.object_count, receipt.estimate.inventory_selected_bytes, receipt.estimate.per_stream_bytes_per_second, receipt.estimate.stream_passes, receipt.estimate.peak_payload_staging_budget));
        formatter.println(&format!(
            "Receipt: {}",
            formatter.sanitize_text(&args.receipt.display().to_string())
        ));
        formatter.println(&format!(
            "Resume arguments: {}",
            serde_json::to_string(&receipt.resume_command)?
        ));
    }
    Ok(ExitCode::Success)
}

fn job_outcome(job: &IntegrityJob) -> (ExitCode, &'static str) {
    let expected = if job.request.mode == "audit" {
        "verified"
    } else {
        "migrated"
    };
    if job.state == "complete"
        && !job.results.is_empty()
        && job.results.len() == job.request.items.len()
        && job.results.iter().all(|item| {
            item.state == expected
                && matches!(
                    item.evidence_source.as_deref(),
                    Some("administrator_supplied_sha256" | "stored_sha256")
                )
        })
    {
        (ExitCode::Success, "complete")
    } else {
        (
            ExitCode::GeneralError,
            if job.state == "complete" || job.state == "failed" {
                "not_verified"
            } else {
                "incomplete"
            },
        )
    }
}

async fn job(args: &JobArgs, api: &dyn RecoveryApi, formatter: &Formatter) -> Result<ExitCode> {
    let job = api
        .integrity_job(&args.target.bucket, &args.target.job_id)
        .await?;
    let view = JobView {
        job: &job,
        retrieved_at: jiff::Timestamp::now().to_string(),
        checked_at: None,
        source_responsibility_retained: true,
    };
    if let Some(path) = &args.output {
        recovery::save_receipt(recovery::prepare_receipt(path)?, path, &view)?;
    }
    let (code, status) = job_outcome(&job);
    recovery::output(
        "integrity_job",
        status,
        "Durable result observed. Running is a persisted state, not proof of worker liveness. Check time is unknown; retrieval time is separate. Source MRF responsibilities remain retained.",
        &view,
        formatter,
    );
    if !formatter.is_json() {
        print_job(&job, formatter);
    }
    Ok(code)
}

async fn control(
    target: &JobTarget,
    operation: &str,
    receipt_path: Option<&PathBuf>,
    endpoint: &str,
    api: &dyn RecoveryApi,
    formatter: &Formatter,
) -> Result<ExitCode> {
    validate_recovery_uuid(&target.job_id)?;
    if let Some(path) = receipt_path {
        let receipt: Receipt = recovery::read_json(path, 1024 * 1024)?;
        if receipt.format_version != 1
            || receipt.endpoint != endpoint
            || receipt.job.bucket != target.bucket
            || receipt.job.id != target.job_id
        {
            return Err(Error::Conflict(
                "receipt does not identify this endpoint, bucket and job".into(),
            ));
        }
        let current = api.integrity_job(&target.bucket, &target.job_id).await?;
        if current.bucket_incarnation != receipt.job.bucket_incarnation
            || current.request != receipt.job.request
        {
            return Err(Error::Conflict("current job differs from the saved receipt; review the durable job before resuming".into()));
        }
        // A receipt must not bypass the selection rules enforced by creation.
        let mut normalized = current.request.clone();
        normalized.validate_and_normalize()?;
        if normalized != current.request {
            return Err(Error::Conflict(
                "saved job selectors are not canonical; review the original job".into(),
            ));
        }
        if current
            .request
            .items
            .iter()
            .any(|item| item.expected_sha256.is_some())
        {
            reference(receipt.digest_source_reference.as_deref().ok_or_else(|| {
                Error::InvalidPath("receipt lacks provenance for supplied digests".into())
            })?)?;
        }
        if current.request.mode == "migrate" {
            if receipt.qualification.as_ref().is_none_or(|record| {
                !record.operator_attested
                    || record.reference.trim().is_empty()
                    || record.deployment_id.trim().is_empty()
            }) || receipt
                .digest_source_reference
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(Error::InvalidPath(
                    "migration receipt lacks deployment qualification or digest provenance".into(),
                ));
            }
            ensure_activated(&api.integrity_readiness().await?)?;
        }
    }
    let result = match api
        .integrity_control(&target.bucket, &target.job_id, operation)
        .await
    {
        Ok(job) => job,
        Err(error) => {
            if matches!(
                error,
                Error::Auth(_)
                    | Error::InvalidPath(_)
                    | Error::Conflict(_)
                    | Error::UnsupportedFeature(_)
            ) {
                return Err(error);
            }
            return Ok(recovery::error(
                "integrity_control",
                &Error::General(format!(
                    "Control outcome is unknown: {error}. Query this same bucket and job ID before explicitly retrying. Cancellation never removes already-published targets."
                )),
                true,
                formatter,
            ));
        }
    };
    let accepted = matches!(
        (operation, result.state.as_str()),
        ("resume", "running") | ("pause", "paused") | ("cancel", "cancelled")
    );
    let (code, status) = if accepted {
        (ExitCode::Success, "accepted")
    } else {
        job_outcome(&result)
    };
    recovery::output(
        "integrity_control",
        status,
        "Control state observed. Resume acceptance does not prove worker liveness; pause/cancel may still be pending. Published targets and source responsibilities remain retained.",
        &result,
        formatter,
    );
    if !formatter.is_json() {
        print_job(&result, formatter);
    }
    Ok(code)
}

fn print_job(job: &IntegrityJob, formatter: &Formatter) {
    formatter.println(&format!(
        "Job {}: {}; bucket incarnation: {}",
        formatter.sanitize_text(&job.id),
        formatter.sanitize_text(&job.state),
        formatter.sanitize_text(&job.bucket_incarnation)
    ));
    formatter.println("OBJECT  SELECTED VERSION  RESULT  EVIDENCE SOURCE  OBSERVED FINGERPRINT");
    for (source, result) in job.request.items.iter().zip(&job.results) {
        formatter.println(&format!(
            "{}  {}  {}  {}  {}",
            formatter.sanitize_text(&source.key),
            formatter.sanitize_text(
                source
                    .version_id
                    .as_deref()
                    .unwrap_or("current at execution")
            ),
            formatter.sanitize_text(&result.state),
            formatter.sanitize_text(result.evidence_source.as_deref().unwrap_or("unknown")),
            formatter.sanitize_text(result.source_fingerprint.as_deref().unwrap_or("unknown"))
        ));
        if let Some(detail) = &result.detail {
            formatter.println(&formatter.sanitize_text(detail));
        }
    }
}

#[cfg(test)]
mod tests;

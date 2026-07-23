//! mb command - Make bucket
//!
//! Creates a new bucket on the specified storage service.

use clap::Args;
use rc_core::{AliasManager, BucketObjectLockConfiguration, CreateBucketOptions};
use rc_s3::S3Client;
use serde::Serialize;

use super::exit_code_for_core_error;
use crate::exit_code::ExitCode;
use crate::output::{
    Formatter, OutputConfig, V3ErrorEnvelope, V3PartialErrorEnvelope, V3SuccessEnvelope,
};

const MB_AFTER_HELP: &str = "\
Examples:
  rc bucket create local/my-bucket
  rc mb local/my-bucket --ignore-existing
  rc bucket create local/archive --with-versioning --with-lock";

/// Create a bucket
#[derive(Args, Debug)]
#[command(after_help = MB_AFTER_HELP)]
pub struct MbArgs {
    /// Target path (alias/bucket)
    pub target: String,

    /// Ignore error if bucket already exists
    #[arg(short = 'p', long)]
    pub ignore_existing: bool,

    /// Region for the bucket (overrides alias default)
    #[arg(long)]
    pub region: Option<String>,

    /// Enable object locking on the bucket
    #[arg(long)]
    pub with_lock: bool,

    /// Enable versioning on the bucket
    #[arg(long)]
    pub with_versioning: bool,
}

#[derive(Debug, Clone, Serialize)]
struct BucketCreateRequested {
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    versioning_enabled: bool,
    object_lock_enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
struct BucketCreationData {
    operation: &'static str,
    bucket: String,
    outcome: &'static str,
    created: bool,
    requested: BucketCreateRequested,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region_semantics: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_versioning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_object_lock: Option<bool>,
    completed_stages: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_stage: Option<&'static str>,
}

impl BucketCreationData {
    fn new(bucket: &str, options: &CreateBucketOptions) -> Self {
        Self {
            operation: "create",
            bucket: bucket.to_string(),
            outcome: "failed",
            created: false,
            requested: BucketCreateRequested {
                region: options.region.clone(),
                versioning_enabled: options.versioning_enabled,
                object_lock_enabled: options.object_lock_enabled,
            },
            effective_region: None,
            region_semantics: options.region.as_ref().map(|_| "service_reported"),
            effective_versioning: None,
            effective_object_lock: None,
            completed_stages: Vec::new(),
            failed_stage: None,
        }
    }
}

#[derive(Debug)]
struct BucketCreationFailure {
    code: ExitCode,
    message: String,
    capability: Option<&'static str>,
    data: BucketCreationData,
}

trait BucketCreationStore {
    async fn bucket_exists(&self, bucket: &str) -> rc_core::Result<bool>;
    async fn create_bucket_with_options(
        &self,
        bucket: &str,
        options: &CreateBucketOptions,
    ) -> rc_core::Result<()>;
    async fn get_bucket_location(&self, bucket: &str) -> rc_core::Result<Option<String>>;
    async fn get_versioning(&self, bucket: &str) -> rc_core::Result<Option<bool>>;
    async fn set_versioning(&self, bucket: &str, enabled: bool) -> rc_core::Result<()>;
    async fn get_bucket_object_lock_configuration(
        &self,
        bucket: &str,
    ) -> rc_core::Result<Option<BucketObjectLockConfiguration>>;
}

impl BucketCreationStore for S3Client {
    async fn bucket_exists(&self, bucket: &str) -> rc_core::Result<bool> {
        rc_core::ObjectStore::bucket_exists(self, bucket).await
    }

    async fn create_bucket_with_options(
        &self,
        bucket: &str,
        options: &CreateBucketOptions,
    ) -> rc_core::Result<()> {
        rc_core::ObjectStore::create_bucket_with_options(self, bucket, options).await
    }

    async fn get_bucket_location(&self, bucket: &str) -> rc_core::Result<Option<String>> {
        rc_core::ObjectStore::get_bucket_location(self, bucket).await
    }

    async fn get_versioning(&self, bucket: &str) -> rc_core::Result<Option<bool>> {
        rc_core::ObjectStore::get_versioning(self, bucket).await
    }

    async fn set_versioning(&self, bucket: &str, enabled: bool) -> rc_core::Result<()> {
        rc_core::ObjectStore::set_versioning(self, bucket, enabled).await
    }

    async fn get_bucket_object_lock_configuration(
        &self,
        bucket: &str,
    ) -> rc_core::Result<Option<BucketObjectLockConfiguration>> {
        rc_core::ObjectStore::get_bucket_object_lock_configuration(self, bucket).await
    }
}

/// Execute the mb command
pub async fn execute(args: MbArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let options = match CreateBucketOptions::for_cli(
        args.region.clone(),
        args.with_versioning,
        args.with_lock,
    ) {
        Ok(options) => options,
        Err(error) => {
            return emit_preflight_failure(
                &formatter,
                exit_code_for_core_error(&error),
                error.to_string(),
                Some("create_bucket_options"),
            );
        }
    };

    // Parse the target path
    let (alias_name, bucket) = match parse_mb_path(&args.target) {
        Ok(parsed) => parsed,
        Err(e) => {
            return emit_preflight_failure(
                &formatter,
                ExitCode::UsageError,
                e,
                Some("bucket_path"),
            );
        }
    };

    // Load alias
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            return emit_preflight_failure(
                &formatter,
                ExitCode::GeneralError,
                format!("Failed to load aliases: {e}"),
                Some("alias_configuration"),
            );
        }
    };

    let alias = match alias_manager.get(&alias_name) {
        Ok(a) => a,
        Err(_) => {
            return emit_preflight_failure(
                &formatter,
                ExitCode::NotFound,
                format!("Alias '{alias_name}' not found"),
                Some("alias"),
            );
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return emit_preflight_failure(
                &formatter,
                ExitCode::NetworkError,
                format!("Failed to create S3 client: {e}"),
                Some("s3_client"),
            );
        }
    };

    match run_bucket_creation(&client, &bucket, &options, args.ignore_existing).await {
        Ok(data) => {
            if formatter.is_json() {
                formatter.json(&V3SuccessEnvelope::bucket_operations(data.clone()));
            } else {
                output_human_success(&formatter, &alias_name, &data);
            }
            ExitCode::Success
        }
        Err(failure) => {
            if formatter.is_json() {
                formatter.json_error(&V3PartialErrorEnvelope::bucket_operations(
                    failure.code,
                    &failure.message,
                    failure.capability,
                    failure.data,
                ));
            } else {
                output_human_failure(&formatter, &failure);
            }
            failure.code
        }
    }
}

async fn run_bucket_creation<S: BucketCreationStore>(
    store: &S,
    bucket: &str,
    options: &CreateBucketOptions,
    ignore_existing: bool,
) -> Result<BucketCreationData, BucketCreationFailure> {
    let mut data = BucketCreationData::new(bucket, options);
    if let Err(error) = options.validate() {
        return Err(stage_failure(
            data,
            "validate_options",
            error,
            Some("create_bucket_options"),
        ));
    }

    let mut existing = false;
    if ignore_existing {
        match store.bucket_exists(bucket).await {
            Ok(true) => {
                existing = true;
                data.completed_stages.push("existing_bucket_detected");
            }
            Ok(false) | Err(_) => {
                // Preserve the legacy `-p` behavior: creation remains authoritative when the
                // preliminary HEAD request is unsupported or inconclusive.
            }
        }
    }

    if !existing {
        match store.create_bucket_with_options(bucket, options).await {
            Ok(()) => {
                data.created = true;
                data.completed_stages.push("bucket_created");
            }
            Err(rc_core::Error::Conflict(_)) if ignore_existing => {
                existing = true;
                data.completed_stages.push("existing_bucket_detected");
            }
            Err(error) => {
                return Err(stage_failure(
                    data,
                    "create_bucket",
                    error,
                    Some("create_bucket"),
                ));
            }
        }
    }

    if let Some(requested_region) = &options.region {
        let reported = store.get_bucket_location(bucket).await.map_err(|error| {
            stage_failure(
                data.clone(),
                "verify_region",
                error,
                Some("get_bucket_location"),
            )
        })?;
        let effective = reported
            .filter(|region| !region.is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());
        data.effective_region = Some(effective.clone());
        if &effective != requested_region {
            return Err(stage_conflict(
                data,
                "verify_region",
                format!(
                    "Requested region '{requested_region}', but the service reports effective location '{effective}'"
                ),
                Some("get_bucket_location"),
            ));
        }
        data.completed_stages.push("region_verified");
    }

    if existing && options.object_lock_enabled {
        verify_object_lock(store, bucket, &mut data).await?;
    }

    if options.versioning_enabled {
        if !options.object_lock_enabled {
            store.set_versioning(bucket, true).await.map_err(|error| {
                stage_failure(
                    data.clone(),
                    "enable_versioning",
                    error,
                    Some("put_bucket_versioning"),
                )
            })?;
            data.completed_stages.push("versioning_enabled");
        }
        let versioning = store.get_versioning(bucket).await.map_err(|error| {
            stage_failure(
                data.clone(),
                "verify_versioning",
                error,
                Some("get_bucket_versioning"),
            )
        })?;
        data.effective_versioning = versioning;
        if versioning != Some(true) {
            return Err(stage_conflict(
                data,
                "verify_versioning",
                "Bucket versioning was not enabled after creation".to_string(),
                Some("get_bucket_versioning"),
            ));
        }
        data.completed_stages.push("versioning_verified");
    }

    if !existing && options.object_lock_enabled {
        verify_object_lock(store, bucket, &mut data).await?;
    }

    data.outcome = if data.created { "created" } else { "existing" };
    Ok(data)
}

async fn verify_object_lock<S: BucketCreationStore>(
    store: &S,
    bucket: &str,
    data: &mut BucketCreationData,
) -> Result<(), BucketCreationFailure> {
    let configuration = store
        .get_bucket_object_lock_configuration(bucket)
        .await
        .map_err(|error| {
            stage_failure(
                data.clone(),
                "verify_object_lock",
                error,
                Some("get_object_lock_configuration"),
            )
        })?;
    let enabled = configuration.is_some_and(|configuration| configuration.enabled);
    data.effective_object_lock = Some(enabled);
    if !enabled {
        return Err(stage_conflict(
            data.clone(),
            "verify_object_lock",
            "Object Lock is not enabled; it cannot be enabled retroactively".to_string(),
            Some("get_object_lock_configuration"),
        ));
    }
    data.completed_stages.push("object_lock_verified");
    Ok(())
}

fn stage_failure(
    mut data: BucketCreationData,
    stage: &'static str,
    error: rc_core::Error,
    capability: Option<&'static str>,
) -> BucketCreationFailure {
    let code = exit_code_for_core_error(&error);
    data.failed_stage = Some(stage);
    data.outcome = if data.created || data.completed_stages.contains(&"versioning_enabled") {
        "partial"
    } else {
        "failed"
    };
    BucketCreationFailure {
        code,
        message: error.to_string(),
        capability,
        data,
    }
}

fn stage_conflict(
    data: BucketCreationData,
    stage: &'static str,
    message: String,
    capability: Option<&'static str>,
) -> BucketCreationFailure {
    stage_failure(data, stage, rc_core::Error::Conflict(message), capability)
}

fn emit_preflight_failure(
    formatter: &Formatter,
    code: ExitCode,
    message: String,
    capability: Option<&str>,
) -> ExitCode {
    if formatter.is_json() {
        formatter.json_error(&V3ErrorEnvelope::bucket_operations(
            code, message, capability,
        ));
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn output_human_success(formatter: &Formatter, alias_name: &str, data: &BucketCreationData) {
    let target = format!("{alias_name}/{}", data.bucket);
    if data.created {
        formatter.success(&format!("Bucket '{target}' created successfully."));
    } else {
        formatter.success(&format!(
            "Bucket '{target}' already exists and matches the request."
        ));
    }
    if let (Some(requested), Some(effective)) = (&data.requested.region, &data.effective_region) {
        formatter.println(&format!(
            "Requested region: {requested}; service-reported location: {effective}. RustFS beta.10 reports its server-global location."
        ));
    }
    if data.effective_versioning == Some(true) {
        formatter.println("Versioning: enabled");
    }
    if data.effective_object_lock == Some(true) {
        formatter.println("Object Lock: enabled (verified)");
    }
}

fn output_human_failure(formatter: &Formatter, failure: &BucketCreationFailure) {
    formatter.error_with_code(failure.code, &failure.message);
    if !failure.data.completed_stages.is_empty() {
        formatter.error(&format!(
            "Completed stages: {}",
            failure.data.completed_stages.join(", ")
        ));
    }
    if let Some(stage) = failure.data.failed_stage {
        formatter.error(&format!("Failed stage: {stage}"));
    }
    if failure.data.created {
        formatter
            .error("The bucket was created and was not removed; fix the reported state and retry.");
    }
}

/// Parse mb target path into (alias, bucket)
fn parse_mb_path(path: &str) -> Result<(String, String), String> {
    let path = path.trim_end_matches('/');

    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(2, '/').collect();

    if parts.len() != 2 {
        return Err(format!(
            "Invalid path format: '{path}'. Expected: alias/bucket"
        ));
    }

    let alias = parts[0].to_string();
    let bucket = parts[1].to_string();

    if bucket.is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }

    // Basic bucket name validation
    if bucket.len() < 3 || bucket.len() > 63 {
        return Err("Bucket name must be between 3 and 63 characters".to_string());
    }

    Ok((alias, bucket))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct FakeBucketStore {
        exists: bool,
        deny_create: bool,
        versioning: Option<bool>,
        lock: Option<rc_core::BucketObjectLockConfiguration>,
        location: Option<String>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeBucketStore {
        fn new() -> Self {
            Self {
                exists: false,
                deny_create: false,
                versioning: Some(true),
                lock: Some(rc_core::BucketObjectLockConfiguration {
                    enabled: true,
                    default_retention: None,
                }),
                location: Some("us-east-1".to_string()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("call log lock").clone()
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().expect("call log lock").push(call);
        }
    }

    impl BucketCreationStore for FakeBucketStore {
        async fn bucket_exists(&self, _bucket: &str) -> rc_core::Result<bool> {
            self.record("bucket_exists");
            Ok(self.exists)
        }

        async fn create_bucket_with_options(
            &self,
            _bucket: &str,
            _options: &rc_core::CreateBucketOptions,
        ) -> rc_core::Result<()> {
            self.record("create_bucket");
            if self.deny_create {
                return Err(rc_core::Error::Auth("Access denied".to_string()));
            }
            Ok(())
        }

        async fn get_bucket_location(&self, _bucket: &str) -> rc_core::Result<Option<String>> {
            self.record("get_bucket_location");
            Ok(self.location.clone())
        }

        async fn get_versioning(&self, _bucket: &str) -> rc_core::Result<Option<bool>> {
            self.record("get_versioning");
            Ok(self.versioning)
        }

        async fn set_versioning(&self, _bucket: &str, _enabled: bool) -> rc_core::Result<()> {
            self.record("set_versioning");
            Ok(())
        }

        async fn get_bucket_object_lock_configuration(
            &self,
            _bucket: &str,
        ) -> rc_core::Result<Option<rc_core::BucketObjectLockConfiguration>> {
            self.record("get_bucket_object_lock_configuration");
            Ok(self.lock.clone())
        }
    }

    #[tokio::test]
    async fn versioning_is_enabled_and_verified_after_creation() {
        let store = FakeBucketStore::new();
        let options = rc_core::CreateBucketOptions::for_cli(None, true, false)
            .expect("valid versioning options");

        let result = run_bucket_creation(&store, "bucket", &options, false)
            .await
            .expect("versioned creation succeeds");

        assert!(result.created);
        assert_eq!(
            store.calls(),
            vec!["create_bucket", "set_versioning", "get_versioning"]
        );
        assert!(result.completed_stages.contains(&"versioning_verified"));
    }

    #[tokio::test]
    async fn explicit_region_is_compared_with_the_service_reported_location() {
        let mut store = FakeBucketStore::new();
        store.location = Some("eu-west-1".to_string());
        let options =
            rc_core::CreateBucketOptions::for_cli(Some("eu-west-1".to_string()), false, false)
                .expect("valid region options");

        let result = run_bucket_creation(&store, "bucket", &options, false)
            .await
            .expect("matching service-reported location succeeds");

        assert_eq!(result.effective_region.as_deref(), Some("eu-west-1"));
        assert_eq!(store.calls(), vec!["create_bucket", "get_bucket_location"]);
    }

    #[tokio::test]
    async fn region_mismatch_is_a_partial_conflict_after_bucket_creation() {
        let store = FakeBucketStore::new();
        let options =
            rc_core::CreateBucketOptions::for_cli(Some("eu-west-1".to_string()), false, false)
                .expect("valid region options");

        let failure = run_bucket_creation(&store, "bucket", &options, false)
            .await
            .expect_err("a different service-reported location must fail");

        assert_eq!(failure.code, ExitCode::Conflict);
        assert_eq!(failure.data.outcome, "partial");
        assert_eq!(failure.data.effective_region.as_deref(), Some("us-east-1"));
        assert_eq!(failure.data.failed_stage, Some("verify_region"));
    }

    #[tokio::test]
    async fn ignore_existing_rejects_a_service_reported_region_mismatch() {
        let mut store = FakeBucketStore::new();
        store.exists = true;
        let options =
            rc_core::CreateBucketOptions::for_cli(Some("eu-west-1".to_string()), false, false)
                .expect("valid region options");

        let failure = run_bucket_creation(&store, "bucket", &options, true)
            .await
            .expect_err("an existing bucket in a different location must fail");

        assert_eq!(failure.code, ExitCode::Conflict);
        assert_eq!(failure.data.outcome, "failed");
        assert!(!failure.data.created);
        assert_eq!(store.calls(), vec!["bucket_exists", "get_bucket_location"]);
    }

    #[tokio::test]
    async fn ignore_existing_may_enable_and_verify_versioning() {
        let mut store = FakeBucketStore::new();
        store.exists = true;
        let options = rc_core::CreateBucketOptions::for_cli(None, true, false)
            .expect("valid versioning options");

        let result = run_bucket_creation(&store, "bucket", &options, true)
            .await
            .expect("existing bucket versioning reconciliation succeeds");

        assert!(!result.created);
        assert_eq!(result.outcome, "existing");
        assert_eq!(
            store.calls(),
            vec!["bucket_exists", "set_versioning", "get_versioning"]
        );
    }

    #[tokio::test]
    async fn object_lock_uses_creation_time_request_and_verifies_both_states() {
        let store = FakeBucketStore::new();
        let options = rc_core::CreateBucketOptions::for_cli(None, false, true)
            .expect("valid Object Lock options");

        let result = run_bucket_creation(&store, "bucket", &options, false)
            .await
            .expect("locked creation succeeds");

        assert_eq!(
            store.calls(),
            vec![
                "create_bucket",
                "get_versioning",
                "get_bucket_object_lock_configuration"
            ]
        );
        assert_eq!(result.effective_object_lock, Some(true));
    }

    #[tokio::test]
    async fn existing_unlocked_bucket_is_a_conflict_without_retroactive_mutation() {
        let mut store = FakeBucketStore::new();
        store.exists = true;
        store.lock = None;
        let options = rc_core::CreateBucketOptions::for_cli(None, false, true)
            .expect("valid Object Lock options");

        let failure = run_bucket_creation(&store, "bucket", &options, true)
            .await
            .expect_err("existing unlocked bucket must fail");

        assert_eq!(failure.code, ExitCode::Conflict);
        assert_eq!(failure.data.failed_stage, Some("verify_object_lock"));
        assert_eq!(
            store.calls(),
            vec!["bucket_exists", "get_bucket_object_lock_configuration"]
        );
    }

    #[tokio::test]
    async fn versioning_verification_mismatch_reports_the_failed_partial_stage() {
        let mut store = FakeBucketStore::new();
        store.versioning = Some(false);
        let options = rc_core::CreateBucketOptions::for_cli(None, true, false)
            .expect("valid versioning options");

        let failure = run_bucket_creation(&store, "bucket", &options, false)
            .await
            .expect_err("verification mismatch must fail");

        assert_eq!(failure.code, ExitCode::Conflict);
        assert_eq!(failure.data.outcome, "partial");
        assert_eq!(failure.data.failed_stage, Some("verify_versioning"));
        assert!(failure.data.completed_stages.contains(&"bucket_created"));
    }

    #[tokio::test]
    async fn invalid_option_state_fails_before_any_store_request() {
        let store = FakeBucketStore::new();
        let invalid = rc_core::CreateBucketOptions {
            region: None,
            versioning_enabled: false,
            object_lock_enabled: true,
        };

        let failure = run_bucket_creation(&store, "bucket", &invalid, false)
            .await
            .expect_err("invalid options must fail before the store is called");

        assert_eq!(failure.code, ExitCode::UsageError);
        assert_eq!(failure.data.failed_stage, Some("validate_options"));
        assert!(store.calls().is_empty());
    }

    #[tokio::test]
    async fn create_access_denial_keeps_the_auth_exit_code_distinct() {
        let mut store = FakeBucketStore::new();
        store.deny_create = true;

        let failure = run_bucket_creation(
            &store,
            "bucket",
            &rc_core::CreateBucketOptions::default(),
            false,
        )
        .await
        .expect_err("access denial must fail");

        assert_eq!(failure.code, ExitCode::AuthError);
        assert_eq!(failure.data.outcome, "failed");
        assert_eq!(store.calls(), vec!["create_bucket"]);
    }

    #[test]
    fn test_parse_mb_path_valid() {
        let (alias, bucket) = parse_mb_path("myalias/mybucket").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_mb_path_trailing_slash() {
        let (alias, bucket) = parse_mb_path("myalias/mybucket/").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_mb_path_no_bucket() {
        assert!(parse_mb_path("myalias").is_err());
    }

    #[test]
    fn test_parse_mb_path_empty_bucket() {
        assert!(parse_mb_path("myalias/").is_err());
    }

    #[test]
    fn test_parse_mb_path_short_bucket() {
        assert!(parse_mb_path("myalias/ab").is_err());
    }

    #[test]
    fn test_parse_mb_path_empty() {
        assert!(parse_mb_path("").is_err());
    }
}

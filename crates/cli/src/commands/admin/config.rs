//! RustFS server configuration commands with client-side preflight planning.

use clap::{Args, Subcommand};
use rc_core::Error;
use rc_core::admin::{
    ConfigApi, ConfigDiff, ConfigDocument, ConfigHistoryEntry, ModuleSwitches,
    config_document_fields, config_import_diff, config_mutation_diff, redact_config_document,
    validate_config_directive, validate_config_import,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
#[command(disable_help_subcommand = true)]
pub enum ConfigCommands {
    /// Read a subsystem or target configuration
    Get(GetArgs),
    /// Set one or more keys after showing a preflight diff
    Set(SetArgs),
    /// Delete a target or selected keys after showing a preflight diff
    Delete(DeleteArgs),
    /// Show server-provided configuration help
    Help(HelpArgs),
    /// List configuration mutation history
    History(HistoryArgs),
    /// Replace configuration from a history entry
    Restore(RestoreArgs),
    /// Export the full server configuration to a file
    Export(ExportArgs),
    /// Replace the full server configuration from a file
    Import(ImportArgs),
    /// Inspect or update event notification and audit module switches
    #[command(name = "module-switch", subcommand)]
    ModuleSwitch(ModuleSwitchCommands),
}

#[derive(Args, Debug)]
pub struct GetArgs {
    pub alias: String,
    pub selector: String,
}

#[derive(Args, Debug)]
pub struct SetArgs {
    pub alias: String,
    pub scope: String,
    #[arg(required = true)]
    pub assignments: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    pub alias: String,
    pub scope: String,
    pub keys: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct HelpArgs {
    pub alias: String,
    pub subsystem: Option<String>,
    pub key: Option<String>,
    #[arg(long)]
    pub env: bool,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    pub alias: String,
    #[arg(long, default_value_t = 10, value_parser = parse_positive_usize)]
    pub count: usize,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    pub alias: String,
    pub restore_id: String,
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm replacement of the complete server configuration
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    pub alias: String,
    #[arg(long)]
    pub file: PathBuf,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    pub alias: String,
    #[arg(long)]
    pub file: PathBuf,
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm replacement of the complete server configuration
    #[arg(long)]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum ModuleSwitchCommands {
    Get(ModuleSwitchGetArgs),
    Set(ModuleSwitchSetArgs),
}

#[derive(Args, Debug)]
pub struct ModuleSwitchGetArgs {
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct ModuleSwitchSetArgs {
    pub alias: String,
    #[arg(long, value_parser = parse_on_off)]
    pub notify: Option<bool>,
    #[arg(long, value_parser = parse_on_off)]
    pub audit: Option<bool>,
    #[arg(long)]
    pub dry_run: bool,
}

fn parse_on_off(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => Err("expected on or off".to_string()),
    }
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| "expected a positive integer".to_string())?;
    if parsed == 0 {
        return Err("expected a positive integer".to_string());
    }
    Ok(parsed)
}

#[derive(Debug, Serialize)]
struct ConfigOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: AdminOperationsData<T>,
}

#[derive(Debug, Serialize)]
struct AdminOperationsData<T> {
    operations: Vec<AdminOperation<T>>,
}

#[derive(Debug, Serialize)]
struct AdminOperation<T> {
    operation: &'static str,
    resource: &'static str,
    state: &'static str,
    operation_id: Option<String>,
    changed: bool,
    result: T,
}

#[derive(Debug, Serialize)]
struct OperationData<T> {
    operation: &'static str,
    result: T,
}

#[derive(Debug, Serialize)]
struct MutationData {
    operation: &'static str,
    dry_run: bool,
    applied_dynamically: Option<bool>,
    diff: ConfigDiff,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ExportData {
    operation: &'static str,
    path: String,
    redacted: bool,
}

#[derive(Debug, Serialize)]
struct HistoryOutputEntry {
    restore_id: String,
    create_time: String,
    data: Option<String>,
}

#[derive(Debug, Serialize)]
struct SwitchDiff {
    notify_before: bool,
    notify_after: bool,
    audit_before: bool,
    audit_after: bool,
}

#[derive(Debug, Serialize)]
struct ConfigErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: ConfigErrorDetail,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ConfigErrorDetail {
    Unsupported {
        #[serde(rename = "type")]
        error_type: &'static str,
        message: String,
        retryable: bool,
        capability: &'static str,
        server: Option<&'static str>,
        suggestion: Option<&'static str>,
    },
    Standard {
        #[serde(rename = "type")]
        error_type: &'static str,
        message: String,
        retryable: bool,
        suggestion: Option<&'static str>,
    },
}

const RESTORE_SAFETY_WARNING: &str = "RustFS rebuilds the complete configuration from this history document; unrelated current values are removed. This unsafe server behavior is tracked by rustfs/backlog#1398.";

pub async fn execute(command: ConfigCommands, formatter: &Formatter) -> ExitCode {
    let alias = command.alias();
    let client = match get_admin_client(alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_with_api(command, &client, formatter).await
}

impl ConfigCommands {
    fn alias(&self) -> &str {
        match self {
            Self::Get(args) => &args.alias,
            Self::Set(args) => &args.alias,
            Self::Delete(args) => &args.alias,
            Self::Help(args) => &args.alias,
            Self::History(args) => &args.alias,
            Self::Restore(args) => &args.alias,
            Self::Export(args) => &args.alias,
            Self::Import(args) => &args.alias,
            Self::ModuleSwitch(ModuleSwitchCommands::Get(args)) => &args.alias,
            Self::ModuleSwitch(ModuleSwitchCommands::Set(args)) => &args.alias,
        }
    }
}

async fn execute_with_api(
    command: ConfigCommands,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> ExitCode {
    let result = match command {
        ConfigCommands::Get(args) => get(args, api, formatter).await,
        ConfigCommands::Set(args) => set(args, api, formatter).await,
        ConfigCommands::Delete(args) => delete(args, api, formatter).await,
        ConfigCommands::Help(args) => help(args, api, formatter).await,
        ConfigCommands::History(args) => history(args, api, formatter).await,
        ConfigCommands::Restore(args) => restore(args, api, formatter).await,
        ConfigCommands::Export(args) => export(args, api, formatter).await,
        ConfigCommands::Import(args) => import(args, api, formatter).await,
        ConfigCommands::ModuleSwitch(command) => module_switch(command, api, formatter).await,
    };
    match result {
        Ok(()) => ExitCode::Success,
        Err(error) => emit_error(&error, formatter),
    }
}

async fn get(args: GetArgs, api: &dyn ConfigApi, formatter: &Formatter) -> rc_core::Result<()> {
    validate_selector(&args.selector)?;
    let document = api.get_config(&args.selector).await?;
    let content = redact_config_document(&document.content);
    output(
        formatter,
        false,
        OperationData {
            operation: "config.get",
            result: &content,
        },
        || formatter.println(&content),
    );
    Ok(())
}

async fn set(args: SetArgs, api: &dyn ConfigApi, formatter: &Formatter) -> rc_core::Result<()> {
    let directive = format!("{} {}", args.scope, args.assignments.join(" "));
    validate_config_directive(&directive, false)?;
    validate_server_keys(api, &args.scope, &args.assignments).await?;
    let current = api.get_config(&args.scope).await?;
    let diff = config_mutation_diff(&current.content, &directive, false)?;
    let applied = if args.dry_run {
        None
    } else {
        Some(api.set_config(&directive).await?.applied_dynamically)
    };
    emit_mutation("config.set", args.dry_run, applied, diff, None, formatter);
    Ok(())
}

async fn delete(
    args: DeleteArgs,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    let directive = if args.keys.is_empty() {
        args.scope.clone()
    } else {
        format!("{} {}", args.scope, args.keys.join(" "))
    };
    validate_config_directive(&directive, true)?;
    validate_server_keys(api, &args.scope, &args.keys).await?;
    let current = api.get_config(&args.scope).await?;
    let diff = config_mutation_diff(&current.content, &directive, true)?;
    let applied = if args.dry_run {
        None
    } else {
        Some(api.delete_config(&directive).await?.applied_dynamically)
    };
    emit_mutation(
        "config.delete",
        args.dry_run,
        applied,
        diff,
        None,
        formatter,
    );
    Ok(())
}

async fn help(args: HelpArgs, api: &dyn ConfigApi, formatter: &Formatter) -> rc_core::Result<()> {
    let result = api
        .config_help(args.subsystem.as_deref(), args.key.as_deref(), args.env)
        .await?;
    output(
        formatter,
        false,
        OperationData {
            operation: "config.help",
            result: &result,
        },
        || {
            formatter.println(&format!("{}: {}", result.subsystem, result.description));
            for key in &result.keys {
                formatter.println(&format!(
                    "{} ({}) - {}",
                    key.key, key.value_type, key.description
                ));
            }
        },
    );
    Ok(())
}

async fn history(
    args: HistoryArgs,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    let entries = api
        .config_history(args.count)
        .await?
        .into_iter()
        .map(redact_history_entry)
        .collect::<Vec<_>>();
    output(
        formatter,
        false,
        OperationData {
            operation: "config.history",
            result: &entries,
        },
        || {
            for entry in &entries {
                formatter.println(&format!("{} {}", entry.restore_id, entry.create_time));
                if let Some(data) = &entry.data {
                    formatter.println(data);
                }
            }
        },
    );
    Ok(())
}

async fn restore(
    args: RestoreArgs,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    validate_restore_id(&args.restore_id)?;
    if !args.dry_run && !args.yes {
        return Err(Error::Config(
            "Restore replaces the complete server configuration; pass --yes to confirm".to_string(),
        ));
    }
    let history = api.config_history(1000).await?;
    let document = history
        .iter()
        .find(|entry| entry.restore_id == args.restore_id)
        .and_then(|entry| entry.data.as_deref())
        .ok_or_else(|| {
            Error::NotFound("Configuration history entry has no restorable data".to_string())
        })?;
    validate_config_directive(document, false)?;
    let current = api.get_full_config().await?;
    let diff = config_import_diff(&current.content, document)?;
    if !args.dry_run {
        api.restore_config(&args.restore_id).await?;
    }
    emit_mutation(
        "config.restore",
        args.dry_run,
        None,
        diff,
        Some(RESTORE_SAFETY_WARNING),
        formatter,
    );
    Ok(())
}

async fn export(
    args: ExportArgs,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    let document = api.get_full_config().await?;
    let content = redact_config_document(&document.content);
    write_private_file(&args.file, content.as_bytes())?;
    let data = ExportData {
        operation: "config.export",
        path: args.file.display().to_string(),
        redacted: true,
    };
    output(formatter, false, data, || {
        formatter.success(&format!(
            "Configuration exported to {}",
            args.file.display()
        ));
    });
    Ok(())
}

async fn import(
    args: ImportArgs,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    if !args.dry_run && !args.yes {
        return Err(Error::Config(
            "Import replaces the complete server configuration; pass --yes to confirm".to_string(),
        ));
    }
    let content = std::fs::read_to_string(&args.file).map_err(|error| {
        Error::Io(std::io::Error::new(
            error.kind(),
            format!("failed to read configuration import: {error}"),
        ))
    })?;
    validate_config_directive(&content, false)?;
    validate_config_import(&content)?;
    validate_server_document(api, &content).await?;
    let current = api.get_full_config().await?;
    let diff = config_import_diff(&current.content, &content)?;
    if !args.dry_run {
        api.import_config(&ConfigDocument { content }).await?;
    }
    emit_mutation(
        "config.import",
        args.dry_run,
        None,
        diff,
        Some("Import replaces the complete server configuration."),
        formatter,
    );
    Ok(())
}

async fn module_switch(
    command: ModuleSwitchCommands,
    api: &dyn ConfigApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    match command {
        ModuleSwitchCommands::Get(_) => {
            let switches = api.get_module_switches().await?;
            output(
                formatter,
                false,
                OperationData {
                    operation: "config.module-switch.get",
                    result: &switches,
                },
                || print_switches(&switches, formatter),
            );
        }
        ModuleSwitchCommands::Set(args) => {
            if args.notify.is_none() && args.audit.is_none() {
                return Err(Error::Config(
                    "At least one of --notify or --audit is required".to_string(),
                ));
            }
            let current = api.get_module_switches().await?;
            let requested = requested_switches(&current, args.notify, args.audit);
            let diff = SwitchDiff {
                notify_before: current.notify_enabled,
                notify_after: requested.notify_enabled,
                audit_before: current.audit_enabled,
                audit_after: requested.audit_enabled,
            };
            if !args.dry_run {
                api.set_module_switches(&requested).await?;
            }
            output(
                formatter,
                !args.dry_run
                    && (diff.notify_before != diff.notify_after
                        || diff.audit_before != diff.audit_after),
                OperationData {
                    operation: "config.module-switch.set",
                    result: &diff,
                },
                || {
                    formatter.println(&format!(
                        "notify: {} -> {}; audit: {} -> {}{}",
                        diff.notify_before,
                        diff.notify_after,
                        diff.audit_before,
                        diff.audit_after,
                        if args.dry_run { " (dry-run)" } else { "" }
                    ));
                },
            );
        }
    }
    Ok(())
}

fn output<T: Serialize>(formatter: &Formatter, changed: bool, data: T, human: impl FnOnce()) {
    if formatter.is_json() {
        formatter.json(&ConfigOutput {
            schema_version: 3,
            output_type: "admin_operations",
            status: "success",
            data: AdminOperationsData {
                operations: vec![AdminOperation {
                    operation: "server-config",
                    resource: "rustfs-server-configuration",
                    state: "succeeded",
                    operation_id: None,
                    changed,
                    result: data,
                }],
            },
        });
    } else {
        human();
    }
}

fn emit_mutation(
    operation: &'static str,
    dry_run: bool,
    applied_dynamically: Option<bool>,
    diff: ConfigDiff,
    warning: Option<&'static str>,
    formatter: &Formatter,
) {
    let data = MutationData {
        operation,
        dry_run,
        applied_dynamically,
        diff,
        warning,
    };
    let changed = !dry_run && !data.diff.changes.is_empty();
    output(formatter, changed, &data, || {
        if let Some(warning) = warning {
            formatter.warning(warning);
        }
        formatter.println(if dry_run {
            "Preflight diff:"
        } else {
            "Applied diff:"
        });
        for change in &data.diff.changes {
            formatter.println(&format!(
                "{} {}: {} -> {}",
                change.scope,
                change.key,
                change.before.as_deref().unwrap_or("<unset>"),
                change.after.as_deref().unwrap_or("<unset>")
            ));
        }
        if data.diff.changes.is_empty() {
            formatter.println("No changes.");
        }
    });
}

fn redact_history_entry(entry: ConfigHistoryEntry) -> HistoryOutputEntry {
    HistoryOutputEntry {
        restore_id: entry.restore_id,
        create_time: entry.create_time,
        data: entry.data.map(|data| redact_config_document(&data)),
    }
}

fn validate_selector(selector: &str) -> rc_core::Result<()> {
    validate_config_directive(selector, true)
}

fn validate_restore_id(restore_id: &str) -> rc_core::Result<()> {
    if restore_id.is_empty()
        || !restore_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(Error::Config(
            "Invalid configuration restore ID".to_string(),
        ));
    }
    Ok(())
}

async fn validate_server_keys(
    api: &dyn ConfigApi,
    scope: &str,
    tokens: &[String],
) -> rc_core::Result<()> {
    let subsystem = scope.split_once(':').map_or(scope, |(value, _)| value);
    if tokens.is_empty() {
        api.config_help(Some(subsystem), None, false).await?;
        return Ok(());
    }
    for token in tokens {
        let key = token.split_once('=').map_or(token.as_str(), |(key, _)| key);
        api.config_help(Some(subsystem), Some(key), false).await?;
    }
    Ok(())
}

async fn validate_server_document(api: &dyn ConfigApi, document: &str) -> rc_core::Result<()> {
    for (subsystem, key) in config_document_fields(document)? {
        api.config_help(Some(&subsystem), Some(&key), false).await?;
    }
    Ok(())
}

fn print_switches(switches: &ModuleSwitches, formatter: &Formatter) {
    formatter.println(&format!(
        "notify: {} (source: {}, persisted: {})",
        switches.notify_enabled, switches.notify_source, switches.persisted_notify_enabled
    ));
    formatter.println(&format!(
        "audit: {} (source: {}, persisted: {})",
        switches.audit_enabled, switches.audit_source, switches.persisted_audit_enabled
    ));
}

fn requested_switches(
    current: &ModuleSwitches,
    notify: Option<bool>,
    audit: Option<bool>,
) -> ModuleSwitches {
    let mut requested = current.clone();
    requested.notify_enabled = notify.unwrap_or(current.persisted_notify_enabled);
    requested.audit_enabled = audit.unwrap_or(current.persisted_audit_enabled);
    requested
}

fn write_private_file(path: &Path, contents: &[u8]) -> rc_core::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(contents)?;
    Ok(())
}

fn emit_error(error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    if formatter.is_json() {
        let detail = if matches!(error, Error::UnsupportedFeature(_)) {
            ConfigErrorDetail::Unsupported {
                error_type: "unsupported_feature",
                message: error.to_string(),
                retryable: false,
                capability: "admin.server-configuration",
                server: Some("rustfs"),
                suggestion: Some("Verify the RustFS server version and advertised capabilities."),
            }
        } else {
            let (error_type, retryable, suggestion) = config_error_metadata(code);
            ConfigErrorDetail::Standard {
                error_type,
                message: error.to_string(),
                retryable,
                suggestion,
            }
        };
        formatter.json_error(&ConfigErrorOutput {
            schema_version: 3,
            output_type: "admin_operations",
            status: "error",
            error: detail,
        });
    } else {
        formatter.error_with_code(code, &error.to_string());
    }
    code
}

const fn config_error_metadata(code: ExitCode) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Review the command arguments and server configuration help."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify the server endpoint and retry."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify the alias credentials and ConfigUpdate permission."),
        ),
        ExitCode::NotFound => (
            "not_found",
            false,
            Some("Verify the requested subsystem, target, or history identifier."),
        ),
        ExitCode::Conflict => (
            "conflict",
            false,
            Some("Refresh the configuration and retry the preflight."),
        ),
        ExitCode::Interrupted => ("interrupted", true, Some("Retry the operation.")),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestApi {
        mutations: AtomicUsize,
    }

    #[async_trait]
    impl ConfigApi for TestApi {
        async fn get_config(&self, _selector: &str) -> rc_core::Result<ConfigDocument> {
            Ok(ConfigDocument {
                content: "scanner speed=default cycle=10".to_string(),
            })
        }

        async fn get_full_config(&self) -> rc_core::Result<ConfigDocument> {
            Ok(ConfigDocument {
                content:
                    "scanner speed=default cycle=10\nidentity_openid client_secret=super-secret"
                        .to_string(),
            })
        }

        async fn set_config(
            &self,
            _directive: &str,
        ) -> rc_core::Result<rc_core::admin::ConfigMutationResult> {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(rc_core::admin::ConfigMutationResult {
                applied_dynamically: true,
            })
        }

        async fn delete_config(
            &self,
            _directive: &str,
        ) -> rc_core::Result<rc_core::admin::ConfigMutationResult> {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(rc_core::admin::ConfigMutationResult {
                applied_dynamically: false,
            })
        }

        async fn config_help(
            &self,
            _subsystem: Option<&str>,
            _key: Option<&str>,
            _env_only: bool,
        ) -> rc_core::Result<rc_core::admin::ConfigHelp> {
            Ok(rc_core::admin::ConfigHelp {
                subsystem: "scanner".to_string(),
                description: "scanner".to_string(),
                multiple_targets: false,
                keys: Vec::new(),
            })
        }

        async fn config_history(&self, _count: usize) -> rc_core::Result<Vec<ConfigHistoryEntry>> {
            Ok(vec![ConfigHistoryEntry {
                restore_id: "restore-1".to_string(),
                create_time: "2026-07-21T00:00:00Z".to_string(),
                data: Some("scanner speed=fast".to_string()),
            }])
        }

        async fn restore_config(&self, _restore_id: &str) -> rc_core::Result<()> {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn import_config(&self, _document: &ConfigDocument) -> rc_core::Result<()> {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn get_module_switches(&self) -> rc_core::Result<ModuleSwitches> {
            Ok(ModuleSwitches {
                notify_enabled: false,
                audit_enabled: false,
                persisted_notify_enabled: false,
                persisted_audit_enabled: false,
                notify_source: "console".to_string(),
                audit_source: "console".to_string(),
            })
        }

        async fn set_module_switches(
            &self,
            switches: &ModuleSwitches,
        ) -> rc_core::Result<ModuleSwitches> {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(switches.clone())
        }
    }

    fn test_formatter() -> Formatter {
        Formatter::new(crate::output::OutputConfig {
            json: false,
            no_color: true,
            no_progress: true,
            quiet: true,
        })
    }

    #[tokio::test]
    async fn set_dry_run_does_not_mutate() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let code = execute_with_api(
            ConfigCommands::Set(SetArgs {
                alias: "local".to_string(),
                scope: "scanner".to_string(),
                assignments: vec!["speed=fast".to_string()],
                dry_run: true,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn set_success_mutates_once() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let code = execute_with_api(
            ConfigCommands::Set(SetArgs {
                alias: "local".to_string(),
                scope: "scanner".to_string(),
                assignments: vec!["speed=fast".to_string()],
                dry_run: false,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_set_returns_usage_without_mutation() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let code = execute_with_api(
            ConfigCommands::Set(SetArgs {
                alias: "local".to_string(),
                scope: "scanner".to_string(),
                assignments: vec!["speed".to_string()],
                dry_run: false,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::UsageError);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn delete_success_and_invalid_input_have_distinct_exit_codes() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let success = execute_with_api(
            ConfigCommands::Delete(DeleteArgs {
                alias: "local".to_string(),
                scope: "scanner".to_string(),
                keys: vec!["speed".to_string()],
                dry_run: false,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        let invalid = execute_with_api(
            ConfigCommands::Delete(DeleteArgs {
                alias: "local".to_string(),
                scope: ":bad".to_string(),
                keys: Vec::new(),
                dry_run: false,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(success, ExitCode::Success);
        assert_eq!(invalid, ExitCode::UsageError);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn confirmed_restore_mutates_once() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let code = execute_with_api(
            ConfigCommands::Restore(RestoreArgs {
                alias: "local".to_string(),
                restore_id: "restore-1".to_string(),
                dry_run: false,
                yes: true,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn import_requires_confirmation_before_file_access() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let code = execute_with_api(
            ConfigCommands::Import(ImportArgs {
                alias: "local".to_string(),
                file: PathBuf::from("missing-secret-config"),
                dry_run: false,
                yes: false,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::UsageError);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn confirmed_import_mutates_once() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join("config.txt");
        std::fs::write(&path, "scanner speed=fast").expect("write import file");
        let code = execute_with_api(
            ConfigCommands::Import(ImportArgs {
                alias: "local".to_string(),
                file: path,
                dry_run: false,
                yes: true,
            }),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn module_switch_dry_run_and_apply_have_distinct_mutation_counts() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let dry_run = execute_with_api(
            ConfigCommands::ModuleSwitch(ModuleSwitchCommands::Set(ModuleSwitchSetArgs {
                alias: "local".to_string(),
                notify: Some(true),
                audit: None,
                dry_run: true,
            })),
            &api,
            &test_formatter(),
        )
        .await;
        let applied = execute_with_api(
            ConfigCommands::ModuleSwitch(ModuleSwitchCommands::Set(ModuleSwitchSetArgs {
                alias: "local".to_string(),
                notify: Some(true),
                audit: None,
                dry_run: false,
            })),
            &api,
            &test_formatter(),
        )
        .await;
        assert_eq!(dry_run, ExitCode::Success);
        assert_eq!(applied, ExitCode::Success);
        assert_eq!(api.mutations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn export_refuses_to_overwrite_an_existing_file() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join("config.txt");
        std::fs::write(&path, "existing").expect("create existing file");

        let error = write_private_file(&path, b"replacement")
            .expect_err("export must not overwrite an existing file");
        assert_eq!(error.exit_code(), ExitCode::GeneralError.as_i32());
        assert_eq!(
            std::fs::read_to_string(path).expect("read existing file"),
            "existing"
        );
    }

    #[tokio::test]
    async fn export_always_redacts_secret_values() {
        let api = TestApi {
            mutations: AtomicUsize::new(0),
        };
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join("config.txt");

        let code = execute_with_api(
            ConfigCommands::Export(ExportArgs {
                alias: "local".to_string(),
                file: path.clone(),
            }),
            &api,
            &test_formatter(),
        )
        .await;

        assert_eq!(code, ExitCode::Success);
        let content = std::fs::read_to_string(path).expect("read exported configuration");
        assert!(content.contains("*redacted*"));
        assert!(!content.contains("super-secret"));
    }

    #[test]
    fn partial_module_switch_update_preserves_persisted_values() {
        let current = ModuleSwitches {
            notify_enabled: true,
            audit_enabled: false,
            persisted_notify_enabled: false,
            persisted_audit_enabled: true,
            notify_source: "environment".to_string(),
            audit_source: "environment".to_string(),
        };

        let requested = requested_switches(&current, Some(true), None);

        assert!(requested.notify_enabled);
        assert!(requested.audit_enabled);
    }

    #[test]
    fn restore_warning_links_known_server_safety_limitation() {
        assert!(RESTORE_SAFETY_WARNING.contains("rustfs/backlog#1398"));
        assert!(RESTORE_SAFETY_WARNING.contains("unrelated current values are removed"));
    }
}

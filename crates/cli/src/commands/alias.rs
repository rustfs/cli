//! Alias management commands
//!
//! Aliases are named references to S3-compatible storage endpoints,
//! including connection details and credentials.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};
use rc_core::{Alias, AliasManager, Error, ObjectStore as _, validate_alias_endpoint};
use rc_s3::S3Client;

/// Alias subcommands for managing storage service connections
#[derive(Subcommand, Debug)]
pub enum AliasCommands {
    /// Add or update an alias
    Set(Box<SetArgs>),

    /// List all configured aliases
    List(ListArgs),

    /// Remove an alias
    Remove(RemoveArgs),

    /// Export aliases as a portable JSON document
    Export(ExportArgs),

    /// Import aliases from a portable JSON document
    Import(ImportArgs),
}

/// Arguments for the `alias set` command
#[derive(clap::Args, Debug)]
pub struct SetArgs {
    /// Alias name (e.g., "local", "s3", "rustfs")
    pub name: String,

    /// S3 endpoint URL (e.g., `http://localhost:9000`, `https://s3.amazonaws.com`)
    pub endpoint: String,

    /// Access key ID (omit with --anonymous)
    pub access_key: Option<String>,

    /// Secret access key (omit with --anonymous)
    pub secret_key: Option<String>,

    /// Send requests without SigV4 credentials
    #[arg(long, default_value = "false")]
    pub anonymous: bool,

    /// Path to PEM client certificate for mTLS
    #[arg(long)]
    pub client_cert: Option<String>,

    /// Path to PEM client private key for mTLS
    #[arg(long)]
    pub client_key: Option<String>,

    /// AWS region (default: us-east-1)
    #[arg(long, default_value = "us-east-1")]
    pub region: String,

    /// Signature version (only v4 is currently supported)
    #[arg(long, default_value = "v4")]
    pub signature: String,

    /// Bucket lookup style: auto, path, or dns (default: auto)
    #[arg(long, default_value = "auto")]
    pub bucket_lookup: String,

    /// Allow insecure TLS connections
    #[arg(long, default_value = "false")]
    pub insecure: bool,

    /// Path to a PEM CA bundle used to verify the endpoint certificate
    #[arg(long)]
    pub ca_bundle: Option<String>,
}

/// Arguments for the `alias list` command
#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Show full details including endpoints
    #[arg(short, long)]
    pub long: bool,
}

/// Arguments for the `alias remove` command
#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Name of the alias to remove
    pub name: String,
}

/// Arguments for the `alias export` command
#[derive(clap::Args, Debug)]
pub struct ExportArgs {
    /// Alias names to export; omit to export every configured alias
    pub names: Vec<String>,

    /// Write the export to a file instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Include access keys and secret keys in the export
    #[arg(long, requires = "acknowledge_credentials")]
    pub include_credentials: bool,

    /// Acknowledge that the exported document contains plaintext credentials
    #[arg(long, requires = "include_credentials")]
    pub acknowledge_credentials: bool,

    /// Replace an existing output file
    #[arg(long, requires = "output")]
    pub force: bool,
}

/// Arguments for the `alias import` command
#[derive(clap::Args, Debug)]
pub struct ImportArgs {
    /// Portable alias JSON document to import
    pub input: PathBuf,

    /// Replace aliases with conflicting names
    #[arg(long)]
    pub replace: bool,
}

const ALIAS_EXPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasExportDocument {
    schema_version: u32,
    aliases: Vec<PortableAlias>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableAlias {
    name: String,
    endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credentials: Option<PortableCredentials>,
    #[serde(default)]
    anonymous: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key: Option<String>,
    region: String,
    signature: String,
    bucket_lookup: String,
    #[serde(default)]
    insecure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca_bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<rc_core::alias::RetryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout: Option<rc_core::alias::TimeoutConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableCredentials {
    access_key: String,
    secret_key: String,
}

/// JSON output for alias list
#[derive(Serialize)]
struct AliasListOutput {
    aliases: Vec<AliasInfo>,
}

/// Alias information for JSON output (without sensitive data)
#[derive(Serialize)]
struct AliasInfo {
    name: String,
    endpoint: String,
    region: String,
    bucket_lookup: String,
    auth_mode: String,
    mtls: bool,
}

impl From<&Alias> for AliasInfo {
    fn from(alias: &Alias) -> Self {
        Self {
            name: alias.name.clone(),
            endpoint: alias.endpoint.clone(),
            region: alias.region.clone(),
            bucket_lookup: alias.bucket_lookup.clone(),
            auth_mode: if alias.anonymous {
                "anonymous"
            } else {
                "sigv4"
            }
            .to_string(),
            mtls: alias.client_cert.is_some() && alias.client_key.is_some(),
        }
    }
}

/// JSON output for alias set/remove operations
#[derive(Serialize)]
struct AliasOperationOutput {
    success: bool,
    alias: String,
    message: String,
}

/// Execute an alias subcommand
pub async fn execute(cmd: AliasCommands, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    match cmd {
        AliasCommands::Set(args) => execute_set(*args, &alias_manager, &formatter).await,
        AliasCommands::List(args) => execute_list(args, &alias_manager, &formatter).await,
        AliasCommands::Remove(args) => execute_remove(args, &alias_manager, &formatter).await,
        AliasCommands::Export(args) => execute_export(args, &alias_manager, &formatter),
        AliasCommands::Import(args) => execute_import(args, &alias_manager, &formatter),
    }
}

async fn execute_set(args: SetArgs, manager: &AliasManager, formatter: &Formatter) -> ExitCode {
    // Validate inputs
    if args.name.is_empty() {
        return formatter.fail(ExitCode::UsageError, "Alias name cannot be empty");
    }

    if args.endpoint.is_empty() {
        return formatter.fail(ExitCode::UsageError, "Endpoint URL cannot be empty");
    }

    if let Err(e) = validate_alias_endpoint(&args.endpoint) {
        return formatter.fail(ExitCode::UsageError, &alias_endpoint_error_message(e));
    }

    if args.client_cert.is_some() != args.client_key.is_some() {
        return formatter.fail(
            ExitCode::UsageError,
            "--client-cert and --client-key must be supplied together",
        );
    }

    let has_access_key = args
        .access_key
        .as_ref()
        .is_some_and(|value| !value.is_empty());
    let has_secret_key = args
        .secret_key
        .as_ref()
        .is_some_and(|value| !value.is_empty());

    if args.anonymous && (has_access_key || has_secret_key) {
        return formatter.fail(
            ExitCode::UsageError,
            "Anonymous aliases must not include access key or secret key credentials",
        );
    }

    if !args.anonymous && (!has_access_key || !has_secret_key) {
        return formatter.fail(
            ExitCode::UsageError,
            "Access key and secret key are required unless --anonymous is set",
        );
    }

    // Validate signature version
    if args.signature == "v2" {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "Only SigV4 aliases are currently supported",
        );
    }
    if args.signature != "v4" {
        return formatter.fail(ExitCode::UsageError, "Signature must be 'v4' or 'v2'");
    }

    // Validate bucket lookup
    if args.bucket_lookup != "auto" && args.bucket_lookup != "path" && args.bucket_lookup != "dns" {
        return formatter.fail(
            ExitCode::UsageError,
            "Bucket lookup must be 'auto', 'path', or 'dns'",
        );
    }

    // Create alias
    let mut alias = Alias::new(
        &args.name,
        &args.endpoint,
        args.access_key.as_deref().unwrap_or_default(),
        args.secret_key.as_deref().unwrap_or_default(),
    );
    alias.anonymous = args.anonymous;
    alias.client_cert = args.client_cert;
    alias.client_key = args.client_key;
    alias.region = args.region;
    alias.signature = args.signature;
    alias.bucket_lookup = args.bucket_lookup;
    alias.insecure = args.insecure;
    alias.ca_bundle = args.ca_bundle;

    if let Err(e) = validate_alias_credentials(&alias).await {
        let code = exit_code_from_error(&e);
        formatter.error_with_code(code, &e.to_string());
        return code;
    }

    // Save alias
    match manager.set(alias) {
        Ok(()) => {
            if formatter.is_json() {
                let output = AliasOperationOutput {
                    success: true,
                    alias: args.name.clone(),
                    message: format!("Alias '{}' configured successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Alias '{styled_name}' configured successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            let code = exit_code_from_error(&e);
            formatter.error_with_code(code, &e.to_string());
            code
        }
    }
}

async fn validate_alias_credentials(alias: &Alias) -> rc_core::Result<()> {
    if alias.anonymous {
        return Ok(());
    }

    let client = S3Client::new(alias.clone()).await?;
    match client.list_buckets().await {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error.to_string();
            if is_alias_auth_validation_failure(&message) {
                Err(Error::Auth("Invalid access key or secret key".to_string()))
            } else if is_alias_authorization_only_failure(&message) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn is_alias_auth_validation_failure(message: &str) -> bool {
    [
        "InvalidAccessKeyId",
        "SignatureDoesNotMatch",
        "InvalidToken",
        "ExpiredToken",
    ]
    .iter()
    .any(|code| message.contains(code))
}

fn is_alias_authorization_only_failure(message: &str) -> bool {
    ["AccessDenied", "AllAccessDisabled"]
        .iter()
        .any(|code| message.contains(code))
}

async fn execute_list(args: ListArgs, manager: &AliasManager, formatter: &Formatter) -> ExitCode {
    match manager.list() {
        Ok(aliases) => {
            if formatter.is_json() {
                let output = AliasListOutput {
                    aliases: aliases.iter().map(AliasInfo::from).collect(),
                };
                formatter.json(&output);
            } else if aliases.is_empty() {
                formatter.println("No aliases configured.");
            } else if args.long {
                // Long format with details
                for alias in &aliases {
                    let styled_name = formatter.style_name(&format!("{:<12}", alias.name));
                    let styled_url = formatter.style_url(&alias.endpoint);
                    let styled_region = formatter.style_date(&alias.region);
                    let styled_lookup = formatter.style_date(&alias.bucket_lookup);
                    let styled_auth = formatter.style_date(if alias.anonymous {
                        "anonymous"
                    } else {
                        "sigv4"
                    });
                    let styled_mtls = formatter.style_date(
                        if alias.client_cert.is_some() && alias.client_key.is_some() {
                            "enabled"
                        } else {
                            "disabled"
                        },
                    );
                    formatter.println(&format!(
                        "{styled_name} {styled_url} (region: {styled_region}, lookup: {styled_lookup}, auth: {styled_auth}, mtls: {styled_mtls})"
                    ));
                }
            } else {
                // Short format
                for alias in &aliases {
                    let styled_name = formatter.style_name(&format!("{:<12}", alias.name));
                    let styled_url = formatter.style_url(&alias.endpoint);
                    formatter.println(&format!("{styled_name} {styled_url}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            let code = exit_code_from_error(&e);
            formatter.error_with_code(code, &e.to_string());
            code
        }
    }
}

fn execute_export(args: ExportArgs, manager: &AliasManager, formatter: &Formatter) -> ExitCode {
    let aliases = if args.names.is_empty() {
        manager.list()
    } else {
        args.names
            .iter()
            .map(|name| manager.get(name))
            .collect::<rc_core::Result<Vec<_>>>()
    };
    let mut aliases = match aliases {
        Ok(aliases) => aliases,
        Err(error) => {
            let code = exit_code_from_error(&error);
            return formatter.fail(code, &error.to_string());
        }
    };
    aliases.sort_by(|left, right| left.name.cmp(&right.name));

    let document = AliasExportDocument {
        schema_version: ALIAS_EXPORT_SCHEMA_VERSION,
        aliases: aliases
            .iter()
            .map(|alias| PortableAlias::from_alias(alias, args.include_credentials))
            .collect(),
    };
    let mut contents = match serde_json::to_vec_pretty(&document) {
        Ok(contents) => contents,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to serialize aliases: {error}"),
            );
        }
    };
    contents.push(b'\n');

    if let Some(output) = args.output {
        match write_export_file(&output, &contents, args.force) {
            Ok(()) => {
                formatter.success(&format!(
                    "Exported {} alias(es) to {}.",
                    document.aliases.len(),
                    output.display()
                ));
                ExitCode::Success
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => formatter.fail(
                ExitCode::Conflict,
                &format!(
                    "Export file '{}' already exists; retry with --force to replace it",
                    output.display()
                ),
            ),
            Err(error) => formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to write export '{}': {error}", output.display()),
            ),
        }
    } else {
        match io::stdout().write_all(&contents) {
            Ok(()) => ExitCode::Success,
            Err(error) => formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to write alias export: {error}"),
            ),
        }
    }
}

fn execute_import(args: ImportArgs, manager: &AliasManager, formatter: &Formatter) -> ExitCode {
    let contents = match fs::read(&args.input) {
        Ok(contents) => contents,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read import '{}': {error}", args.input.display()),
            );
        }
    };
    let document: AliasExportDocument = match serde_json::from_slice(&contents) {
        Ok(document) => document,
        Err(error) => {
            return formatter.fail(
                ExitCode::UsageError,
                &format!("Malformed alias import document: {error}"),
            );
        }
    };
    if document.schema_version != ALIAS_EXPORT_SCHEMA_VERSION {
        return formatter.fail(
            ExitCode::UsageError,
            &format!(
                "Unsupported alias export schema version {}; expected {}",
                document.schema_version, ALIAS_EXPORT_SCHEMA_VERSION
            ),
        );
    }
    if document.aliases.is_empty() {
        return formatter.fail(
            ExitCode::UsageError,
            "Alias import document contains no aliases",
        );
    }

    let aliases = match document
        .aliases
        .into_iter()
        .map(PortableAlias::into_alias)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(aliases) => aliases,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };
    let imported = aliases.len();
    match manager.import(aliases, args.replace) {
        Ok(()) => {
            formatter.success(&format!("Imported {imported} alias(es)."));
            ExitCode::Success
        }
        Err(error) => {
            let code = if matches!(&error, Error::Config(_)) {
                ExitCode::Conflict
            } else {
                exit_code_from_error(&error)
            };
            formatter.fail(code, &error.to_string())
        }
    }
}

impl PortableAlias {
    fn from_alias(alias: &Alias, include_credentials: bool) -> Self {
        Self {
            name: alias.name.clone(),
            endpoint: alias.endpoint.clone(),
            credentials: (include_credentials
                && !alias.anonymous
                && !alias.access_key.is_empty()
                && !alias.secret_key.is_empty())
            .then(|| PortableCredentials {
                access_key: alias.access_key.clone(),
                secret_key: alias.secret_key.clone(),
            }),
            anonymous: alias.anonymous,
            client_cert: alias.client_cert.clone(),
            client_key: alias.client_key.clone(),
            region: alias.region.clone(),
            signature: alias.signature.clone(),
            bucket_lookup: alias.bucket_lookup.clone(),
            insecure: alias.insecure,
            ca_bundle: alias.ca_bundle.clone(),
            retry: alias.retry.clone(),
            timeout: alias.timeout.clone(),
        }
    }

    fn into_alias(self) -> Result<Alias, String> {
        if !is_valid_portable_alias_name(&self.name) {
            return Err(format!(
                "Invalid alias name '{}'; use letters, numbers, underscores, or hyphens",
                self.name
            ));
        }
        validate_alias_endpoint(&self.endpoint)
            .map_err(|error| format!("Alias '{}' has an invalid endpoint: {error}", self.name))?;
        if self.signature != "v4" {
            return Err(format!(
                "Alias '{}' uses unsupported signature '{}'; expected v4",
                self.name, self.signature
            ));
        }
        if !matches!(self.bucket_lookup.as_str(), "auto" | "path" | "dns") {
            return Err(format!(
                "Alias '{}' has invalid bucket lookup '{}'",
                self.name, self.bucket_lookup
            ));
        }
        if self.client_cert.is_some() != self.client_key.is_some() {
            return Err(format!(
                "Alias '{}' must include both client_cert and client_key",
                self.name
            ));
        }

        let (access_key, secret_key) = match self.credentials {
            Some(credentials)
                if credentials.access_key.is_empty() || credentials.secret_key.is_empty() =>
            {
                return Err(format!(
                    "Alias '{}' contains incomplete credentials",
                    self.name
                ));
            }
            Some(credentials) => (credentials.access_key, credentials.secret_key),
            None => (String::new(), String::new()),
        };
        if self.anonymous && (!access_key.is_empty() || !secret_key.is_empty()) {
            return Err(format!(
                "Anonymous alias '{}' must not include credentials",
                self.name
            ));
        }

        Ok(Alias {
            name: self.name,
            endpoint: self.endpoint,
            access_key,
            secret_key,
            anonymous: self.anonymous,
            client_cert: self.client_cert,
            client_key: self.client_key,
            region: self.region,
            signature: self.signature,
            bucket_lookup: self.bucket_lookup,
            insecure: self.insecure,
            ca_bundle: self.ca_bundle,
            retry: self.retry,
            timeout: self.timeout,
        })
    }
}

fn is_valid_portable_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn write_export_file(destination: &Path, contents: &[u8], force: bool) -> io::Result<()> {
    let directory = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "export destination has no parent directory",
        )
    })?;
    let directory = if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        directory
    };
    fs::create_dir_all(directory)?;

    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(contents)?;
    temporary.as_file_mut().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file_mut()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    if force {
        temporary
            .persist(destination)
            .map_err(|error| error.error)?;
    } else {
        temporary
            .persist_noclobber(destination)
            .map_err(|error| error.error)?;
    }
    Ok(())
}

async fn execute_remove(
    args: RemoveArgs,
    manager: &AliasManager,
    formatter: &Formatter,
) -> ExitCode {
    match manager.remove(&args.name) {
        Ok(()) => {
            if formatter.is_json() {
                let output = AliasOperationOutput {
                    success: true,
                    alias: args.name.clone(),
                    message: format!("Alias '{}' removed successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Alias '{styled_name}' removed successfully."));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::AliasNotFound(_)) => {
            formatter.error(&format!("Alias '{}' not found", args.name));
            ExitCode::NotFound
        }
        Err(e) => {
            let code = exit_code_from_error(&e);
            formatter.error_with_code(code, &e.to_string());
            code
        }
    }
}

fn alias_endpoint_error_message(error: rc_core::Error) -> String {
    match error {
        rc_core::Error::Config(message) => message,
        other => format!("Invalid endpoint: {other}"),
    }
}

fn exit_code_from_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::commands::{Cli, Commands};

    #[test]
    fn test_set_args_defaults() {
        // Verify default values are applied correctly
        let args = SetArgs {
            name: "test".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            access_key: Some("accesskey".to_string()),
            secret_key: Some("secretkey".to_string()),
            anonymous: false,
            client_cert: None,
            client_key: None,
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
            ca_bundle: None,
        };

        assert_eq!(args.region, "us-east-1");
        assert_eq!(args.signature, "v4");
        assert_eq!(args.bucket_lookup, "auto");
        assert!(!args.insecure);
    }

    #[test]
    fn test_alias_info_from_alias() {
        let alias = Alias::new("test", "http://localhost:9000", "key", "secret");
        let info = AliasInfo::from(&alias);

        assert_eq!(info.name, "test");
        assert_eq!(info.endpoint, "http://localhost:9000");
        assert_eq!(info.region, "us-east-1");
    }

    #[test]
    fn export_redacts_credentials_by_default() {
        let alias = Alias::new(
            "local",
            "http://localhost:9000",
            "visible-access",
            "visible-secret",
        );

        let encoded = serde_json::to_string(&PortableAlias::from_alias(&alias, false)).unwrap();

        assert!(!encoded.contains("visible-access"));
        assert!(!encoded.contains("visible-secret"));
        assert!(!encoded.contains("credentials"));
    }

    #[test]
    fn export_includes_credentials_only_when_requested() {
        let alias = Alias::new(
            "local",
            "http://localhost:9000",
            "visible-access",
            "visible-secret",
        );

        let encoded = serde_json::to_string(&PortableAlias::from_alias(&alias, true)).unwrap();

        assert!(encoded.contains("visible-access"));
        assert!(encoded.contains("visible-secret"));
    }

    #[test]
    fn cli_requires_explicit_credential_export_acknowledgement() {
        let error =
            Cli::try_parse_from(["rc", "alias", "export", "--include-credentials"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let cli = Cli::try_parse_from([
            "rc",
            "alias",
            "export",
            "--include-credentials",
            "--acknowledge-credentials",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Alias(AliasCommands::Export(ExportArgs {
                include_credentials: true,
                acknowledge_credentials: true,
                ..
            }))
        ));
    }

    #[test]
    fn import_rejects_unknown_fields_and_invalid_endpoints() {
        let malformed = br#"{
            "schema_version": 1,
            "aliases": [],
            "unexpected": true
        }"#;
        assert!(serde_json::from_slice::<AliasExportDocument>(malformed).is_err());

        let portable = PortableAlias {
            name: "local".to_string(),
            endpoint: "ftp://localhost:9000".to_string(),
            credentials: None,
            anonymous: false,
            client_cert: None,
            client_key: None,
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
            ca_bundle: None,
            retry: None,
            timeout: None,
        };
        assert!(portable.into_alias().is_err());
    }

    #[test]
    fn redacted_alias_imports_without_synthesizing_credentials() {
        let portable = PortableAlias {
            name: "local".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            credentials: None,
            anonymous: false,
            client_cert: None,
            client_key: None,
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
            ca_bundle: None,
            retry: None,
            timeout: None,
        };

        let alias = portable.into_alias().unwrap();

        assert!(alias.access_key.is_empty());
        assert!(alias.secret_key.is_empty());
        assert!(!alias.anonymous);
    }

    #[test]
    fn export_file_refuses_implicit_overwrite_and_force_is_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("nested/aliases.json");
        write_export_file(&destination, b"first", false).unwrap();

        let error = write_export_file(&destination, b"second", false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&destination).unwrap(), b"first");

        write_export_file(&destination, b"second", true).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"second");
    }

    #[tokio::test]
    async fn test_execute_set_rejects_invalid_endpoint_url() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = AliasManager::with_config_manager(rc_core::ConfigManager::with_path(
            temp_dir.path().join("config.toml"),
        ));
        let formatter = Formatter::new(OutputConfig::default());
        let args = set_args("http://rustfs-node{1...32}:9000");

        let exit_code = execute_set(args, &manager, &formatter).await;

        assert_eq!(exit_code, ExitCode::UsageError);
        assert!(manager.get("rustfs").is_err());
    }

    #[tokio::test]
    async fn test_execute_set_rejects_invalid_credentials() {
        let endpoint = spawn_s3_error_server("InvalidAccessKeyId").await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = AliasManager::with_config_manager(rc_core::ConfigManager::with_path(
            temp_dir.path().join("config.toml"),
        ));
        let formatter = Formatter::new(OutputConfig::default());

        let exit_code = execute_set(set_args(&endpoint), &manager, &formatter).await;

        assert_eq!(exit_code, ExitCode::AuthError);
        assert!(manager.get("rustfs").is_err());
    }

    #[tokio::test]
    async fn test_execute_set_allows_authenticated_access_denied() {
        let endpoint = spawn_s3_error_server("AccessDenied").await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = AliasManager::with_config_manager(rc_core::ConfigManager::with_path(
            temp_dir.path().join("config.toml"),
        ));
        let formatter = Formatter::new(OutputConfig::default());

        let exit_code = execute_set(set_args(&endpoint), &manager, &formatter).await;

        assert_eq!(exit_code, ExitCode::Success);
        assert!(manager.get("rustfs").is_ok());
    }

    #[test]
    fn test_alias_auth_validation_preserves_signing_configuration_errors() {
        assert!(is_alias_auth_validation_failure("InvalidAccessKeyId"));
        assert!(is_alias_auth_validation_failure("SignatureDoesNotMatch"));
        assert!(!is_alias_auth_validation_failure(
            "AuthorizationHeaderMalformed"
        ));
        assert!(!is_alias_auth_validation_failure("RequestTimeTooSkewed"));
    }

    #[test]
    fn test_alias_endpoint_error_message_omits_config_prefix() {
        let message = alias_endpoint_error_message(rc_core::Error::Config(
            "Endpoint must include a host".to_string(),
        ));

        assert_eq!(message, "Endpoint must include a host");
    }

    fn set_args(endpoint: &str) -> SetArgs {
        SetArgs {
            name: "rustfs".to_string(),
            endpoint: endpoint.to_string(),
            access_key: Some("accesskey".to_string()),
            secret_key: Some("secretkey".to_string()),
            anonymous: false,
            client_cert: None,
            client_key: None,
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
            ca_bundle: None,
        }
    }

    async fn spawn_s3_error_server(code: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await;
                let body = format!("<Error><Code>{code}</Code><Message>test</Message></Error>");
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }
}

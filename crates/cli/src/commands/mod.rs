//! CLI command definitions and execution
//!
//! This module contains all CLI commands and their implementations.
//! Commands are organized by functionality and follow the pattern established
//! in the command implementation template.

use std::io::{IsTerminal, stderr, stdout};

use clap::{Parser, Subcommand, ValueEnum};
use rc_core::config::Defaults;
use rc_core::{ConfigManager, RequestHeader, set_global_request_headers};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const COMPATIBILITY_AFTER_HELP: &str = "\
Compatibility:
  Canonical commands: alias, admin, bucket, object, and operational utilities
  Compatibility aliases: mc-style names delegate to canonical rc implementations
  Version gated: commands fail closed when RustFS does not advertise required support
  Server blocked: unavailable mc families are listed with blockers in the compatibility matrix

See: https://github.com/rustfs/cli/blob/main/docs/reference/rc/mc-compatibility.md";

mod admin;
mod alias;
mod anonymous;
mod bucket;
mod cat;
mod completions;
mod cors;
pub mod cp;
pub mod diff;
mod du;
mod encryption;
mod event;
mod find;
mod head;
mod ilm;
mod legalhold;
mod lock;
mod ls;
mod mb;
mod mirror;
mod multipart;
mod mv;
mod object;
mod ops_output;
mod ping;
mod pipe;
mod quota;
mod rb;
mod ready;
mod replicate;
mod retention;
mod rm;
mod share;
mod sql;
mod stat;
mod tag;
mod transfer_fidelity;
mod tree;
mod undo;
mod version;
mod watch;

fn exit_code_for_core_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

fn validate_version_selector(version_id: Option<&str>, rewind: Option<&str>) -> Result<(), String> {
    if version_id.is_some() && rewind.is_some() {
        return Err("--version-id cannot be combined with --rewind".to_string());
    }
    if version_id.is_some_and(str::is_empty) {
        return Err("--version-id cannot be empty".to_string());
    }
    Ok(())
}

/// rc - Rust S3 CLI Client
///
/// A command-line interface for S3-compatible object storage services.
/// Supports RustFS, AWS S3, and other S3-compatible backends.
#[derive(Parser, Debug)]
#[command(name = "rc")]
#[command(author, version, about, long_about = None)]
#[command(after_help = COMPATIBILITY_AFTER_HELP)]
#[command(propagate_version = true)]
pub struct Cli {
    /// Output format: auto-detect, human-readable, or JSON
    #[arg(long, global = true, value_enum)]
    pub format: Option<OutputFormat>,

    /// Output format: human-readable or JSON
    #[arg(long, global = true, default_value = "false")]
    pub json: bool,

    /// Disable colored output
    #[arg(long, global = true, default_value = "false")]
    pub no_color: bool,

    /// Disable progress bar
    #[arg(long, global = true, default_value = "false")]
    pub no_progress: bool,

    /// Suppress non-error output
    #[arg(short, long, global = true, default_value = "false")]
    pub quiet: bool,

    /// Enable debug logging
    #[arg(long, global = true, default_value = "false")]
    pub debug: bool,

    /// Add an x-amz-* request header to signed S3 requests
    #[arg(short = 'H', long = "header", global = true, value_parser = parse_request_header)]
    pub request_headers: Vec<RequestHeader>,

    #[command(subcommand)]
    pub command: Commands,
}

fn parse_request_header(value: &str) -> Result<RequestHeader, String> {
    let header = RequestHeader::parse(value).map_err(|error| error.to_string())?;
    if header
        .name
        .eq_ignore_ascii_case("x-amz-bypass-governance-retention")
    {
        return Err(
            "Use the retention or remove command's explicit --bypass flag for governance retention bypass"
                .to_string(),
        );
    }
    Ok(header)
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    Auto,
    Human,
    Json,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum OutputBehavior {
    HumanDefault,
    StructuredDefault,
}

#[derive(Copy, Clone, Debug)]
struct GlobalOutputOptions {
    format: Option<OutputFormat>,
    json: bool,
    no_color: bool,
    force_color: bool,
    no_progress: bool,
    quiet: bool,
}

impl GlobalOutputOptions {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            format: cli.format,
            json: cli.json,
            no_color: cli.no_color,
            force_color: false,
            no_progress: cli.no_progress,
            quiet: cli.quiet,
        }
    }

    fn apply_defaults(mut self, defaults: &Defaults) -> Result<Self, String> {
        if self.format.is_none() && !self.json {
            self.format = Some(match defaults.output.as_str() {
                "human" => OutputFormat::Human,
                "json" => OutputFormat::Json,
                value => return Err(format!("Invalid default output format '{value}'")),
            });
        }

        if !self.no_color {
            match defaults.color.as_str() {
                "auto" => {}
                "always" => self.force_color = true,
                "never" => self.no_color = true,
                value => return Err(format!("Invalid default color mode '{value}'")),
            }
        }
        if !defaults.progress {
            self.no_progress = true;
        }

        Ok(self)
    }

    fn resolve(self, behavior: OutputBehavior) -> OutputConfig {
        let stdout_is_tty = stdout().is_terminal();
        let stderr_is_tty = stderr().is_terminal();

        let selected_format = if self.json {
            OutputFormat::Json
        } else {
            self.format.unwrap_or(match behavior {
                OutputBehavior::HumanDefault => OutputFormat::Human,
                OutputBehavior::StructuredDefault => OutputFormat::Auto,
            })
        };

        let json = match selected_format {
            OutputFormat::Json => true,
            OutputFormat::Human => false,
            OutputFormat::Auto => !stdout_is_tty,
        };

        OutputConfig {
            json,
            no_color: self.no_color || (!self.force_color && !stdout_is_tty) || json,
            no_progress: self.no_progress || !stderr_is_tty || json,
            quiet: self.quiet,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage storage service aliases
    #[command(subcommand)]
    Alias(alias::AliasCommands),

    /// Manage IAM users, policies, groups, and service accounts
    #[command(subcommand)]
    Admin(admin::AdminCommands),

    /// Manage bucket-oriented workflows
    Bucket(bucket::BucketArgs),

    /// Manage object-oriented workflows
    Object(object::ObjectArgs),

    // Phase 2: Basic commands
    /// Deprecated: use `rc bucket list` or `rc object list`
    Ls(ls::LsArgs),

    /// Deprecated: use `rc bucket create`
    Mb(mb::MbArgs),

    /// Deprecated: use `rc bucket remove`
    Rb(rb::RbArgs),

    /// Deprecated: use `rc object show`
    Cat(cat::CatArgs),

    /// Deprecated: use `rc object head`
    Head(head::HeadArgs),

    /// Deprecated: use `rc object stat`
    Stat(stat::StatArgs),

    // Phase 3: Transfer commands
    /// Deprecated: use `rc object copy`
    Cp(Box<cp::CpArgs>),

    /// Download one remote object to the local filesystem
    Get(Box<cp::GetArgs>),

    /// Upload one or more local paths to remote object storage
    Put(Box<cp::PutArgs>),

    /// Deprecated: use `rc object move`
    Mv(mv::MvArgs),

    /// Deprecated: use `rc object remove`
    Rm(rm::RmArgs),

    /// Restore a versioned PUT or DELETE operation without removing data history
    Undo(undo::UndoArgs),

    /// Stream stdin to an object
    Pipe(pipe::PipeArgs),

    // Phase 4: Advanced commands
    /// Deprecated: use `rc object find`
    Find(find::FindArgs),

    /// Deprecated: use `rc bucket event`
    Event(event::EventArgs),

    /// Deprecated: use `rc bucket cors`
    #[command(subcommand)]
    Cors(cors::CorsCommands),

    /// Show differences between locations
    Diff(diff::DiffArgs),

    /// Mirror objects between locations
    Mirror(mirror::MirrorArgs),

    /// Deprecated: use `rc object tree`
    Tree(tree::TreeArgs),

    /// Deprecated: use `rc object share`
    Share(share::ShareArgs),

    /// Run S3 Select SQL on an object
    Sql(sql::SqlArgs),

    // Phase 5: Optional commands (capability-dependent)
    /// Deprecated: use `rc bucket version`
    #[command(subcommand)]
    Version(version::VersionCommands),

    /// Manage bucket and object tags
    #[command(subcommand)]
    Tag(tag::TagCommands),

    /// Deprecated: use `rc bucket anonymous`
    #[command(subcommand)]
    Anonymous(anonymous::AnonymousCommands),

    /// Deprecated: use `rc bucket quota`
    #[command(subcommand)]
    Quota(quota::QuotaCommands),

    /// Deprecated: use `rc bucket lifecycle`
    Ilm(ilm::IlmArgs),

    /// Deprecated: use `rc bucket replication`
    Replicate(replicate::ReplicateArgs),

    /// Manage object retention with mc-compatible command syntax
    Retention(retention::RetentionArgs),

    /// Manage object legal hold with mc-compatible command syntax
    Legalhold(legalhold::LegalHoldArgs),

    // Phase 6: Utilities
    /// Generate shell completion scripts
    Completions(completions::CompletionsArgs),

    /// Summarize storage usage
    Du(du::DuArgs),

    /// Check service liveness and round-trip latency
    Ping(ping::PingArgs),

    /// Check whether required dependencies are ready
    Ready(ready::ReadyArgs),

    /// Stream live object notifications
    Watch(watch::WatchArgs),
}

/// Execute the CLI command and return an exit code
pub async fn execute(cli: Cli) -> ExitCode {
    set_global_request_headers(cli.request_headers.clone());
    let cli_output_options = GlobalOutputOptions::from_cli(&cli);
    let defaults = match ConfigManager::new() {
        Ok(manager) if manager.config_path().exists() => match manager.load() {
            Ok(config) => Some(config.defaults),
            Err(error) => {
                return Formatter::new(cli_output_options.resolve(OutputBehavior::HumanDefault))
                    .fail(
                        ExitCode::GeneralError,
                        &format!("Failed to load configuration: {error}"),
                    );
            }
        },
        Ok(_) => None,
        Err(error) => {
            return Formatter::new(cli_output_options.resolve(OutputBehavior::HumanDefault)).fail(
                ExitCode::GeneralError,
                &format!("Failed to load configuration: {error}"),
            );
        }
    };
    let output_options = cli_output_options;
    let output_options = if let Some(defaults) = &defaults {
        match output_options.apply_defaults(defaults) {
            Ok(options) => options,
            Err(error) => {
                return Formatter::new(cli_output_options.resolve(OutputBehavior::HumanDefault))
                    .fail(
                        ExitCode::GeneralError,
                        &format!("Failed to load configuration: {error}"),
                    );
            }
        }
    } else {
        output_options
    };

    match cli.command {
        Commands::Alias(cmd) => {
            alias::execute(cmd, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Admin(cmd) => {
            admin::execute(cmd, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Bucket(args) => {
            bucket::execute(
                args,
                output_options.resolve(OutputBehavior::StructuredDefault),
            )
            .await
        }
        Commands::Object(args) => {
            let behavior = match &args.command {
                object::ObjectCommands::Show(_) | object::ObjectCommands::Head(_) => {
                    OutputBehavior::HumanDefault
                }
                _ => OutputBehavior::StructuredDefault,
            };
            object::execute(args, output_options.resolve(behavior)).await
        }
        Commands::Ls(args) => {
            ls::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Mb(args) => {
            mb::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Rb(args) => {
            rb::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Cat(args) => {
            cat::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Head(args) => {
            head::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Stat(args) => {
            stat::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Cp(args) => {
            cp::execute(*args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Get(args) => {
            cp::execute_get(*args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Put(args) => {
            cp::execute_put(*args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Mv(args) => {
            mv::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Rm(args) => {
            rm::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Undo(args) => {
            undo::execute(
                args,
                output_options.resolve(OutputBehavior::StructuredDefault),
            )
            .await
        }
        Commands::Pipe(args) => {
            pipe::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Find(args) => {
            find::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Event(args) => {
            event::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Cors(cmd) => {
            cors::execute(
                cors::CorsArgs { command: cmd },
                output_options.resolve(OutputBehavior::HumanDefault),
            )
            .await
        }
        Commands::Diff(args) => {
            diff::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Mirror(args) => {
            mirror::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Tree(args) => {
            tree::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Share(args) => {
            share::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Sql(args) => {
            sql::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Version(cmd) => {
            version::execute(
                version::VersionArgs { command: cmd },
                output_options.resolve(OutputBehavior::HumanDefault),
            )
            .await
        }
        Commands::Tag(cmd) => {
            tag::execute(
                tag::TagArgs { command: cmd },
                output_options.resolve(OutputBehavior::HumanDefault),
            )
            .await
        }
        Commands::Anonymous(cmd) => {
            anonymous::execute(
                anonymous::AnonymousArgs { command: cmd },
                output_options.resolve(OutputBehavior::HumanDefault),
            )
            .await
        }
        Commands::Quota(cmd) => {
            quota::execute(
                quota::QuotaArgs { command: cmd },
                output_options.resolve(OutputBehavior::HumanDefault),
            )
            .await
        }
        Commands::Ilm(args) => {
            ilm::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Replicate(args) => {
            replicate::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Retention(args) => {
            retention::execute(
                args,
                output_options.resolve(OutputBehavior::StructuredDefault),
            )
            .await
        }
        Commands::Legalhold(args) => {
            legalhold::execute(
                args,
                output_options.resolve(OutputBehavior::StructuredDefault),
            )
            .await
        }
        Commands::Completions(args) => completions::execute(args),
        Commands::Watch(args) => {
            watch::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Du(args) => {
            du::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Ping(args) => {
            ping::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Ready(args) => {
            ready::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn structured_default_uses_auto_format_when_not_explicit() {
        let options = GlobalOutputOptions {
            format: None,
            json: false,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };

        let resolved = options.resolve(OutputBehavior::StructuredDefault);
        assert_eq!(resolved.json, !std::io::stdout().is_terminal());
    }

    #[test]
    fn human_default_keeps_human_format_when_not_explicit() {
        let options = GlobalOutputOptions {
            format: None,
            json: false,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };

        let resolved = options.resolve(OutputBehavior::HumanDefault);
        assert!(!resolved.json);
    }

    #[test]
    fn explicit_json_overrides_behavior_defaults() {
        let options = GlobalOutputOptions {
            format: Some(OutputFormat::Human),
            json: true,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };

        let resolved = options.resolve(OutputBehavior::HumanDefault);
        assert!(resolved.json);
    }

    #[test]
    fn explicit_human_overrides_structured_default() {
        let options = GlobalOutputOptions {
            format: Some(OutputFormat::Human),
            json: false,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };

        let resolved = options.resolve(OutputBehavior::StructuredDefault);
        assert!(!resolved.json);
    }

    #[test]
    fn explicit_auto_overrides_human_default() {
        let options = GlobalOutputOptions {
            format: Some(OutputFormat::Auto),
            json: false,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };

        let resolved = options.resolve(OutputBehavior::HumanDefault);
        assert_eq!(resolved.json, !std::io::stdout().is_terminal());
    }

    #[test]
    fn configured_defaults_control_output_color_and_progress() {
        let options = GlobalOutputOptions {
            format: None,
            json: false,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };
        let defaults = Defaults {
            output: "json".to_string(),
            color: "never".to_string(),
            progress: false,
        };

        let resolved = options
            .apply_defaults(&defaults)
            .expect("valid defaults")
            .resolve(OutputBehavior::HumanDefault);

        assert!(resolved.json);
        assert!(resolved.no_color);
        assert!(resolved.no_progress);
    }

    #[test]
    fn invalid_configured_defaults_are_rejected() {
        let options = GlobalOutputOptions {
            format: None,
            json: false,
            no_color: false,
            force_color: false,
            no_progress: false,
            quiet: false,
        };
        let defaults = Defaults {
            output: "yaml".to_string(),
            ..Defaults::default()
        };

        assert!(options.apply_defaults(&defaults).is_err());
    }

    #[test]
    fn cli_accepts_global_custom_amz_header() {
        let cli = Cli::try_parse_from([
            "rc",
            "-H",
            "x-amz-bucket-encrypt-enabled:1",
            "bucket",
            "list",
            "local/",
        ])
        .expect("parse custom header");

        assert_eq!(cli.request_headers.len(), 1);
        assert_eq!(cli.request_headers[0].name, "x-amz-bucket-encrypt-enabled");
        assert_eq!(cli.request_headers[0].value, "1");
    }

    #[test]
    fn cli_rejects_non_amz_custom_header() {
        let error = Cli::try_parse_from(["rc", "-H", "authorization:secret", "ls", "local/"])
            .expect_err("non amz header should fail");

        assert!(
            error
                .to_string()
                .contains("Only x-amz-* custom request headers are supported")
        );
    }

    #[test]
    fn cli_accepts_repeatable_watch_event_filters() {
        let cli = Cli::try_parse_from([
            "rc",
            "watch",
            "local/photos",
            "--event",
            "put,delete",
            "--event",
            "get",
            "--prefix",
            "incoming/",
        ])
        .expect("parse watch command");

        let Commands::Watch(args) = cli.command else {
            panic!("expected watch command");
        };
        assert_eq!(args.events, vec!["put,delete", "get"]);
        assert_eq!(args.prefix.as_deref(), Some("incoming/"));
    }

    #[test]
    fn cli_requires_bypass_flag_instead_of_custom_governance_header() {
        let error = Cli::try_parse_from([
            "rc",
            "-H",
            "x-amz-bypass-governance-retention:true",
            "rm",
            "local/bucket/key.txt",
        ])
        .expect_err("governance bypass header should require --bypass");

        assert!(error.to_string().contains("explicit --bypass flag"));
    }

    #[test]
    fn cli_accepts_bucket_cors_subcommand() {
        let cli = Cli::try_parse_from(["rc", "bucket", "cors", "list", "local/my-bucket"])
            .expect("parse bucket cors");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Cors(cors::CorsCommands::List(arg)) => {
                    assert_eq!(arg.path, "local/my-bucket");
                }
                other => panic!("expected bucket cors list command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_list_alias() {
        let cli =
            Cli::try_parse_from(["rc", "bucket", "ls", "local/"]).expect("parse bucket ls alias");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::List(arg) => {
                    assert_eq!(arg.path, "local/");
                }
                other => panic!("expected bucket list alias, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_cors_subcommand() {
        let cli = Cli::try_parse_from(["rc", "cors", "remove", "local/my-bucket"])
            .expect("parse top-level cors");

        match cli.command {
            Commands::Cors(cors::CorsCommands::Remove(arg)) => {
                assert_eq!(arg.path, "local/my-bucket");
            }
            other => panic!("expected top-level cors remove command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_cors_get_alias() {
        let cli =
            Cli::try_parse_from(["rc", "cors", "get", "local/my-bucket"]).expect("parse cors get");

        match cli.command {
            Commands::Cors(cors::CorsCommands::List(arg)) => {
                assert_eq!(arg.path, "local/my-bucket");
            }
            other => panic!("expected top-level cors get alias, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_event_subcommand() {
        let cli = Cli::try_parse_from(["rc", "event", "list", "local/my-bucket"])
            .expect("parse top-level event");

        match cli.command {
            Commands::Event(event::EventArgs {
                command: event::EventCommands::List(arg),
            }) => {
                assert_eq!(arg.path, "local/my-bucket");
            }
            other => panic!("expected top-level event list command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_event_add_subcommand() {
        let cli = Cli::try_parse_from([
            "rc",
            "event",
            "add",
            "local/my-bucket",
            "arn:aws:sqs:us-east-1:123456789012:jobs",
            "--event",
            "put,delete",
            "--force",
        ])
        .expect("parse top-level event add");

        match cli.command {
            Commands::Event(event::EventArgs {
                command: event::EventCommands::Add(arg),
            }) => {
                assert_eq!(arg.path, "local/my-bucket");
                assert_eq!(arg.arn, "arn:aws:sqs:us-east-1:123456789012:jobs");
                assert_eq!(arg.events, vec!["put,delete".to_string()]);
                assert!(arg.force);
            }
            other => panic!("expected top-level event add command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_sql_select_options() {
        let cli = Cli::try_parse_from([
            "rc",
            "sql",
            "local/reports/data.jsonl",
            "--query",
            "SELECT * FROM S3Object",
            "--input-format",
            "json",
            "--output-format",
            "json",
            "--compression",
            "gzip",
        ])
        .expect("parse sql command");

        match cli.command {
            Commands::Sql(arg) => {
                assert_eq!(arg.path, "local/reports/data.jsonl");
                assert_eq!(arg.query, "SELECT * FROM S3Object");
                assert!(matches!(arg.input_format, sql::InputFormatArg::Json));
                assert!(matches!(arg.output_format, sql::OutputFormatArg::Json));
                assert!(matches!(arg.compression, sql::CompressionArg::Gzip));
            }
            other => panic!("expected sql command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_sql_defaults() {
        let cli = Cli::try_parse_from([
            "rc",
            "sql",
            "local/reports/data.csv",
            "--query",
            "SELECT s._1 FROM S3Object s",
        ])
        .expect("parse sql command defaults");

        match cli.command {
            Commands::Sql(arg) => {
                assert_eq!(arg.path, "local/reports/data.csv");
                assert_eq!(arg.query, "SELECT s._1 FROM S3Object s");
                assert!(matches!(arg.input_format, sql::InputFormatArg::Csv));
                assert!(matches!(arg.output_format, sql::OutputFormatArg::Csv));
                assert!(matches!(arg.compression, sql::CompressionArg::None));
            }
            other => panic!("expected sql command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_object_list_alias() {
        let cli = Cli::try_parse_from(["rc", "object", "ls", "local/my-bucket/logs/"])
            .expect("parse object ls alias");

        match cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::List(arg) => {
                    assert_eq!(arg.path, "local/my-bucket/logs/");
                }
                other => panic!("expected object list alias, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_event_remove_subcommand() {
        let cli = Cli::try_parse_from([
            "rc",
            "event",
            "remove",
            "local/my-bucket",
            "arn:aws:sns:us-east-1:123456789012:alerts",
            "--force",
        ])
        .expect("parse top-level event remove");

        match cli.command {
            Commands::Event(event::EventArgs {
                command: event::EventCommands::Remove(arg),
            }) => {
                assert_eq!(arg.path, "local/my-bucket");
                assert_eq!(arg.arn, "arn:aws:sns:us-east-1:123456789012:alerts");
                assert!(arg.force);
            }
            other => panic!("expected top-level event remove command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_cors_get_alias() {
        let cli = Cli::try_parse_from(["rc", "bucket", "cors", "get", "local/my-bucket"])
            .expect("parse bucket cors get");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Cors(cors::CorsCommands::List(arg)) => {
                    assert_eq!(arg.path, "local/my-bucket");
                }
                other => panic!("expected bucket cors get alias, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_cors_set_with_positional_source() {
        let cli =
            Cli::try_parse_from(["rc", "bucket", "cors", "set", "local/my-bucket", "cors.xml"])
                .expect("parse bucket cors set with positional source");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Cors(cors::CorsCommands::Set(arg)) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert_eq!(arg.source.as_deref(), Some("cors.xml"));
                }
                other => panic!("expected bucket cors set command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_cors_set_with_positional_source() {
        let cli = Cli::try_parse_from(["rc", "cors", "set", "local/my-bucket", "cors.xml"])
            .expect("parse top-level cors set with positional source");

        match cli.command {
            Commands::Cors(cors::CorsCommands::Set(arg)) => {
                assert_eq!(arg.path, "local/my-bucket");
                assert_eq!(arg.source.as_deref(), Some("cors.xml"));
                assert_eq!(arg.file, None);
                assert!(!arg.force);
            }
            other => panic!("expected top-level cors set command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_top_level_cors_set_with_legacy_file_flag() {
        let cli = Cli::try_parse_from([
            "rc",
            "cors",
            "set",
            "local/my-bucket",
            "--file",
            "cors.json",
            "--force",
        ])
        .expect("parse top-level cors set with --file");

        match cli.command {
            Commands::Cors(cors::CorsCommands::Set(arg)) => {
                assert_eq!(arg.path, "local/my-bucket");
                assert_eq!(arg.source, None);
                assert_eq!(arg.file.as_deref(), Some("cors.json"));
                assert!(arg.force);
            }
            other => panic!("expected top-level cors set command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_cors_list_force_flag() {
        let cli =
            Cli::try_parse_from(["rc", "bucket", "cors", "list", "local/my-bucket", "--force"])
                .expect("parse bucket cors list with force");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Cors(cors::CorsCommands::List(arg)) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert!(arg.force);
                }
                other => panic!("expected bucket cors list command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_lifecycle_subcommand() {
        let cli = Cli::try_parse_from([
            "rc",
            "bucket",
            "lifecycle",
            "rule",
            "list",
            "local/my-bucket",
        ])
        .expect("parse bucket lifecycle rule list");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Lifecycle(ilm::IlmArgs {
                    command: ilm::IlmCommands::Rule(ilm::rule::RuleCommands::List(arg)),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert!(!arg.force);
                }
                other => panic!(
                    "expected bucket lifecycle rule list command, got {:?}",
                    other
                ),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_replication_subcommand() {
        let cli = Cli::try_parse_from(["rc", "bucket", "replication", "status", "local/my-bucket"])
            .expect("parse bucket replication status");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Replication(replicate::ReplicateArgs {
                    command: replicate::ReplicateCommands::Status(arg),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert!(!arg.force);
                }
                other => panic!(
                    "expected bucket replication status command, got {:?}",
                    other
                ),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_replication_diff_prefix() {
        let cli = Cli::try_parse_from([
            "rc",
            "bucket",
            "replication",
            "diff",
            "local/my-bucket",
            "--prefix",
            "reports/2026/",
        ])
        .expect("parse bucket replication diff");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Replication(replicate::ReplicateArgs {
                    command: replicate::ReplicateCommands::Diff(arg),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert_eq!(arg.prefix.as_deref(), Some("reports/2026/"));
                }
                other => panic!("expected bucket replication diff command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_replication_add_tls_flags() {
        let cli = Cli::try_parse_from([
            "rc",
            "bucket",
            "replication",
            "add",
            "local/my-bucket",
            "--remote-bucket",
            "backup/archive",
            "--insecure",
        ])
        .expect("parse bucket replication add with insecure");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Replication(replicate::ReplicateArgs {
                    command: replicate::ReplicateCommands::Add(arg),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert_eq!(arg.remote_bucket, "backup/archive");
                    assert!(arg.insecure);
                }
                other => panic!("expected bucket replication add command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_replication_check_confirmation() {
        let cli = Cli::try_parse_from([
            "rc",
            "bucket",
            "replication",
            "check",
            "local/my-bucket",
            "--yes",
            "--force",
        ])
        .expect("parse bucket replication check");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Replication(replicate::ReplicateArgs {
                    command: replicate::ReplicateCommands::Check(arg),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert!(arg.yes);
                    assert!(arg.force);
                }
                other => panic!("expected bucket replication check command, got {other:?}"),
            },
            other => panic!("expected bucket command, got {other:?}"),
        }
    }

    #[test]
    fn cli_accepts_bucket_replication_resync_lifecycle() {
        let start = Cli::try_parse_from([
            "rc",
            "bucket",
            "replication",
            "resync",
            "start",
            "local/my-bucket",
            "--target-arn",
            "arn:rustfs:replication::id:backup",
            "--older-than",
            "7d",
            "--reset-id",
            "caller-id",
            "--yes",
        ])
        .expect("parse bucket replication resync start");

        match start.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Replication(replicate::ReplicateArgs {
                    command:
                        replicate::ReplicateCommands::Resync(replicate::ResyncCommands::Start(arg)),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert_eq!(
                        arg.target_arn.as_deref(),
                        Some("arn:rustfs:replication::id:backup")
                    );
                    assert_eq!(arg.older_than.as_deref(), Some("7d"));
                    assert_eq!(arg.reset_id.as_deref(), Some("caller-id"));
                    assert!(arg.yes);
                }
                other => panic!("expected bucket replication resync start, got {other:?}"),
            },
            other => panic!("expected bucket command, got {other:?}"),
        }

        let status = Cli::try_parse_from([
            "rc",
            "bucket",
            "replication",
            "resync",
            "status",
            "local/my-bucket",
            "--target-arn",
            "arn:rustfs:replication::id:backup",
        ])
        .expect("parse bucket replication resync status");

        match status.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Replication(replicate::ReplicateArgs {
                    command:
                        replicate::ReplicateCommands::Resync(replicate::ResyncCommands::Status(arg)),
                }) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert_eq!(
                        arg.target_arn.as_deref(),
                        Some("arn:rustfs:replication::id:backup")
                    );
                }
                other => panic!("expected bucket replication resync status, got {other:?}"),
            },
            other => panic!("expected bucket command, got {other:?}"),
        }
    }

    #[test]
    fn cli_accepts_bucket_remove_subcommand() {
        let cli = Cli::try_parse_from(["rc", "bucket", "remove", "local/my-bucket"])
            .expect("parse bucket remove");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Remove(arg) => {
                    assert_eq!(arg.target, "local/my-bucket");
                    assert!(!arg.force);
                    assert!(!arg.dangerous);
                    assert!(!arg.yes);
                }
                other => panic!("expected bucket remove command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_requires_complete_dangerous_bucket_remove_guards() {
        let cli = Cli::try_parse_from([
            "rc",
            "bucket",
            "remove",
            "local/my-bucket",
            "--force",
            "--dangerous",
            "--yes",
        ])
        .expect("parse guarded bucket remove");
        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Remove(arg) => {
                    assert!(arg.force);
                    assert!(arg.dangerous);
                    assert!(arg.yes);
                }
                other => panic!("expected bucket remove command, got {other:?}"),
            },
            other => panic!("expected bucket command, got {other:?}"),
        }

        assert!(
            Cli::try_parse_from([
                "rc",
                "bucket",
                "remove",
                "local/my-bucket",
                "--dangerous",
                "--yes",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "rc",
                "bucket",
                "remove",
                "local/my-bucket",
                "--force",
                "--dangerous",
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_accepts_object_remove_subcommand() {
        let cli = Cli::try_parse_from([
            "rc",
            "object",
            "remove",
            "local/my-bucket/report.csv",
            "--dry-run",
        ])
        .expect("parse object remove");

        match cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Remove(arg) => {
                    assert_eq!(arg.paths, vec!["local/my-bucket/report.csv".to_string()]);
                    assert!(arg.dry_run);
                }
                other => panic!("expected object remove command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_bucket_event_remove_subcommand() {
        let cli = Cli::try_parse_from([
            "rc",
            "bucket",
            "event",
            "remove",
            "local/my-bucket",
            "arn:aws:sns:us-east-1:123456789012:alerts",
        ])
        .expect("parse bucket event remove");

        match cli.command {
            Commands::Bucket(args) => match args.command {
                bucket::BucketCommands::Event(event::EventCommands::Remove(arg)) => {
                    assert_eq!(arg.path, "local/my-bucket");
                    assert_eq!(arg.arn, "arn:aws:sns:us-east-1:123456789012:alerts");
                }
                other => panic!("expected bucket event remove command, got {:?}", other),
            },
            other => panic!("expected bucket command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_rm_purge_flag() {
        let cli = Cli::try_parse_from(["rc", "rm", "local/my-bucket/object.txt", "--purge"])
            .expect("parse rm purge");

        match cli.command {
            Commands::Rm(arg) => {
                assert_eq!(arg.paths, vec!["local/my-bucket/object.txt".to_string()]);
                assert!(arg.purge);
            }
            other => panic!("expected rm command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_object_remove_purge_flag() {
        let cli = Cli::try_parse_from([
            "rc",
            "object",
            "remove",
            "local/my-bucket/object.txt",
            "--purge",
        ])
        .expect("parse object remove purge");

        match cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Remove(arg) => {
                    assert_eq!(arg.paths, vec!["local/my-bucket/object.txt".to_string()]);
                    assert!(arg.purge);
                }
                other => panic!("expected object remove command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_object_stat_subcommand() {
        let cli = Cli::try_parse_from(["rc", "object", "stat", "local/my-bucket/report.json"])
            .expect("parse object stat");

        match cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Stat(arg) => {
                    assert_eq!(arg.path, "local/my-bucket/report.json");
                }
                other => panic!("expected object stat command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_object_copy_with_transfer_options() {
        let cli = Cli::try_parse_from([
            "rc",
            "object",
            "copy",
            "./report.json",
            "local/my-bucket/reports/",
            "--content-type",
            "application/json",
            "--storage-class",
            "STANDARD",
            "--metadata-directive",
            "replace",
            "--tagging-directive",
            "replace",
            "--cache-control",
            "max-age=3600",
            "--metadata",
            "owner=analytics",
            "--tags",
            "env=prod",
            "--checksum",
            "sha256",
            "--retention-mode",
            "governance",
            "--retain-until",
            "2031-01-02T03:04:05Z",
            "--legal-hold",
            "on",
            "--dry-run",
        ])
        .expect("parse object copy with transfer options");

        match cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Copy(arg) => {
                    assert_eq!(arg.sources, ["./report.json"]);
                    assert_eq!(arg.target, "local/my-bucket/reports/");
                    assert_eq!(arg.content_type.as_deref(), Some("application/json"));
                    assert_eq!(arg.storage_class.as_deref(), Some("STANDARD"));
                    assert_eq!(
                        arg.metadata_directive,
                        Some(transfer_fidelity::MetadataDirectiveArg::Replace)
                    );
                    assert_eq!(
                        arg.tagging_directive,
                        Some(transfer_fidelity::TaggingDirectiveArg::Replace)
                    );
                    assert_eq!(arg.fidelity.cache_control.as_deref(), Some("max-age=3600"));
                    assert_eq!(arg.fidelity.metadata, ["owner=analytics"]);
                    assert_eq!(arg.fidelity.tags, ["env=prod"]);
                    assert_eq!(arg.fidelity.checksum.as_deref(), Some("sha256"));
                    assert_eq!(arg.fidelity.retention_mode.as_deref(), Some("governance"));
                    assert_eq!(arg.fidelity.legal_hold.as_deref(), Some("on"));
                    assert!(arg.dry_run);
                }
                other => panic!("expected object copy command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_multiple_copy_sources_and_global_transfer_controls() {
        let cli = Cli::try_parse_from([
            "rc",
            "cp",
            "./a.csv",
            "./b.csv",
            "local/reports/",
            "--include",
            "*.csv",
            "--exclude",
            "private-*",
            "--newer-than",
            "1h",
            "--concurrency",
            "8",
            "--rate-limit",
            "10MiB/s",
            "--retry-attempts",
            "5",
            "--continue-on-error",
            "--summary",
        ])
        .expect("parse multi-source transfer controls");

        match cli.command {
            Commands::Cp(args) => {
                assert_eq!(args.sources, ["./a.csv", "./b.csv"]);
                assert_eq!(args.target, "local/reports/");
                assert_eq!(args.include, ["*.csv"]);
                assert_eq!(args.exclude, ["private-*"]);
                assert_eq!(args.newer_than.as_deref(), Some("1h"));
                assert_eq!(args.concurrency, Some(8));
                assert_eq!(args.rate_limit.as_deref(), Some("10MiB/s"));
                assert_eq!(args.retry_attempts, Some(5));
                assert!(args.continue_on_error);
                assert!(args.summary);
            }
            other => panic!("expected cp command, got {other:?}"),
        }
    }

    #[test]
    fn cli_accepts_get_with_copy_options() {
        let cli = Cli::try_parse_from([
            "rc",
            "get",
            "local/reports/report.json",
            "./report.json",
            "--enc-c-source-key-env",
            "RC_SSE_C_KEY",
            "--retry-attempts",
            "5",
        ])
        .expect("parse get compatibility command");

        match cli.command {
            Commands::Get(args) => {
                assert_eq!(args.transfer.sources, ["local/reports/report.json"]);
                assert_eq!(args.transfer.target, "./report.json");
                assert_eq!(
                    args.transfer.enc_c_source_key_env.as_deref(),
                    Some("RC_SSE_C_KEY")
                );
                assert_eq!(args.transfer.retry_attempts, Some(5));
            }
            other => panic!("expected get command, got {other:?}"),
        }
    }

    #[test]
    fn cli_accepts_put_with_multiple_sources_and_copy_options() {
        let cli = Cli::try_parse_from([
            "rc",
            "put",
            "./january.csv",
            "./february.csv",
            "local/reports/",
            "--storage-class",
            "STANDARD",
            "--concurrency",
            "8",
            "--summary",
        ])
        .expect("parse put compatibility command");

        match cli.command {
            Commands::Put(args) => {
                assert_eq!(args.transfer.sources, ["./january.csv", "./february.csv"]);
                assert_eq!(args.transfer.target, "local/reports/");
                assert_eq!(args.transfer.storage_class.as_deref(), Some("STANDARD"));
                assert_eq!(args.transfer.concurrency, Some(8));
                assert!(args.transfer.summary);
            }
            other => panic!("expected put command, got {other:?}"),
        }
    }

    #[test]
    fn cli_accepts_object_move_with_recursive_dry_run() {
        let cli = Cli::try_parse_from([
            "rc",
            "object",
            "move",
            "local/source-bucket/logs/",
            "local/archive-bucket/logs/",
            "--recursive",
            "--dry-run",
            "--continue-on-error",
        ])
        .expect("parse object move with recursive dry-run");

        match cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Move(arg) => {
                    assert_eq!(arg.source, "local/source-bucket/logs/");
                    assert_eq!(arg.target, "local/archive-bucket/logs/");
                    assert!(arg.recursive);
                    assert!(arg.dry_run);
                    assert!(arg.continue_on_error);
                }
                other => panic!("expected object move command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_object_show_and_head_options() {
        let show_cli = Cli::try_parse_from([
            "rc",
            "object",
            "show",
            "local/my-bucket/report.json",
            "--version-id",
            "v1",
            "--rewind",
            "1h",
        ])
        .expect("parse object show options");

        match show_cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Show(arg) => {
                    assert_eq!(arg.path, "local/my-bucket/report.json");
                    assert_eq!(arg.version_id.as_deref(), Some("v1"));
                    assert_eq!(arg.rewind.as_deref(), Some("1h"));
                }
                other => panic!("expected object show command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }

        let head_cli = Cli::try_parse_from([
            "rc",
            "object",
            "head",
            "local/my-bucket/report.json",
            "--bytes",
            "128",
            "--version-id",
            "v2",
        ])
        .expect("parse object head options");

        match head_cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Head(arg) => {
                    assert_eq!(arg.path, "local/my-bucket/report.json");
                    assert_eq!(arg.bytes, Some(128));
                    assert_eq!(arg.version_id.as_deref(), Some("v2"));
                }
                other => panic!("expected object head command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn cli_accepts_object_find_and_tree_options() {
        let find_cli = Cli::try_parse_from([
            "rc",
            "object",
            "find",
            "local/my-bucket/logs/",
            "--name",
            "*.json",
            "--maxdepth",
            "2",
            "--count",
            "--print",
        ])
        .expect("parse object find options");

        match find_cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Find(arg) => {
                    assert_eq!(arg.path, "local/my-bucket/logs/");
                    assert_eq!(arg.name.as_deref(), Some("*.json"));
                    assert_eq!(arg.maxdepth, 2);
                    assert!(arg.count);
                    assert!(arg.print);
                }
                other => panic!("expected object find command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }

        let tree_cli = Cli::try_parse_from([
            "rc",
            "object",
            "tree",
            "local/my-bucket/logs/",
            "--level",
            "4",
            "--size",
            "--pattern",
            "*.json",
            "--full-path",
        ])
        .expect("parse object tree options");

        match tree_cli.command {
            Commands::Object(args) => match args.command {
                object::ObjectCommands::Tree(arg) => {
                    assert_eq!(arg.path, "local/my-bucket/logs/");
                    assert_eq!(arg.level, 4);
                    assert!(arg.size);
                    assert_eq!(arg.pattern.as_deref(), Some("*.json"));
                    assert!(arg.full_path);
                }
                other => panic!("expected object tree command, got {:?}", other),
            },
            other => panic!("expected object command, got {:?}", other),
        }
    }

    #[test]
    fn version_selector_rejects_ambiguous_or_empty_values() {
        assert!(validate_version_selector(Some("v1"), None).is_ok());
        assert!(validate_version_selector(None, Some("1h")).is_ok());
        assert!(validate_version_selector(Some("v1"), Some("1h")).is_err());
        assert!(validate_version_selector(Some(""), None).is_err());
    }

    #[test]
    fn versioning_errors_map_to_stable_exit_codes() {
        assert_eq!(
            exit_code_for_core_error(&rc_core::Error::VersionNotFound {
                path: "local/bucket/key".to_string(),
                version_id: "v1".to_string(),
            }),
            ExitCode::NotFound
        );
        assert_eq!(
            exit_code_for_core_error(&rc_core::Error::Auth("denied".to_string())),
            ExitCode::AuthError
        );
        assert_eq!(
            exit_code_for_core_error(&rc_core::Error::GovernanceDenied {
                path: "local/bucket/key".to_string(),
                version_id: Some("v1".to_string()),
            }),
            ExitCode::Conflict
        );
    }
}

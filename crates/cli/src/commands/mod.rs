//! CLI command definitions and execution
//!
//! This module contains all CLI commands and their implementations.
//! Commands are organized by functionality and follow the pattern established
//! in the command implementation template.

use std::io::{IsTerminal, stderr, stdout};

use clap::{Parser, Subcommand, ValueEnum};

use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

mod admin;
mod alias;
mod anonymous;
mod bucket;
mod cat;
mod completions;
mod cors;
pub mod cp;
pub mod diff;
mod event;
mod find;
mod head;
mod ilm;
mod ls;
mod mb;
mod mirror;
mod mv;
mod object;
mod pipe;
mod quota;
mod rb;
mod replicate;
mod rm;
mod share;
mod stat;
mod tag;
mod tree;
mod version;

/// rc - Rust S3 CLI Client
///
/// A command-line interface for S3-compatible object storage services.
/// Supports RustFS, AWS S3, and other S3-compatible backends.
#[derive(Parser, Debug)]
#[command(name = "rc")]
#[command(author, version, about, long_about = None)]
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

    #[command(subcommand)]
    pub command: Commands,
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
    no_progress: bool,
    quiet: bool,
}

impl GlobalOutputOptions {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            format: cli.format,
            json: cli.json,
            no_color: cli.no_color,
            no_progress: cli.no_progress,
            quiet: cli.quiet,
        }
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
            no_color: self.no_color || !stdout_is_tty || json,
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
    Cp(cp::CpArgs),

    /// Deprecated: use `rc object move`
    Mv(mv::MvArgs),

    /// Deprecated: use `rc object remove`
    Rm(rm::RmArgs),

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

    // Phase 6: Utilities
    /// Generate shell completion scripts
    Completions(completions::CompletionsArgs),
    // /// Manage object retention
    // Retention(retention::RetentionArgs),
    // /// Watch for object events
    // Watch(watch::WatchArgs),
    // /// Run S3 Select queries
    // Sql(sql::SqlArgs),
}

/// Execute the CLI command and return an exit code
pub async fn execute(cli: Cli) -> ExitCode {
    let output_options = GlobalOutputOptions::from_cli(&cli);

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
            cp::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Mv(args) => {
            mv::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
        }
        Commands::Rm(args) => {
            rm::execute(args, output_options.resolve(OutputBehavior::HumanDefault)).await
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
        Commands::Completions(args) => completions::execute(args),
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
            no_progress: false,
            quiet: false,
        };

        let resolved = options.resolve(OutputBehavior::HumanDefault);
        assert!(resolved.json);
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
        let cli = Cli::try_parse_from(["rc", "cors", "get", "local/my-bucket"])
            .expect("parse top-level cors get");

        match cli.command {
            Commands::Cors(cors::CorsCommands::List(arg)) => {
                assert_eq!(arg.path, "local/my-bucket");
            }
            other => panic!("expected top-level cors get alias, got {:?}", other),
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
}

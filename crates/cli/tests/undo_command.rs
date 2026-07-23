use clap::Parser;
use rustfs_cli::commands::{Cli, Commands};

#[test]
fn undo_parses_safe_selection_and_execution_controls() {
    let cli = Cli::try_parse_from([
        "rc",
        "undo",
        "local/archive/reports/",
        "--recursive",
        "--dry-run",
        "--concurrency",
        "3",
    ])
    .expect("parse undo");

    match cli.command {
        Commands::Undo(args) => {
            assert_eq!(args.path, "local/archive/reports/");
            assert!(args.recursive);
            assert!(args.dry_run);
            assert_eq!(args.concurrency, 3);
            assert!(args.version_id.is_none());
        }
        other => panic!("expected undo command, got {other:?}"),
    }
}

#[test]
fn undo_parses_explicit_version_for_one_object() {
    let cli = Cli::try_parse_from([
        "rc",
        "undo",
        "local/archive/report.txt",
        "--version-id",
        "v1",
    ])
    .expect("parse explicit undo version");

    match cli.command {
        Commands::Undo(args) => {
            assert_eq!(args.version_id.as_deref(), Some("v1"));
            assert!(!args.recursive);
            assert!(!args.dry_run);
        }
        other => panic!("expected undo command, got {other:?}"),
    }
}

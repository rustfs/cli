//! CLI help contract tests.
//!
//! These tests verify command and option discoverability through `--help` output.
//! They provide a fast contract layer for all command paths, without requiring
//! a running S3 backend.

use std::path::PathBuf;
use std::process::{Command, Output};

const GLOBAL_OPTIONS: &[&str] = &[
    "--format",
    "--json",
    "--no-color",
    "--no-progress",
    "--quiet",
    "--debug",
    "--help",
    "--version",
];

#[derive(Debug)]
struct HelpCase {
    args: &'static [&'static str],
    usage: &'static str,
    expected_tokens: &'static [&'static str],
}

fn rc_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return PathBuf::from(path);
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has parent directory")
        .parent()
        .expect("workspace root exists")
        .to_path_buf();

    let debug_binary = workspace_root.join("target/debug/rc");
    if debug_binary.exists() {
        return debug_binary;
    }

    workspace_root.join("target/release/rc")
}

fn run_rc(args: &[&str]) -> Output {
    Command::new(rc_binary())
        .args(args)
        .output()
        .expect("failed to execute rc")
}

fn assert_help_case(case: &HelpCase) {
    let mut args = case.args.to_vec();
    args.push("--help");

    let output = run_rc(&args);
    let command_label = if case.args.is_empty() {
        "rc".to_string()
    } else {
        format!("rc {}", case.args.join(" "))
    };

    assert!(
        output.status.success(),
        "help should succeed for {command_label}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let normalized_stdout = stdout.replace("Usage: rc.exe ", "Usage: rc ");
    assert!(
        normalized_stdout.contains(case.usage),
        "usage marker `{}` missing for {command_label}\nstdout:\n{}",
        case.usage,
        stdout
    );

    for option in GLOBAL_OPTIONS {
        assert!(
            stdout.contains(option),
            "global option `{option}` missing in help for {command_label}\nstdout:\n{}",
            stdout
        );
    }

    for token in case.expected_tokens {
        assert!(
            stdout.contains(token),
            "expected token `{token}` missing in help for {command_label}\nstdout:\n{}",
            stdout
        );
    }
}

#[test]
fn top_level_command_help_contract() {
    let cases = [
        HelpCase {
            args: &[],
            usage: "Usage: rc [OPTIONS] <COMMAND>",
            expected_tokens: &[
                "alias",
                "admin",
                "bucket",
                "object",
                "ls",
                "mb",
                "rb",
                "cat",
                "head",
                "stat",
                "cp",
                "mv",
                "rm",
                "pipe",
                "find",
                "diff",
                "mirror",
                "tree",
                "share",
                "version",
                "tag",
                "quota",
                "anonymous",
                "ilm",
                "replicate",
                "completions",
            ],
        },
        HelpCase {
            args: &["alias"],
            usage: "Usage: rc alias [OPTIONS] <COMMAND>",
            expected_tokens: &["set", "list", "remove"],
        },
        HelpCase {
            args: &["admin"],
            usage: "Usage: rc admin [OPTIONS] <COMMAND>",
            expected_tokens: &["info", "heal", "user", "policy", "group", "service-account"],
        },
        HelpCase {
            args: &["bucket"],
            usage: "Usage: rc bucket [OPTIONS] <COMMAND>",
            expected_tokens: &[
                "list",
                "create",
                "remove",
                "event",
                "version",
                "quota",
                "anonymous",
                "lifecycle",
                "replication",
                "Examples:",
                "rc bucket list local/",
            ],
        },
        HelpCase {
            args: &["object"],
            usage: "Usage: rc object [OPTIONS] <COMMAND>",
            expected_tokens: &[
                "list",
                "copy",
                "move",
                "remove",
                "stat",
                "show",
                "head",
                "find",
                "tree",
                "share",
                "Examples:",
                "rc object copy ./report.json local/my-bucket/reports/",
            ],
        },
        HelpCase {
            args: &["ls"],
            usage: "Usage: rc ls [OPTIONS] <PATH>",
            expected_tokens: &["--recursive", "--versions", "--incomplete", "--summarize"],
        },
        HelpCase {
            args: &["mb"],
            usage: "Usage: rc mb [OPTIONS] <TARGET>",
            expected_tokens: &[
                "--ignore-existing",
                "--region",
                "--with-lock",
                "--with-versioning",
                "Examples:",
                "rc bucket create local/my-bucket",
            ],
        },
        HelpCase {
            args: &["rb"],
            usage: "Usage: rc rb [OPTIONS] <TARGET>",
            expected_tokens: &["--force", "--dangerous"],
        },
        HelpCase {
            args: &["cat"],
            usage: "Usage: rc cat [OPTIONS] <PATH>",
            expected_tokens: &["--enc-key", "--rewind", "--version-id"],
        },
        HelpCase {
            args: &["head"],
            usage: "Usage: rc head [OPTIONS] <PATH>",
            expected_tokens: &["--lines", "--bytes", "--version-id"],
        },
        HelpCase {
            args: &["stat"],
            usage: "Usage: rc stat [OPTIONS] <PATH>",
            expected_tokens: &["--version-id", "--rewind"],
        },
        HelpCase {
            args: &["cp"],
            usage: "Usage: rc cp [OPTIONS] <SOURCE> <TARGET>",
            expected_tokens: &[
                "--recursive",
                "--preserve",
                "--continue-on-error",
                "--overwrite",
                "--dry-run",
                "--storage-class",
                "--content-type",
                "Examples:",
                "rc object copy ./report.json local/my-bucket/reports/",
            ],
        },
        HelpCase {
            args: &["mv"],
            usage: "Usage: rc mv [OPTIONS] <SOURCE> <TARGET>",
            expected_tokens: &["--recursive", "--continue-on-error", "--dry-run"],
        },
        HelpCase {
            args: &["rm"],
            usage: "Usage: rc rm [OPTIONS] <PATHS>...",
            expected_tokens: &[
                "--recursive",
                "--force",
                "--dry-run",
                "--incomplete",
                "--versions",
                "--bypass",
                "Examples:",
                "rc rm local/my-bucket/reports/ --recursive --dry-run",
            ],
        },
        HelpCase {
            args: &["pipe"],
            usage: "Usage: rc pipe [OPTIONS] <TARGET>",
            expected_tokens: &["--content-type", "--storage-class"],
        },
        HelpCase {
            args: &["find"],
            usage: "Usage: rc find [OPTIONS] <PATH>",
            expected_tokens: &[
                "--name",
                "--larger",
                "--smaller",
                "--newer",
                "--older",
                "--maxdepth",
                "--count",
                "--exec",
                "--print",
            ],
        },
        HelpCase {
            args: &["diff"],
            usage: "Usage: rc diff [OPTIONS] <FIRST> <SECOND>",
            expected_tokens: &["--recursive", "--diff-only"],
        },
        HelpCase {
            args: &["mirror"],
            usage: "Usage: rc mirror [OPTIONS] <SOURCE> <TARGET>",
            expected_tokens: &["--remove", "--overwrite", "--dry-run", "--parallel"],
        },
        HelpCase {
            args: &["tree"],
            usage: "Usage: rc tree [OPTIONS] <PATH>",
            expected_tokens: &[
                "--level",
                "--size",
                "--dirs-only",
                "--pattern",
                "--full-path",
            ],
        },
        HelpCase {
            args: &["share"],
            usage: "Usage: rc share [OPTIONS] <PATH>",
            expected_tokens: &["--expire", "--upload", "--content-type"],
        },
        HelpCase {
            args: &["version"],
            usage: "Usage: rc version [OPTIONS] <COMMAND>",
            expected_tokens: &["enable", "suspend", "info", "list"],
        },
        HelpCase {
            args: &["tag"],
            usage: "Usage: rc tag [OPTIONS] <COMMAND>",
            expected_tokens: &["list", "set", "remove"],
        },
        HelpCase {
            args: &["quota"],
            usage: "Usage: rc quota [OPTIONS] <COMMAND>",
            expected_tokens: &["set", "info", "clear"],
        },
        HelpCase {
            args: &["event"],
            usage: "Usage: rc event [OPTIONS] <COMMAND>",
            expected_tokens: &[
                "add",
                "list",
                "remove",
                "Examples:",
                "rc bucket event list local/my-bucket",
            ],
        },
        HelpCase {
            args: &["anonymous"],
            usage: "Usage: rc anonymous [OPTIONS] <COMMAND>",
            expected_tokens: &["set", "set-json", "get", "get-json", "list", "links"],
        },
        HelpCase {
            args: &["completions"],
            usage: "Usage: rc completions [OPTIONS] <SHELL>",
            expected_tokens: &["[possible values: bash, elvish, fish, powershell, zsh]"],
        },
    ];

    for case in cases {
        assert_help_case(&case);
    }
}

#[test]
fn nested_subcommand_help_contract() {
    let cases = [
        HelpCase {
            args: &["alias", "set"],
            usage: "Usage: rc alias set [OPTIONS] <NAME> <ENDPOINT> <ACCESS_KEY> <SECRET_KEY>",
            expected_tokens: &["--region", "--signature", "--bucket-lookup", "--insecure"],
        },
        HelpCase {
            args: &["alias", "list"],
            usage: "Usage: rc alias list [OPTIONS]",
            expected_tokens: &["--long"],
        },
        HelpCase {
            args: &["alias", "remove"],
            usage: "Usage: rc alias remove [OPTIONS] <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "info", "cluster"],
            usage: "Usage: rc admin info cluster [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "info", "server"],
            usage: "Usage: rc admin info server [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "info", "disk"],
            usage: "Usage: rc admin info disk [OPTIONS] <ALIAS>",
            expected_tokens: &["--offline", "--healing"],
        },
        HelpCase {
            args: &["admin", "heal", "status"],
            usage: "Usage: rc admin heal status [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "heal", "start"],
            usage: "Usage: rc admin heal start [OPTIONS] <ALIAS>",
            expected_tokens: &[
                "--bucket",
                "--prefix",
                "--scan-mode",
                "--remove",
                "--recreate",
                "--dry-run",
            ],
        },
        HelpCase {
            args: &["admin", "heal", "stop"],
            usage: "Usage: rc admin heal stop [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "user", "ls"],
            usage: "Usage: rc admin user ls [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "user", "add"],
            usage: "Usage: rc admin user add [OPTIONS] <ALIAS> <ACCESS_KEY> <SECRET_KEY>",
            expected_tokens: &[
                "Examples:",
                "rc admin user add local analyst analyst-secret",
            ],
        },
        HelpCase {
            args: &["bucket", "create"],
            usage: "Usage: rc bucket create [OPTIONS] <TARGET>",
            expected_tokens: &[
                "--ignore-existing",
                "--region",
                "--with-lock",
                "--with-versioning",
                "Examples:",
                "rc bucket create local/my-bucket",
            ],
        },
        HelpCase {
            args: &["bucket", "replication", "add"],
            usage: "Usage: rc bucket replication add [OPTIONS] --remote-bucket <TARGET_ALIAS/BUCKET> <PATH>",
            expected_tokens: &[
                "--remote-bucket",
                "--priority",
                "--healthcheck-seconds",
                "Examples:",
                "rc bucket replication add local/my-bucket --remote-bucket backup/archive",
            ],
        },
        HelpCase {
            args: &["bucket", "event", "add"],
            usage: "Usage: rc bucket event add [OPTIONS] <PATH> <ARN>",
            expected_tokens: &[
                "--event",
                "--force",
                "Examples:",
                "rc bucket event add local/my-bucket arn:aws:sqs:us-east-1:123456789012:jobs --event put",
            ],
        },
        HelpCase {
            args: &["object", "copy"],
            usage: "Usage: rc object copy [OPTIONS] <SOURCE> <TARGET>",
            expected_tokens: &[
                "--recursive",
                "--preserve",
                "--continue-on-error",
                "--overwrite",
                "--dry-run",
                "--storage-class",
                "--content-type",
                "Examples:",
                "rc object copy ./report.json local/my-bucket/reports/",
            ],
        },
        HelpCase {
            args: &["object", "show"],
            usage: "Usage: rc object show [OPTIONS] <PATH>",
            expected_tokens: &["--enc-key", "--rewind", "--version-id"],
        },
        HelpCase {
            args: &["object", "stat"],
            usage: "Usage: rc object stat [OPTIONS] <PATH>",
            expected_tokens: &["--version-id", "--rewind"],
        },
        HelpCase {
            args: &["object", "share"],
            usage: "Usage: rc object share [OPTIONS] <PATH>",
            expected_tokens: &["--expire", "--upload", "--content-type"],
        },
        HelpCase {
            args: &["admin", "user", "info"],
            usage: "Usage: rc admin user info [OPTIONS] <ALIAS> <ACCESS_KEY>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "user", "rm"],
            usage: "Usage: rc admin user rm [OPTIONS] <ALIAS> <ACCESS_KEY>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "user", "enable"],
            usage: "Usage: rc admin user enable [OPTIONS] <ALIAS> <ACCESS_KEY>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "user", "disable"],
            usage: "Usage: rc admin user disable [OPTIONS] <ALIAS> <ACCESS_KEY>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "policy", "ls"],
            usage: "Usage: rc admin policy ls [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "policy", "create"],
            usage: "Usage: rc admin policy create [OPTIONS] <ALIAS> <NAME> <POLICY_FILE>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "policy", "info"],
            usage: "Usage: rc admin policy info [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "policy", "rm"],
            usage: "Usage: rc admin policy rm [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "policy", "attach"],
            usage: "Usage: rc admin policy attach [OPTIONS] <ALIAS> <POLICIES>",
            expected_tokens: &["--user", "--group"],
        },
        HelpCase {
            args: &["admin", "group", "ls"],
            usage: "Usage: rc admin group ls [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "group", "add"],
            usage: "Usage: rc admin group add [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &["--members"],
        },
        HelpCase {
            args: &["admin", "group", "info"],
            usage: "Usage: rc admin group info [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "group", "rm"],
            usage: "Usage: rc admin group rm [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "group", "enable"],
            usage: "Usage: rc admin group enable [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "group", "disable"],
            usage: "Usage: rc admin group disable [OPTIONS] <ALIAS> <NAME>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "group", "add-members"],
            usage: "Usage: rc admin group add-members [OPTIONS] <ALIAS> <NAME> <MEMBERS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "group", "rm-members"],
            usage: "Usage: rc admin group rm-members [OPTIONS] <ALIAS> <NAME> <MEMBERS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "service-account", "ls"],
            usage: "Usage: rc admin service-account ls [OPTIONS] <ALIAS>",
            expected_tokens: &["--user"],
        },
        HelpCase {
            args: &["admin", "service-account", "create"],
            usage: "Usage: rc admin service-account create [OPTIONS] <ALIAS> <ACCESS_KEY> <SECRET_KEY>",
            expected_tokens: &["--name", "--description", "--policy", "--expiry"],
        },
        HelpCase {
            args: &["admin", "service-account", "info"],
            usage: "Usage: rc admin service-account info [OPTIONS] <ALIAS> <ACCESS_KEY>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["admin", "service-account", "rm"],
            usage: "Usage: rc admin service-account rm [OPTIONS] <ALIAS> <ACCESS_KEY>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["version", "enable"],
            usage: "Usage: rc version enable [OPTIONS] <PATH>",
            expected_tokens: &["--force"],
        },
        HelpCase {
            args: &["version", "suspend"],
            usage: "Usage: rc version suspend [OPTIONS] <PATH>",
            expected_tokens: &["--force"],
        },
        HelpCase {
            args: &["version", "info"],
            usage: "Usage: rc version info [OPTIONS] <PATH>",
            expected_tokens: &["--force"],
        },
        HelpCase {
            args: &["version", "list"],
            usage: "Usage: rc version list [OPTIONS] <PATH>",
            expected_tokens: &["--max", "--force"],
        },
        HelpCase {
            args: &["tag", "list"],
            usage: "Usage: rc tag list [OPTIONS] <PATH>",
            expected_tokens: &["--force"],
        },
        HelpCase {
            args: &["tag", "set"],
            usage: "Usage: rc tag set [OPTIONS] <PATH>",
            expected_tokens: &["--tags", "--force"],
        },
        HelpCase {
            args: &["tag", "remove"],
            usage: "Usage: rc tag remove [OPTIONS] <PATH>",
            expected_tokens: &["--force"],
        },
        HelpCase {
            args: &["quota", "set"],
            usage: "Usage: rc quota set [OPTIONS] <PATH> <SIZE>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["quota", "info"],
            usage: "Usage: rc quota info [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["quota", "clear"],
            usage: "Usage: rc quota clear [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["anonymous", "set"],
            usage: "Usage: rc anonymous set [OPTIONS] <PERMISSION> <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["anonymous", "set-json"],
            usage: "Usage: rc anonymous set-json [OPTIONS] <FILE> <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["anonymous", "get"],
            usage: "Usage: rc anonymous get [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["anonymous", "get-json"],
            usage: "Usage: rc anonymous get-json [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["anonymous", "list"],
            usage: "Usage: rc anonymous list [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["anonymous", "links"],
            usage: "Usage: rc anonymous links [OPTIONS] <PATH>",
            expected_tokens: &["--recursive"],
        },
        // ILM commands
        HelpCase {
            args: &["ilm"],
            usage: "Usage: rc ilm [OPTIONS] <COMMAND>",
            expected_tokens: &["rule", "tier", "restore"],
        },
        HelpCase {
            args: &["ilm", "rule"],
            usage: "Usage: rc ilm rule [OPTIONS] <COMMAND>",
            expected_tokens: &["add", "edit", "list", "remove", "export", "import"],
        },
        HelpCase {
            args: &["ilm", "rule", "add"],
            usage: "Usage: rc ilm rule add [OPTIONS] <PATH>",
            expected_tokens: &[
                "--expiry-days",
                "--transition-days",
                "--storage-class",
                "--prefix",
            ],
        },
        HelpCase {
            args: &["ilm", "rule", "list"],
            usage: "Usage: rc ilm rule list [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["ilm", "rule", "remove"],
            usage: "Usage: rc ilm rule remove [OPTIONS] <PATH>",
            expected_tokens: &["--id", "--all"],
        },
        HelpCase {
            args: &["ilm", "tier"],
            usage: "Usage: rc ilm tier [OPTIONS] <COMMAND>",
            expected_tokens: &["add", "edit", "list", "info", "remove"],
        },
        HelpCase {
            args: &["ilm", "tier", "add"],
            usage: "Usage: rc ilm tier add [OPTIONS]",
            expected_tokens: &["--endpoint", "--access-key", "--secret-key", "--bucket"],
        },
        HelpCase {
            args: &["ilm", "tier", "list"],
            usage: "Usage: rc ilm tier list [OPTIONS] <ALIAS>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["ilm", "tier", "remove"],
            usage: "Usage: rc ilm tier remove [OPTIONS]",
            expected_tokens: &["--force"],
        },
        HelpCase {
            args: &["ilm", "restore"],
            usage: "Usage: rc ilm restore [OPTIONS] <PATH>",
            expected_tokens: &["--days"],
        },
        // Replicate commands
        HelpCase {
            args: &["replicate"],
            usage: "Usage: rc replicate [OPTIONS] <COMMAND>",
            expected_tokens: &[
                "add",
                "update",
                "list",
                "status",
                "remove",
                "export",
                "import",
                "Examples:",
                "rc bucket replication add local/my-bucket --remote-bucket backup/archive",
            ],
        },
        HelpCase {
            args: &["replicate", "add"],
            usage: "Usage: rc replicate add [OPTIONS]",
            expected_tokens: &[
                "--remote-bucket",
                "--priority",
                "Examples:",
                "rc bucket replication add local/my-bucket --remote-bucket backup/archive",
            ],
        },
        HelpCase {
            args: &["event", "add"],
            usage: "Usage: rc event add [OPTIONS] <PATH> <ARN>",
            expected_tokens: &[
                "--event",
                "--force",
                "Examples:",
                "rc event add local/my-bucket arn:aws:sns:us-east-1:123456789012:alerts --event delete",
            ],
        },
        HelpCase {
            args: &["replicate", "list"],
            usage: "Usage: rc replicate list [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["replicate", "status"],
            usage: "Usage: rc replicate status [OPTIONS] <PATH>",
            expected_tokens: &[],
        },
        HelpCase {
            args: &["replicate", "remove"],
            usage: "Usage: rc replicate remove [OPTIONS] <PATH>",
            expected_tokens: &["--id", "--all"],
        },
    ];

    for case in cases {
        assert_help_case(&case);
    }
}

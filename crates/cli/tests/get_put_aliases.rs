use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rc_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return PathBuf::from(path);
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has parent directory")
        .parent()
        .expect("workspace root exists")
        .join("target/debug/rc")
}

fn run_rc(config_dir: &Path, args: &[&str]) -> Output {
    Command::new(rc_binary())
        .env("RC_CONFIG_DIR", config_dir)
        .current_dir(config_dir)
        .args(args)
        .output()
        .expect("run rc")
}

#[test]
fn directional_aliases_treat_slash_paths_on_the_local_side_as_local() {
    let sandbox = tempfile::tempdir().expect("create sandbox");

    let get = run_rc(
        sandbox.path(),
        &[
            "get",
            "unconfigured-alias/reports/report.txt",
            "downloads/report.txt",
        ],
    );
    assert_eq!(get.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&get.stderr).contains("Alias 'unconfigured-alias' not found"));

    let put = run_rc(
        sandbox.path(),
        &[
            "put",
            "uploads/report.txt",
            "unconfigured-alias/reports/report.txt",
        ],
    );
    assert_eq!(put.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&put.stderr).contains("Source not found: uploads/report.txt"));
}

#[test]
fn put_uses_the_same_missing_source_behavior_as_cp() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let missing = sandbox.path().join("missing-source.txt");
    let source = missing.to_string_lossy().into_owned();
    let args = [source.as_str(), "unconfigured-alias/reports/report.txt"];

    let cp = run_rc(sandbox.path(), &["cp", args[0], args[1]]);
    let put = run_rc(sandbox.path(), &["put", args[0], args[1]]);

    assert_eq!(put.status.code(), cp.status.code());
    assert_eq!(put.stdout, cp.stdout);
    assert_eq!(put.stderr, cp.stderr);
    assert_eq!(put.status.code(), Some(5));
}

#[test]
fn get_uses_the_same_missing_alias_behavior_as_cp() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let target = sandbox.path().join("report.txt");
    let target = target.to_string_lossy().into_owned();
    let args = ["unconfigured-alias/reports/report.txt", target.as_str()];

    let cp = run_rc(sandbox.path(), &["cp", args[0], args[1]]);
    let get = run_rc(sandbox.path(), &["get", args[0], args[1]]);

    assert_eq!(get.status.code(), cp.status.code());
    assert_eq!(get.stdout, cp.stdout);
    assert_eq!(get.stderr, cp.stderr);
    assert_eq!(get.status.code(), Some(5));
}

#[test]
fn direction_mismatches_fail_with_usage_before_remote_access() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let local_source = sandbox.path().join("source.txt");
    std::fs::write(&local_source, "payload").expect("write local source");
    let local_target = sandbox.path().join("target.txt");
    let source = local_source.to_string_lossy().into_owned();
    let target = local_target.to_string_lossy().into_owned();

    let get = run_rc(sandbox.path(), &["get", source.as_str(), target.as_str()]);
    assert_eq!(get.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&get.stderr)
            .contains("get requires exactly one remote source and one local target")
    );

    let put = run_rc(
        sandbox.path(),
        &[
            "put",
            "unconfigured-alias/reports/report.txt",
            target.as_str(),
        ],
    );
    assert_eq!(put.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&put.stderr)
            .contains("put requires one or more local sources and one remote target")
    );
}

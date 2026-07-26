#![cfg(not(windows))]

mod admin_support;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use admin_support::{rc_binary, rc_host_alias};
use serde_json::Value;

const ACCESS_KEY: &str = "ACCESS_KEY";
const SECRET_KEY: &str = "SECRET_KEY";
const ALIAS: &str = "realilm";
const BUCKET: &str = "rc-manual-transition-real-backend";
const RUSTFS_BINARY_ENV: &str = "RC_REAL_RUSTFS_BINARY";

struct RustfsProcess {
    child: Child,
}

impl Drop for RustfsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires RC_REAL_RUSTFS_BINARY pointing at a rustfs binary"]
fn ilm_transition_status_wait_cancel_hits_real_rustfs_backend() {
    let Some(rustfs_binary) = std::env::var_os(RUSTFS_BINARY_ENV).map(PathBuf::from) else {
        eprintln!("skipping real backend ILM transition CLI e2e; set {RUSTFS_BINARY_ENV}");
        return;
    };
    assert!(
        rustfs_binary.is_file(),
        "{RUSTFS_BINARY_ENV} must point at a rustfs binary: {}",
        rustfs_binary.display()
    );

    let config_dir = tempfile::tempdir().expect("create rc config dir");
    let data_dir = tempfile::tempdir().expect("create rustfs data dir");
    let address = free_loopback_addr();
    let endpoint = format!("http://{address}");
    let mut rustfs = start_rustfs(&rustfs_binary, &address, data_dir.path());
    wait_for_rc_ready(config_dir.path(), &endpoint);

    run_rc_success(
        config_dir.path(),
        &endpoint,
        &["mb", &format!("{ALIAS}/{BUCKET}")],
        "create test bucket",
    );
    let run = run_rc_success(
        config_dir.path(),
        &endpoint,
        &[
            "--json",
            "admin",
            "ilm",
            "transition",
            "run",
            ALIAS,
            BUCKET,
            "--async",
            "--max-objects",
            "1",
        ],
        "start async manual transition job",
    );
    let run_json: Value = serde_json::from_slice(&run.stdout).expect("run stdout should be JSON");
    assert_eq!(run_json["type"], "manual_transition_run");
    assert_eq!(run_json["data"]["state"], "accepted");
    assert_eq!(run_json["data"]["mode"], "durable_job");
    let job_id = run_json["data"]["job_id"]
        .as_str()
        .expect("async run must return job_id");
    assert!(!job_id.is_empty(), "async run must return non-empty job_id");
    assert_eq!(
        run_json["data"]["status_endpoint"],
        run_json["data"]["cancel_endpoint"]
    );

    let status = run_rc_success(
        config_dir.path(),
        &endpoint,
        &[
            "--json",
            "admin",
            "ilm",
            "transition",
            "status",
            ALIAS,
            job_id,
        ],
        "read manual transition job status",
    );
    let status_json: Value =
        serde_json::from_slice(&status.stdout).expect("status stdout should be JSON");
    assert_eq!(status_json["type"], "manual_transition_job_status");
    assert_eq!(status_json["data"]["job_id"], job_id);

    let wait = run_rc_success(
        config_dir.path(),
        &endpoint,
        &[
            "--json",
            "admin",
            "ilm",
            "transition",
            "wait",
            ALIAS,
            job_id,
            "--poll-interval-seconds",
            "1",
            "--timeout-seconds",
            "30",
        ],
        "wait for manual transition job",
    );
    let wait_json: Value =
        serde_json::from_slice(&wait.stdout).expect("wait stdout should be JSON");
    assert_eq!(wait_json["type"], "manual_transition_job_wait");
    assert_eq!(wait_json["data"]["job_id"], job_id);
    assert!(
        matches!(
            wait_json["data"]["status"].as_str(),
            Some("completed" | "partial")
        ),
        "wait should return a successful terminal state: {wait_json}"
    );

    let cancel = run_rc_success(
        config_dir.path(),
        &endpoint,
        &[
            "--json",
            "admin",
            "ilm",
            "transition",
            "cancel",
            ALIAS,
            job_id,
        ],
        "cancel/query terminal manual transition job",
    );
    let cancel_json: Value =
        serde_json::from_slice(&cancel.stdout).expect("cancel stdout should be JSON");
    assert_eq!(cancel_json["type"], "manual_transition_job_cancel");
    assert_eq!(cancel_json["data"]["job_id"], job_id);
    assert!(
        matches!(
            cancel_json["data"]["status"].as_str(),
            Some("completed" | "partial" | "failed" | "unknown")
        ),
        "terminal cancel must not rewrite a finished job into cancelled: {cancel_json}"
    );

    let _ = rustfs.child.kill();
}

fn start_rustfs(binary: &Path, address: &SocketAddr, data_dir: &Path) -> RustfsProcess {
    let child = Command::new(binary)
        .args([
            "--address",
            &address.to_string(),
            "--access-key",
            ACCESS_KEY,
            "--secret-key",
            SECRET_KEY,
            data_dir
                .to_str()
                .expect("temp data dir path should be UTF-8"),
        ])
        .env("RUSTFS_CONSOLE_ENABLE", "false")
        .env("RUSTFS_SCANNER_ENABLED", "false")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start rustfs process");
    RustfsProcess { child }
}

fn wait_for_rc_ready(config_dir: &Path, endpoint: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let output = run_rc(config_dir, endpoint, &["ls", ALIAS]);
        if output.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rustfs backend did not become ready: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(250));
    }
}

fn run_rc_success(config_dir: &Path, endpoint: &str, args: &[&str], context: &str) -> Output {
    let output = run_rc(config_dir, endpoint, args);
    assert!(
        output.status.success(),
        "{context} failed: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_rc(config_dir: &Path, endpoint: &str, args: &[&str]) -> Output {
    Command::new(rc_binary())
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .env(format!("RC_HOST_{ALIAS}"), rc_host_alias(endpoint))
        .output()
        .expect("run rc command")
}

fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind free loopback port");
    listener.local_addr().expect("read loopback address")
}

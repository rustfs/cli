use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn rc_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rc"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    command
}

fn run_rc(args: &[&str], config_dir: &Path) -> Output {
    rc_command()
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .output()
        .expect("run rc")
}

fn run_rc_with_alias(args: &[&str], config_dir: &Path, env_alias: &str) -> Output {
    rc_command()
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .env("RC_HOST_local", env_alias)
        .output()
        .expect("run rc")
}

fn start_s3_test_server(response_body: &'static str) -> (String, Receiver<String>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind S3 test server");
    listener
        .set_nonblocking(true)
        .expect("set S3 test server nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept S3 request: {error}"),
            }
        };
        // Accepted sockets can inherit the listener's nonblocking mode on some
        // platforms. Request reads must block so the child has time to write.
        stream
            .set_nonblocking(false)
            .expect("set S3 request stream blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set request read timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let bytes_read = stream.read(&mut buffer).expect("read S3 request");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).expect("request should be UTF-8");
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request target")
            .to_string();
        sender.send(target).expect("send request target");

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write S3 response");
    });

    let authority = endpoint
        .split_once("://")
        .expect("endpoint should have a scheme")
        .1;
    (
        format!("http://ACCESS_KEY:SECRET_KEY@{authority}"),
        receiver,
        handle,
    )
}

const EMPTY_MULTIPART_LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>bucket</Bucket>
  <MaxUploads>1000</MaxUploads>
  <IsTruncated>false</IsTruncated>
</ListMultipartUploadsResult>"#;

const SINGLE_MULTIPART_LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>bucket</Bucket>
  <MaxUploads>1000</MaxUploads>
  <IsTruncated>false</IsTruncated>
  <Upload>
    <Key>object.bin</Key>
    <UploadId>upload-1</UploadId>
    <StorageClass>STANDARD</StorageClass>
    <Initiated>2026-07-21T04:00:00.000Z</Initiated>
  </Upload>
</ListMultipartUploadsResult>"#;

#[test]
fn ls_incomplete_requires_an_exact_object_before_alias_resolution() {
    let config_dir = tempfile::tempdir().expect("create config directory");

    let output = run_rc(
        &["ls", "local", "--incomplete", "--json"],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["schema_version"], 3);
    assert_eq!(stdout["status"], "error");
    assert_eq!(stdout["error"]["type"], "usage_error");
}

#[test]
fn rm_incomplete_rejects_conflicting_destructive_modes() {
    let config_dir = tempfile::tempdir().expect("create config directory");

    let output = run_rc(
        &[
            "rm",
            "local/bucket/object.bin",
            "--incomplete",
            "--versions",
            "--json",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["schema_version"], 3);
    assert_eq!(stdout["status"], "error");
    assert_eq!(stdout["error"]["type"], "usage_error");
}

#[test]
fn rm_incomplete_returns_not_found_for_an_unknown_alias() {
    let config_dir = tempfile::tempdir().expect("create config directory");

    let output = run_rc(
        &[
            "rm",
            "missing-alias/bucket/object.bin",
            "--incomplete",
            "--json",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["schema_version"], 3);
    assert_eq!(stdout["status"], "error");
    assert_eq!(stdout["error"]["type"], "not_found");
    assert!(
        stdout["error"]["message"]
            .as_str()
            .expect("error should be a string")
            .contains("not found")
    );
}

#[test]
fn ls_incomplete_reports_an_empty_server_listing() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let (env_alias, request_receiver, server_handle) = start_s3_test_server(EMPTY_MULTIPART_LIST);

    let output = run_rc_with_alias(
        &["ls", "local/bucket/object.bin", "--incomplete", "--json"],
        config_dir.path(),
        &env_alias,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["schema_version"], 3);
    assert_eq!(stdout["type"], "multipart_uploads");
    assert_eq!(stdout["status"], "success");
    assert_eq!(stdout["data"]["items"], serde_json::json!([]));
    assert_eq!(stdout["data"]["pagination"]["truncated"], false);
    let target = request_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("capture multipart listing request");
    assert!(target.contains("uploads"), "unexpected target: {target}");
    server_handle.join().expect("S3 server should finish");
}

#[test]
fn ls_incomplete_quiet_mode_suppresses_success_output() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let (env_alias, request_receiver, server_handle) = start_s3_test_server(EMPTY_MULTIPART_LIST);

    let output = run_rc_with_alias(
        &[
            "ls",
            "local/bucket/object.bin",
            "--incomplete",
            "--quiet",
            "--json",
        ],
        config_dir.path(),
        &env_alias,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let target = request_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("capture multipart listing request");
    assert!(target.contains("uploads"), "unexpected target: {target}");
    server_handle.join().expect("S3 server should finish");
}

#[test]
fn ls_incomplete_human_output_contains_upload_metadata_table() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let (env_alias, request_receiver, server_handle) = start_s3_test_server(SINGLE_MULTIPART_LIST);

    let output = run_rc_with_alias(
        &[
            "ls",
            "local/bucket/object.bin",
            "--incomplete",
            "--format",
            "human",
            "--no-color",
        ],
        config_dir.path(),
        &env_alias,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("UPLOAD ID"));
    assert!(stdout.contains("STORAGE CLASS"));
    assert!(stdout.contains("upload-1"));
    assert!(stdout.contains("object.bin"));
    let target = request_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("capture multipart listing request");
    assert!(target.contains("uploads"), "unexpected target: {target}");
    server_handle.join().expect("S3 server should finish");
}

#[test]
fn rustfs_beta_10_exact_key_dry_run_never_sends_abort_requests() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let (env_alias, request_receiver, server_handle) = start_s3_test_server(SINGLE_MULTIPART_LIST);

    let output = run_rc_with_alias(
        &[
            "rm",
            "local/bucket/object.bin",
            "--incomplete",
            "--dry-run",
            "--json",
        ],
        config_dir.path(),
        &env_alias,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["schema_version"], 3);
    assert_eq!(stdout["status"], "success");
    assert_eq!(stdout["data"]["operation"], "abort");
    assert_eq!(stdout["data"]["dry_run"], true);
    assert_eq!(
        stdout["data"]["results"][0]["upload"]["upload_id"],
        "upload-1"
    );
    assert_eq!(stdout["data"]["results"][0]["state"], "would_abort");
    let target = request_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("capture multipart listing request");
    assert!(target.contains("uploads"), "unexpected target: {target}");
    assert!(
        target.contains("prefix=object.bin"),
        "RustFS beta.10 exact-key contract requires the object as prefix: {target}"
    );
    server_handle.join().expect("S3 server should finish");
}

#[test]
fn ls_incomplete_bucket_mode_is_rejected_before_alias_resolution() {
    let config_dir = tempfile::tempdir().expect("create config directory");

    let output = run_rc(
        &["ls", "missing/bucket", "--incomplete", "--json"],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["status"], "error");
    assert_eq!(stdout["error"]["type"], "unsupported_feature");
    assert_eq!(
        stdout["error"]["capability"],
        "list_multipart_uploads_prefix"
    );
    assert!(
        stdout["error"]["message"]
            .as_str()
            .expect("message should be a string")
            .contains("rustfs/backlog#1384")
    );
}

#[test]
fn rm_incomplete_recursive_mode_is_rejected_before_alias_resolution() {
    let config_dir = tempfile::tempdir().expect("create config directory");

    let output = run_rc(
        &[
            "rm",
            "missing/bucket/logs/",
            "--incomplete",
            "--recursive",
            "--json",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    assert_eq!(stdout["status"], "error");
    assert_eq!(stdout["error"]["type"], "unsupported_feature");
    assert_eq!(
        stdout["error"]["capability"],
        "list_multipart_uploads_prefix"
    );
}

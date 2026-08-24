//! End-to-end mirror planning tests backed by a read-only mock S3 endpoint.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

fn rc_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rc"))
}

fn run_rc(args: &[&str], config_dir: &Path, endpoint: Option<&str>) -> Output {
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("RC_CONFIG_DIR", config_dir);
    if let Some(endpoint) = endpoint {
        command.env(
            "RC_HOST_test",
            format!(
                "http://accesskey:secretkey@{}",
                endpoint.trim_start_matches("http://")
            ),
        );
    }
    command.output().expect("execute rc")
}

fn start_list_server(
    expected_requests: usize,
    paginated_source: bool,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock S3 endpoint");
    listener
        .set_nonblocking(true)
        .expect("configure nonblocking listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("mock endpoint"));
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        let mut source_page = 0usize;
        while requests.len() < expected_requests && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept mock S3 request: {error}"),
            };
            stream
                .set_nonblocking(false)
                .expect("configure blocking mock connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set request timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let read = stream.read(&mut chunk).expect("read mock S3 request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request).into_owned();
            let request_line = request.lines().next().unwrap_or_default().to_string();
            let body = if request_line.contains("/source-bucket") && paginated_source {
                let response = if source_page == 0 {
                    paginated_source_first_response()
                } else {
                    paginated_source_second_response()
                };
                source_page += 1;
                response
            } else if request_line.contains("/source-bucket") {
                source_list_response()
            } else {
                empty_list_response()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write mock S3 response");
            requests.push(request_line);
        }
        requests
    });
    (endpoint, handle)
}

fn source_list_response() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>source-bucket</Name>
  <Prefix>source/</Prefix>
  <KeyCount>1</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>source/nested/file.txt</Key>
    <LastModified>2026-07-21T04:00:00.000Z</LastModified>
    <ETag>&quot;source-etag&quot;</ETag>
    <Size>4</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
</ListBucketResult>"#
}

fn empty_list_response() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>target-bucket</Name>
  <Prefix>backup/</Prefix>
  <KeyCount>0</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
</ListBucketResult>"#
}

fn paginated_source_first_response() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>source-bucket</Name>
  <Prefix>source/</Prefix>
  <KeyCount>1</KeyCount>
  <MaxKeys>1</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>page-2</NextContinuationToken>
  <Contents>
    <Key>source/a.txt</Key>
    <LastModified>2026-07-21T04:00:00.000Z</LastModified>
    <ETag>&quot;etag-a&quot;</ETag>
    <Size>1</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
</ListBucketResult>"#
}

fn paginated_source_second_response() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>source-bucket</Name>
  <Prefix>source/</Prefix>
  <KeyCount>1</KeyCount>
  <MaxKeys>1</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>source/z.txt</Key>
    <LastModified>2026-07-21T04:00:01.000Z</LastModified>
    <ETag>&quot;etag-z&quot;</ETag>
    <Size>1</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
</ListBucketResult>"#
}

fn assert_read_only_list_requests(
    handle: thread::JoinHandle<Vec<String>>,
    expected_requests: usize,
) {
    let requests = handle.join().expect("join mock S3 endpoint");
    assert_eq!(requests.len(), expected_requests, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("GET ") && request.contains("list-type=2")),
        "dry-run sent a mutating or unexpected request: {requests:?}"
    );
}

fn parse_success(output: &Output) -> serde_json::Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("mirror output is JSON")
}

#[test]
fn local_to_remote_dry_run_plans_nested_paths_without_mutation() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let source = tempfile::tempdir().expect("create local source");
    std::fs::create_dir(source.path().join("nested")).expect("create nested source");
    std::fs::write(source.path().join("nested/file.txt"), b"data").expect("write local source");
    let (endpoint, handle) = start_list_server(1, false);

    let output = run_rc(
        &[
            "--json",
            "mirror",
            source.path().to_str().expect("source path is UTF-8"),
            "test/target-bucket/backup/",
            "--dry-run",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["dry_run"], true);
    assert_read_only_list_requests(handle, 1);
}

#[test]
fn remote_to_local_dry_run_does_not_create_the_target_root() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let target_parent = tempfile::tempdir().expect("create target parent");
    let target = target_parent.path().join("new/restore");
    let (endpoint, handle) = start_list_server(1, false);

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            target.to_str().expect("target path is UTF-8"),
            "--dry-run",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["dry_run"], true);
    assert!(!target.exists());
    assert_read_only_list_requests(handle, 1);
}

#[test]
fn remote_to_remote_dry_run_reads_both_manifests_without_mutation() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, handle) = start_list_server(2, false);

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            "test/target-bucket/backup/",
            "--dry-run",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["removed"], 0);
    assert_eq!(payload["dry_run"], true);
    assert_read_only_list_requests(handle, 2);
}

#[test]
fn remote_pagination_is_consumed_before_the_deterministic_plan_is_emitted() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let target_parent = tempfile::tempdir().expect("create target parent");
    let target = target_parent.path().join("restore");
    let (endpoint, handle) = start_list_server(2, true);

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            target.to_str().expect("target path is UTF-8"),
            "--dry-run",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 2);
    assert!(!target.exists());
    let requests = handle.join().expect("join paginated mock S3 endpoint");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests[1].contains("continuation-token=page-2"));
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
}

fn destination_list_response_with(etag: &str, size: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>target-bucket</Name>
  <Prefix>backup/</Prefix>
  <KeyCount>1</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>backup/nested/file.txt</Key>
    <LastModified>2026-07-21T04:00:00.000Z</LastModified>
    <ETag>&quot;{etag}&quot;</ETag>
    <Size>{size}</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
</ListBucketResult>"#
    )
}

fn start_identity_server(
    expected_requests: usize,
    destination_etag: &'static str,
    identity_etag: Option<&'static str>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    start_identity_server_with(expected_requests, destination_etag, identity_etag, 4)
}

fn start_identity_server_with(
    expected_requests: usize,
    destination_etag: &'static str,
    identity_etag: Option<&'static str>,
    destination_size: u64,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock S3 endpoint");
    listener
        .set_nonblocking(true)
        .expect("configure nonblocking listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("mock endpoint"));
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        while requests.len() < expected_requests && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept mock S3 request: {error}"),
            };
            stream
                .set_nonblocking(false)
                .expect("configure blocking mock connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set request timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let read = stream.read(&mut chunk).expect("read mock S3 request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request).into_owned();
            let request_line = request.lines().next().unwrap_or_default().to_string();
            let response = if request_line.starts_with("HEAD ") {
                let mut headers = vec![
                    "HTTP/1.1 200 OK".to_string(),
                    "content-length: 4".to_string(),
                    format!("etag: \"{destination_etag}\""),
                    "last-modified: Tue, 21 Jul 2026 04:00:00 GMT".to_string(),
                    "connection: close".to_string(),
                ];
                if let Some(identity_etag) = identity_etag {
                    headers.push(format!("x-amz-meta-rc-source-etag: {identity_etag}"));
                }
                format!("{}\r\n\r\n", headers.join("\r\n"))
            } else {
                let body = if request_line.contains("/source-bucket") {
                    source_list_response().to_string()
                } else {
                    destination_list_response_with(destination_etag, destination_size)
                };
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
            };
            stream
                .write_all(response.as_bytes())
                .expect("write mock S3 response");
            requests.push(request_line);
        }
        requests
    });
    (endpoint, handle)
}

fn assert_read_only_identity_requests(
    handle: thread::JoinHandle<Vec<String>>,
    expected_requests: usize,
) {
    let requests = handle.join().expect("join mock S3 endpoint");
    assert_eq!(requests.len(), expected_requests, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|request| { request.starts_with("GET ") || request.starts_with("HEAD ") }),
        "identity planning sent a mutating request: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("HEAD ") && request.contains("target-bucket")),
        "auto compare should HeadObject the destination: {requests:?}"
    );
}

#[test]
fn remote_to_remote_auto_compare_skips_when_destination_preserves_source_etag() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, handle) = start_identity_server(3, "multipart-etag-1", Some("source-etag"));

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            "test/target-bucket/backup/",
            "--overwrite",
            "--dry-run",
            "--compare",
            "auto",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 0);
    assert_eq!(payload["skipped"], 1);
    assert_eq!(payload["dry_run"], true);
    assert_read_only_identity_requests(handle, 3);
}

#[test]
fn remote_to_remote_etag_compare_recopies_when_stored_etags_differ() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, handle) = start_identity_server(2, "multipart-etag-1", Some("source-etag"));

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            "test/target-bucket/backup/",
            "--overwrite",
            "--dry-run",
            "--compare",
            "etag",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["dry_run"], true);
    let requests = handle.join().expect("join mock S3 endpoint");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests.iter().all(|request| request.starts_with("GET ")),
        "etag compare should not HeadObject destinations: {requests:?}"
    );
}

#[test]
fn remote_to_remote_auto_compare_recopies_when_destination_identity_is_missing() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, handle) = start_identity_server(3, "multipart-etag-1", None);

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            "test/target-bucket/backup/",
            "--overwrite",
            "--dry-run",
            "--compare",
            "auto",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["skipped"], 0);
    assert_eq!(payload["dry_run"], true);
    assert_read_only_identity_requests(handle, 3);
}

#[test]
fn remote_to_remote_auto_compare_recopies_when_destination_identity_mismatches() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, handle) = start_identity_server(3, "multipart-etag-1", Some("other-etag"));

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            "test/target-bucket/backup/",
            "--overwrite",
            "--dry-run",
            "--compare",
            "auto",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["skipped"], 0);
    assert_eq!(payload["dry_run"], true);
    assert_read_only_identity_requests(handle, 3);
}

#[test]
fn remote_to_remote_auto_compare_recopies_when_sizes_differ_without_head() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, handle) = start_identity_server_with(2, "multipart-etag-1", None, 8);

    let output = run_rc(
        &[
            "--json",
            "mirror",
            "test/source-bucket/source/",
            "test/target-bucket/backup/",
            "--overwrite",
            "--dry-run",
            "--compare",
            "auto",
        ],
        config_dir.path(),
        Some(&endpoint),
    );

    let payload = parse_success(&output);
    assert_eq!(payload["copied"], 1);
    assert_eq!(payload["skipped"], 0);
    assert_eq!(payload["dry_run"], true);
    let requests = handle.join().expect("join mock S3 endpoint");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests.iter().all(|request| request.starts_with("GET ")),
        "size mismatch should not HeadObject destinations: {requests:?}"
    );
}

#[test]
fn local_to_local_is_rejected_with_the_unsupported_exit_code() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let source = tempfile::tempdir().expect("create source root");
    let target = tempfile::tempdir().expect("create target root");

    let output = run_rc(
        &[
            "--json",
            "mirror",
            source.path().to_str().expect("source path is UTF-8"),
            target.path().to_str().expect("target path is UTF-8"),
        ],
        config_dir.path(),
        None,
    );

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("mirror error is JSON");
    assert_eq!(payload["code"], 7);
    assert!(
        payload["error"]
            .as_str()
            .is_some_and(|message| message.contains("Local-to-local mirror is out of scope"))
    );
}

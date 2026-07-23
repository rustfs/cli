#![cfg(not(windows))]

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
}

#[derive(Debug)]
struct Response {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Response {
    fn xml(body: String) -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            body,
        }
    }

    fn empty() -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn head(size: usize) -> Self {
        Self::head_with_etag(size, "destination-etag")
    }

    fn head_with_etag(size: usize, etag: &str) -> Self {
        Self {
            status: "200 OK",
            headers: vec![
                ("content-length", size.to_string()),
                ("etag", format!("\"{etag}\"")),
            ],
            body: String::new(),
        }
    }

    fn access_denied() -> Self {
        Self {
            status: "403 Forbidden",
            headers: Vec::new(),
            body: "<Error><Code>AccessDenied</Code><Message>denied</Message></Error>".to_string(),
        }
    }
}

type Handler = dyn Fn(&Request) -> Response + Send + Sync + 'static;

struct S3Mock {
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl S3Mock {
    fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind S3 mock");
        listener
            .set_nonblocking(true)
            .expect("set S3 mock nonblocking");
        let address = listener.local_addr().expect("S3 mock address");
        let endpoint = format!("http://{address}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        let handler: Arc<Handler> = Arc::new(handler);

        let handle = thread::spawn(move || {
            while !shutdown_for_thread.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted S3 connection blocking");
                        let requests = Arc::clone(&requests_for_thread);
                        let handler = Arc::clone(&handler);
                        thread::spawn(move || {
                            let mut pending = Vec::new();
                            while let Some(request) = read_request(&mut stream, &mut pending) {
                                requests
                                    .lock()
                                    .expect("record S3 request")
                                    .push(request.clone());
                                write_response(&mut stream, handler(&request));
                            }
                        });
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept S3 request: {error}"),
                }
            }
        });

        Self {
            endpoint,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("read S3 requests").clone()
    }
}

impl Drop for S3Mock {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("S3 mock thread should finish");
        }
    }
}

fn read_request(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Option<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("set S3 request timeout");
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        if let Some(position) = pending.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        let read = match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                return None;
            }
            Err(error) => panic!("read S3 request: {error}"),
        };
        pending.extend_from_slice(&buffer[..read]);
    };
    let header_text =
        String::from_utf8(pending[..header_end].to_vec()).expect("S3 headers should be UTF-8");
    let mut lines = header_text.lines();
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next()?.to_string();
    let target = request_parts.next()?.to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect::<BTreeMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while pending.len().saturating_sub(header_end) < content_length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        pending.extend_from_slice(&buffer[..read]);
    }
    pending.drain(..header_end + content_length);

    Some(Request {
        method,
        target,
        headers,
    })
}

fn write_response(stream: &mut TcpStream, response: Response) {
    let has_content_length = response
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    let mut head = format!(
        "HTTP/1.1 {}\r\ncontent-type: application/xml\r\nconnection: keep-alive\r\n",
        response.status
    );
    if !has_content_length {
        head.push_str(&format!("content-length: {}\r\n", response.body.len()));
    }
    for (name, value) in response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .expect("write S3 response headers");
    stream
        .write_all(response.body.as_bytes())
        .expect("write S3 response body");
    stream.flush().expect("flush S3 response");
}

fn rc_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return PathBuf::from(path);
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate parent")
        .parent()
        .expect("workspace root")
        .to_path_buf();
    workspace_root.join("target/debug/rc")
}

fn run_rc(mock: &S3Mock, args: &[&str]) -> Output {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let authority = mock
        .endpoint
        .strip_prefix("http://")
        .expect("mock endpoint has scheme");
    let alias = format!("http://ACCESS_KEY:SECRET_KEY@{authority}");
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_test", alias)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .output()
        .expect("run rc command")
}

fn run_rc_with_stdin(mock: &S3Mock, args: &[&str], input: &[u8]) -> Output {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let authority = mock
        .endpoint
        .strip_prefix("http://")
        .expect("mock endpoint has scheme");
    let alias = format!("http://ACCESS_KEY:SECRET_KEY@{authority}");
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args(args)
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_test", alias)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rc command");
    child
        .stdin
        .take()
        .expect("pipe command stdin")
        .write_all(input)
        .expect("write pipe input");
    child.wait_with_output().expect("wait for rc command")
}

fn list_result(keys: &[String], truncated: bool, next_token: Option<&str>) -> Response {
    let contents = keys
        .iter()
        .map(|key| {
            format!(
                "<Contents><Key>{key}</Key><Size>1</Size><ETag>\"source-etag\"</ETag></Contents>"
            )
        })
        .collect::<String>();
    let next_token = next_token
        .map(|token| format!("<NextContinuationToken>{token}</NextContinuationToken>"))
        .unwrap_or_default();
    Response::xml(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>source</Name><Prefix>src/</Prefix>{contents}\
         <IsTruncated>{truncated}</IsTruncated>{next_token}</ListBucketResult>"
    ))
}

fn one_object_list() -> Response {
    list_result(&["src/a.txt".to_string()], false, None)
}

fn large_object_list(size: u64) -> Response {
    Response::xml(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>source</Name><Prefix>src/</Prefix>\
         <Contents><Key>src/large.bin</Key><Size>{size}</Size><ETag>\"source-etag\"</ETag></Contents>\
         <IsTruncated>false</IsTruncated></ListBucketResult>"
    ))
}

fn copy_result() -> Response {
    Response::xml(
        "<CopyObjectResult><ETag>\"destination-etag\"</ETag>\
         <LastModified>2026-07-23T00:00:00Z</LastModified></CopyObjectResult>"
            .to_string(),
    )
}

fn is_list_request(request: &Request) -> bool {
    request.method == "GET"
        && (request.target.starts_with("/source?") || request.target.starts_with("/source/?"))
        && request.target.contains("list-type=2")
}

fn is_copy_request(request: &Request) -> bool {
    request.method == "PUT" && request.headers.contains_key("x-amz-copy-source")
}

#[test]
fn single_copy_and_pipe_send_supported_storage_classes() {
    let mock = S3Mock::start(|request| {
        if request.method == "HEAD" && request.target == "/source/a.txt" {
            return Response::head_with_etag(7, "source-etag");
        }
        if is_copy_request(request) {
            return copy_result();
        }
        if request.method == "HEAD" && request.target == "/destination/b.txt" {
            return Response {
                status: "200 OK",
                headers: vec![
                    ("content-length", "7".to_string()),
                    ("etag", "\"destination-etag\"".to_string()),
                    ("x-amz-storage-class", "REDUCED_REDUNDANCY".to_string()),
                ],
                body: String::new(),
            };
        }
        if request.method == "PUT" && request.target.starts_with("/destination/pipe.txt") {
            return Response::empty();
        }
        if request.method == "PUT" && request.target.starts_with("/destination/upload.txt") {
            return Response::empty();
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let copy = run_rc(
        &mock,
        &[
            "cp",
            "test/source/a.txt",
            "test/destination/b.txt",
            "--storage-class",
            "REDUCED_REDUNDANCY",
        ],
    );
    assert!(
        copy.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&copy.stdout),
        String::from_utf8_lossy(&copy.stderr)
    );

    let pipe = run_rc_with_stdin(
        &mock,
        &[
            "pipe",
            "test/destination/pipe.txt",
            "--storage-class",
            "STANDARD",
        ],
        b"payload",
    );
    assert!(
        pipe.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&pipe.stdout),
        String::from_utf8_lossy(&pipe.stderr),
        mock.requests()
    );

    let source_dir = tempfile::tempdir().expect("create upload source directory");
    let source = source_dir.path().join("upload.txt");
    std::fs::write(&source, b"payload").expect("write upload source");
    let upload = run_rc(
        &mock,
        &[
            "cp",
            source.to_str().expect("source path is valid UTF-8"),
            "test/destination/upload.txt",
            "--storage-class",
            "STANDARD",
        ],
    );
    assert!(
        upload.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&upload.stdout),
        String::from_utf8_lossy(&upload.stderr),
        mock.requests()
    );

    let requests = mock.requests();
    let copy_request = requests
        .iter()
        .find(|request| is_copy_request(request))
        .expect("copy request");
    assert_eq!(
        copy_request.headers.get("x-amz-storage-class"),
        Some(&"REDUCED_REDUNDANCY".to_string())
    );
    let pipe_request = requests
        .iter()
        .find(|request| {
            request.method == "PUT" && request.target.starts_with("/destination/pipe.txt")
        })
        .expect("pipe put request");
    assert_eq!(
        pipe_request.headers.get("x-amz-storage-class"),
        Some(&"STANDARD".to_string())
    );
    let upload_request = requests
        .iter()
        .find(|request| {
            request.method == "PUT" && request.target.starts_with("/destination/upload.txt")
        })
        .expect("local upload put request");
    assert_eq!(
        upload_request.headers.get("x-amz-storage-class"),
        Some(&"STANDARD".to_string())
    );
}

#[test]
fn transfer_fidelity_options_reach_copy_upload_and_pipe_requests() {
    let mock = S3Mock::start(|request| {
        if request.method == "HEAD" && request.target == "/source/a.txt" {
            return Response::head_with_etag(7, "source-etag");
        }
        if is_copy_request(request) {
            return copy_result();
        }
        if request.method == "HEAD" && request.target == "/destination/preserved.txt" {
            return Response::head_with_etag(7, "destination-etag");
        }
        if request.method == "HEAD" && request.target == "/destination/upload.txt" {
            return Response {
                status: "200 OK",
                headers: vec![
                    ("content-length", "7".to_string()),
                    ("etag", "\"destination-etag\"".to_string()),
                    (
                        "x-amz-checksum-sha256",
                        "I59Z7VXnN8dxR89VrQwbAwttfudIp0JpUvm4UtWpNeU=".to_string(),
                    ),
                ],
                body: String::new(),
            };
        }
        if request.method == "PUT"
            && (request.target.starts_with("/destination/upload.txt")
                || request.target.starts_with("/destination/pipe.txt"))
        {
            return Response::empty();
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let copy = run_rc(
        &mock,
        &[
            "cp",
            "test/source/a.txt",
            "test/destination/preserved.txt",
            "--preserve",
            "--retention-mode",
            "GOVERNANCE",
            "--retain-until",
            "2099-01-02T03:04:05Z",
            "--legal-hold",
            "ON",
        ],
    );
    assert!(
        copy.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&copy.stdout),
        String::from_utf8_lossy(&copy.stderr),
        mock.requests()
    );

    let source_dir = tempfile::tempdir().expect("create upload source directory");
    let source = source_dir.path().join("upload.txt");
    std::fs::write(&source, b"payload").expect("write upload source");
    let upload = run_rc(
        &mock,
        &[
            "cp",
            source.to_str().expect("source path is valid UTF-8"),
            "test/destination/upload.txt",
            "--cache-control",
            "max-age=3600",
            "--content-language",
            "en-US",
            "--expires",
            "2099-01-02T03:04:05Z",
            "--metadata",
            "owner=analytics",
            "--tags",
            "env=prod",
            "--checksum",
            "sha256",
            "--retention-mode",
            "GOVERNANCE",
            "--retain-until",
            "2099-01-02T03:04:05Z",
            "--legal-hold",
            "ON",
        ],
    );
    assert!(
        upload.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&upload.stdout),
        String::from_utf8_lossy(&upload.stderr),
        mock.requests()
    );

    let pipe = run_rc_with_stdin(
        &mock,
        &[
            "pipe",
            "test/destination/pipe.txt",
            "--cache-control",
            "no-cache",
            "--metadata",
            "source=stdin",
            "--tags",
            "stream=true",
        ],
        b"payload",
    );
    assert!(
        pipe.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&pipe.stdout),
        String::from_utf8_lossy(&pipe.stderr),
        mock.requests()
    );

    let requests = mock.requests();
    let copy_request = requests
        .iter()
        .find(|request| is_copy_request(request))
        .expect("copy request");
    assert_eq!(
        copy_request.headers.get("x-amz-metadata-directive"),
        Some(&"COPY".to_string())
    );
    assert_eq!(
        copy_request.headers.get("x-amz-object-lock-mode"),
        Some(&"GOVERNANCE".to_string())
    );
    assert_eq!(
        copy_request.headers.get("x-amz-object-lock-legal-hold"),
        Some(&"ON".to_string())
    );

    let upload_request = requests
        .iter()
        .find(|request| {
            request.method == "PUT" && request.target.starts_with("/destination/upload.txt")
        })
        .expect("upload request");
    assert_eq!(
        upload_request.headers.get("cache-control"),
        Some(&"max-age=3600".to_string())
    );
    assert_eq!(
        upload_request.headers.get("content-language"),
        Some(&"en-US".to_string())
    );
    assert_eq!(
        upload_request.headers.get("x-amz-meta-owner"),
        Some(&"analytics".to_string())
    );
    assert_eq!(
        upload_request.headers.get("x-amz-tagging"),
        Some(&"env=prod".to_string())
    );
    assert_eq!(
        upload_request.headers.get("x-amz-sdk-checksum-algorithm"),
        Some(&"SHA256".to_string())
    );
    assert_eq!(
        upload_request.headers.get("x-amz-object-lock-mode"),
        Some(&"GOVERNANCE".to_string())
    );
    assert_eq!(
        upload_request.headers.get("x-amz-object-lock-legal-hold"),
        Some(&"ON".to_string())
    );

    let pipe_request = requests
        .iter()
        .find(|request| {
            request.method == "PUT" && request.target.starts_with("/destination/pipe.txt")
        })
        .expect("pipe request");
    assert_eq!(
        pipe_request.headers.get("cache-control"),
        Some(&"no-cache".to_string())
    );
    assert_eq!(
        pipe_request.headers.get("x-amz-meta-source"),
        Some(&"stdin".to_string())
    );
    assert_eq!(
        pipe_request.headers.get("x-amz-tagging"),
        Some(&"stream=true".to_string())
    );
}

#[test]
fn fidelity_preflight_returns_stable_exit_codes_and_redacts_dry_run_secrets() {
    let mock = S3Mock::start(|_| Response {
        status: "500 Internal Server Error",
        headers: Vec::new(),
        body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
    });

    for args in [
        vec![
            "cp",
            "test/source/a.txt",
            "test/destination/b.txt",
            "--metadata-directive",
            "replace",
        ],
        vec![
            "cp",
            "test/source/a.txt",
            "test/destination/b.txt",
            "--checksum",
            "sha256",
        ],
        vec![
            "cp",
            "test/source/a.txt",
            "test/destination/b.txt",
            "--tagging-directive",
            "copy",
        ],
    ] {
        let output = run_rc(&mock, &args);
        assert_eq!(
            output.status.code(),
            Some(7),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let source_dir = tempfile::tempdir().expect("create local source directory");
    let source = source_dir.path().join("source.txt");
    std::fs::write(&source, b"payload").expect("write local source");
    let invalid_direction = run_rc(
        &mock,
        &[
            "cp",
            source.to_str().expect("source path is valid UTF-8"),
            "test/destination/b.txt",
            "--preserve",
        ],
    );
    assert_eq!(invalid_direction.status.code(), Some(2));

    let secret_path = "/definitely/missing/secret-transfer-key";
    let dry_run = run_rc(
        &mock,
        &[
            "cp",
            "test/source/a.txt",
            "test/destination/b.txt",
            "--preserve",
            "--enc-c-source-key-file",
            secret_path,
            "--enc-c-destination-key-file",
            secret_path,
            "--dry-run",
        ],
    );
    assert!(
        dry_run.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(combined.contains("metadata=copy"));
    assert!(combined.contains("encryption=sse-c"));
    assert!(!combined.contains(secret_path));
    assert!(
        mock.requests().is_empty(),
        "preflight and dry-run paths must not contact S3"
    );
}

#[test]
fn checksum_verification_mismatch_returns_conflict_exit_code() {
    let mock = S3Mock::start(|request| {
        if request.method == "PUT" && request.target.starts_with("/destination/checksum.txt") {
            return Response::empty();
        }
        if request.method == "HEAD" && request.target == "/destination/checksum.txt" {
            return Response {
                status: "200 OK",
                headers: vec![
                    ("content-length", "7".to_string()),
                    ("etag", "\"destination-etag\"".to_string()),
                    (
                        "x-amz-checksum-sha256",
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
                    ),
                ],
                body: String::new(),
            };
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });
    let source_dir = tempfile::tempdir().expect("create checksum source directory");
    let source = source_dir.path().join("checksum.txt");
    std::fs::write(&source, b"payload").expect("write checksum source");

    let output = run_rc(
        &mock,
        &[
            "cp",
            source.to_str().expect("source path is valid UTF-8"),
            "test/destination/checksum.txt",
            "--checksum",
            "sha256",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(6),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_ascii_lowercase()
            .contains("checksum")
    );
}

#[test]
fn storage_class_dry_run_reports_policy_without_copy_mutation() {
    let mock = S3Mock::start(|request| {
        if request.method == "HEAD" && request.target == "/source/a.txt" {
            return Response::head_with_etag(7, "source-etag");
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "test/source/a.txt",
            "test/destination/b.txt",
            "--storage-class",
            "STANDARD",
            "--dry-run",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("[policy:storage-class=STANDARD]"));
    assert!(
        mock.requests()
            .iter()
            .all(|request| !is_copy_request(request))
    );
}

#[test]
fn recursive_same_alias_copy_paginates_and_emits_deterministic_plan() {
    let first_page = (0..1_000)
        .rev()
        .map(|index| format!("src/item-{index:04}.txt"))
        .collect::<Vec<_>>();
    let mock = S3Mock::start(move |request| {
        if is_list_request(request) {
            if request.target.contains("continuation-token=page-2") {
                return list_result(&["src/item-1000.txt".to_string()], false, None);
            }
            return list_result(&first_page, true, Some("page-2"));
        }
        if is_copy_request(request) {
            return copy_result();
        }
        if request.method == "HEAD" && request.target.starts_with("/destination/dst/") {
            return Response::head(1);
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "--dry-run",
            "--concurrency",
            "1",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    let requests = mock.requests();
    let list_requests = requests
        .iter()
        .filter(|request| is_list_request(request))
        .collect::<Vec<_>>();
    assert_eq!(list_requests.len(), 2);
    assert!(
        list_requests[1]
            .target
            .contains("continuation-token=page-2")
    );

    let copies = requests
        .iter()
        .filter(|request| is_copy_request(request))
        .collect::<Vec<_>>();
    assert!(copies.is_empty(), "dry-run pagination must not mutate");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let planned = stdout
        .lines()
        .filter(|line| line.starts_with("Would copy:"))
        .collect::<Vec<_>>();
    assert_eq!(planned.len(), 1_001);
    for (index, line) in planned.iter().enumerate() {
        assert_eq!(
            *line,
            format!(
                "Would copy: test/source/src/item-{index:04}.txt -> test/destination/dst/item-{index:04}.txt"
            )
        );
    }
}

#[test]
fn overlapping_same_bucket_prefix_is_conflict_before_any_copy() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return one_object_list();
        }
        Response::empty()
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "test/archive/src/",
            "test/archive/src/backup/",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(6),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !mock.requests().iter().any(is_copy_request),
        "overlap validation must happen before mutation"
    );
}

#[test]
fn overwrite_false_reports_existing_destination_as_skipped() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return one_object_list();
        }
        if request.method == "HEAD" && request.target == "/destination/dst/a.txt" {
            return Response::head(1);
        }
        if is_copy_request(request) {
            return copy_result();
        }
        Response::empty()
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "--overwrite=false",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    assert!(
        !mock.requests().iter().any(is_copy_request),
        "an existing destination must not be overwritten"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains("skipped"),
        "skip-existing must be explicit in output"
    );
}

#[test]
fn recursive_copy_access_denial_uses_auth_exit_code_without_mutation() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return Response::access_denied();
        }
        Response::empty()
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!mock.requests().iter().any(is_copy_request));
}

#[test]
fn recursive_copy_empty_prefix_is_explicit_and_deterministic() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return list_result(&[], false, None);
        }
        Response::empty()
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Summary: 0 planned, 0 skipped, 0 succeeded, 0 failed, 0 cancelled, 0 B transferred"
    );
    assert!(!mock.requests().iter().any(is_copy_request));
}

#[test]
fn recursive_copy_dry_run_maps_objects_without_mutation() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return list_result(
                &["src/b.txt".to_string(), "src/sub/a.txt".to_string()],
                false,
                None,
            );
        }
        Response::empty()
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "--dry-run",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test/source/src/b.txt -> test/destination/dst/b.txt"));
    assert!(stdout.contains("test/source/src/sub/a.txt -> test/destination/dst/sub/a.txt"));
    assert!(
        mock.requests()
            .iter()
            .all(|request| !is_copy_request(request) && request.method != "DELETE"),
        "dry run must not mutate the destination"
    );
}

#[test]
fn recursive_copy_executes_the_planned_server_side_copy() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return one_object_list();
        }
        if is_copy_request(request) {
            return copy_result();
        }
        if request.method == "HEAD" && request.target == "/destination/dst/a.txt" {
            return Response::head(1);
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "--concurrency",
            "1",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    let requests = mock.requests();
    let copies = requests
        .iter()
        .filter(|request| is_copy_request(request))
        .collect::<Vec<_>>();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].target, "/destination/dst/a.txt?x-id=CopyObject");
    assert_eq!(
        copies[0].headers.get("x-amz-copy-source"),
        Some(&"source/src/a.txt".to_string())
    );
}

#[test]
fn recursive_copy_continue_on_error_retains_successful_results() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return list_result(
                &["src/a.txt".to_string(), "src/b.txt".to_string()],
                false,
                None,
            );
        }
        if is_copy_request(request) && request.target.contains("/a.txt") {
            return Response::access_denied();
        }
        if is_copy_request(request) {
            return copy_result();
        }
        if request.method == "HEAD" && request.target == "/destination/dst/b.txt" {
            return Response::head(1);
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "--continue-on-error",
            "--retry-attempts",
            "1",
            "--concurrency",
            "1",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test/source/src/b.txt -> test/destination/dst/b.txt"));
    assert!(stdout.contains("1 succeeded"));
    assert!(stdout.contains("1 failed"));
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| is_copy_request(request))
            .count(),
        2
    );
}

#[test]
fn recursive_copy_sigint_drains_started_copy_and_returns_130() {
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return list_result(
                &["src/a.txt".to_string(), "src/b.txt".to_string()],
                false,
                None,
            );
        }
        if is_copy_request(request) {
            thread::sleep(Duration::from_millis(250));
            return copy_result();
        }
        if request.method == "HEAD" && request.target == "/destination/dst/a.txt" {
            return Response::head(1);
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });
    let config_dir = tempfile::tempdir().expect("create config directory");
    let authority = mock
        .endpoint
        .strip_prefix("http://")
        .expect("mock endpoint has scheme");
    let alias = format!("http://ACCESS_KEY:SECRET_KEY@{authority}");
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    let child = command
        .args([
            "cp",
            "--recursive",
            "--concurrency",
            "1",
            "test/source/src/",
            "test/destination/dst/",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_test", alias)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rc command");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !mock.requests().iter().any(is_copy_request) {
        assert!(
            Instant::now() < deadline,
            "copy request did not start: {:#?}",
            mock.requests()
        );
        thread::sleep(Duration::from_millis(5));
    }
    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal_status.success());

    let output = child
        .wait_with_output()
        .expect("wait for interrupted command");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test/source/src/a.txt -> test/destination/dst/a.txt"));
    assert!(stdout.contains("1 succeeded"));
    assert!(stdout.contains("1 cancelled"));
    let copies = mock
        .requests()
        .into_iter()
        .filter(is_copy_request)
        .collect::<Vec<_>>();
    assert_eq!(copies.len(), 1, "SIGINT must stop scheduling new copies");
    assert!(copies[0].target.contains("/a.txt"));
}

#[test]
fn recursive_large_copy_uses_multipart_and_reports_abort_cleanup() {
    const LARGE_SIZE: u64 = 5 * 1024 * 1024 * 1024 + 1;
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return large_object_list(LARGE_SIZE);
        }
        if request.method == "HEAD" && request.target == "/source/src/large.bin" {
            return Response {
                status: "200 OK",
                headers: vec![
                    ("content-length", LARGE_SIZE.to_string()),
                    ("etag", "\"source-etag\"".to_string()),
                    ("x-amz-meta-owner", "analytics".to_string()),
                ],
                body: String::new(),
            };
        }
        if request.method == "POST"
            && request.target.starts_with("/destination/dst/large.bin?")
            && request.target.contains("uploads")
        {
            return Response::xml(
                "<InitiateMultipartUploadResult>\
                 <Bucket>destination</Bucket><Key>dst/large.bin</Key>\
                 <UploadId>cleanup-upload-id</UploadId>\
                 </InitiateMultipartUploadResult>"
                    .to_string(),
            );
        }
        if request.method == "PUT"
            && request.target.contains("partNumber=1")
            && request.target.contains("uploadId=cleanup-upload-id")
        {
            return Response::access_denied();
        }
        if request.method == "DELETE" && request.target.contains("uploadId=cleanup-upload-id") {
            return Response::empty();
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });

    let output = run_rc(
        &mock,
        &[
            "cp",
            "--recursive",
            "--preserve",
            "--retry-attempts",
            "1",
            "test/source/src/",
            "test/destination/dst/",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cleanup-upload-id"), "{stderr}");
    assert!(stderr.contains("abort: succeeded"), "{stderr}");
    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    let create_request = requests
        .iter()
        .find(|request| request.method == "POST")
        .expect("multipart create request");
    assert_eq!(
        create_request.headers.get("x-amz-meta-owner"),
        Some(&"analytics".to_string())
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        1
    );
    assert!(
        requests.iter().all(|request| {
            request.method != "GET"
                || is_list_request(request)
                || request.target.contains("list-type=2")
        }),
        "multipart server-side copy must not download the source: {requests:#?}"
    );
}

#[test]
fn recursive_multipart_sigint_aborts_once_and_reports_cancelled_cleanup() {
    const LARGE_SIZE: u64 = 5 * 1024 * 1024 * 1024 + 1;
    let mock = S3Mock::start(|request| {
        if is_list_request(request) {
            return large_object_list(LARGE_SIZE);
        }
        if request.method == "HEAD" && request.target == "/source/src/large.bin" {
            return Response::head_with_etag(LARGE_SIZE as usize, "source-etag");
        }
        if request.method == "POST"
            && request.target.starts_with("/destination/dst/large.bin?")
            && request.target.contains("uploads")
        {
            return Response::xml(
                "<InitiateMultipartUploadResult>\
                 <Bucket>destination</Bucket><Key>dst/large.bin</Key>\
                 <UploadId>cancel-upload-id</UploadId>\
                 </InitiateMultipartUploadResult>"
                    .to_string(),
            );
        }
        if request.method == "PUT"
            && request.target.contains("partNumber=1")
            && request.target.contains("uploadId=cancel-upload-id")
        {
            thread::sleep(Duration::from_millis(500));
            return Response::xml(
                "<CopyPartResult><ETag>\"part-etag\"</ETag></CopyPartResult>".to_string(),
            );
        }
        if request.method == "DELETE" && request.target.contains("uploadId=cancel-upload-id") {
            return Response::empty();
        }
        Response {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: "<Error><Code>UnexpectedRequest</Code></Error>".to_string(),
        }
    });
    let config_dir = tempfile::tempdir().expect("create config directory");
    let authority = mock
        .endpoint
        .strip_prefix("http://")
        .expect("mock endpoint has scheme");
    let alias = format!("http://ACCESS_KEY:SECRET_KEY@{authority}");
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    let child = command
        .args([
            "cp",
            "--recursive",
            "--concurrency",
            "1",
            "test/source/src/",
            "test/destination/dst/",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_test", alias)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rc command");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !mock.requests().iter().any(|request| {
        request.method == "PUT"
            && request.target.contains("partNumber=1")
            && request.target.contains("uploadId=cancel-upload-id")
    }) {
        assert!(
            Instant::now() < deadline,
            "multipart part request did not start: {:#?}",
            mock.requests()
        );
        thread::sleep(Duration::from_millis(5));
    }
    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal_status.success());

    let output = child
        .wait_with_output()
        .expect("wait for interrupted command");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout: {}\nstderr: {}\nrequests: {:#?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        mock.requests()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 failed"), "{stdout}");
    assert!(stdout.contains("1 cancelled"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cancel-upload-id"), "{stderr}");
    assert!(stderr.contains("abort: succeeded"), "{stderr}");
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| {
                request.method == "DELETE" && request.target.contains("cancel-upload-id")
            })
            .count(),
        1
    );
}

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, Copy)]
enum ResponseMode {
    Read,
    Stat,
    Delete,
    MissingVersion,
    DeleteMarker,
    AccessDenied,
    GovernanceDenied,
    RecursiveVersions,
    PartialRecursiveVersions,
}

struct TestServer {
    authority: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(mode: ResponseMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local HTTP server");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let authority = listener
            .local_addr()
            .expect("local HTTP server address")
            .to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets can inherit nonblocking mode on some platforms.
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted stream blocking");
                        if let Some(request) = read_request(&mut stream) {
                            let response = response_for(mode, &request);
                            thread_requests
                                .lock()
                                .expect("record request")
                                .push(request);
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => panic!("accept test request: {error}"),
                }
            }
        });

        Self {
            authority,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn endpoint_with_credentials(&self) -> String {
        format!("http://accesskey:secretkey@{}", self.authority)
    }

    fn captured_requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("captured requests").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
    let binary_name = format!("rc{}", std::env::consts::EXE_SUFFIX);
    let debug_binary = workspace_root.join("target/debug").join(&binary_name);
    if debug_binary.exists() {
        return debug_binary;
    }
    workspace_root.join("target/release").join(binary_name)
}

fn run_rc(server: Option<&TestServer>, args: &[&str]) -> Output {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let mut command = Command::new(rc_binary());
    command
        .args(args)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("RC_CONFIG_DIR", config_dir.path());
    if let Some(server) = server {
        command.env("RC_HOST_test", server.endpoint_with_credentials());
    }
    command.output().expect("run rc command")
}

fn read_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set request read timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return None;
            }
            Err(_) => return None,
        }
    };

    let headers_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = headers_text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
        .filter(|(key, _)| key != "authorization" && key != "x-amz-security-token")
        .collect();
    let content_length = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or_default();
    let chunked = headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    if headers
        .iter()
        .any(|(key, value)| key == "expect" && value.eq_ignore_ascii_case("100-continue"))
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").ok()?;
    }
    if chunked {
        while !buffer[header_end..]
            .windows(5)
            .any(|window| window == b"0\r\n\r\n")
        {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    } else {
        while buffer.len().saturating_sub(header_end) < content_length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }
    let body_end = if chunked {
        buffer.len()
    } else {
        (header_end + content_length).min(buffer.len())
    };
    let body = String::from_utf8_lossy(&buffer[header_end..body_end]).into_owned();

    Some(CapturedRequest {
        method,
        path,
        headers,
        body,
    })
}

fn response_for(mode: ResponseMode, request: &CapturedRequest) -> String {
    match mode {
        ResponseMode::Read => http_response(
            200,
            &[("Content-Type", "text/plain"), ("x-amz-version-id", "v1")],
            "old",
        ),
        ResponseMode::Stat => http_response(
            200,
            &[
                ("Content-Type", "text/plain"),
                ("x-amz-version-id", "v1"),
                ("ETag", "\"etag-v1\""),
            ],
            "",
        ),
        ResponseMode::Delete => http_response(
            204,
            &[("x-amz-version-id", "v1"), ("x-amz-delete-marker", "false")],
            "",
        ),
        ResponseMode::MissingVersion => s3_error(404, "NoSuchVersion", "version missing"),
        ResponseMode::DeleteMarker => http_response(
            405,
            &[
                ("x-amz-error-code", "MethodNotAllowed"),
                ("x-amz-version-id", "marker-v1"),
                ("x-amz-delete-marker", "true"),
            ],
            "<Error><Code>MethodNotAllowed</Code><Message>delete marker</Message></Error>",
        ),
        ResponseMode::AccessDenied => s3_error(403, "AccessDenied", "policy denied"),
        ResponseMode::GovernanceDenied => {
            s3_error(403, "AccessDenied", "governance retention is active")
        }
        ResponseMode::RecursiveVersions => recursive_versions_response(request),
        ResponseMode::PartialRecursiveVersions => partial_recursive_versions_response(request),
    }
}

fn recursive_versions_response(request: &CapturedRequest) -> String {
    if request.method == "GET" && request.path.contains("versions") {
        if request.path.contains("key-marker") {
            return http_response(
                200,
                &[("Content-Type", "application/xml")],
                second_version_page(),
            );
        }
        return http_response(
            200,
            &[("Content-Type", "application/xml")],
            first_version_page(),
        );
    }
    if request.method == "POST" && request.path.contains("delete") {
        return http_response(
            200,
            &[("Content-Type", "application/xml")],
            delete_versions_result(),
        );
    }
    s3_error(500, "UnexpectedRequest", "unexpected version test request")
}

fn partial_recursive_versions_response(request: &CapturedRequest) -> String {
    if request.method == "GET" && request.path.contains("versions") {
        if request.path.contains("key-marker") {
            return http_response(
                200,
                &[("Content-Type", "application/xml")],
                second_version_page(),
            );
        }
        return http_response(
            200,
            &[("Content-Type", "application/xml")],
            first_version_page(),
        );
    }
    if request.method == "POST" && request.path.contains("delete") {
        return http_response(
            200,
            &[("Content-Type", "application/xml")],
            partial_delete_versions_result(),
        );
    }
    s3_error(500, "UnexpectedRequest", "unexpected version test request")
}

fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    response
}

fn s3_error(status: u16, code: &str, message: &str) -> String {
    let body = format!("<Error><Code>{code}</Code><Message>{message}</Message></Error>");
    http_response(
        status,
        &[
            ("Content-Type", "application/xml"),
            ("x-amz-error-code", code),
        ],
        &body,
    )
}

fn first_version_page() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name><Prefix>logs/</Prefix><KeyMarker></KeyMarker><VersionIdMarker></VersionIdMarker>
  <NextKeyMarker>logs/a.txt</NextKeyMarker><NextVersionIdMarker>marker-v1</NextVersionIdMarker>
  <MaxKeys>1000</MaxKeys><IsTruncated>true</IsTruncated>
  <Version><Key>logs/a.txt</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest><LastModified>2026-07-20T00:00:00.000Z</LastModified><ETag>"etag-a"</ETag><Size>3</Size><StorageClass>STANDARD</StorageClass></Version>
  <DeleteMarker><Key>logs/a.txt</Key><VersionId>marker-v1</VersionId><IsLatest>true</IsLatest><LastModified>2026-07-21T00:00:00.000Z</LastModified></DeleteMarker>
</ListVersionsResult>"#
}

fn second_version_page() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name><Prefix>logs/</Prefix><KeyMarker>logs/a.txt</KeyMarker><VersionIdMarker>marker-v1</VersionIdMarker>
  <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
  <Version><Key>logs/b.txt</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest><LastModified>2026-07-21T00:00:00.000Z</LastModified><ETag>"etag-b"</ETag><Size>3</Size><StorageClass>STANDARD</StorageClass></Version>
</ListVersionsResult>"#
}

fn delete_versions_result() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Deleted><Key>logs/a.txt</Key><VersionId>v1</VersionId></Deleted>
  <Deleted><Key>logs/a.txt</Key><VersionId>marker-v1</VersionId><DeleteMarker>true</DeleteMarker><DeleteMarkerVersionId>marker-v1</DeleteMarkerVersionId></Deleted>
  <Deleted><Key>logs/b.txt</Key><VersionId>v2</VersionId></Deleted>
</DeleteResult>"#
}

fn partial_delete_versions_result() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Deleted><Key>logs/a.txt</Key><VersionId>v1</VersionId></Deleted>
  <Error><Key>logs/a.txt</Key><VersionId>marker-v1</VersionId><Code>AccessDenied</Code><Message>governance retention is active</Message></Error>
  <Deleted><Key>logs/b.txt</Key><VersionId>v2</VersionId></Deleted>
</DeleteResult>"#
}

#[test]
fn version_aware_commands_select_exact_versions_successfully() {
    let cat_server = TestServer::start(ResponseMode::Read);
    let cat = run_rc(
        Some(&cat_server),
        &["cat", "test/bucket/key.txt", "--version-id", "v1"],
    );
    assert!(
        cat.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&cat.stderr)
    );
    assert_eq!(cat.stdout, b"old");
    assert!(
        cat_server.captured_requests()[0]
            .path
            .contains("versionId=v1")
    );

    let head_server = TestServer::start(ResponseMode::Read);
    let head = run_rc(
        Some(&head_server),
        &[
            "head",
            "test/bucket/key.txt",
            "--bytes",
            "3",
            "--version-id",
            "v1",
        ],
    );
    assert!(
        head.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&head.stderr)
    );
    assert_eq!(head.stdout, b"old");
    assert!(
        head_server.captured_requests()[0]
            .path
            .contains("versionId=v1")
    );

    let stat_server = TestServer::start(ResponseMode::Stat);
    let stat = run_rc(
        Some(&stat_server),
        &[
            "--json",
            "stat",
            "test/bucket/key.txt",
            "--version-id",
            "v1",
        ],
    );
    assert!(
        stat.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stat.stderr)
    );
    let stat_json: serde_json::Value =
        serde_json::from_slice(&stat.stdout).expect("stat JSON output");
    assert_eq!(stat_json["schema_version"], 3);
    assert_eq!(stat_json["type"], "versioned_objects");
    assert_eq!(stat_json["data"]["operation"], "stat");
    assert_eq!(stat_json["data"]["object"]["version_id"], "v1");
    assert!(
        stat_server.captured_requests()[0]
            .path
            .contains("versionId=v1")
    );

    let rm_server = TestServer::start(ResponseMode::Delete);
    let rm = run_rc(
        Some(&rm_server),
        &[
            "--json",
            "rm",
            "test/bucket/key.txt",
            "--version-id",
            "v1",
            "--bypass",
        ],
    );
    assert!(
        rm.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    let rm_json: serde_json::Value = serde_json::from_slice(&rm.stdout).expect("rm JSON output");
    assert_eq!(rm_json["schema_version"], 3);
    assert_eq!(rm_json["data"]["operation"], "remove");
    assert_eq!(rm_json["data"]["removed"][0]["version_id"], "v1");
    let request = &rm_server.captured_requests()[0];
    assert!(request.path.contains("versionId=v1"));
    assert_eq!(
        request.header("x-amz-bypass-governance-retention"),
        Some("true")
    );
}

#[test]
fn missing_versions_return_not_found_for_each_affected_command() {
    let cases: &[&[&str]] = &[
        &["cat", "test/bucket/key.txt", "--version-id", "missing"],
        &["head", "test/bucket/key.txt", "--version-id", "missing"],
        &["stat", "test/bucket/key.txt", "--version-id", "missing"],
        &["rm", "test/bucket/key.txt", "--version-id", "missing"],
    ];

    for args in cases {
        let server = TestServer::start(ResponseMode::MissingVersion);
        let output = run_rc(Some(&server), args);
        assert_eq!(
            output.status.code(),
            Some(5),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn read_commands_report_delete_markers_distinctly() {
    let cases: &[&[&str]] = &[
        &["cat", "test/bucket/key.txt", "--version-id", "marker-v1"],
        &["head", "test/bucket/key.txt", "--version-id", "marker-v1"],
        &["stat", "test/bucket/key.txt", "--version-id", "marker-v1"],
    ];

    for args in cases {
        let server = TestServer::start(ResponseMode::DeleteMarker);
        let output = run_rc(Some(&server), args);
        assert_eq!(
            output.status.code(),
            Some(5),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("delete marker"),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn invalid_version_selectors_return_usage_for_each_affected_command() {
    let cases: &[&[&str]] = &[
        &[
            "cat",
            "test/bucket/key.txt",
            "--version-id",
            "v1",
            "--rewind",
            "1h",
        ],
        &["head", "test/bucket/key.txt", "--version-id", ""],
        &[
            "stat",
            "test/bucket/key.txt",
            "--version-id",
            "v1",
            "--rewind",
            "1h",
        ],
        &[
            "rm",
            "test/bucket/key.txt",
            "--version-id",
            "v1",
            "--versions",
        ],
    ];

    for args in cases {
        let output = run_rc(None, args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn version_selector_json_error_is_one_v3_record_on_stderr() {
    let output = run_rc(
        None,
        &[
            "--json",
            "rm",
            "test/bucket/key.txt",
            "--version-id",
            "v1",
            "--versions",
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "JSON errors belong on stderr");
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr must be one JSON record");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "versioned_objects");
    assert_eq!(payload["status"], "error");
    assert_eq!(payload["error"]["type"], "usage_error");
}

#[test]
fn versioned_stat_json_error_is_one_v3_record_on_stderr() {
    let server = TestServer::start(ResponseMode::MissingVersion);
    let output = run_rc(
        Some(&server),
        &[
            "--json",
            "stat",
            "test/bucket/key.txt",
            "--version-id",
            "missing",
        ],
    );

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty(), "JSON errors belong on stderr");
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr must be one JSON record");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "versioned_objects");
    assert_eq!(payload["status"], "error");
    assert_eq!(payload["error"]["type"], "not_found");
}

#[test]
fn access_and_governance_denials_have_distinct_exit_codes() {
    let access_server = TestServer::start(ResponseMode::AccessDenied);
    let access = run_rc(
        Some(&access_server),
        &["rm", "test/bucket/key.txt", "--version-id", "v1"],
    );
    assert_eq!(access.status.code(), Some(4));

    let governance_server = TestServer::start(ResponseMode::GovernanceDenied);
    let governance = run_rc(
        Some(&governance_server),
        &["rm", "test/bucket/key.txt", "--version-id", "v1"],
    );
    assert_eq!(governance.status.code(), Some(6));
}

#[test]
fn recursive_version_removal_paginates_and_deletes_markers() {
    let server = TestServer::start(ResponseMode::RecursiveVersions);
    let output = run_rc(
        Some(&server),
        &[
            "--json",
            "rm",
            "test/bucket/logs/",
            "--recursive",
            "--versions",
            "--bypass",
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("recursive rm JSON output");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["data"]["summary"]["removed"], 3);
    assert_eq!(payload["data"]["removed"].as_array().map(Vec::len), Some(3));
    assert!(
        payload["data"]["removed"]
            .as_array()
            .expect("removed versions")
            .iter()
            .any(|entry| entry["delete_marker"] == true)
    );

    let requests = server.captured_requests();
    let list_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "GET" && request.path.contains("versions"))
        .collect();
    assert_eq!(list_requests.len(), 2, "requests: {requests:#?}");
    assert!(list_requests[1].path.contains("key-marker"));
    assert!(list_requests[1].path.contains("version-id-marker"));
    let delete_request = requests
        .iter()
        .find(|request| request.method == "POST" && request.path.contains("delete"))
        .expect("delete versions request");
    assert!(delete_request.body.contains("<VersionId>v1</VersionId>"));
    assert!(
        delete_request
            .body
            .contains("<VersionId>marker-v1</VersionId>")
    );
    assert_eq!(
        delete_request.header("x-amz-bypass-governance-retention"),
        Some("true")
    );
}

#[test]
fn partial_version_removal_preserves_successes_and_version_aware_failures() {
    let server = TestServer::start(ResponseMode::PartialRecursiveVersions);
    let output = run_rc(
        Some(&server),
        &[
            "--json",
            "rm",
            "test/bucket/logs/",
            "--recursive",
            "--versions",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(6),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "partial errors belong on stderr");
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("partial error must be one JSON record");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["status"], "error");
    assert_eq!(payload["error"]["type"], "conflict");
    assert_eq!(payload["data"]["outcome"], "partial");
    assert_eq!(payload["data"]["summary"]["removed"], 2);
    assert_eq!(payload["data"]["summary"]["failed"], 1);
    assert_eq!(payload["data"]["removed"][0]["version_id"], "v1");
    assert_eq!(payload["data"]["failed"][0]["version_id"], "marker-v1");
}

#[test]
fn version_removal_json_dry_run_is_one_planned_v3_record() {
    let server = TestServer::start(ResponseMode::RecursiveVersions);
    let output = run_rc(
        Some(&server),
        &[
            "--json",
            "rm",
            "test/bucket/logs/",
            "--recursive",
            "--versions",
            "--dry-run",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("dry-run stdout must be one JSON record");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["data"]["outcome"], "planned");
    assert_eq!(payload["data"]["dry_run"], true);
    assert_eq!(payload["data"]["summary"]["planned"], 3);
    assert_eq!(payload["data"]["removed"].as_array().map(Vec::len), Some(0));
    assert!(
        server
            .captured_requests()
            .iter()
            .all(|request| request.method != "POST"),
        "dry-run must not issue delete requests"
    );
}

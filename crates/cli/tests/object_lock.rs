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
    BucketLock,
    BucketLockWithoutDefault,
    EmptyRetention,
    GovernanceRetention,
    ComplianceRetention,
    LegalHoldOn,
    LegalHoldOff,
    MissingVersion,
    AccessDenied,
    BypassDenied,
    Unsupported,
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

    Some(CapturedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&buffer[header_end..body_end]).into_owned(),
    })
}

fn response_for(mode: ResponseMode, request: &CapturedRequest) -> String {
    if matches!(mode, ResponseMode::AccessDenied) {
        let encryption_material = request
            .header("x-amz-server-side-encryption-customer-key")
            .unwrap_or("no-custom-encryption-header");
        return s3_error(
            403,
            "AccessDenied",
            &format!("policy denied for accesskey secretkey {encryption_material}"),
        );
    }
    if matches!(mode, ResponseMode::BypassDenied) && request.method == "PUT" {
        return s3_error(403, "AccessDenied", "bypass permission denied");
    }
    if matches!(mode, ResponseMode::Unsupported) {
        return s3_error(501, "NotImplemented", "object lock is unavailable");
    }
    if matches!(mode, ResponseMode::MissingVersion) {
        return s3_error(404, "NoSuchVersion", "version missing");
    }
    if request.method == "PUT" {
        return http_response(200, &[], "");
    }

    let body = match mode {
        ResponseMode::BucketLock => bucket_lock_xml(),
        ResponseMode::BucketLockWithoutDefault => bucket_lock_without_default_xml(),
        ResponseMode::EmptyRetention | ResponseMode::BypassDenied => retention_xml(None),
        ResponseMode::GovernanceRetention => {
            retention_xml(Some(("GOVERNANCE", "2099-01-01T00:00:00Z")))
        }
        ResponseMode::ComplianceRetention => {
            retention_xml(Some(("COMPLIANCE", "2099-01-01T00:00:00Z")))
        }
        ResponseMode::LegalHoldOn => legal_hold_xml("ON"),
        ResponseMode::LegalHoldOff => legal_hold_xml("OFF"),
        ResponseMode::MissingVersion | ResponseMode::AccessDenied | ResponseMode::Unsupported => {
            unreachable!("error modes return before selecting a response body")
        }
    };
    http_response(200, &[("Content-Type", "application/xml")], &body)
}

fn bucket_lock_xml() -> String {
    concat!(
        r#"<ObjectLockConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        "<ObjectLockEnabled>Enabled</ObjectLockEnabled>",
        "<Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>30</Days>",
        "</DefaultRetention></Rule></ObjectLockConfiguration>"
    )
    .to_string()
}

fn bucket_lock_without_default_xml() -> String {
    concat!(
        r#"<ObjectLockConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        "<ObjectLockEnabled>Enabled</ObjectLockEnabled>",
        "</ObjectLockConfiguration>"
    )
    .to_string()
}

fn retention_xml(value: Option<(&str, &str)>) -> String {
    match value {
        Some((mode, until)) => format!(
            concat!(
                r#"<Retention xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
                "<Mode>{mode}</Mode><RetainUntilDate>{until}</RetainUntilDate></Retention>"
            ),
            mode = mode,
            until = until
        ),
        None => r#"<Retention xmlns="http://s3.amazonaws.com/doc/2006-03-01/"/>"#.to_string(),
    }
}

fn legal_hold_xml(status: &str) -> String {
    format!(
        r#"<LegalHold xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>{status}</Status></LegalHold>"#
    )
}

fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        501 => "Not Implemented",
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

fn assert_exit(output: &Output, code: i32, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{context}: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_v3_lock_success(output: &Output, operation: &str) -> serde_json::Value {
    assert_exit(output, 0, operation);
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid lock JSON output");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "locks");
    assert_eq!(payload["status"], "success");
    assert_eq!(payload["data"]["operation"], operation);
    payload
}

#[test]
fn bucket_lock_info_round_trips_to_deterministic_v3_and_table_output() {
    let json_server = TestServer::start(ResponseMode::BucketLock);
    let json = run_rc(
        Some(&json_server),
        &["--json", "bucket", "lock", "info", "test/records"],
    );
    let payload = assert_v3_lock_success(&json, "bucket_lock_info");
    let item = &payload["data"]["items"][0];
    assert_eq!(item["bucket"], "records");
    assert_eq!(item["object_lock_enabled"], true);
    assert_eq!(item["default_retention"]["mode"], "governance");
    assert_eq!(item["default_retention"]["duration"]["unit"], "days");
    assert_eq!(item["default_retention"]["duration"]["value"], 30);

    let table_server = TestServer::start(ResponseMode::BucketLock);
    let table = run_rc(
        Some(&table_server),
        &[
            "--format",
            "human",
            "bucket",
            "lock",
            "info",
            "test/records",
        ],
    );
    assert_exit(&table, 0, "bucket lock table");
    let stdout = String::from_utf8_lossy(&table.stdout);
    assert!(stdout.contains("BUCKET"));
    assert!(stdout.contains("GOVERNANCE"));
    assert!(stdout.contains("30 days"));
}

#[test]
fn bucket_lock_info_has_auth_and_unsupported_exit_codes_without_secret_leaks() {
    for (mode, code, error_type) in [
        (ResponseMode::AccessDenied, 4, "auth_error"),
        (ResponseMode::Unsupported, 7, "unsupported_feature"),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(
            Some(&server),
            &[
                "--json",
                "--header",
                "x-amz-server-side-encryption-customer-key:customer-key-material",
                "bucket",
                "lock",
                "info",
                "test/records",
            ],
        );
        assert_exit(&output, code, "bucket lock info failure");
        assert!(output.stdout.is_empty());
        let payload: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("one v3 JSON error");
        assert_eq!(payload["type"], "locks");
        assert_eq!(payload["error"]["type"], error_type);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("accesskey"));
        assert!(!stderr.contains("secretkey"));
        assert!(!stderr.contains("customer-key-material"));
        if matches!(mode, ResponseMode::AccessDenied) {
            assert!(stderr.contains("[REDACTED]"));
        }
    }
}

#[test]
fn bucket_lock_set_and_clear_round_trip_and_validate_duration_before_io() {
    let set_server = TestServer::start(ResponseMode::BucketLock);
    let set = run_rc(
        Some(&set_server),
        &[
            "--json",
            "bucket",
            "lock",
            "set",
            "test/records",
            "--mode",
            "governance",
            "--days",
            "30",
        ],
    );
    assert_v3_lock_success(&set, "bucket_lock_set");
    let requests = set_server.captured_requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
    assert!(requests.iter().any(|request| {
        request.method == "PUT"
            && request.path.contains("object-lock")
            && request.body.contains("<Mode>GOVERNANCE</Mode>")
            && request.body.contains("<Days>30</Days>")
    }));

    let clear_server = TestServer::start(ResponseMode::BucketLockWithoutDefault);
    let clear = run_rc(
        Some(&clear_server),
        &["--json", "bucket", "lock", "clear", "test/records"],
    );
    let clear_payload = assert_v3_lock_success(&clear, "bucket_lock_clear");
    assert!(clear_payload["data"]["items"][0]["default_retention"].is_null());
    let clear_requests = clear_server.captured_requests();
    let clear_put = clear_requests
        .iter()
        .find(|request| request.method == "PUT")
        .expect("clear sends one PUT");
    assert!(
        clear_put
            .body
            .contains("<ObjectLockEnabled>Enabled</ObjectLockEnabled>")
    );
    assert!(!clear_put.body.contains("<DefaultRetention>"));

    for args in [
        vec![
            "bucket",
            "lock",
            "set",
            "test/records",
            "--mode",
            "governance",
            "--days",
            "0",
        ],
        vec![
            "bucket",
            "lock",
            "set",
            "test/records",
            "--mode",
            "compliance",
            "--days",
            "1",
            "--years",
            "1",
        ],
        vec![
            "bucket",
            "lock",
            "set",
            "test/records",
            "--mode",
            "governance",
            "--years",
            "2147483648",
        ],
    ] {
        let output = run_rc(None, &args);
        assert_exit(&output, 2, "invalid bucket lock duration");
    }
}

#[test]
fn bucket_lock_mutations_map_auth_and_unsupported_failures() {
    let cases: &[(ResponseMode, &[&str], i32)] = &[
        (
            ResponseMode::AccessDenied,
            &[
                "bucket",
                "lock",
                "set",
                "test/records",
                "--mode",
                "governance",
                "--years",
                "1",
            ],
            4,
        ),
        (
            ResponseMode::AccessDenied,
            &["bucket", "lock", "clear", "test/records"],
            4,
        ),
        (
            ResponseMode::Unsupported,
            &["bucket", "lock", "clear", "test/records"],
            7,
        ),
    ];
    for (mode, args, code) in cases {
        let server = TestServer::start(*mode);
        let output = run_rc(Some(&server), args);
        assert_exit(&output, *code, "bucket lock mutation failure");
    }
}

#[test]
fn retention_default_compatibility_delegates_to_bucket_lock_commands() {
    for (args, operation, mode) in [
        (
            vec!["--json", "retention", "info", "--default", "test/records"],
            "bucket_lock_info",
            ResponseMode::BucketLock,
        ),
        (
            vec![
                "--json",
                "retention",
                "set",
                "--default",
                "governance",
                "30d",
                "test/records",
            ],
            "bucket_lock_set",
            ResponseMode::BucketLock,
        ),
        (
            vec!["--json", "retention", "clear", "--default", "test/records"],
            "bucket_lock_clear",
            ResponseMode::BucketLockWithoutDefault,
        ),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(Some(&server), &args);
        assert_v3_lock_success(&output, operation);
    }

    let invalid = run_rc(
        None,
        &[
            "retention",
            "set",
            "--default",
            "governance",
            "1m",
            "test/records",
        ],
    );
    assert_exit(&invalid, 2, "ambiguous bucket retention unit");
}

#[test]
fn retention_info_supports_compatibility_and_canonical_version_selectors() {
    for args in [
        vec![
            "--json",
            "retention",
            "info",
            "test/records/invoice.pdf",
            "--version-id",
            "v2",
        ],
        vec![
            "--json",
            "object",
            "retention",
            "info",
            "test/records/invoice.pdf",
            "--version-id",
            "v2",
        ],
    ] {
        let server = TestServer::start(ResponseMode::GovernanceRetention);
        let output = run_rc(Some(&server), &args);
        let payload = assert_v3_lock_success(&output, "retention_info");
        let item = &payload["data"]["items"][0];
        assert_eq!(item["version_id"], "v2");
        assert_eq!(item["retention"]["mode"], "governance");
        assert_eq!(item["retention"]["retain_until"], "2099-01-01T00:00:00Z");
        assert!(server.captured_requests()[0].path.contains("versionId=v2"));
    }
}

#[test]
fn retention_info_maps_missing_version_and_access_denial() {
    for (mode, code) in [
        (ResponseMode::MissingVersion, 5),
        (ResponseMode::AccessDenied, 4),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(
            Some(&server),
            &[
                "retention",
                "info",
                "test/records/invoice.pdf",
                "--version-id",
                "v2",
            ],
        );
        assert_exit(&output, code, "retention info failure");
    }
}

#[test]
fn retention_set_uses_utc_and_explicit_governance_bypass() {
    let server = TestServer::start(ResponseMode::EmptyRetention);
    let output = run_rc(
        Some(&server),
        &[
            "--json",
            "retention",
            "set",
            "governance",
            "2099-03-04T05:06:07Z",
            "test/records/invoice.pdf",
            "--version-id",
            "v2",
            "--bypass",
        ],
    );
    let payload = assert_v3_lock_success(&output, "retention_set");
    assert_eq!(
        payload["data"]["items"][0]["retention"]["retain_until"],
        "2099-03-04T05:06:07Z"
    );
    let requests = server.captured_requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
    let put = requests
        .iter()
        .find(|request| request.method == "PUT")
        .expect("retention set sends PUT");
    assert!(put.path.contains("versionId=v2"));
    assert_eq!(
        put.header("x-amz-bypass-governance-retention"),
        Some("true")
    );
    assert!(put.body.contains("2099-03-04T05:06:07Z"));

    let implicit_server = TestServer::start(ResponseMode::EmptyRetention);
    let implicit = run_rc(
        Some(&implicit_server),
        &[
            "retention",
            "set",
            "governance",
            "2099-03-04T05:06:07Z",
            "test/records/invoice.pdf",
        ],
    );
    assert_exit(&implicit, 0, "retention set without bypass");
    let implicit_requests = implicit_server.captured_requests();
    let implicit_put = implicit_requests
        .iter()
        .find(|request| request.method == "PUT")
        .expect("retention set sends PUT without bypass");
    assert_eq!(
        implicit_put.header("x-amz-bypass-governance-retention"),
        None,
        "the bypass header must only be sent for an explicit --bypass flag"
    );
}

#[test]
fn retention_set_rejects_past_overflow_and_compliance_weakening_before_put() {
    for validity in ["2000-01-01T00:00:00Z", "999999999999999999999y"] {
        let output = run_rc(
            None,
            &[
                "retention",
                "set",
                "governance",
                validity,
                "test/records/invoice.pdf",
            ],
        );
        assert_exit(&output, 2, "invalid retention date");
    }

    let compliance_server = TestServer::start(ResponseMode::ComplianceRetention);
    let weakening = run_rc(
        Some(&compliance_server),
        &[
            "retention",
            "set",
            "compliance",
            "2098-01-01T00:00:00Z",
            "test/records/invoice.pdf",
            "--bypass",
        ],
    );
    assert_exit(&weakening, 6, "compliance weakening");
    assert_eq!(
        compliance_server
            .captured_requests()
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        0,
        "client-side compliance checks must prevent mutation"
    );
}

#[test]
fn retention_set_requires_server_authorization_for_bypass() {
    let server = TestServer::start(ResponseMode::BypassDenied);
    let output = run_rc(
        Some(&server),
        &[
            "retention",
            "set",
            "governance",
            "2099-03-04T05:06:07Z",
            "test/records/invoice.pdf",
            "--bypass",
        ],
    );
    assert_exit(&output, 4, "retention bypass authorization");
    let requests = server.captured_requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "GET")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .find(|request| request.method == "PUT")
            .and_then(|request| request.header("x-amz-bypass-governance-retention")),
        Some("true")
    );
}

#[test]
fn retention_clear_succeeds_and_protects_active_modes_before_put() {
    let expired_server = TestServer::start(ResponseMode::EmptyRetention);
    let cleared = run_rc(
        Some(&expired_server),
        &[
            "--json",
            "retention",
            "clear",
            "test/records/invoice.pdf",
            "--version-id",
            "v2",
        ],
    );
    let payload = assert_v3_lock_success(&cleared, "retention_clear");
    assert!(payload["data"]["items"][0]["retention"].is_null());
    let clear_requests = expired_server.captured_requests();
    let clear_put = clear_requests
        .iter()
        .find(|request| request.method == "PUT")
        .expect("retention clear sends PUT");
    assert!(clear_put.body.contains("<Retention"));
    assert!(!clear_put.body.contains("<Mode>"));
    assert!(!clear_put.body.contains("<RetainUntilDate>"));

    for (mode, args) in [
        (
            ResponseMode::GovernanceRetention,
            vec!["retention", "clear", "test/records/invoice.pdf"],
        ),
        (
            ResponseMode::ComplianceRetention,
            vec!["retention", "clear", "test/records/invoice.pdf", "--bypass"],
        ),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(Some(&server), &args);
        assert_exit(&output, 6, "protected retention clear");
        assert_eq!(
            server
                .captured_requests()
                .iter()
                .filter(|request| request.method == "PUT")
                .count(),
            0
        );
    }
}

#[test]
fn retention_clear_maps_missing_version_and_access_denial() {
    for (mode, code) in [
        (ResponseMode::MissingVersion, 5),
        (ResponseMode::AccessDenied, 4),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(
            Some(&server),
            &[
                "retention",
                "clear",
                "test/records/invoice.pdf",
                "--version-id",
                "v2",
            ],
        );
        assert_exit(&output, code, "retention clear failure");
    }
}

#[test]
fn legalhold_info_supports_on_off_and_version_specific_reads() {
    for (mode, expected) in [
        (ResponseMode::LegalHoldOn, true),
        (ResponseMode::LegalHoldOff, false),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(
            Some(&server),
            &[
                "--json",
                "legalhold",
                "info",
                "test/records/invoice.pdf",
                "--version-id",
                "v2",
            ],
        );
        let payload = assert_v3_lock_success(&output, "legal_hold_info");
        assert_eq!(payload["data"]["items"][0]["legal_hold"], expected);
        assert_eq!(payload["data"]["items"][0]["version_id"], "v2");
        assert!(server.captured_requests()[0].path.contains("versionId=v2"));
    }
}

#[test]
fn legalhold_info_maps_missing_version_and_access_denial() {
    for (mode, code) in [
        (ResponseMode::MissingVersion, 5),
        (ResponseMode::AccessDenied, 4),
    ] {
        let server = TestServer::start(mode);
        let output = run_rc(
            Some(&server),
            &[
                "legalhold",
                "info",
                "test/records/invoice.pdf",
                "--version-id",
                "v2",
            ],
        );
        assert_exit(&output, code, "legal hold info failure");
    }
}

#[test]
fn legalhold_set_and_clear_emit_on_and_off_for_compatibility_and_canonical_paths() {
    for (args, operation, expected) in [
        (
            vec![
                "--json",
                "legalhold",
                "set",
                "test/records/invoice.pdf",
                "--version-id",
                "v2",
            ],
            "legal_hold_set",
            "<Status>ON</Status>",
        ),
        (
            vec![
                "--json",
                "object",
                "legalhold",
                "clear",
                "test/records/invoice.pdf",
                "--version-id",
                "v2",
            ],
            "legal_hold_clear",
            "<Status>OFF</Status>",
        ),
    ] {
        let server = TestServer::start(ResponseMode::LegalHoldOn);
        let output = run_rc(Some(&server), &args);
        let payload = assert_v3_lock_success(&output, operation);
        assert_eq!(
            payload["data"]["items"][0]["legal_hold"],
            operation == "legal_hold_set"
        );
        let request = &server.captured_requests()[0];
        assert_eq!(request.method, "PUT");
        assert!(request.path.contains("versionId=v2"));
        assert!(request.body.contains(expected));
    }
}

#[test]
fn legalhold_mutations_map_missing_version_and_access_denial_for_each_command() {
    for command in ["set", "clear"] {
        for (mode, code) in [
            (ResponseMode::MissingVersion, 5),
            (ResponseMode::AccessDenied, 4),
        ] {
            let server = TestServer::start(mode);
            let output = run_rc(
                Some(&server),
                &[
                    "legalhold",
                    command,
                    "test/records/invoice.pdf",
                    "--version-id",
                    "v2",
                ],
            );
            assert_exit(&output, code, "legal hold mutation failure");
        }
    }
}

#[test]
fn object_lock_commands_reject_empty_versions_before_network_io() {
    for args in [
        vec![
            "retention",
            "info",
            "test/records/invoice.pdf",
            "--version-id",
            "",
        ],
        vec![
            "legalhold",
            "set",
            "test/records/invoice.pdf",
            "--version-id",
            "",
        ],
    ] {
        let output = run_rc(None, &args);
        assert_exit(&output, 2, "empty version selector");
    }
}

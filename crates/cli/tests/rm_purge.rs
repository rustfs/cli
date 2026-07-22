#![cfg(not(windows))]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

struct TestServer {
    authority: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start() -> Self {
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
                        if let Some(request) = read_request(&mut stream) {
                            let response = response_for(&request);
                            thread_requests
                                .lock()
                                .expect("record request")
                                .push(request);
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
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

    let debug_binary = workspace_root.join("target/debug/rc");
    if debug_binary.exists() {
        return debug_binary;
    }

    workspace_root.join("target/release/rc")
}

fn read_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set stream read timeout");

    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if header_end_position(&buffer).is_some() {
                    break;
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break;
            }
            Err(_) => return None,
        }
    }

    let header_end = header_end_position(&buffer)?;
    let request = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = request.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
        .collect();

    drain_request_body(stream, &mut buffer, header_end + 4, &headers)?;

    Some(CapturedRequest {
        method,
        path,
        headers,
    })
}

fn header_end_position(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn drain_request_body(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    body_start: usize,
    headers: &[(String, String)],
) -> Option<()> {
    if let Some(content_length) =
        header_value(headers, "content-length").and_then(|value| value.parse::<usize>().ok())
    {
        drain_content_length_body(stream, buffer, body_start, content_length)
    } else if header_value(headers, "transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        drain_chunked_body(stream, buffer, body_start)
    } else {
        Some(())
    }
}

fn drain_content_length_body(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    body_start: usize,
    content_length: usize,
) -> Option<()> {
    let mut body_read = buffer.len().saturating_sub(body_start);
    let mut chunk = [0_u8; 1024];

    while body_read < content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
        body_read += n;
    }

    Some(())
}

fn drain_chunked_body(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    body_start: usize,
) -> Option<()> {
    let mut chunk = [0_u8; 1024];

    while !buffer[body_start..]
        .windows(5)
        .any(|window| window == b"0\r\n\r\n")
    {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }

    Some(())
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn response_for(request: &CapturedRequest) -> String {
    match request.method.as_str() {
        "GET" if request.path.contains("list-type=2") => xml_response(200, list_objects_body()),
        "GET" if request.path.contains("versions") => xml_response(200, list_versions_body()),
        "DELETE" => xml_response(204, ""),
        "POST" if request.path.contains("delete") => xml_response(200, delete_objects_body()),
        _ => xml_response(500, "<Error><Code>UnexpectedRequest</Code></Error>"),
    }
}

fn xml_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        _ => "Internal Server Error",
    };

    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn list_objects_body() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix>purge-prefix/</Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>purge-prefix/a.txt</Key>
    <LastModified>2026-05-01T00:00:00.000Z</LastModified>
    <ETag>&quot;etag-a&quot;</ETag>
    <Size>1</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>purge-prefix/nested/b.txt</Key>
    <LastModified>2026-05-01T00:00:00.000Z</LastModified>
    <ETag>&quot;etag-b&quot;</ETag>
    <Size>1</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
</ListBucketResult>"#
}

fn list_versions_body() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix>purge-prefix/</Prefix>
  <KeyMarker></KeyMarker>
  <VersionIdMarker></VersionIdMarker>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Version>
    <Key>purge-prefix/a.txt</Key>
    <VersionId>v1</VersionId>
    <IsLatest>false</IsLatest>
    <LastModified>2026-05-01T00:00:00.000Z</LastModified>
    <ETag>&quot;etag-a&quot;</ETag>
    <Size>1</Size>
    <StorageClass>STANDARD</StorageClass>
  </Version>
  <DeleteMarker>
    <Key>purge-prefix/a.txt</Key>
    <VersionId>v2</VersionId>
    <IsLatest>true</IsLatest>
    <LastModified>2026-05-01T00:00:01.000Z</LastModified>
  </DeleteMarker>
  <Version>
    <Key>purge-prefix/nested/b.txt</Key>
    <VersionId>v3</VersionId>
    <IsLatest>true</IsLatest>
    <LastModified>2026-05-01T00:00:00.000Z</LastModified>
    <ETag>&quot;etag-b&quot;</ETag>
    <Size>1</Size>
    <StorageClass>STANDARD</StorageClass>
  </Version>
</ListVersionsResult>"#
}

fn delete_objects_body() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Deleted><Key>purge-prefix/a.txt</Key></Deleted>
  <Deleted><Key>purge-prefix/nested/b.txt</Key></Deleted>
</DeleteResult>"#
}

#[test]
fn rm_recursive_purge_deletes_each_key_with_force_header() {
    let server = TestServer::start();
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "rm",
            "--recursive",
            "--purge",
            "test/bucket/purge-prefix/",
        ])
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_test", server.endpoint_with_credentials())
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(payload["status"], "success");
    assert_eq!(payload["total"], 2);

    let requests = server.captured_requests();
    let list_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "GET" && request.path.contains("versions"))
        .collect();
    assert_eq!(list_requests.len(), 1, "requests: {requests:#?}");
    assert!(
        list_requests[0].path.contains("prefix=purge-prefix%2F"),
        "version list request should include the recursive prefix: {requests:#?}"
    );

    let delete_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .collect();
    assert_eq!(delete_requests.len(), 2, "requests: {requests:#?}");

    let delete_paths: Vec<_> = delete_requests
        .iter()
        .map(|request| {
            request
                .path
                .split_once('?')
                .map_or(request.path.as_str(), |(path, _)| path)
        })
        .collect();
    assert!(
        delete_paths.contains(&"/bucket/purge-prefix/a.txt"),
        "requests: {requests:#?}"
    );
    assert!(
        delete_paths.contains(&"/bucket/purge-prefix/nested/b.txt"),
        "requests: {requests:#?}"
    );

    for request in delete_requests {
        assert_eq!(
            request.header("x-rustfs-force-delete"),
            Some("true"),
            "request should force-delete through RustFS: {request:#?}"
        );
    }

    assert!(
        !requests
            .iter()
            .any(|request| request.method == "POST" && request.path.contains("delete")),
        "recursive purge should not use the batch delete path: {requests:#?}"
    );
}

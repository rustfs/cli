#[allow(dead_code)]
mod admin_support;

use admin_support::{rc_binary, rc_host_alias};
use jsonschema::Validator;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("read request headers");
        assert!(read > 0, "connection closed before request headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers_text = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = headers_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("valid content length"))
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let read = stream.read(&mut chunk).expect("read request body");
        assert!(read > 0, "connection closed before request body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let mut lines = headers_text.lines();
    let request_line = lines.next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("request method").to_string();
    let target = parts.next().expect("request target").to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.to_string(), value.trim().to_string()))
        })
        .collect();
    CapturedRequest {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    }
}

fn write_response(stream: &mut TcpStream, status: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write response headers");
    stream.write_all(body).expect("write response body");
    stream.flush().expect("flush response");
}

fn start_roundtrip_server() -> (
    String,
    mpsc::Receiver<CapturedRequest>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind roundtrip server");
    listener
        .set_nonblocking(true)
        .expect("configure nonblocking listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut uploaded = Vec::new();
        for expected_method in ["PUT", "GET", "DELETE"] {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "timed out waiting for request");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept request: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("configure blocking request stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set request timeout");
            let request = read_request(&mut stream);
            assert_eq!(request.method, expected_method);
            match expected_method {
                "PUT" => {
                    uploaded = request.body.clone();
                    write_response(&mut stream, "200 OK", b"");
                }
                "GET" => write_response(&mut stream, "200 OK", &uploaded),
                "DELETE" => write_response(&mut stream, "204 No Content", b""),
                _ => unreachable!("fixed request sequence"),
            }
            sender.send(request).expect("send captured request");
        }
    });
    (endpoint, receiver, handle)
}

fn output_v3_validator() -> Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

#[test]
fn kms_roundtrip_uses_sse_kms_verifies_and_permanently_cleans_up() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_roundtrip_server();
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "--debug",
            "admin",
            "kms",
            "roundtrip",
            "myalias",
            "diagnostic-bucket",
            "--key-id",
            "key-1",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run roundtrip command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let value: Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    assert_eq!(value["data"]["operation"], "roundtrip");
    assert_eq!(value["data"]["bucket"], "diagnostic-bucket");
    assert_eq!(value["data"]["key_id"], "key-1");
    assert_eq!(value["data"]["passed"], true);
    assert_eq!(value["data"]["cleanup_passed"], true);
    let errors = output_v3_validator()
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "invalid v3 output: {}",
        errors.join("\n")
    );

    let put = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured PUT");
    let get = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured GET");
    let delete = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured DELETE");
    assert_eq!(put.body.len(), 4096);
    assert_eq!(put.header("x-amz-server-side-encryption"), Some("aws:kms"));
    assert_eq!(
        put.header("x-amz-server-side-encryption-aws-kms-key-id"),
        Some("key-1")
    );
    let temporary_path = put.target.split('?').next().expect("PUT object path");
    assert_eq!(
        temporary_path,
        get.target.split('?').next().expect("GET object path")
    );
    assert_eq!(
        temporary_path,
        delete.target.split('?').next().expect("DELETE object path")
    );
    assert_eq!(delete.header("x-rustfs-force-delete"), Some("true"));
    assert!(!stdout.contains(temporary_path));
    assert!(!stderr.contains(temporary_path));
    for forbidden in ["ciphertext", "digest", "data_key", "object_name"] {
        assert!(!stdout.contains(forbidden));
    }
    handle.join().expect("roundtrip server finished");
}

#[test]
fn kms_roundtrip_refuses_without_confirmation_before_alias_lookup() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "kms",
            "roundtrip",
            "missing-alias",
            "diagnostic-bucket",
            "--key-id",
            "key-1",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run refused roundtrip command");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("requires --yes"));
    assert!(!stderr.contains("Alias 'missing-alias' not found"));
}

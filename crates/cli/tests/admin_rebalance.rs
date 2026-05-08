#![cfg(not(windows))]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct CapturedAdminRequest {
    method: String,
    target: String,
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

fn read_admin_request(stream: &mut TcpStream) -> CapturedAdminRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = stream.read(&mut buffer).expect("read admin request");
        if bytes_read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..bytes_read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = String::from_utf8(request).expect("admin request should be UTF-8");
    let request_line = request.lines().next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("request method").to_string();
    let target = parts.next().expect("request target").to_string();

    CapturedAdminRequest { method, target }
}

fn start_admin_test_server(
    response_body: &'static str,
) -> (String, Receiver<CapturedAdminRequest>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin test server");
    listener
        .set_nonblocking(true)
        .expect("set admin test server nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept admin request: {e}"),
            }
        };
        let request = read_admin_request(&mut stream);
        sender.send(request).expect("send captured request");

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write admin response");
    });

    (endpoint, receiver, handle)
}

fn rc_host_alias(endpoint: &str) -> String {
    let (_, endpoint_authority) = endpoint.split_once("://").expect("endpoint has scheme");
    format!("http://ACCESS_KEY:SECRET_KEY@{endpoint_authority}")
}

#[test]
fn rebalance_status_dispatches_to_rebalance_status_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"id":"rebalance-123","pools":[],"stoppedAt":"2026-05-07T00:00:00Z"}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "rebalance", "status", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(payload["id"], "rebalance-123");
    assert_eq!(payload["stoppedAt"], "2026-05-07T00:00:00Z");
    assert_eq!(payload["pools"].as_array().expect("pools array").len(), 0);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/rebalance/status");

    handle.join().expect("admin test server finished");
}

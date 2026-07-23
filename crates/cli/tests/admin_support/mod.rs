use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug)]
#[allow(dead_code)]
pub struct CapturedAdminRequest {
    #[allow(dead_code)]
    pub method: String,
    pub target: String,
    #[allow(dead_code)]
    pub body: Vec<u8>,
}

pub fn rc_binary() -> PathBuf {
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
    let header_end = loop {
        let bytes_read = stream.read(&mut buffer).expect("read admin request");
        assert!(
            bytes_read > 0,
            "client closed before request headers completed"
        );
        request.extend_from_slice(&buffer[..bytes_read]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let headers = String::from_utf8(request[..header_end].to_vec())
        .expect("admin request headers should be UTF-8");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("valid content length"))
        })
        .unwrap_or(0);
    while request.len() - header_end < content_length {
        let bytes_read = stream.read(&mut buffer).expect("read admin request body");
        assert!(
            bytes_read > 0,
            "client closed before request body completed"
        );
        request.extend_from_slice(&buffer[..bytes_read]);
    }

    let request_line = headers.lines().next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("request method").to_string();
    let target = parts.next().expect("request target").to_string();

    let body = request[header_end..header_end + content_length].to_vec();

    CapturedAdminRequest {
        method,
        target,
        body,
    }
}

// Each integration-test binary compiles this shared module independently, so a
// helper used by one binary can legitimately be unused by another.
#[allow(dead_code)]
pub fn start_admin_test_server(
    response_body: &'static str,
) -> (String, Receiver<CapturedAdminRequest>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin test server");
    listener
        .set_nonblocking(true)
        .expect("set admin test server nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept admin request: {e}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set admin request stream blocking");
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

#[allow(dead_code)]
pub fn start_admin_test_server_with_endpoint_response(
    response_template: &'static str,
) -> (String, Receiver<CapturedAdminRequest>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin test server");
    listener
        .set_nonblocking(true)
        .expect("set admin test server nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let response_body = response_template.replace("SELF_ENDPOINT", &endpoint);
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept admin request: {e}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set admin request stream blocking");
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

#[allow(dead_code)]
pub fn start_admin_response_test_server(
    response_status: &str,
    content_type: &str,
    response_body: String,
) -> (String, Receiver<CapturedAdminRequest>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin test server");
    listener
        .set_nonblocking(true)
        .expect("set admin test server nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let (sender, receiver) = mpsc::channel();
    let response_status = response_status.to_string();
    let content_type = content_type.to_string();

    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept admin request: {e}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set admin request stream blocking");
        let request = read_admin_request(&mut stream);
        sender.send(request).expect("send captured request");

        let response = format!(
            "HTTP/1.1 {response_status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write admin response");
    });

    (endpoint, receiver, handle)
}

#[allow(dead_code)]
pub fn start_admin_sequence_test_server(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, Receiver<CapturedAdminRequest>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin test server");
    listener
        .set_nonblocking(true)
        .expect("set admin test server nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        for (response_status, response_body) in responses {
            let deadline = Instant::now() + Duration::from_secs(120);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(accepted) => break accepted,
                    Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept admin request: {e}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("set admin request stream blocking");
            let request = read_admin_request(&mut stream);
            sender.send(request).expect("send captured request");

            let response = format!(
                "HTTP/1.1 {response_status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write admin response");
        }
    });

    (endpoint, receiver, handle)
}

pub fn rc_host_alias(endpoint: &str) -> String {
    let (_, endpoint_authority) = endpoint.split_once("://").expect("endpoint has scheme");
    format!("http://ACCESS_KEY:SECRET_KEY@{endpoint_authority}")
}

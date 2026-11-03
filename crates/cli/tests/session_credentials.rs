#[allow(dead_code)]
mod admin_support;

use std::process::{Command, Output};

use admin_support::{rc_binary, start_admin_response_test_server};

const TOKEN: &str = "temporary-TOKEN+/=";
const LIST: &str =
    "<ListBucketResult><Name>bucket</Name><IsTruncated>false</IsTruncated></ListBucketResult>";

fn run(
    endpoint: &str,
    config: &std::path::Path,
    args: &[&str],
    log_filter: Option<&str>,
) -> Output {
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    let authority = endpoint.strip_prefix("http://").unwrap();
    command
        .args(args)
        .env_remove("RUST_LOG")
        .env("RC_CONFIG_DIR", config)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("RC_HOST_temporary", format!(
            "http://temporary-access:temporary-secret@{authority}?session_token=temporary-TOKEN%2B%2F%3D"
        ));
    if let Some(log_filter) = log_filter {
        command.env("RUST_LOG", log_filter);
    }
    command.output().expect("run rc")
}

fn assert_redacted(output: &Output) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in [TOKEN, "temporary-secret", "temporary-access"] {
        assert!(!text.contains(secret), "credential leaked: {text}");
    }
}

#[test]
fn s3_and_admin_sign_with_the_same_process_only_session_token() {
    for log_filter in [
        None,
        Some("debug"),
        Some("trace"),
        Some("aws_smithy_runtime::client::orchestrator=trace"),
    ] {
        for (args, body) in [
            (vec!["--debug", "ls", "temporary/bucket"], LIST),
            (vec!["--debug", "admin", "user", "list", "temporary"], "{}"),
        ] {
            let config = tempfile::tempdir().unwrap();
            let (endpoint, requests, server) =
                start_admin_response_test_server("200 OK", "application/xml", body.into());
            let output = run(&endpoint, config.path(), &args, log_filter);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_redacted(&output);
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(
                request.headers.lines().any(|line| {
                    line.split_once(':').is_some_and(|(name, value)| {
                        name.eq_ignore_ascii_case("x-amz-security-token") && value.trim() == TOKEN
                    })
                }),
                "{}",
                request.headers
            );
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("signedheaders=")
            );
            assert!(!request.target.contains("session_token"));
            assert!(!config.path().join("config.toml").exists());
            server.join().unwrap();
        }
    }
}

#[test]
fn expired_or_invalid_tokens_return_auth_error_without_changing_aliases() {
    for code in ["ExpiredToken", "InvalidToken"] {
        for admin in [false, true] {
            let config = tempfile::tempdir().unwrap();
            let config_path = config.path().join("config.toml");
            let saved = "schema_version = 1\n[[aliases]]\nname = 'temporary'\nendpoint = 'http://stored.local'\naccess_key = 'stored-access'\nsecret_key = 'stored-secret'\n";
            std::fs::write(&config_path, saved).unwrap();
            let body = if admin {
                format!(r#"{{"Code":"{code}","Message":"{TOKEN} temporary-secret"}}"#)
            } else {
                format!(
                    "<Error><Code>{code}</Code><Message>{TOKEN} temporary-secret</Message></Error>"
                )
            };
            let (endpoint, requests, server) =
                start_admin_response_test_server("400 Bad Request", "application/xml", body);
            let args = if admin {
                vec!["--json", "admin", "user", "list", "temporary"]
            } else {
                vec!["--json", "ls", "temporary/bucket"]
            };
            let output = run(&endpoint, config.path(), &args, None);
            assert_eq!(
                output.status.code(),
                Some(4),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("renew"));
            assert_redacted(&output);
            let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["code"], 4);
            assert_eq!(error["details"]["type"], "auth_error");
            assert_eq!(std::fs::read_to_string(config_path).unwrap(), saved);
            requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            server.join().unwrap();
        }
    }
}

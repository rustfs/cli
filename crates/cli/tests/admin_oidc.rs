#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_response_test_server};
use jsonschema::Validator;
use serde_json::Value;

const PROVIDERS: &str = r#"{
  "providers":[{
    "provider_id":"corp",
    "source":"persisted",
    "editable":true,
    "enabled":true,
    "display_name":"Corporate",
    "config_url":"https://idp.example",
    "issuer":"https://idp.example",
    "client_id":"rustfs-console",
    "client_secret_configured":true,
    "scopes":["openid","profile","email"],
    "other_audiences":["rustfs"],
    "redirect_uri":null,
    "redirect_uri_dynamic":true,
    "claim_name":"policy",
    "claim_prefix":"",
    "role_policy":"",
    "groups_claim":"groups",
    "roles_claim":"roles",
    "email_claim":"email",
    "username_claim":"preferred_username",
    "hide_from_ui":false
  }],
  "restart_required":false
}"#;

fn output_v3_validator() -> Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

fn assert_valid_v3(value: &Value) {
    let errors = output_v3_validator()
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "invalid v3 output:\n{}",
        errors.join("\n")
    );
}

#[test]
fn oidc_list_uses_typed_secret_free_schema() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", PROVIDERS.to_string());

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "idp", "openid", "list", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(!stdout.contains("client_secret\""));
    assert!(!stdout.contains("MUST_NOT_APPEAR"));
    let value: Value = serde_json::from_str(&stdout).expect("JSON stdout");
    assert_eq!(value["type"], "oidc");
    assert_eq!(value["data"]["operation"], "list");
    assert_eq!(value["data"]["providers"][0]["provider_id"], "corp");
    assert_eq!(
        value["data"]["providers"][0]["client_secret_configured"],
        true
    );
    assert_valid_v3(&value);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/oidc/config");
    handle.join().expect("server");
}

#[test]
fn oidc_get_filters_exact_provider_without_an_unbacked_route() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", PROVIDERS.to_string());

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "idp", "openid", "get", "myalias", "corp"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
    assert_eq!(value["data"]["operation"], "get");
    assert_eq!(value["data"]["provider"]["provider_id"], "corp");
    assert_valid_v3(&value);
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("request");
    assert_eq!(request.target, "/rustfs/admin/v3/oidc/config");
    handle.join().expect("server");
}

#[test]
fn oidc_validate_is_non_mutating_and_never_sends_a_client_secret() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let response = r#"{
      "valid":true,
      "message":"OIDC configuration is valid",
      "issuer":"https://idp.example",
      "authorization_endpoint":"https://idp.example/authorize",
      "token_endpoint":"https://idp.example/token"
    }"#;
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", response.to_string());

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "idp",
            "openid",
            "validate",
            "myalias",
            "corp",
            "--config-url",
            "https://idp.example",
            "--client-id",
            "rustfs-console",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
    assert_eq!(value["data"]["operation"], "validate");
    assert_eq!(value["data"]["valid"], true);
    assert_valid_v3(&value);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rustfs/admin/v3/oidc/validate");
    let request_body = String::from_utf8(request.body).expect("UTF-8 request");
    assert!(!request_body.contains("client_secret"));
    let body: Value = serde_json::from_str(&request_body).expect("JSON request");
    assert_eq!(body["provider_id"], "corp");
    assert_eq!(body["scopes"][0], "openid");
    handle.join().expect("server");
}

#[test]
fn oidc_routes_fail_closed_and_redact_server_error_bodies() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "404 Not Found",
        "application/json",
        r#"{"message":"MUST_NOT_APPEAR"}"#.to_string(),
    );
    let output = Command::new(rc_binary())
        .args(["--json", "admin", "idp", "openid", "list", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");

    assert_eq!(output.status.code(), Some(7));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(!stderr.contains("MUST_NOT_APPEAR"));
    let value: Value = serde_json::from_str(&stderr).expect("JSON stderr");
    assert_eq!(value["error"]["type"], "unsupported_feature");
    assert_eq!(value["error"]["capability"], "admin.oidc-config-read");
    assert_valid_v3(&value);
    handle.join().expect("server");
}

#[test]
fn oidc_validate_rejects_invalid_urls_before_network_io() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args([
            "admin",
            "idp",
            "openid",
            "validate",
            "myalias",
            "corp",
            "--config-url",
            "file:///etc/passwd",
            "--client-id",
            "console",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_myalias",
            "http://ACCESS_KEY:SECRET_KEY@127.0.0.1:1",
        )
        .output()
        .expect("run rc");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("absolute HTTP URL"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

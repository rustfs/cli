#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_response_test_server, start_admin_sequence_test_server,
};
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

const EMPTY_PROVIDERS: &str = r#"{"providers":[],"restart_required":false}"#;
const VALIDATION_OK: &str = r#"{
  "valid":true,
  "message":"OIDC configuration is valid",
  "issuer":"https://idp.example",
  "authorization_endpoint":"https://idp.example/authorize",
  "token_endpoint":"https://idp.example/token"
}"#;
const MUTATION_OK: &str =
    r#"{"success":true,"message":"OIDC provider saved","restart_required":true}"#;
const ENV_PROVIDER: &str = r#"{
  "providers":[{
    "provider_id":"corp",
    "source":"env",
    "editable":false,
    "enabled":true,
    "display_name":"Corporate",
    "config_url":"https://idp.example",
    "issuer":"https://idp.example",
    "client_id":"rustfs-console",
    "client_secret_configured":true,
    "scopes":["openid"],
    "other_audiences":[],
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

#[test]
fn oidc_set_creates_a_complete_provider_after_preflight() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", EMPTY_PROVIDERS),
        ("200 OK", VALIDATION_OK),
        ("200 OK", MUTATION_OK),
    ]);
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "idp",
            "openid",
            "set",
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
    assert_eq!(value["data"]["operation"], "set");
    assert_eq!(value["data"]["created"], true);
    assert_eq!(value["data"]["restart_required"], true);
    assert_valid_v3(&value);

    let get = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("GET request");
    let validate = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("validate request");
    let put = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("PUT request");
    assert_eq!(get.method, "GET");
    assert_eq!(validate.method, "POST");
    assert_eq!(validate.target, "/rustfs/admin/v3/oidc/validate");
    assert_eq!(put.method, "PUT");
    assert_eq!(put.target, "/rustfs/admin/v3/oidc/config/corp");
    let body: Value = serde_json::from_slice(&put.body).expect("PUT JSON");
    assert_eq!(body["config_url"], "https://idp.example");
    assert_eq!(body["client_id"], "rustfs-console");
    assert_eq!(body["scopes"][0], "openid");
    assert!(body.get("provider_id").is_none());
    assert!(body.get("client_secret").is_none());
    handle.join().expect("server");
}

#[test]
fn oidc_set_preserves_omitted_fields_and_secret() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", PROVIDERS),
        ("200 OK", VALIDATION_OK),
        ("200 OK", MUTATION_OK),
    ]);
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "idp",
            "openid",
            "update",
            "myalias",
            "corp",
            "--display-name",
            "Updated Corporate",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
    assert_eq!(value["data"]["operation"], "update");
    assert_eq!(value["data"]["changes"].as_array().map(Vec::len), Some(1));

    let _get = receiver.recv().expect("GET");
    let _validate = receiver.recv().expect("validate");
    let put = receiver.recv().expect("PUT");
    let body: Value = serde_json::from_slice(&put.body).expect("PUT JSON");
    assert_eq!(body["display_name"], "Updated Corporate");
    assert_eq!(body["config_url"], "https://idp.example");
    assert_eq!(body["issuer"], "https://idp.example");
    assert_eq!(body["other_audiences"][0], "rustfs");
    assert!(body.get("client_secret").is_none());
    handle.join().expect("server");
}

#[test]
fn oidc_secret_file_replacement_is_acknowledged_and_redacted() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let secret_file = config_dir.path().join("oidc-secret");
    let secret = format!("generated-secret-{}", std::process::id());
    std::fs::write(&secret_file, format!("{secret}\n")).expect("write secret file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600))
            .expect("protect secret file");
    }
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", PROVIDERS),
        ("200 OK", VALIDATION_OK),
        ("200 OK", MUTATION_OK),
    ]);
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "idp",
            "openid",
            "set",
            "myalias",
            "corp",
            "--client-secret-file",
            secret_file.to_str().expect("UTF-8 path"),
            "--replace-client-secret",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&secret));
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
    assert_eq!(
        value["data"]["changes"]
            .as_array()
            .and_then(|changes| changes
                .iter()
                .find(|change| change["field"] == "client_secret"))
            .map(|change| &change["after"]),
        Some(&Value::String("[replaced]".to_string()))
    );
    let _get = receiver.recv().expect("GET");
    let validate = receiver.recv().expect("validate");
    assert!(!String::from_utf8_lossy(&validate.body).contains(&secret));
    assert!(
        serde_json::from_slice::<Value>(&validate.body)
            .expect("validate JSON")
            .get("client_secret")
            .is_none()
    );
    let put = receiver.recv().expect("PUT");
    let body: Value = serde_json::from_slice(&put.body).expect("PUT JSON");
    assert_eq!(body["client_secret"], secret);
    handle.join().expect("server");
}

#[test]
fn oidc_secret_can_be_replaced_from_stdin_without_argv_exposure() {
    use std::io::Write as _;

    let config_dir = tempfile::tempdir().expect("create config dir");
    let secret = format!("stdin-generated-secret-{}", std::process::id());
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", PROVIDERS),
        ("200 OK", VALIDATION_OK),
        ("200 OK", MUTATION_OK),
    ]);
    let mut child = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "idp",
            "openid",
            "set",
            "myalias",
            "corp",
            "--client-secret-stdin",
            "--replace-client-secret",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rc");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(format!("{secret}\n").as_bytes())
        .expect("write client secret");
    let output = child.wait_with_output().expect("wait for rc");
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&secret));
    let _get = receiver.recv().expect("GET");
    let validate = receiver.recv().expect("validate");
    assert!(!String::from_utf8_lossy(&validate.body).contains(&secret));
    let put = receiver.recv().expect("PUT");
    assert_eq!(
        serde_json::from_slice::<Value>(&put.body).expect("PUT JSON")["client_secret"],
        secret
    );
    handle.join().expect("server");
}

#[test]
fn oidc_dry_run_validates_but_never_puts() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_sequence_test_server(vec![("200 OK", PROVIDERS), ("200 OK", VALIDATION_OK)]);
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "idp",
            "openid",
            "disable",
            "myalias",
            "corp",
            "--dry-run",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
    assert_eq!(value["data"]["operation"], "disable");
    assert_eq!(value["data"]["dry_run"], true);
    assert_eq!(receiver.recv().expect("GET").method, "GET");
    assert_eq!(receiver.recv().expect("validate").method, "POST");
    handle.join().expect("server");
}

#[test]
fn oidc_disable_preserves_unrelated_fields_in_put() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", PROVIDERS),
        ("200 OK", VALIDATION_OK),
        ("200 OK", MUTATION_OK),
    ]);
    let output = Command::new(rc_binary())
        .args([
            "--json", "admin", "idp", "openid", "disable", "myalias", "corp",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert!(output.status.success());
    let _get = receiver.recv().expect("GET");
    let _validate = receiver.recv().expect("validate");
    let put = receiver.recv().expect("PUT");
    let body: Value = serde_json::from_slice(&put.body).expect("PUT JSON");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["display_name"], "Corporate");
    assert_eq!(body["client_id"], "rustfs-console");
    assert!(body.get("client_secret").is_none());
    handle.join().expect("server");
}

#[test]
fn oidc_environment_provider_fails_before_validation_or_mutation() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_sequence_test_server(vec![("200 OK", ENV_PROVIDER)]);
    let output = Command::new(rc_binary())
        .args([
            "--json", "admin", "idp", "openid", "disable", "myalias", "corp",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert_eq!(output.status.code(), Some(6));
    let value: Value = serde_json::from_slice(&output.stderr).expect("JSON stderr");
    assert_eq!(value["error"]["type"], "conflict");
    assert_eq!(receiver.recv().expect("GET").method, "GET");
    handle.join().expect("server");
}

#[test]
fn oidc_validation_and_mutation_failures_have_distinct_classes() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", PROVIDERS),
        ("400 Bad Request", r#"{"message":"issuer unavailable"}"#),
    ]);
    let validation_failure = Command::new(rc_binary())
        .args([
            "--json", "admin", "idp", "openid", "disable", "myalias", "corp",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert_eq!(validation_failure.status.code(), Some(1));
    let error: Value =
        serde_json::from_slice(&validation_failure.stderr).expect("validation error JSON");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.ends_with("OIDC issuer validation failed"))
    );
    assert!(!String::from_utf8_lossy(&validation_failure.stderr).contains("issuer unavailable"));
    handle.join().expect("validation server");

    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", PROVIDERS),
        ("200 OK", VALIDATION_OK),
        ("409 Conflict", r#"{"message":"changed"}"#),
    ]);
    let conflict = Command::new(rc_binary())
        .args([
            "--json", "admin", "idp", "openid", "disable", "myalias", "corp",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc");
    assert_eq!(conflict.status.code(), Some(6));
    let error: Value = serde_json::from_slice(&conflict.stderr).expect("conflict JSON");
    assert_eq!(error["error"]["type"], "conflict");
    assert!(!String::from_utf8_lossy(&conflict.stderr).contains("\"changed\""));
    handle.join().expect("conflict server");
}

#[test]
fn oidc_secret_input_requires_explicit_replacement_acknowledgement() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args([
            "admin",
            "idp",
            "openid",
            "set",
            "myalias",
            "corp",
            "--client-secret-stdin",
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
        String::from_utf8_lossy(&output.stderr).contains("--replace-client-secret"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

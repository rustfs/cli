#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_sequence_test_server, start_admin_test_server,
};

const EDIT_INFO_RESPONSE: &str = r#"{
    "enabled":true,
    "name":"primary",
    "sites":[{
        "endpoint":"https://secondary.example.test",
        "name":"secondary",
        "deploymentID":"deployment-2",
        "sync":"future-sync-mode",
        "defaultbandwidth":{"futureShape":[1,{"safe":true}]},
        "replicate-ilm-expiry":true,
        "objectNamingMode":"path",
        "skipTlsVerify":false,
        "caCertPem":"ORIGINAL-CA-MUST-NOT-PRINT",
        "apiVersion":"v1",
        "futurePeer":{"mode":"preserved","sessionToken":"OPAQUE-TOKEN-MUST-NOT-PRINT"}
    }],
    "serviceAccountAccessKey":"DISCARDED-SERVICE-KEY",
    "apiVersion":"v1"
}"#;

const EDIT_SUCCESS_RESPONSE: &str =
    r#"{"success":true,"status":"updated","errorDetail":"","apiVersion":"v1"}"#;

fn run_edit_and_capture_body(info: &'static str, options: &[String]) -> serde_json::Value {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_sequence_test_server(vec![("200 OK", info), ("200 OK", EDIT_SUCCESS_RESPONSE)]);
    let mut command = Command::new(rc_binary());
    command.args([
        "--json",
        "admin",
        "replicate",
        "edit",
        "myalias",
        "--site",
        "deployment-2",
        "--yes",
    ]);
    command.args(options);
    let output = command
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    let edit_request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured edit request");
    handle.join().expect("admin test server finished");
    serde_json::from_slice(&edit_request.body).expect("edit request JSON")
}

fn first_system_certificate() -> String {
    include_str!("../../core/src/admin/test_ca.pem").to_string()
}

#[test]
fn replicate_info_dispatches_to_site_replication_info() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"enabled":false}"#);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "info", "myalias"])
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
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "admin_operations");
    assert_eq!(payload["data"]["operations"][0]["changed"], false);
    assert_eq!(payload["data"]["operations"][0]["result"]["enabled"], false);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/site-replication/info");

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_info_human_output_lists_sites() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"enabled":true,"name":"site1","sites":[{"name":"site1","endpoint":"http://10.0.0.5:9000"},{"name":"site2","endpoint":"http://10.0.0.6:9000"}]}"#,
    );

    let output = Command::new(rc_binary())
        .args(["admin", "replicate", "info", "myalias"])
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
    assert!(stdout.contains("site1"), "stdout: {stdout}");
    assert!(stdout.contains("http://10.0.0.6:9000"), "stdout: {stdout}");

    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_status_requests_default_summary_sections() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"enabled":true,"MaxBuckets":2,"MaxUsers":1,"MaxGroups":0,"MaxPolicies":5,"Sites":{"dep-1":{"name":"site1","endpoint":"http://10.0.0.5:9000"}}}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "status", "myalias"])
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
    assert_eq!(payload["enabled"], true);
    assert_eq!(payload["MaxBuckets"], 2);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/site-replication/status?buckets=true&users=true&groups=true&policies=true"
    );

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_status_forwards_selected_section_flags() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"enabled":true}"#);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "status",
            "myalias",
            "--buckets",
            "--metrics",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/site-replication/status?buckets=true&metrics=true"
    );

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_add_dispatches_with_resolved_alias_sites() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"success":true,"status":"Requested sites were configured for replication successfully."}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "add", "sitea", "siteb"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_sitea", rc_host_alias(&endpoint))
        .env("RC_HOST_siteb", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(payload["success"], true);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/rustfs/admin/v3/site-replication/add");

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_add_rejects_single_alias() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["admin", "replicate", "add", "onlyone"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert!(!output.status.success());
}

#[test]
fn replicate_remove_all_dispatches_to_site_replication_remove() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"status":"Requested site(s) were removed from cluster replication successfully."}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "remove", "myalias", "--all"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/rustfs/admin/v3/site-replication/remove");

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_remove_requires_site_or_all() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["admin", "replicate", "remove", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn replicate_info_json_uses_safe_projection() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_test_server(EDIT_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "info", "myalias"])
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
    assert_eq!(
        payload["data"]["operations"][0]["result"]["sites"][0]["hasCustomCA"],
        true
    );
    assert_eq!(
        payload["data"]["operations"][0]["result"]["sites"][0]["sync"],
        "future-sync-mode"
    );
    for sensitive in [
        "serviceAccountAccessKey",
        "DISCARDED-SERVICE-KEY",
        "caCertPem",
        "ORIGINAL-CA-MUST-NOT-PRINT",
        "sessionToken",
        "OPAQUE-TOKEN-MUST-NOT-PRINT",
    ] {
        assert!(
            !stdout.contains(sensitive),
            "stdout leaked {sensitive}: {stdout}"
        );
    }
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_overlays_endpoint_and_preserves_peer_document() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", EDIT_INFO_RESPONSE),
        ("200 OK", EDIT_SUCCESS_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "deployment-2",
            "--endpoint",
            "https://new.example.test",
            "--yes",
        ])
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
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "admin_operations");
    assert_eq!(payload["data"]["operations"][0]["state"], "succeeded");
    assert_eq!(payload["data"]["operations"][0]["changed"], true);
    assert!(!stdout.contains("ORIGINAL-CA-MUST-NOT-PRINT"));
    assert!(!stdout.contains("DISCARDED-SERVICE-KEY"));

    let info_request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    assert_eq!(info_request.method, "GET");
    let edit_request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured edit request");
    assert_eq!(edit_request.method, "PUT");
    let body: serde_json::Value =
        serde_json::from_slice(&edit_request.body).expect("edit request JSON");
    assert_eq!(body["endpoint"], "https://new.example.test");
    assert_eq!(body["caCertPem"], "ORIGINAL-CA-MUST-NOT-PRINT");
    assert_eq!(body["sync"], "future-sync-mode");
    assert_eq!(body["defaultbandwidth"]["futureShape"][1]["safe"], true);
    assert_eq!(body["futurePeer"]["mode"], "preserved");
    assert_eq!(
        body["futurePeer"]["sessionToken"],
        "OPAQUE-TOKEN-MUST-NOT-PRINT"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_maps_typed_state_change_to_conflict_exit_code() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", EDIT_INFO_RESPONSE),
        (
            "200 OK",
            r#"{"success":false,"status":"site replication state changed","errorDetail":"","apiVersion":"v1"}"#,
        ),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "deployment-2",
            "--endpoint",
            "https://new.example.test",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error");
    assert_eq!(payload["error"]["type"], "conflict");
    assert!(!stderr.contains("site replication state changed"));
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured edit request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_put_encodes_tls_and_ca_tristate() {
    const INSECURE_INFO: &str = r#"{
        "enabled":true,
        "sites":[{
            "endpoint":"https://secondary.example.test",
            "name":"secondary",
            "deploymentID":"deployment-2",
            "skipTlsVerify":true,
            "caCertPem":"ORIGINAL-CA"
        }]
    }"#;

    let verified = run_edit_and_capture_body(INSECURE_INFO, &["--verify-tls".into()]);
    assert_eq!(verified["skipTlsVerify"], false);
    assert_eq!(verified["caCertPem"], "ORIGINAL-CA");

    let insecure = run_edit_and_capture_body(EDIT_INFO_RESPONSE, &["--skip-tls-verify".into()]);
    assert_eq!(insecure["skipTlsVerify"], true);
    assert_eq!(insecure["caCertPem"], "");

    let cleared = run_edit_and_capture_body(EDIT_INFO_RESPONSE, &["--clear-ca-cert".into()]);
    assert_eq!(cleared["skipTlsVerify"], false);
    assert_eq!(cleared["caCertPem"], "");

    let temp_dir = tempfile::tempdir().expect("create certificate temp dir");
    let ca_path = temp_dir.path().join("ca.pem");
    let certificate = first_system_certificate();
    std::fs::write(&ca_path, &certificate).expect("write test CA certificate");
    let ca_set = run_edit_and_capture_body(
        EDIT_INFO_RESPONSE,
        &["--ca-cert".into(), ca_path.to_string_lossy().into_owned()],
    );
    assert_eq!(ca_set["skipTlsVerify"], false);
    assert_eq!(ca_set["caCertPem"], certificate);

    let renamed =
        run_edit_and_capture_body(EDIT_INFO_RESPONSE, &["--name".into(), "renamed".into()]);
    assert_eq!(renamed["deploymentID"], "deployment-2");
    assert_eq!(renamed["name"], "renamed");

    let converted_to_http = run_edit_and_capture_body(
        EDIT_INFO_RESPONSE,
        &[
            "--endpoint".into(),
            "http://secondary.example.test".into(),
            "--clear-ca-cert".into(),
        ],
    );
    assert_eq!(
        converted_to_http["endpoint"],
        "http://secondary.example.test"
    );
    assert_eq!(converted_to_http["skipTlsVerify"], false);
    assert_eq!(converted_to_http["caCertPem"], "");

    const HTTP_INACTIVE_TLS_INFO: &str = r#"{
        "enabled":true,
        "sites":[{
            "endpoint":"http://old.example.test",
            "name":"secondary",
            "deploymentID":"deployment-2",
            "skipTlsVerify":false,
            "caCertPem":""
        }]
    }"#;
    let http_edit = run_edit_and_capture_body(
        HTTP_INACTIVE_TLS_INFO,
        &["--endpoint".into(), "http://new.example.test".into()],
    );
    assert_eq!(http_edit["endpoint"], "http://new.example.test");
}

#[test]
fn replicate_edit_requires_confirmation_before_alias_lookup() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "missing-alias",
            "--site",
            "deployment-2",
            "--endpoint",
            "https://new.example.test",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "admin_operations");
    assert_eq!(payload["error"]["type"], "usage_error");
    assert!(
        payload["error"]["message"]
            .as_str()
            .expect("message")
            .contains("--yes")
    );
    assert!(!stderr.contains("Alias 'missing-alias' not found"));
}

#[test]
fn replicate_edit_requires_at_least_one_edit_flag_before_alias_lookup() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "missing-alias",
            "--site",
            "deployment-2",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(
        stderr.contains("at least one edit option"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("missing-alias"));
}

#[test]
fn replicate_edit_rejects_conflicting_tls_flags_in_clap() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "replicate",
            "edit",
            "missing-alias",
            "--site",
            "deployment-2",
            "--skip-tls-verify",
            "--verify-tls",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("cannot be used with"), "stderr: {stderr}");

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "replicate",
            "edit",
            "missing-alias",
            "--site",
            "deployment-2",
            "--skip-tls-verify",
            "--ca-cert",
            "ca.pem",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("cannot be used with"), "stderr: {stderr}");
}

#[test]
fn replicate_edit_rejects_conflicting_ca_flags_in_clap() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "replicate",
            "edit",
            "missing-alias",
            "--site",
            "deployment-2",
            "--ca-cert",
            "ca.pem",
            "--clear-ca-cert",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("cannot be used with"), "stderr: {stderr}");
}

#[test]
fn replicate_edit_reports_not_found_for_non_exact_site() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(EDIT_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "second",
            "--endpoint",
            "https://new.example.test",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error");
    assert_eq!(payload["error"]["type"], "not_found");
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    assert_eq!(request.method, "GET");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_rejects_no_effective_change_as_usage() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(EDIT_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "deployment-2",
            "--endpoint",
            "https://secondary.example.test/",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error");
    assert_eq!(payload["error"]["type"], "usage_error");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_rejects_ca_whitespace_only_change_without_put() {
    let certificate = first_system_certificate();
    let info = serde_json::json!({
        "enabled": true,
        "sites": [{
            "endpoint": "https://secondary.example.test",
            "name": "secondary",
            "deploymentID": "deployment-2",
            "skipTlsVerify": false,
            "caCertPem": certificate,
        }]
    })
    .to_string();
    let info: &'static str = Box::leak(info.into_boxed_str());
    let config_dir = tempfile::tempdir().expect("create config dir");
    let temp_dir = tempfile::tempdir().expect("create certificate temp dir");
    let ca_path = temp_dir.path().join("ca.pem");
    std::fs::write(&ca_path, format!("{certificate}\n   \n")).expect("write whitespace-variant CA");
    let (endpoint, receiver, handle) = start_admin_test_server(info);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "deployment-2",
            "--ca-cert",
        ])
        .arg(&ca_path)
        .arg("--yes")
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_reports_ambiguous_exact_name_as_conflict() {
    const AMBIGUOUS_INFO: &str = r#"{
        "enabled":true,
        "name":"primary",
        "sites":[
            {"endpoint":"https://one.example.test","name":"duplicate","deploymentID":"one"},
            {"endpoint":"https://two.example.test","name":"duplicate","deploymentID":"two"}
        ]
    }"#;
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(AMBIGUOUS_INFO);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "duplicate",
            "--endpoint",
            "https://new.example.test",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error");
    assert_eq!(payload["error"]["type"], "conflict");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_renames_local_deployment_by_id() {
    const LOCAL_INFO: &str = r#"{
        "enabled":true,
        "name":"primary",
        "sites":[{
            "endpoint":"https://primary.example.test",
            "name":"primary",
            "deploymentID":"local-deployment",
            "skipTlsVerify":false,
            "caCertPem":""
        }]
    }"#;
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", LOCAL_INFO),
        ("200 OK", EDIT_SUCCESS_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "local-deployment",
            "--name",
            "primary-renamed",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    let edit = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured edit request");
    let body: serde_json::Value = serde_json::from_slice(&edit.body).expect("edit request JSON");
    assert_eq!(body["deploymentID"], "local-deployment");
    assert_eq!(body["name"], "primary-renamed");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_rejects_malformed_selected_peer_before_put() {
    const MALFORMED_INFO: &str = r#"{
        "enabled":true,
        "sites":[{
            "endpoint":"https://secondary.example.test",
            "deploymentID":"deployment-2"
        }]
    }"#;
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(MALFORMED_INFO);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "deployment-2",
            "--endpoint",
            "https://new.example.test",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(1));
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_edit_rejects_http_final_state_with_active_tls_values() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(EDIT_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "edit",
            "myalias",
            "--site",
            "deployment-2",
            "--endpoint",
            "http://secondary.example.test",
            "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured info request");
    handle.join().expect("admin test server finished");
}

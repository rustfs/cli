#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_response_test_server, start_admin_sequence_test_server,
};
use jsonschema::Validator;
use serde_json::Value;

const STORAGE_RESPONSE: &str = r#"{
  "info":{
    "disks":[
      {"endpoint":"http://node1:9000","path":"/data1","state":"online","runtimeState":"online","totalspace":100,"usedspace":40,"availspace":60,"pool_index":0,"set_index":0,"disk_index":0},
      {"endpoint":"http://node1:9000","path":"/data2","state":"offline","runtimeState":"offline","offlineDurationSeconds":30,"totalspace":200,"usedspace":50,"availspace":150,"pool_index":0,"set_index":0,"disk_index":1}
    ],
    "backend":{"BackendType":"Erasure","OnlineDisks":{"set-1":1},"OfflineDisks":{"set-1":1},"StandardSCParity":1,"TotalSets":[1],"DrivesPerSet":[2]}
  },
  "admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities"}
}"#;
const CAPABILITY_INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
const UNKNOWN_CAPABILITY_INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.11","drives":[]}]}}"#;
const RUNTIME_CAPABILITIES_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled"},"memory_sampling":{"state":"unsupported"},"platform":{"state":"supported"},"topology":{"state":"supported"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"supported"}}"#;
const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
const SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"supported"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;
const STORAGE_METRICS_RESPONSE: &str = r#"{
  "info":{
    "disks":[
      {
        "endpoint":"http://node1:9000",
        "path":"/data1",
        "state":"online",
        "runtimeState":"online",
        "totalspace":100,
        "usedspace":40,
        "availspace":60,
        "readthroughput":125.5,
        "writethroughput":81.25,
        "readlatency":0.75,
        "writelatency":1.25,
        "utilization":40.0,
        "metrics":{
          "last_minute":{"read":{"count":4}},
          "api_calls":{"GetObject":9},
          "total_waiting":2,
          "total_errors_availability":3,
          "total_errors_timeout":4,
          "total_writes":5,
          "total_deletes":6
        },
        "pool_index":0,
        "set_index":0,
        "disk_index":0
      },
      {
        "endpoint":"http://node1:9000",
        "path":"/data2",
        "state":"online",
        "totalspace":200,
        "usedspace":50,
        "availspace":150,
        "pool_index":0,
        "set_index":0,
        "disk_index":1
      }
    ],
    "backend":{"BackendType":"Erasure","OnlineDisks":{"set-1":2},"OfflineDisks":{}}
  }
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

#[test]
fn storage_info_json_is_typed_v3_output() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        STORAGE_RESPONSE.to_string(),
    );
    let output = Command::new(rc_binary())
        .args(["--json", "admin", "info", "storage", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "storage_info");
    assert_eq!(value["data"]["summary"]["total_capacity_bytes"], 300);
    assert_eq!(value["data"]["summary"]["online_disks"], 1);
    assert_eq!(value["data"]["disks"][1]["offline_duration_seconds"], 30);
    let errors = output_v3_validator()
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "invalid v3 output:\n{}",
        errors.join("\n")
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.target, "/rustfs/admin/v3/storageinfo");
    handle.join().expect("admin test server finished");
}

#[test]
fn storage_info_human_output_is_deterministic_and_marks_offline_disks() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        STORAGE_RESPONSE.to_string(),
    );
    let output = Command::new(rc_binary())
        .args(["admin", "info", "storage", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("Storage Information"), "stdout: {stdout}");
    assert!(
        stdout.contains("Disks:          2 (1 online, 1 offline)"),
        "stdout: {stdout}"
    );
    let data1 = stdout.find("/data1").expect("data1 row");
    let data2 = stdout.find("/data2").expect("data2 row");
    assert!(
        data1 < data2,
        "disk rows should retain deterministic server order"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn storage_info_permission_denial_and_unsupported_have_distinct_exit_codes() {
    for (status, expected_code) in [("403 Forbidden", 4), ("404 Not Found", 7)] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) = start_admin_response_test_server(
            status,
            "application/json",
            r#"{"code":"AccessDenied","message":"denied or unavailable"}"#.to_string(),
        );
        let output = Command::new(rc_binary())
            .args(["admin", "info", "storage", "myalias"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(output.status.code(), Some(expected_code));
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn storage_info_rejects_malformed_payload() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        r#"{"info":"wrong"}"#.to_string(),
    );
    let output = Command::new(rc_binary())
        .args(["admin", "info", "storage", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(1));
    handle.join().expect("admin test server finished");
}

#[test]
fn storage_metrics_json_is_capability_gated_and_labels_observations() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", CAPABILITY_INFO_RESPONSE),
        ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", STORAGE_METRICS_RESPONSE),
    ]);
    let output = Command::new(rc_binary())
        .args(["--json", "admin", "info", "storage", "myalias", "--metrics"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    let first = &value["data"]["disks"][0]["observations"];
    assert_eq!(first["source"], "storage_info");
    assert_eq!(first["mode"], "observed");
    assert_eq!(first["read_throughput"], 125.5);
    assert_eq!(first["write_latency"], 1.25);
    assert_eq!(first["utilization"], 40.0);
    assert_eq!(first["operation_counters"]["total_waiting"], 2);
    assert_eq!(first["operation_counters"]["api_calls"]["GetObject"], 9);

    let missing = &value["data"]["disks"][1]["observations"];
    assert_eq!(missing["source"], "storage_info");
    assert_eq!(missing["mode"], "observed");
    assert!(missing["read_throughput"].is_null());
    assert!(missing["write_throughput"].is_null());
    assert!(missing["read_latency"].is_null());
    assert!(missing["write_latency"].is_null());
    assert!(missing["utilization"].is_null());
    assert!(missing["operation_counters"].is_null());
    assert!(output_v3_validator().is_valid(&value));

    let requests = (0..5)
        .map(|_| {
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("captured admin request")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        vec![
            "/rustfs/admin/v3/info",
            "/rustfs/admin/v4/runtime/capabilities",
            "/rustfs/admin/v4/extensions/catalog",
            "/rustfs/admin/v4/cluster/snapshot",
            "/rustfs/admin/v3/storageinfo",
        ]
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.target.contains("/speedtest/drive"))
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn storage_metrics_human_output_is_observed_and_explicitly_unavailable() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", CAPABILITY_INFO_RESPONSE),
        ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", STORAGE_METRICS_RESPONSE),
    ]);
    let output = Command::new(rc_binary())
        .args(["admin", "info", "storage", "myalias", "--metrics"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(
        stdout.contains("Observed storage metrics"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("not a benchmark"), "stdout: {stdout}");
    assert!(stdout.contains("read_throughput=125.5"), "stdout: {stdout}");
    assert!(
        stdout.contains("read_throughput=unavailable"),
        "stdout: {stdout}"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn storage_metrics_fail_closed_when_capability_is_unknown() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", UNKNOWN_CAPABILITY_INFO_RESPONSE),
        ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
    ]);
    let output = Command::new(rc_binary())
        .args(["--json", "admin", "info", "storage", "myalias", "--metrics"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(7));
    let value: Value = serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
    assert_eq!(value["error"]["type"], "unsupported_feature");
    assert_eq!(
        value["error"]["capability"],
        "admin.diagnostics.drive-observations"
    );
    let requests = (0..4)
        .map(|_| {
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("captured admin request")
        })
        .collect::<Vec<_>>();
    assert!(
        requests
            .iter()
            .all(|request| request.target != "/rustfs/admin/v3/storageinfo")
    );
    handle.join().expect("admin test server finished");
}

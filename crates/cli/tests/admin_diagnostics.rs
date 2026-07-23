#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};
use jsonschema::Validator;
use serde_json::Value;

const INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
const RUNTIME_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled"},"memory_sampling":{"state":"unsupported"},"platform":{"state":"supported"},"topology":{"state":"supported"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"supported"}}"#;
const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
const SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"supported"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;
const DEVNULL_RESPONSE: &str = r#"{"kind":"client-devnull","measured":true,"aggregate_write_throughput_bytes_per_sec":131072.0,"rx_bytes":65536,"duration_secs":0.5}"#;

fn output_v3_validator() -> Validator {
    let schema_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("schemas/output_v3.json");
    let schema = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", schema_path.display()));
    let schema: Value = serde_json::from_str(&schema).expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

#[test]
fn client_devnull_json_runs_bounded_preflight_and_upload() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        ("200 OK", RUNTIME_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", DEVNULL_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "diagnostics",
            "client-devnull",
            "myalias",
            "--size",
            "64KiB",
            "--timeout",
            "5s",
            "--concurrency",
            "1",
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
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    let result = &value["data"]["operations"][0]["result"];
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "admin_operations");
    assert_eq!(result["direction"], "client-to-server");
    assert_eq!(result["requested_bytes"], 65_536);
    assert_eq!(result["received_bytes"], 65_536);
    assert_eq!(result["concurrency"], 1);
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
            "/rustfs/admin/v3/speedtest/client/devnull",
        ]
    );
    assert_eq!(requests[4].method, "POST");
    assert_eq!(requests[4].body.len(), 65_536);
    assert!(requests[4].headers.contains("content-length: 65536"));
    handle.join().expect("admin test server finished");
}

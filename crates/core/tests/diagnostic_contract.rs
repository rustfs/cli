use rc_core::admin::{ClusterSnapshotDocument, DetailedHealthSnapshot, ExtensionsCatalog};

fn valid_health_snapshot() -> serde_json::Value {
    serde_json::json!({
        "version": "1.0.0-beta.10",
        "cpu": {
            "logical_cores": 8,
            "brand": "test-cpu",
            "frequency_mhz": 2400,
            "usage_percent": 12.5
        },
        "memory": {
            "total_bytes": 1024,
            "used_bytes": 512,
            "available_bytes": 512,
            "total_swap_bytes": 0,
            "used_swap_bytes": 0
        },
        "os": {
            "os": "linux",
            "kernel_version": "6.8",
            "os_version": "test",
            "hostname": "node-1",
            "arch": "x86_64",
            "uptime_secs": 60
        },
        "process": {
            "pid": 42,
            "cpu_usage_percent": 1.25,
            "memory_bytes": 128
        },
        "drives": [],
        "unsupported_probes": []
    })
}

#[test]
fn detailed_health_preserves_unsupported_probes_and_future_fields() {
    let value = serde_json::json!({
        "version": "1.0.0-beta.10",
        "deployment_id": "deployment-1",
        "region": "us-east-1",
        "timestamp": "2026-07-22T00:00:00Z",
        "cpu": {
            "logical_cores": 8,
            "brand": "test-cpu",
            "frequency_mhz": 2400,
            "usage_percent": 12.5,
            "future_cpu_field": true
        },
        "memory": {
            "total_bytes": 1024,
            "used_bytes": 512,
            "available_bytes": 512,
            "total_swap_bytes": 0,
            "used_swap_bytes": 0
        },
        "os": {
            "os": "linux",
            "kernel_version": "6.8",
            "os_version": "test",
            "hostname": "node-1",
            "arch": "x86_64",
            "uptime_secs": 60
        },
        "process": {
            "pid": 42,
            "cpu_usage_percent": 1.25,
            "memory_bytes": 128
        },
        "drives": [{
            "endpoint": "node-1",
            "drive_path": "/data1",
            "state": "ok",
            "total_space": 100,
            "used_space": 40,
            "available_space": 60,
            "read_throughput": 10.5,
            "write_throughput": 8.5,
            "read_latency": 0.1,
            "write_latency": 0.2,
            "future_drive_field": "kept"
        }],
        "unsupported_probes": ["perf-net", "config-obd"],
        "future_top_level": {"kept": true}
    });

    let snapshot: DetailedHealthSnapshot =
        serde_json::from_value(value).expect("health fixture should deserialize");

    assert_eq!(snapshot.unsupported_probes, ["perf-net", "config-obd"]);
    assert_eq!(snapshot.cpu.extra["future_cpu_field"], true);
    assert_eq!(snapshot.drives[0].extra["future_drive_field"], "kept");
    assert_eq!(snapshot.extra["future_top_level"]["kept"], true);
}

#[test]
fn detailed_health_rejects_missing_envelope_and_measurement_fields() {
    let mut fixtures = vec![
        ("empty envelope", serde_json::json!({})),
        ("empty cpu", valid_health_snapshot()),
        ("empty memory", valid_health_snapshot()),
        ("empty os", valid_health_snapshot()),
        ("empty process", valid_health_snapshot()),
        ("missing drives", valid_health_snapshot()),
        ("missing unsupported probes", valid_health_snapshot()),
    ];
    fixtures[1].1["cpu"] = serde_json::json!({});
    fixtures[2].1["memory"] = serde_json::json!({});
    fixtures[3].1["os"] = serde_json::json!({});
    fixtures[4].1["process"] = serde_json::json!({});
    fixtures[5]
        .1
        .as_object_mut()
        .expect("fixture must be an object")
        .remove("drives");
    fixtures[6]
        .1
        .as_object_mut()
        .expect("fixture must be an object")
        .remove("unsupported_probes");

    for (name, fixture) in fixtures {
        assert!(
            serde_json::from_value::<DetailedHealthSnapshot>(fixture).is_err(),
            "{name} must not deserialize as a valid health snapshot"
        );
    }
}

#[test]
fn cluster_snapshot_accepts_beta10_current_main_and_null_shapes() {
    let beta10 = serde_json::json!({
        "snapshot": {
            "summary": {
                "runtime": {"state": "supported", "future_status_field": 9},
                "topology": {"state": "supported"},
                "membership": {"state": "supported"},
                "peer_health": {"state": "supported"},
                "rpc_boundary": {"state": "supported"},
                "observability": {"state": "supported"},
                "workload_admission": {"state": "supported"},
                "actionable_pressure": {"state": "disabled"}
            },
            "runtime_capabilities_path": "/rustfs/admin/v4/runtime/capabilities",
            "extensions_catalog_path": "/rustfs/admin/v4/extensions/catalog",
            "topology": {"mode": "distributed"},
            "membership": {"nodes": [], "drives": []},
            "pool_state": {"pools": []},
            "local_storage": {"nodes": []},
            "peer_health": {"peers": []},
            "rpc_boundary": {"control_channels": [], "data_channels": []},
            "observability": {},
            "workload_admission": [],
            "runtime_status": {"state": "ready"},
            "actionable_pressure": false,
            "future_snapshot_field": 7
        },
        "future_envelope_field": true
    });
    let current = serde_json::json!({
        "snapshot": {
            "summary": {
                "runtime": {"state": "supported"},
                "topology": {"state": "supported"},
                "membership": {"state": "supported"},
                "storage": {"state": "supported"},
                "peer_health": {"state": "supported"},
                "listing": {"state": "supported"},
                "usage": {"state": "unknown", "reason": "scanner data is stale"},
                "rpc_boundary": {"state": "supported"},
                "observability": {"state": "supported"},
                "workload_admission": {"state": "supported"},
                "actionable_pressure": {"state": "disabled"}
            },
            "runtime_capabilities_path": "/rustfs/admin/v4/runtime/capabilities",
            "extensions_catalog_path": "/rustfs/admin/v4/extensions/catalog",
            "components": {
                "storage": {"source": "runtime_readiness", "condition": "healthy", "status": {"state": "supported"}},
                "peer_health": {"source": "cluster_peer_health", "condition": "healthy", "status": {"state": "supported"}},
                "listing": {
                    "source": "workload_admission+internode_metrics",
                    "condition": "healthy",
                    "status": {"state": "supported"},
                    "internode_stall_timeouts_total": 2,
                    "hint": "inspect metrics"
                },
                "usage": {
                    "source": "scanner_metrics",
                    "condition": "stale",
                    "status": {"state": "unknown"},
                    "dirty_pending_buckets": 3,
                    "last_usage_save_unix_secs": 123,
                    "last_usage_save_result": "skipped_stale"
                },
                "workload_admission": {"source": "workload_admission", "condition": "healthy", "status": {"state": "supported"}}
            },
            "topology": {}, "membership": {}, "pool_state": {}, "local_storage": {},
            "peer_health": {}, "rpc_boundary": {}, "observability": {},
            "workload_admission": [], "runtime_status": {}, "actionable_pressure": false
        }
    });

    let beta10: ClusterSnapshotDocument =
        serde_json::from_value(beta10).expect("beta.10 snapshot should deserialize");
    let current: ClusterSnapshotDocument =
        serde_json::from_value(current).expect("current snapshot should deserialize");
    let unavailable: ClusterSnapshotDocument =
        serde_json::from_value(serde_json::json!({"snapshot": null}))
            .expect("null snapshot should deserialize");

    let beta10 = beta10.snapshot.expect("beta.10 snapshot should exist");
    assert!(beta10.components.is_none());
    assert_eq!(beta10.summary.runtime.extra["future_status_field"], 9);
    assert_eq!(beta10.extra["future_snapshot_field"], 7);
    let current = current.snapshot.expect("current snapshot should exist");
    assert_eq!(
        current
            .components
            .expect("current components should exist")
            .usage
            .expect("usage component should exist")
            .condition,
        "stale"
    );
    assert!(unavailable.snapshot.is_none());
}

#[test]
fn cluster_snapshot_requires_the_snapshot_envelope_field() {
    assert!(serde_json::from_value::<ClusterSnapshotDocument>(serde_json::json!({})).is_err());
    let unavailable = serde_json::from_value::<ClusterSnapshotDocument>(serde_json::json!({
        "snapshot": null
    }))
    .expect("an explicit null snapshot must remain valid");
    assert!(unavailable.snapshot.is_none());
}

#[test]
fn extension_catalog_preserves_schema_and_runtime_future_fields() {
    let value = serde_json::json!({
        "extensions": [{
            "schema_version": "rustfs.extension-schema.v1",
            "extension_id": "builtin:ops-diagnostics",
            "display_name": "Operations Diagnostics",
            "provider": "rustfs",
            "version": "1",
            "kind": "ops_diagnostics",
            "runtime": {"api_version": "v1", "boundary": "builtin", "future_runtime": true},
            "capabilities": ["ops.diagnostics.v1"],
            "disabled_by_default": false,
            "future_schema": "kept"
        }],
        "runtime_capabilities": {"ops_diagnostics": {"runtime_capability_summary": {"state": "supported"}}},
        "cluster_snapshot": {"path": "/rustfs/admin/v4/cluster/snapshot"},
        "external_plugin_flow": {"enabled": false},
        "future_catalog": 11
    });

    let catalog: ExtensionsCatalog =
        serde_json::from_value(value).expect("catalog should deserialize");

    assert_eq!(catalog.extensions[0].capabilities, ["ops.diagnostics.v1"]);
    assert_eq!(catalog.extensions[0].extra["future_schema"], "kept");
    assert_eq!(catalog.extra["future_catalog"], 11);
}

#[test]
fn extension_catalog_requires_object_valued_contexts() {
    assert!(
        serde_json::from_value::<ExtensionsCatalog>(serde_json::json!({"extensions": []})).is_err()
    );

    for field in [
        "runtime_capabilities",
        "cluster_snapshot",
        "external_plugin_flow",
    ] {
        for invalid in [serde_json::Value::Null, serde_json::json!([])] {
            let mut fixture = serde_json::json!({
                "extensions": [],
                "runtime_capabilities": {},
                "cluster_snapshot": {},
                "external_plugin_flow": {}
            });
            fixture[field] = invalid;
            assert!(
                serde_json::from_value::<ExtensionsCatalog>(fixture).is_err(),
                "{field} must be a JSON object"
            );
        }
    }
}

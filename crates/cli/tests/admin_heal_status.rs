#![cfg(not(windows))]

mod admin_support;

use std::process::{Command, Output};
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};
use serde_json::Value;

fn run_status(response: &'static str, json: bool, token: bool) -> Output {
    let config_dir = tempfile::tempdir().expect("create isolated config");
    let (endpoint, receiver, handle) = start_admin_test_server(response);
    let mut command = Command::new(rc_binary());
    command.arg("--no-color");
    if json {
        command.arg("--json");
    }
    command.args(["admin", "heal", "status", "myalias"]);
    if token {
        command.args(["--client-token", "25080000-0000-4000-8000-000000000001"]);
    }
    let output = command
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .output()
        .expect("run heal status");
    assert_eq!(
        output.status.code(),
        Some(0),
        "status lookup must retain its exit contract: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("capture status request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        if token {
            "/rustfs/admin/v3/heal/?clientToken=25080000-0000-4000-8000-000000000001"
        } else {
            "/rustfs/admin/v3/background-heal/status"
        }
    );
    handle.join().expect("status server finished");
    output
}

fn status_outputs(response: &'static str, token: bool) -> (Value, String, String) {
    let json_output = run_status(response, true, token);
    assert!(json_output.stderr.is_empty());
    let json: Value = serde_json::from_slice(&json_output.stdout).expect("decode status output");
    let schema: Value = serde_json::from_str(include_str!("../../../schemas/output_v2.json"))
        .expect("decode output schema");
    let validator = jsonschema::validator_for(&serde_json::json!({
        "$ref": "#/definitions/healStatus",
        "definitions": schema["definitions"],
    }))
    .expect("compile heal status schema");
    assert!(validator.is_valid(&json), "status violates schema: {json}");
    let text_output = run_status(response, false, token);
    (
        json,
        String::from_utf8(text_output.stdout).expect("status text"),
        String::from_utf8(text_output.stderr).expect("status warnings"),
    )
}

#[test]
fn degraded_status_preserves_partial_coverage_in_json_and_text() {
    let (json, text, warning) = status_outputs(
        r#"{"state":"degraded","clusterStatusComplete":false,"coverage":{"expected":4,"responded":3,"unknown":1,"reasons":["peer_status_unavailable"]}}"#,
        false,
    );
    assert_eq!(json["state"], "degraded");
    assert_eq!(json["healing"], false);
    assert_eq!(json["clusterStatusComplete"], false);
    assert_eq!(json["coverage"]["expected"], 4);
    assert_eq!(json["coverage"]["responded"], 3);
    assert_eq!(json["coverage"]["unknown"], 1);
    assert_eq!(json["coverage"]["reasons"][0], "peer_status_unavailable");
    assert!(text.contains("Heal Status: Degraded"));
    assert!(text.contains("3/4 responded"));
    assert!(text.contains("1 unknown"));
    assert!(text.contains("peer_status_unavailable"));
    assert!(warning.contains("incomplete"));
    assert!(!text.contains("Idle"));
    assert!(!text.contains("No active heal operation"));
}

#[test]
fn explicit_unknown_and_future_states_never_become_idle() {
    for response in [
        r#"{"state":"unknown"}"#,
        r#"{"state":"future_state"}"#,
        r#"{"state":"future_state","currentScanMode":2,"bitrotStartTime":"2026-09-13T13:00:00Z"}"#,
    ] {
        let (json, text, warning) = status_outputs(response, false);
        assert_eq!(json["state"], "unknown");
        assert_eq!(json["healing"], false);
        assert!(text.contains("Heal Status: Unknown"));
        assert!(warning.contains("unknown"));
        assert!(!text.contains("Idle"));
        assert!(!text.contains("Finished"));
        assert!(!text.contains("No active heal operation"));
        assert!(json.get("clusterStatusComplete").is_none());
        assert!(json.get("coverage").is_none());
    }
}

#[test]
fn unknown_runtime_retains_observed_tasks_without_claiming_a_known_state() {
    let (json, text, warning) = status_outputs(
        r#"{"state":"future_state","healOperations":{"queueLength":2,"activeTasks":1}}"#,
        false,
    );
    assert_eq!(json["state"], "unknown");
    assert_eq!(json["healing"], true);
    assert_eq!(json["healQueueLength"], 2);
    assert_eq!(json["healActiveTasks"], 1);
    assert!(text.contains("Heal Status: Unknown"));
    assert!(text.contains("2 queued, 1 active"));
    assert!(warning.contains("unknown"));
    assert!(!text.contains("No active heal operation"));
}

#[test]
fn active_partial_status_keeps_activity_and_incomplete_warning() {
    let (json, text, warning) = status_outputs(
        r#"{"state":"active","clusterStatusComplete":false,"coverage":{"expected":4,"responded":3,"unknown":1,"reasons":["future_reason"]}}"#,
        false,
    );
    assert_eq!(json["state"], "active");
    assert_eq!(json["healing"], true);
    assert_eq!(json["clusterStatusComplete"], false);
    assert_eq!(json["coverage"]["reasons"][0], "future_reason");
    assert!(text.contains("Heal Status: In Progress"));
    assert!(text.contains("3/4 responded"));
    assert!(text.contains("future_reason"));
    assert!(warning.contains("incomplete"));
}

#[test]
fn complete_idle_and_runtime_unavailability_remain_distinct() {
    for (response, state, label) in [
        (
            r#"{"state":"idle","clusterStatusComplete":true,"coverage":{"expected":4,"responded":4,"unknown":0,"reasons":[]}}"#,
            "idle",
            "Idle",
        ),
        (
            r#"{"state":"disabled","clusterStatusComplete":true,"coverage":{"expected":4,"responded":4,"unknown":0,"reasons":[]}}"#,
            "disabled",
            "Disabled",
        ),
        (
            r#"{"state":"uninitialized","clusterStatusComplete":true,"coverage":{"expected":4,"responded":4,"unknown":0,"reasons":[]}}"#,
            "uninitialized",
            "Uninitialized",
        ),
    ] {
        let (json, text, warning) = status_outputs(response, false);
        assert_eq!(json["state"], state);
        assert_eq!(json["healing"], false);
        assert_eq!(json["clusterStatusComplete"], true);
        assert_eq!(json["coverage"]["unknown"], 0);
        assert!(text.contains(&format!("Heal Status: {label}")));
        assert!(text.contains("4/4 responded"));
        assert!(warning.is_empty());
        if state != "idle" {
            assert!(!text.contains("No active heal operation"));
        }
    }
}

#[test]
fn missing_coverage_counts_stay_unknown_and_legacy_state_still_works() {
    let (json, text, warning) = status_outputs(r#"{"state":"degraded","coverage":{}}"#, false);
    assert!(json.get("clusterStatusComplete").is_none());
    for count in ["expected", "responded", "unknown"] {
        assert!(json["coverage"].get(count).is_none());
    }
    assert!(text.contains("?/? responded"));
    assert!(warning.contains("incomplete"));

    for (response, healing, label) in [
        (r#"{}"#, false, "Idle"),
        (r#"{"healQueueLength":1}"#, true, "In Progress"),
        (r#"{"currentScanMode":2}"#, true, "In Progress"),
    ] {
        let (json, text, _) = status_outputs(response, false);
        assert!(json.get("state").is_none());
        assert!(json.get("clusterStatusComplete").is_none());
        assert!(json.get("coverage").is_none());
        assert_eq!(json["healing"], healing);
        assert!(text.contains(&format!("Heal Status: {label}")));
        assert!(text.contains("Cluster status: Unknown"));
    }
}

#[test]
fn coverage_reasons_preserve_json_but_cannot_inject_terminal_lines() {
    let (json, text, warning) = status_outputs(
        r#"{"state":"degraded","coverage":{"reasons":["future\nforged_line\u001b[2J"]}}"#,
        false,
    );
    assert_eq!(
        json["coverage"]["reasons"][0],
        "future\nforged_line\u{1b}[2J"
    );
    assert!(!text.contains('\u{1b}'));
    assert!(!text.lines().any(|line| line.starts_with("forged_line")));
    assert!(warning.contains("incomplete"));
}

#[test]
fn explicit_incompleteness_cannot_fall_back_to_legacy_idle() {
    for response in [
        r#"{"clusterStatusComplete":false}"#,
        r#"{"state":"idle","clusterStatusComplete":false}"#,
        r#"{"state":"idle","coverage":{"expected":4,"responded":3}}"#,
        r#"{"state":"idle","coverage":{"unknown":1}}"#,
        r#"{"state":"idle","coverage":{"reasons":["peer_status_unavailable"]}}"#,
    ] {
        let (_, text, warning) = status_outputs(response, false);
        assert!(text.contains("Heal Status: Unknown"));
        assert!(warning.contains("incomplete"));
        assert!(!text.contains("No active heal operation"));
    }
}

#[test]
fn token_status_preserves_legacy_summaries_and_terminal_error_detail() {
    for (response, summary, label, healing) in [
        (r#"{"summary":"running"}"#, "running", "In Progress", true),
        (r#"{"summary":"finished"}"#, "finished", "Finished", false),
        (r#"{"summary":"stopped"}"#, "stopped", "Stopped", false),
        (r#"{"summary":"notFound"}"#, "notFound", "Not Found", false),
        (
            r#"{"summary":"stopped","detail":"heal traversal completed with errors: 1 failed objects","progress":{"objectsScanned":1,"objectsFailed":1},"outcome":{"execution":{"state":"completed_with_errors"}}}"#,
            "stopped",
            "Stopped",
            false,
        ),
    ] {
        let (json, text, warning) = status_outputs(response, true);
        assert_eq!(json["summary"], summary);
        assert_eq!(json["healing"], healing);
        assert!(json.get("state").is_none());
        assert!(json.get("clusterStatusComplete").is_none());
        assert!(json.get("coverage").is_none());
        assert!(text.contains(&format!("Heal Status: {label}")));
        assert!(!text.contains("Cluster status:"));
        assert!(warning.is_empty());
        if response.contains("completed_with_errors") {
            assert_eq!(json["itemsFailed"], 1);
            assert!(
                json["detail"]
                    .as_str()
                    .unwrap()
                    .contains("completed with errors")
            );
            assert!(text.contains("completed with errors"));
        }
    }
}

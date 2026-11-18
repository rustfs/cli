mod admin_support;

use std::process::{Command, Output};

use admin_support::{
    rc_binary, rc_host_alias, start_admin_response_test_server, start_admin_test_server,
};
use serde_json::{Value, json};

fn add_group(endpoint: &str, options: &[&str]) -> Output {
    let config = tempfile::tempdir().expect("create config directory");
    Command::new(rc_binary())
        .args(["admin", "group", "add", "myalias", "team"])
        .args(options)
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_myalias", rc_host_alias(endpoint))
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .output()
        .expect("run group add")
}

#[test]
fn group_add_sends_initial_members_including_an_empty_list() {
    for (options, members, message) in [
        (
            vec!["--json"],
            json!([]),
            "Group 'team' created successfully",
        ),
        (
            vec!["--json", "--members", "alice,bob"],
            json!(["alice", "bob"]),
            "Group 'team' created successfully with members",
        ),
    ] {
        let (endpoint, requests, server) = start_admin_test_server("");
        let output = add_group(&endpoint, &options);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let request = requests.try_recv().expect("group add must send a request");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/rustfs/admin/v3/update-group-members");
        let body: Value = serde_json::from_slice(&request.body).expect("request JSON");
        assert_eq!(body["group"], "team");
        assert_eq!(body["members"], members);
        assert_eq!(body["isRemove"], false);
        let result: Value = serde_json::from_slice(&output.stdout).expect("success JSON");
        assert_eq!(
            result,
            json!({"success": true, "name": "team", "message": message})
        );
        server.join().expect("join test server");
    }
}

#[test]
fn group_add_without_members_reports_server_rejection() {
    let (endpoint, requests, server) =
        start_admin_response_test_server("403 Forbidden", "application/xml", String::new());
    let output = add_group(&endpoint, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Failed to create group"));
    requests.try_recv().expect("server received group add");
    server.join().expect("join test server");
}

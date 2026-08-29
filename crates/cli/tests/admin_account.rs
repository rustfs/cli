//! Exit-code scenarios for `rc admin account …` and the `rc admin user`
//! credential commands.
//!
//! Two per command, per the PR checklist in AGENTS.md. Several of these guard a
//! specific way an earlier revision lost data or truncated a secret, so they
//! assert on what reached the wire rather than only on the exit status.
#![cfg(not(windows))]

mod admin_support;

use std::fs;
use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_response_test_server, start_admin_sequence_test_server,
    start_admin_test_server,
};

/// An alias pointing at a port nothing is listening on.
///
/// Used by the tests that must prove a command fails *before* any request: if
/// one were sent, the command would report a network error instead of the
/// usage error being asserted.
const UNREACHABLE_ENDPOINT: &str = "http://127.0.0.1:1";

const ACCOUNT_INFO_BODY: &str = r#"{"access_key":"admin","identity_type":"root","is_admin":true,"status":"enabled","credentials_source":"env","mutable":{"password":false,"username":false},"mfa":{"enabled":false,"pending":false,"recovery_codes_remaining":0,"enrollment_available":true}}"#;

const RECOVERY_CODES_BODY: &str = r#"{"recovery_codes":["AAAA1111BBBB2222CCCC","DDDD3333EEEE4444FFFF"],"generated_at":"2026-01-01T00:00:00Z"}"#;

fn rc() -> Command {
    Command::new(rc_binary())
}

// ---------------------------------------------------------------------------
// account info
// ---------------------------------------------------------------------------

#[test]
fn account_info_succeeds_against_a_server_that_answers() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(ACCOUNT_INFO_BODY);

    let output = rc()
        .args(["--json", "admin", "account", "info", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/account/info");
    handle.join().expect("admin test server finished");
}

#[test]
fn account_info_reports_auth_error_when_the_server_refuses() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "403 Forbidden",
        "application/json",
        r#"{"Code":"AccessDenied","Message":"not allowed"}"#.to_string(),
    );

    let output = rc()
        .args(["--json", "admin", "account", "info", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(4), "expected AuthError");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

// ---------------------------------------------------------------------------
// account passwd
// ---------------------------------------------------------------------------

#[test]
fn account_passwd_rejects_two_sources_for_one_secret_before_connecting() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = rc()
        .args([
            "--json",
            "admin",
            "account",
            "passwd",
            "myalias",
            "--current-password-from-env",
            "RC_TEST_CURRENT",
            "--current-password-file",
            "/dev/null",
            "--new-password-from-env",
            "RC_TEST_NEW",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(UNREACHABLE_ENDPOINT))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2), "expected UsageError");
}

#[test]
fn account_passwd_sends_a_secret_longer_than_the_former_thirty_three_byte_bound() {
    // The regression: the password file went through the SSE-C reader, which
    // stopped at 33 bytes, so a 40-character secret key became a password made
    // of its first 33 bytes with nothing reporting it.
    let config_dir = tempfile::tempdir().expect("create config dir");
    let secret_dir = tempfile::tempdir().expect("create secret dir");
    let long_secret = "L".repeat(40);
    let current = secret_dir.path().join("current");
    let new = secret_dir.path().join("new");
    fs::write(&current, "old-password\n").expect("write current password");
    fs::write(&new, format!("{long_secret}\n")).expect("write new password");

    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"sessions_revoked":0}"#);

    let output = rc()
        .args([
            "--json",
            "admin",
            "account",
            "passwd",
            "myalias",
            "--current-password-file",
            current.to_str().expect("utf-8 path"),
            "--new-password-file",
            new.to_str().expect("utf-8 path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    let body = String::from_utf8_lossy(&request.body);
    assert!(
        body.contains(&long_secret),
        "the whole 40-byte secret must reach the server, got: {body}"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn account_passwd_json_succeeds_when_the_optional_identity_lookup_fails() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", r#"{"sessions_revoked":3}"#),
        (
            "403 Forbidden",
            r#"{"Code":"AccessDenied","Message":"identity lookup denied"}"#,
        ),
    ]);

    let output = rc()
        .args([
            "--json",
            "admin",
            "account",
            "passwd",
            "myalias",
            "--current-password-from-env",
            "RC_TEST_CURRENT",
            "--new-password-from-env",
            "RC_TEST_NEW",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .env("RC_TEST_CURRENT", "old-password")
        .env("RC_TEST_NEW", "new-password")
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("password change JSON output");
    assert_eq!(json["success"], true);
    assert_eq!(json["sessions_revoked"], 3);
    assert!(
        json.get("access_key").is_none(),
        "an optional identity lookup failure must omit the key, output: {json}"
    );

    let password_request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured password request");
    assert_eq!(password_request.method, "POST");
    assert_eq!(password_request.target, "/rustfs/admin/v3/account/password");
    let identity_request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured identity request");
    assert_eq!(identity_request.method, "GET");
    assert_eq!(identity_request.target, "/rustfs/admin/v3/account/info");
    handle.join().expect("admin test server finished");
}

// ---------------------------------------------------------------------------
// account mfa status / enroll
// ---------------------------------------------------------------------------

#[test]
fn mfa_status_succeeds_against_a_server_that_answers() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"enabled":false,"pending":false,"algorithm":"SHA1","digits":6,"period_seconds":30,"recovery_codes_remaining":0,"enrollment_available":true}"#,
    );

    let output = rc()
        .args(["--json", "admin", "account", "mfa", "status", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.target, "/rustfs/admin/v3/account/mfa");
    handle.join().expect("admin test server finished");
}

#[test]
fn mfa_enroll_reports_unsupported_when_the_server_has_no_such_route() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "501 Not Implemented",
        "application/json",
        r#"{"Code":"NotImplemented","Message":"at-rest protection is not configured"}"#.to_string(),
    );

    let output = rc()
        .args(["--json", "admin", "account", "mfa", "enroll", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(7), "expected UnsupportedFeature");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

// ---------------------------------------------------------------------------
// account mfa activate / recovery-codes
// ---------------------------------------------------------------------------

#[test]
fn mfa_activate_refuses_an_occupied_output_path_before_the_server_rotates() {
    // The lost-codes case. `write_recovery_codes` will not clobber, and by the
    // time it runs the server has already activated: the set it refuses to
    // write is the only copy there will ever be. So the path is checked first,
    // and no request may leave.
    let config_dir = tempfile::tempdir().expect("create config dir");
    let out_dir = tempfile::tempdir().expect("create output dir");
    let occupied = out_dir.path().join("codes.txt");
    fs::write(&occupied, "an earlier set\n").expect("occupy the output path");

    let output = rc()
        .args([
            "--json",
            "admin",
            "account",
            "mfa",
            "activate",
            "myalias",
            "--code",
            "123456",
            "--output-file",
            occupied.to_str().expect("utf-8 path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(UNREACHABLE_ENDPOINT))
        .output()
        .expect("run rc command");

    // Conflict, not a network error: nothing was sent to the unreachable host.
    assert_eq!(
        output.status.code(),
        Some(6),
        "expected Conflict, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&occupied).expect("read back"),
        "an earlier set\n",
        "the existing file must be untouched"
    );
}

#[test]
fn mfa_activate_writes_the_codes_to_a_fresh_owner_only_file() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let out_dir = tempfile::tempdir().expect("create output dir");
    let target = out_dir.path().join("codes.txt");
    let (endpoint, receiver, handle) = start_admin_test_server(RECOVERY_CODES_BODY);

    let output = rc()
        .args([
            "--json",
            "admin",
            "account",
            "mfa",
            "activate",
            "myalias",
            "--code",
            "123456",
            "--output-file",
            target.to_str().expect("utf-8 path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written = fs::read_to_string(&target).expect("read the codes back");
    assert!(written.contains("AAAA1111BBBB2222CCCC"), "got: {written}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "codes file must be owner-only");
    }
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

#[test]
fn recovery_codes_prints_the_set_when_the_file_cannot_be_written() {
    // The server has already invalidated the previous set, so the new one must
    // reach the operator even though the write failed. A mistyped directory
    // passes the up-front path check — nothing is there — and then fails at the
    // open, which is precisely the window the fallback exists for.
    let config_dir = tempfile::tempdir().expect("create config dir");
    let out_dir = tempfile::tempdir().expect("create output dir");
    let target = out_dir.path().join("no-such-directory").join("codes.txt");

    let (endpoint, receiver, handle) = start_admin_test_server(RECOVERY_CODES_BODY);

    let output = rc()
        .args([
            "admin",
            "account",
            "mfa",
            "recovery-codes",
            "myalias",
            "--code",
            "123456",
            "--output-file",
            target.to_str().expect("utf-8 path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_ne!(
        output.status.code(),
        Some(0),
        "a failed write must not report success"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("AAAA1111BBBB2222CCCC"),
        "the codes must be printed rather than lost, stdout: {stdout}"
    );
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

// ---------------------------------------------------------------------------
// account mfa disable
// ---------------------------------------------------------------------------

#[test]
fn mfa_disable_reports_conflicting_password_flags_before_consuming_a_code() {
    // Resolving the code may prompt, and a TOTP code is single-use inside a
    // 30-second window. The flag conflict has to surface first.
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = rc()
        .args([
            "--json",
            "admin",
            "account",
            "mfa",
            "disable",
            "myalias",
            "--code",
            "123456",
            "--password-from-env",
            "RC_TEST_PW",
            "--password-file",
            "/dev/null",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(UNREACHABLE_ENDPOINT))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2), "expected UsageError");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not both"), "stderr: {stderr}");
}

#[test]
fn mfa_disable_requires_a_password_source_without_a_terminal() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = rc()
        .args([
            "--json", "admin", "account", "mfa", "disable", "myalias", "--code", "123456",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(UNREACHABLE_ENDPOINT))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2), "expected UsageError");
}

// ---------------------------------------------------------------------------
// user passwd / user mfa
// ---------------------------------------------------------------------------

#[test]
fn user_passwd_requires_a_password_source_without_a_terminal() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = rc()
        .args(["--json", "admin", "user", "passwd", "myalias", "analyst"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(UNREACHABLE_ENDPOINT))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2), "expected UsageError");
}

#[test]
fn user_passwd_targets_the_named_identity() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let secret_dir = tempfile::tempdir().expect("create secret dir");
    let password = secret_dir.path().join("password");
    fs::write(&password, "a-new-password\n").expect("write password");

    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"sessions_revoked":2}"#);

    let output = rc()
        .args([
            "--json",
            "admin",
            "user",
            "passwd",
            "myalias",
            "analyst",
            "--password-file",
            password.to_str().expect("utf-8 path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "PUT");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/set-user-secret-key?accessKey=analyst"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn user_mfa_reset_requires_yes_in_json_mode() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = rc()
        .args([
            "--json", "admin", "user", "mfa", "reset", "myalias", "analyst",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(UNREACHABLE_ENDPOINT))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2), "expected UsageError");
}

#[test]
fn user_mfa_reset_sends_the_delete_when_confirmed() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server("");

    let output = rc()
        .args([
            "--json", "admin", "user", "mfa", "reset", "myalias", "analyst", "--yes",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "DELETE");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/user/mfa?accessKey=analyst"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn user_mfa_status_reports_auth_error_when_the_server_refuses() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "403 Forbidden",
        "application/json",
        r#"{"Code":"AccessDenied","Message":"not allowed"}"#.to_string(),
    );

    let output = rc()
        .args([
            "--json", "admin", "user", "mfa", "status", "myalias", "analyst",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(4), "expected AuthError");
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

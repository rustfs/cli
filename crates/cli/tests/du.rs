mod admin_support;

use std::process::{Command, Output};

use admin_support::{rc_binary, rc_host_alias, start_admin_binary_sequence_test_server};
use serde_json::Value;

fn run_du(args: &[&str], endpoint: &str) -> Output {
    let config = tempfile::tempdir().expect("temporary config");
    Command::new(rc_binary())
        .args(["--json", "du"])
        .args(args)
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_local", rc_host_alias(endpoint))
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .output()
        .expect("run rc du")
}

#[test]
fn missing_directory_needs_an_explicit_trailing_slash_to_report_zero() {
    let empty = b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
    for (target, expected_exit) in [("local/photos/missing", 5), ("local/photos/missing/", 0)] {
        let (endpoint, requests, server) = start_admin_binary_sequence_test_server(vec![(
            "200 OK",
            "application/xml",
            empty.to_vec(),
        )]);
        let output = run_du(&[target], &endpoint);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        server.join().expect("serve empty prefix");
        assert!(
            requests
                .into_iter()
                .next()
                .expect("lookup")
                .target
                .contains("prefix=missing%2F")
        );
        if expected_exit == 0 {
            let json: Value = serde_json::from_slice(&output.stdout).expect("usage JSON");
            assert_eq!(json["data"]["object_count"], 0);
        } else {
            let json: Value = serde_json::from_slice(&output.stderr).expect("error JSON");
            assert_eq!(json["error"]["type"], "not_found");
        }
    }
}

#[test]
fn scans_exclude_directory_markers_but_include_empty_files_and_nonempty_objects() {
    let objects = b"<ListBucketResult><Contents><Key>empty/</Key><Size>0</Size></Contents><Contents><Key>zero</Key><Size>0</Size></Contents><Contents><Key>payload/</Key><Size>9</Size></Contents></ListBucketResult>";
    let versions = b"<ListVersionsResult><Version><Key>empty/</Key><Size>0</Size><IsLatest>true</IsLatest></Version><Version><Key>zero</Key><Size>0</Size><IsLatest>true</IsLatest></Version><Version><Key>payload/</Key><Size>9</Size><IsLatest>true</IsLatest></Version><DeleteMarker><Key>deleted</Key><IsLatest>true</IsLatest></DeleteMarker></ListVersionsResult>";
    for (flags, response) in [
        (&[][..], &objects[..]),
        (&["--versions"][..], &versions[..]),
    ] {
        let (endpoint, _requests, server) = start_admin_binary_sequence_test_server(vec![(
            "200 OK",
            "application/xml",
            response.to_vec(),
        )]);
        let mut args = vec!["local/photos"];
        args.extend_from_slice(flags);
        let output = run_du(&args, &endpoint);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        server.join().expect("serve listing");
        let json: Value = serde_json::from_slice(&output.stdout).expect("usage JSON");
        let data = &json["data"];
        assert_eq!(data["object_count"], 2);
        assert_eq!(data["total_bytes"], 9);
        if !flags.is_empty() {
            assert_eq!(data["version_count"], 2);
            assert_eq!(data["delete_marker_count"], 1);
        }
    }
}

#[test]
fn default_du_lists_s3_objects_for_cluster_bucket_and_prefix() {
    let buckets = "<ListAllMyBucketsResult><Buckets><Bucket><Name>photos</Name></Bucket></Buckets></ListAllMyBucketsResult>";
    let objects = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>logs/a</Key><Size>17</Size></Contents></ListBucketResult>";
    for (target, scope) in [
        ("local", "cluster"),
        ("local/photos", "bucket"),
        ("local/photos/logs", "prefix"),
        ("local/photos/logs/", "prefix"),
    ] {
        let mut responses = Vec::new();
        if scope == "cluster" {
            responses.push(("200 OK", "application/xml", buckets.as_bytes().to_vec()));
        }
        if scope == "prefix" && !target.ends_with('/') {
            responses.push(("200 OK", "application/xml", objects.as_bytes().to_vec()));
        }
        responses.push(("200 OK", "application/xml", objects.as_bytes().to_vec()));
        let (endpoint, requests, server) = start_admin_binary_sequence_test_server(responses);
        let output = run_du(&[target], &endpoint);
        assert!(
            output.status.success(),
            "{target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        server.join().expect("serve S3 listings");
        let targets: Vec<_> = requests.into_iter().map(|request| request.target).collect();
        assert!(targets.iter().all(|path| !path.contains("/admin/")));
        let listing = targets.last().expect("object listing");
        assert!(
            listing.starts_with("/photos") && listing.contains("list-type=2"),
            "{listing}"
        );
        if scope == "prefix" {
            assert!(
                listing
                    .split(['?', '&'])
                    .any(|param| param == "prefix=logs%2F"),
                "{listing}"
            );
        }
        let json: Value = serde_json::from_slice(&output.stdout).expect("usage JSON");
        let data = &json["data"];
        assert_eq!(data["scope"], scope);
        assert_eq!(data["source"], "client_scan");
        assert_eq!(data["total_bytes"], 17);
        assert_eq!(data["object_count"], 1);
        assert!(data["snapshot_at"].is_null());
    }
}

#[test]
fn du_versions_and_incomplete_uploads_use_the_same_directory_prefix() {
    let versions = b"<ListVersionsResult><IsTruncated>false</IsTruncated></ListVersionsResult>";
    let uploads = b"<ListMultipartUploadsResult><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>";
    let directory =
        b"<ListBucketResult><Contents><Key>logs/</Key><Size>0</Size></Contents></ListBucketResult>";
    let (endpoint, requests, server) = start_admin_binary_sequence_test_server(vec![
        ("200 OK", "application/xml", directory.to_vec()),
        ("200 OK", "application/xml", versions.to_vec()),
        ("200 OK", "application/xml", uploads.to_vec()),
    ]);
    let output = run_du(
        &[
            "--fallback",
            "--versions",
            "--incomplete",
            "local/photos/logs",
        ],
        &endpoint,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().expect("serve version and upload listings");
    let mut requests = requests.into_iter();
    for operation in ["max-keys=1", "versions", "uploads"] {
        let target = requests.next().expect("listing request").target;
        assert!(target.contains(operation), "{target}");
        assert!(
            target
                .split(['?', '&'])
                .any(|param| param == "prefix=logs%2F"),
            "{target}"
        );
    }
}

#[test]
fn snapshot_only_rejects_dimensions_that_require_listing() {
    for args in [
        vec!["--snapshot", "local/photos/2026/"],
        vec!["--snapshot", "--incomplete", "local/photos"],
    ] {
        let output = run_du(&args, "http://127.0.0.1:1");
        assert_eq!(output.status.code(), Some(7));
        let json: Value = serde_json::from_slice(&output.stderr).expect("usage error JSON");
        assert_eq!(json["error"]["capability"], "admin.data-usage");
    }
}

#[test]
fn admin_denial_only_triggers_listing_with_explicit_fallback() {
    for options in [
        vec!["--snapshot"],
        vec!["--fallback"],
        vec!["--snapshot", "--fallback"],
    ] {
        let fallback = options.contains(&"--fallback");
        let mut responses = vec![(
            "403 Forbidden",
            "application/json",
            br#"{"Code":"AccessDenied","Message":"Access Denied"}"#.to_vec(),
        )];
        if fallback {
            responses.push((
                "200 OK",
                "application/xml",
                b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".to_vec(),
            ));
        }
        let (endpoint, requests, server) = start_admin_binary_sequence_test_server(responses);
        let mut args = options;
        args.push("local/photos");
        let output = run_du(&args, &endpoint);
        assert_eq!(
            output.status.code(),
            Some(if fallback { 0 } else { 4 }),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        server.join().expect("serve responses");
        let mut requests = requests.into_iter();
        assert_eq!(
            requests.next().expect("capability discovery").target,
            "/rustfs/admin/v3/info"
        );
        if fallback {
            assert!(
                requests
                    .next()
                    .expect("object listing")
                    .target
                    .contains("list-type=2")
            );
            let json: Value = serde_json::from_slice(&output.stdout).expect("usage JSON");
            assert_eq!(json["data"]["source"], "client_scan");
        }
    }
}

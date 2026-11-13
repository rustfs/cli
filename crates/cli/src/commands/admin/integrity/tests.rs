use std::path::Path;

use super::*;
use crate::commands::admin::recovery::test_support::{self as support, *};

fn target() -> JobTarget {
    JobTarget {
        alias: "local".into(),
        bucket: "bucket".into(),
        job_id: ID.into(),
    }
}

fn create_args(root: &Path, mode: &str) -> CreateArgs {
    let manifest = root.join("manifest.json");
    let inventory_file = root.join("inventory.json");
    std::fs::write(&manifest, serde_json::to_vec(&request(mode).items).unwrap()).unwrap();
    std::fs::write(
        &inventory_file,
        serde_json::to_vec(&InventorySelection {
            bucket: "bucket".into(),
            inventory: support::inventory(),
        })
        .unwrap(),
    )
    .unwrap();
    CreateArgs {
        alias: "local".into(),
        bucket: "bucket".into(),
        manifest,
        inventory_file,
        receipt: root.join("receipt.json"),
        bytes_per_second: 65536,
        max_object_bytes: 1024,
        digest_source_reference: Some("backup/original-3177".into()),
    }
}

fn migrate_args(create: CreateArgs) -> MigrateArgs {
    MigrateArgs {
        create,
        deployment_id: "deployment-a".into(),
        qualification_reference: "qualification-3177".into(),
        acknowledge_qualified_deployment: true,
    }
}

fn receipt(mode: &str) -> Receipt {
    Receipt {
        format_version: 1,
        alias: "local".into(),
        endpoint: ENDPOINT.into(),
        job: support::job(mode, "paused", "pending"),
        inventory_observed_at: "2026-11-13T00:00:00Z".into(),
        inventory_bucket_incarnation: INCARNATION.into(),
        digest_source_reference: Some("backup/original-3177".into()),
        qualification: (mode == "migrate").then(|| Qualification {
            deployment_id: "deployment-a".into(),
            reference: "qualification-3177".into(),
            operator_attested: true,
        }),
        readiness: None,
        estimate: WorkEstimate {
            object_count: 1,
            inventory_selected_bytes: Some(128),
            per_stream_bytes_per_second: 65536,
            stream_passes: if mode == "migrate" { 3 } else { 1 },
            minimum_stream_seconds: None,
            peak_payload_staging_budget: 1024,
            note: "test".into(),
        },
        query_command: vec![],
        resume_command: vec![],
    }
}

#[tokio::test]
async fn readiness_and_inventory_preserve_success_partial_and_auth_exit_codes() {
    let readiness_command = IntegrityCommands::Readiness(AliasArgs {
        alias: "local".into(),
    });
    for reply in [
        Ok(serde_json::json!({"new_writes_enabled_here":false,"fleet_capability_verified":false})),
        Err(Error::Auth("denied".into())),
    ] {
        let expected = if reply.is_ok() {
            ExitCode::Success
        } else {
            ExitCode::AuthError
        };
        let api = MockApi::default();
        api.readiness.lock().unwrap().push_back(reply);
        assert_eq!(
            execute_with_api(&readiness_command, ENDPOINT, &api, &formatter()).await,
            expected
        );
    }
    let command = IntegrityCommands::Inventory(InventoryArgs {
        alias: "local".into(),
        bucket: "bucket".into(),
        prefix: String::new(),
        key_marker: None,
        version_marker: None,
        limit: 100,
    });
    for partial in [false, true] {
        let api = MockApi::default();
        let mut page = support::inventory();
        page.is_truncated = partial;
        page.next_key_marker = partial.then(|| "object".into());
        page.next_version_marker = partial.then(|| "null".into());
        api.inventories.lock().unwrap().push_back(Ok(page));
        assert_eq!(
            execute_with_api(&command, ENDPOINT, &api, &formatter()).await,
            if partial {
                ExitCode::GeneralError
            } else {
                ExitCode::Success
            }
        );
        assert_eq!(
            *api.calls.lock().unwrap(),
            vec![Call::Inventory("bucket".into())],
            "inventory never follows pages automatically"
        );
    }
}

#[tokio::test]
async fn audit_creation_saves_identity_and_null_version_without_resuming() {
    let root = tempfile::tempdir().unwrap();
    let args = create_args(root.path(), "audit");
    let receipt_path = args.receipt.clone();
    let api = MockApi::default();
    api.creates
        .lock()
        .unwrap()
        .push_back(Ok(support::job("audit", "paused", "pending")));
    assert_eq!(
        execute_with_api(
            &IntegrityCommands::Audit(args),
            ENDPOINT,
            &api,
            &formatter()
        )
        .await,
        ExitCode::Success
    );
    let saved: Receipt = recovery::read_json(&receipt_path, 1024 * 1024).unwrap();
    assert_eq!(saved.job.id, ID);
    assert_eq!(
        saved.job.request.items[0].version_id.as_deref(),
        Some("null")
    );
    assert_eq!(saved.endpoint, ENDPOINT);
    assert_eq!(
        saved.digest_source_reference.as_deref(),
        Some("backup/original-3177")
    );
    assert_eq!(saved.estimate.stream_passes, 1);
    assert_eq!(saved.estimate.minimum_stream_seconds, Some(0.001953125));
    assert_eq!(saved.estimate.peak_payload_staging_budget, 1024);
    assert_eq!(saved.resume_command[3], "resume");
    assert_eq!(
        *api.calls.lock().unwrap(),
        vec![Call::Create("bucket".into(), request("audit"))]
    );
}

#[tokio::test]
async fn audit_creation_unknown_ack_and_missing_provenance_do_not_retry() {
    let root = tempfile::tempdir().unwrap();
    let args = create_args(root.path(), "audit");
    let receipt_path = args.receipt.clone();
    let api = MockApi::default();
    api.creates
        .lock()
        .unwrap()
        .push_back(Err(Error::Network("ack lost".into())));
    assert_eq!(
        execute_with_api(
            &IntegrityCommands::Audit(args),
            ENDPOINT,
            &api,
            &formatter()
        )
        .await,
        ExitCode::GeneralError
    );
    assert!(!receipt_path.exists());
    assert_eq!(api.calls.lock().unwrap().len(), 1);
    let mut args = create_args(root.path(), "audit");
    args.digest_source_reference = None;
    let api = MockApi::default();
    assert_eq!(
        execute_with_api(
            &IntegrityCommands::Audit(args),
            ENDPOINT,
            &api,
            &formatter()
        )
        .await,
        ExitCode::UsageError
    );
    assert!(api.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn audit_creation_preserves_definitive_rejection_exit_codes() {
    for (error, expected) in [
        (Error::Auth("denied".into()), ExitCode::AuthError),
        (Error::Conflict("changed".into()), ExitCode::Conflict),
        (
            Error::UnsupportedFeature("old node".into()),
            ExitCode::UnsupportedFeature,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let args = create_args(root.path(), "audit");
        let path = args.receipt.clone();
        let api = MockApi::default();
        api.creates.lock().unwrap().push_back(Err(error));
        assert_eq!(
            execute_with_api(
                &IntegrityCommands::Audit(args),
                ENDPOINT,
                &api,
                &formatter()
            )
            .await,
            expected
        );
        assert!(!path.exists());
        assert_eq!(api.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn migrate_creation_requires_activation_and_keeps_qualification_in_local_receipt() {
    for activated in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let args = create_args(root.path(), "migrate");
        let receipt_path = args.receipt.clone();
        let api = MockApi::default();
        api.readiness.lock().unwrap().push_back(Ok(serde_json::json!({"local_reader_supported":true,"new_writes_enabled_here":activated,"fleet_capability_verified":false})));
        if activated {
            api.creates
                .lock()
                .unwrap()
                .push_back(Ok(support::job("migrate", "paused", "pending")));
        }
        assert_eq!(
            execute_with_api(
                &IntegrityCommands::Migrate(migrate_args(args)),
                ENDPOINT,
                &api,
                &formatter()
            )
            .await,
            if activated {
                ExitCode::Success
            } else {
                ExitCode::UnsupportedFeature
            }
        );
        assert_eq!(
            api.calls.lock().unwrap().len(),
            if activated { 2 } else { 1 }
        );
        assert_eq!(receipt_path.exists(), activated);
        if activated {
            let saved: Receipt = recovery::read_json(&receipt_path, 1024 * 1024).unwrap();
            assert_eq!(saved.qualification.unwrap().reference, "qualification-3177");
            assert_eq!(saved.estimate.stream_passes, 3);
            assert_eq!(saved.estimate.minimum_stream_seconds, Some(0.005859375));
            assert_eq!(saved.estimate.peak_payload_staging_budget, 1024);
            assert_eq!(
                saved.job.request,
                request("migrate"),
                "local attestation must not change the server manifest"
            );
        }
    }
}

#[tokio::test]
async fn receipt_collision_before_and_after_create_never_overwrites_or_resumes() {
    for collide_during_create in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let args = create_args(root.path(), "audit");
        let path = args.receipt.clone();
        let api = MockApi {
            create_receipt_collision: collide_during_create.then(|| path.clone()),
            ..Default::default()
        };
        if collide_during_create {
            api.creates
                .lock()
                .unwrap()
                .push_back(Ok(support::job("audit", "paused", "pending")));
        } else {
            std::fs::write(&path, b"existing receipt").unwrap();
        }
        assert_ne!(
            execute_with_api(
                &IntegrityCommands::Audit(args),
                ENDPOINT,
                &api,
                &formatter()
            )
            .await,
            ExitCode::Success
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if collide_during_create {
                b"another operation owns this receipt".as_slice()
            } else {
                b"existing receipt"
            }
        );
        assert_eq!(
            api.calls.lock().unwrap().len(),
            usize::from(collide_during_create)
        );
    }
}

#[tokio::test]
async fn job_completion_requires_every_item_and_a_known_evidence_source() {
    for (state, item_state, evidence, expected) in [
        (
            "complete",
            "verified",
            Some("administrator_supplied_sha256"),
            ExitCode::Success,
        ),
        (
            "complete",
            "verified",
            Some("stored_sha256"),
            ExitCode::Success,
        ),
        (
            "complete",
            "mismatch",
            Some("administrator_supplied_sha256"),
            ExitCode::GeneralError,
        ),
        ("complete", "verified", None, ExitCode::GeneralError),
        (
            "running",
            "verified",
            Some("administrator_supplied_sha256"),
            ExitCode::GeneralError,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("result.json");
        let mut job = support::job("audit", state, item_state);
        job.results[0].evidence_source = evidence.map(str::to_owned);
        let api = MockApi::default();
        api.jobs.lock().unwrap().push_back(Ok(job));
        assert_eq!(
            execute_with_api(
                &IntegrityCommands::Job(JobArgs {
                    target: target(),
                    output: Some(output.clone())
                }),
                ENDPOINT,
                &api,
                &formatter()
            )
            .await,
            expected
        );
        let saved: Value = recovery::read_json(&output, 1024 * 1024).unwrap();
        assert!(saved["checked_at"].is_null());
        assert!(saved["retrieved_at"].is_string());
        assert_eq!(saved["source_responsibility_retained"], true);
        assert_eq!(
            saved["job"]["results"][0]["evidence_source"],
            serde_json::to_value(evidence).unwrap()
        );
    }
}

#[tokio::test]
async fn resume_requires_matching_receipt_and_rechecks_migration_activation() {
    for mismatch in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("receipt.json");
        let mut saved = receipt("audit");
        // Older estimates remain advisory; request identity controls resume eligibility.
        saved.estimate.peak_payload_staging_budget = 0;
        if mismatch {
            saved.endpoint = "http://different:9000/".into();
        }
        recovery::save_receipt(recovery::prepare_receipt(&path).unwrap(), &path, &saved).unwrap();
        let api = MockApi::default();
        if !mismatch {
            api.jobs.lock().unwrap().push_back(Ok(saved.job.clone()));
            api.controls
                .lock()
                .unwrap()
                .push_back(Ok(support::job("audit", "running", "pending")));
        }
        assert_eq!(
            execute_with_api(
                &IntegrityCommands::Resume(ResumeArgs {
                    target: target(),
                    receipt: path
                }),
                ENDPOINT,
                &api,
                &formatter()
            )
            .await,
            if mismatch {
                ExitCode::Conflict
            } else {
                ExitCode::Success
            }
        );
        assert_eq!(
            api.calls.lock().unwrap().len(),
            if mismatch { 0 } else { 2 }
        );
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("receipt.json");
    let saved = receipt("migrate");
    recovery::save_receipt(recovery::prepare_receipt(&path).unwrap(), &path, &saved).unwrap();
    let api = MockApi::default();
    api.jobs.lock().unwrap().push_back(Ok(saved.job));
    api.readiness.lock().unwrap().push_back(Ok(
        serde_json::json!({"local_reader_supported":true,"new_writes_enabled_here":false}),
    ));
    assert_eq!(
        execute_with_api(
            &IntegrityCommands::Resume(ResumeArgs {
                target: target(),
                receipt: path
            }),
            ENDPOINT,
            &api,
            &formatter()
        )
        .await,
        ExitCode::UnsupportedFeature
    );
    assert_eq!(
        api.calls.lock().unwrap().len(),
        2,
        "disabled migration never reaches control"
    );
}

#[tokio::test]
async fn pause_and_cancel_distinguish_pending_terminal_and_unknown_ack() {
    for operation in ["pause", "cancel"] {
        for (state, expected) in [
            (
                if operation == "pause" {
                    "paused"
                } else {
                    "cancelled"
                },
                ExitCode::Success,
            ),
            (
                if operation == "pause" {
                    "pause_requested"
                } else {
                    "cancel_requested"
                },
                ExitCode::GeneralError,
            ),
        ] {
            let api = MockApi::default();
            api.controls
                .lock()
                .unwrap()
                .push_back(Ok(support::job("migrate", state, "migrated")));
            let command = if operation == "pause" {
                IntegrityCommands::Pause(target())
            } else {
                IntegrityCommands::Cancel(target())
            };
            assert_eq!(
                execute_with_api(&command, ENDPOINT, &api, &formatter()).await,
                expected
            );
            assert_eq!(
                *api.calls.lock().unwrap(),
                vec![Call::Control("bucket".into(), ID.into(), operation.into())]
            );
        }
        let api = MockApi::default();
        api.controls
            .lock()
            .unwrap()
            .push_back(Err(Error::Network("ack lost".into())));
        let command = if operation == "pause" {
            IntegrityCommands::Pause(target())
        } else {
            IntegrityCommands::Cancel(target())
        };
        assert_eq!(
            execute_with_api(&command, ENDPOINT, &api, &formatter()).await,
            ExitCode::GeneralError
        );
        assert_eq!(
            api.calls.lock().unwrap().len(),
            1,
            "ambiguous controls must not retry or delete targets"
        );
    }
}

#[tokio::test]
async fn resume_does_not_upgrade_an_existing_stored_digest_migration_to_this_product_workflow() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("receipt.json");
    let mut saved = receipt("migrate");
    saved.job.request.items[0].expected_sha256 = None;
    recovery::save_receipt(recovery::prepare_receipt(&path).unwrap(), &path, &saved).unwrap();
    let api = MockApi::default();
    api.jobs.lock().unwrap().push_back(Ok(saved.job));
    assert_eq!(
        execute_with_api(
            &IntegrityCommands::Resume(ResumeArgs {
                target: target(),
                receipt: path,
            }),
            ENDPOINT,
            &api,
            &formatter(),
        )
        .await,
        ExitCode::UsageError,
    );
    assert_eq!(
        api.calls.lock().unwrap().len(),
        1,
        "invalid selection must not reach readiness or control"
    );
}

#[test]
fn selection_and_receipt_preserve_current_vs_null_and_strip_endpoint_credentials() {
    let root = tempfile::tempdir().unwrap();
    let args = create_args(root.path(), "audit");
    let (selected, _, estimate) = selection(&args, "audit").unwrap();
    assert_eq!(selected.items[0].version_id.as_deref(), Some("null"));
    assert_eq!(estimate.inventory_selected_bytes, Some(128));
    let mut items = selected.items;
    items[0].version_id = None;
    std::fs::write(&args.manifest, serde_json::to_vec(&items).unwrap()).unwrap();
    let (selected, _, estimate) = selection(&args, "audit").unwrap();
    assert!(selected.items[0].version_id.is_none());
    assert!(estimate.inventory_selected_bytes.is_none());
    assert!(estimate.minimum_stream_seconds.is_none());
    assert_eq!(
        safe_endpoint("https://user:secret@example.test:9000/path?token=private#fragment").unwrap(),
        "https://example.test:9000/path"
    );
}

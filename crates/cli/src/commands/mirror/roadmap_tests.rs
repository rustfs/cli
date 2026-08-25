use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser;
use jiff::Timestamp;
use rc_core::{Error, RemotePath, TransferControls, TransferOutcomeState, TransferSelection};

use super::*;

fn snapshot(size: u64, modified: &str, etag: Option<&str>) -> MirrorSnapshot {
    snapshot_with_identity(size, modified, etag, None)
}

fn snapshot_with_identity(
    size: u64,
    modified: &str,
    etag: Option<&str>,
    identity_etag: Option<&str>,
) -> MirrorSnapshot {
    MirrorSnapshot {
        size_bytes: Some(size),
        modified: Some(modified.parse::<Timestamp>().expect("valid test timestamp")),
        etag: etag.map(ToOwned::to_owned),
        identity_etag: identity_etag.map(ToOwned::to_owned),
    }
}

fn remote_entry(alias: &str, key: &str, relative: &str, etag: &str) -> MirrorEntry {
    MirrorEntry {
        relative_path: relative.to_string(),
        location: MirrorLocation::Remote(RemotePath::new(alias, "bucket", key)),
        snapshot: snapshot(4, "2026-07-21T04:00:00Z", Some(etag)),
    }
}

fn local_entry(root: &str, relative: &str) -> MirrorEntry {
    MirrorEntry {
        relative_path: relative.to_string(),
        location: MirrorLocation::Local(PathBuf::from(root).join(relative)),
        snapshot: snapshot(4, "2026-07-21T04:00:00Z", None),
    }
}

fn manifest(entries: impl IntoIterator<Item = MirrorEntry>) -> MirrorManifest {
    let entries = entries
        .into_iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    MirrorManifest {
        entries,
        ..MirrorManifest::default()
    }
}

#[test]
fn relative_paths_are_normalized_and_traversal_is_rejected() {
    assert_eq!(
        normalize_relative_path("nested//./report.txt").expect("normal relative path"),
        "nested/report.txt"
    );
    for value in [
        "../secret",
        "nested/../../secret",
        "/absolute",
        "C:/windows",
        "nested/bad?.txt",
        "nested/control\u{0007}.txt",
    ] {
        assert!(normalize_relative_path(value).is_err(), "accepted {value}");
    }
}

#[test]
fn planners_map_all_supported_directions_without_changing_relative_paths() {
    let cases = [
        (
            manifest([local_entry("/source", "nested/report.txt")]),
            MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
            MirrorLocation::Remote(RemotePath::new("dst", "bucket", "backup/nested/report.txt")),
        ),
        (
            manifest([remote_entry(
                "src",
                "source/nested/report.txt",
                "nested/report.txt",
                "etag-1",
            )]),
            MirrorEndpointSpec::Local(PathBuf::from("/target")),
            MirrorLocation::Local(PathBuf::from("/target").join("nested").join("report.txt")),
        ),
        (
            manifest([remote_entry(
                "src",
                "source/nested/report.txt",
                "nested/report.txt",
                "etag-1",
            )]),
            MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
            MirrorLocation::Remote(RemotePath::new("dst", "bucket", "backup/nested/report.txt")),
        ),
    ];

    for (source, target, expected_target) in cases {
        let plan = build_copy_plan(
            &source,
            &MirrorManifest::default(),
            &target,
            &TransferSelection::default(),
            false,
            CompareMode::Auto,
        )
        .expect("build copy plan");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative_path, "nested/report.txt");
        assert_eq!(plan.items[0].payload.target, expected_target);
    }
}

#[test]
fn equivalent_destination_is_restart_safe_and_not_planned_again() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "etag-1",
    )]);
    let target = manifest([remote_entry(
        "dst",
        "backup/report.txt",
        "report.txt",
        "etag-1",
    )]);

    let plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build restart plan");

    assert!(plan.items.is_empty());
    assert_eq!(plan.summary.skipped, 1);
}

#[test]
fn changed_destination_requires_overwrite() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "etag-source",
    )]);
    let target = manifest([remote_entry(
        "dst",
        "backup/report.txt",
        "report.txt",
        "etag-target",
    )]);
    let destination = MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup"));

    let skipped = build_copy_plan(
        &source,
        &target,
        &destination,
        &TransferSelection::default(),
        false,
        CompareMode::Auto,
    )
    .expect("build non-overwrite plan");
    let overwritten = build_copy_plan(
        &source,
        &target,
        &destination,
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build overwrite plan");

    assert!(skipped.items.is_empty());
    assert_eq!(skipped.summary.skipped, 1);
    assert_eq!(overwritten.items.len(), 1);
}

#[test]
fn removals_never_cross_a_source_symlink_boundary() {
    let mut source = MirrorManifest::default();
    source.protected_paths.push("linked".to_string());
    let target = manifest([
        remote_entry("dst", "backup/linked/a.txt", "linked/a.txt", "a"),
        remote_entry("dst", "backup/stale.txt", "stale.txt", "b"),
    ]);

    let plan = build_remove_plan(&source, &target, &TransferSelection::default())
        .expect("build safe removal plan");

    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].relative_path, "stale.txt");
    assert_eq!(plan.summary.skipped, 1);
}

#[cfg(unix)]
#[test]
fn local_enumeration_skips_symlinks_without_following_them() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("create local mirror root");
    let outside = tempfile::tempdir().expect("create outside directory");
    std::fs::create_dir_all(root.path().join("nested")).expect("create nested directory");
    std::fs::write(root.path().join("nested/file.txt"), b"data").expect("write local file");
    std::fs::write(outside.path().join("secret.txt"), b"secret").expect("write outside file");
    symlink(outside.path(), root.path().join("linked")).expect("create directory symlink");

    let manifest = enumerate_local_manifest(root.path(), MissingRootPolicy::Error)
        .expect("enumerate local root");

    assert_eq!(manifest.entries.len(), 1);
    assert!(manifest.entries.contains_key("nested/file.txt"));
    assert_eq!(manifest.protected_paths, ["linked"]);
    assert_eq!(manifest.ignored, 1);
}

#[derive(Default)]
struct MockMirrorIo {
    copy_calls: Mutex<Vec<String>>,
    remove_calls: Mutex<Vec<String>>,
    copy_failures: HashSet<String>,
}

#[async_trait::async_trait]
impl MirrorIo for MockMirrorIo {
    async fn copy(&self, operation: MirrorCopyOperation) -> rc_core::Result<u64> {
        self.copy_calls
            .lock()
            .expect("copy calls lock")
            .push(operation.relative_path.clone());
        if self.copy_failures.contains(&operation.relative_path) {
            Err(Error::Conflict(format!(
                "Source changed during mirror: {}",
                operation.relative_path
            )))
        } else {
            Ok(operation.source.snapshot.size_bytes.unwrap_or_default())
        }
    }

    async fn remove(&self, operation: MirrorRemoveOperation) -> rc_core::Result<u64> {
        self.remove_calls
            .lock()
            .expect("remove calls lock")
            .push(operation.relative_path);
        Ok(0)
    }
}

#[tokio::test]
async fn copy_failure_preserves_partial_results_and_blocks_every_removal() {
    let source = manifest([
        remote_entry("src", "source/a.txt", "a.txt", "a"),
        remote_entry("src", "source/b.txt", "b.txt", "b"),
    ]);
    let target = manifest([remote_entry(
        "dst",
        "backup/stale.txt",
        "stale.txt",
        "stale",
    )]);
    let copy_plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build copy plan");
    let remove_plan =
        build_remove_plan(&source, &target, &TransferSelection::default()).expect("remove plan");
    let io = Arc::new(MockMirrorIo {
        copy_failures: HashSet::from(["b.txt".to_string()]),
        ..MockMirrorIo::default()
    });
    let controls = TransferControls {
        concurrency: 2,
        continue_on_error: true,
        ..TransferControls::default()
    };

    let report = execute_operation_plans(Arc::clone(&io), copy_plan, remove_plan, controls, true)
        .await
        .expect("valid executor controls");

    assert_eq!(report.copy.summary.successful, 1);
    assert_eq!(report.copy.summary.failed, 1);
    assert!(report.remove.is_none());
    assert!(
        io.remove_calls
            .lock()
            .expect("remove calls lock")
            .is_empty()
    );
    assert!(matches!(
        report.copy.outcomes[1].state,
        TransferOutcomeState::Failed { .. }
    ));
}

#[tokio::test]
async fn a_removal_retry_treats_an_already_absent_target_as_complete() {
    let root = tempfile::tempdir().expect("create local target root");
    let target = MirrorEntry {
        relative_path: "stale.txt".to_string(),
        location: MirrorLocation::Local(root.path().join("stale.txt")),
        snapshot: snapshot(4, "2026-07-21T04:00:00Z", None),
    };
    let io = LiveMirrorIo {
        source: RuntimeEndpoint::Local {
            root: root.path().to_path_buf(),
        },
        target: RuntimeEndpoint::Local {
            root: root.path().to_path_buf(),
        },
        compare: CompareMode::Auto,
    };

    let removed = io
        .remove(MirrorRemoveOperation {
            relative_path: "stale.txt".to_string(),
            target,
        })
        .await
        .expect("already absent removal is idempotent");

    assert_eq!(removed, 0);
}

#[test]
fn changed_local_source_is_rejected_before_mutation() {
    let root = tempfile::tempdir().expect("create source root");
    let path = root.path().join("file.txt");
    std::fs::write(&path, b"old").expect("write initial source");
    let entry = enumerate_local_manifest(root.path(), MissingRootPolicy::Error)
        .expect("enumerate source")
        .entries
        .remove("file.txt")
        .expect("source entry");
    std::fs::write(&path, b"changed-size").expect("change source");

    let result = validate_local_entry(&entry);

    assert!(matches!(result, Err(Error::Conflict(_))));
}

#[tokio::test]
async fn failed_staged_download_is_cleaned_without_replacing_destination() {
    let root = tempfile::tempdir().expect("create target root");
    let destination = root.path().join("file.txt");
    let staging_path = root.path().join(".file.txt.rc-mirror-stage");
    std::fs::write(&destination, b"old").expect("write original destination");
    std::fs::write(&staging_path, b"partial").expect("write staged download");

    let result = finish_staged_download(
        tempfile::TempPath::try_from_path(staging_path.clone())
            .expect("take ownership of staging path"),
        &destination,
        Err(Error::Conflict("Source changed during mirror".to_string())),
        None,
        true,
    )
    .await;

    assert!(result.is_err());
    assert!(!staging_path.exists());
    assert_eq!(
        std::fs::read(&destination).expect("read original destination"),
        b"old"
    );
}

#[test]
fn empty_manifests_produce_empty_deterministic_plans() {
    let copy = build_copy_plan(
        &MirrorManifest::default(),
        &MirrorManifest::default(),
        &MirrorEndpointSpec::Local(PathBuf::from("/target")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("empty copy plan");
    let remove = build_remove_plan(
        &MirrorManifest::default(),
        &MirrorManifest::default(),
        &TransferSelection::default(),
    )
    .expect("empty remove plan");

    assert!(copy.items.is_empty());
    assert!(remove.items.is_empty());
    assert_eq!(copy.summary.planned, 0);
    assert_eq!(remove.summary.planned, 0);
}

#[test]
fn filtered_nested_tree_is_stably_sorted() {
    let source = manifest([
        local_entry("/source", "z/last.txt"),
        local_entry("/source", "a/first.txt"),
        local_entry("/source", "a/private.txt"),
    ]);
    let selection = TransferSelection::new(
        &["**/*.txt".to_string()],
        &["**/private.txt".to_string()],
        None,
        None,
        None,
    )
    .expect("valid filters");

    let plan = build_copy_plan(
        &source,
        &MirrorManifest::default(),
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "root")),
        &selection,
        false,
        CompareMode::Auto,
    )
    .expect("filtered plan");

    assert_eq!(
        plan.items
            .iter()
            .map(|item| item.relative_path.as_str())
            .collect::<Vec<_>>(),
        ["a/first.txt", "z/last.txt"]
    );
    assert_eq!(plan.summary.skipped, 1);
}

#[test]
fn target_mapping_rejects_non_normal_relative_paths() {
    let target = MirrorEndpointSpec::Local(PathBuf::from("/target"));
    assert!(target.location_for("../outside").is_err());
    assert!(normalized_remote_root_prefix("/absolute").is_err());
}

#[test]
fn manifest_rejects_normalization_collisions() {
    let mut entries = BTreeMap::new();
    insert_manifest_entry(&mut entries, remote_entry("src", "root/a//b", "a/b", "one"))
        .expect("insert first entry");
    let result = insert_manifest_entry(&mut entries, remote_entry("src", "root/a/b", "a/b", "two"));
    assert!(matches!(result, Err(Error::Conflict(_))));
}

#[derive(Debug, PartialEq, Eq)]
struct PathUploadCall {
    key: String,
    content_type: Option<String>,
    condition: RemoteWriteCondition,
    size: u64,
    staging: PathBuf,
    identity_etag: Option<String>,
}

struct TestRemoteSource {
    info: ObjectInfo,
    after_info: Option<ObjectInfo>,
    download_size: u64,
    head_error: Option<String>,
    head_calls: Mutex<Vec<String>>,
    download_calls: Arc<Mutex<Vec<PathBuf>>>,
    block_after_download: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl MirrorRemoteTransfer for TestRemoteSource {
    async fn mirror_head(&self, path: &RemotePath) -> rc_core::Result<ObjectInfo> {
        let call_count = {
            let mut calls = self.head_calls.lock().expect("head calls lock");
            calls.push(path.key.clone());
            calls.len()
        };
        if let Some(error) = &self.head_error {
            return Err(Error::Network(error.clone()));
        }
        if call_count > 1
            && let Some(after_info) = &self.after_info
        {
            return Ok(after_info.clone());
        }
        Ok(self.info.clone())
    }

    async fn mirror_download(
        &self,
        _path: &RemotePath,
        destination: &Path,
    ) -> rc_core::Result<u64> {
        self.download_calls
            .lock()
            .expect("download calls lock")
            .push(destination.to_path_buf());
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(destination)
            .await?;
        file.set_len(self.download_size).await?;
        if let Some(started) = &self.block_after_download {
            started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(self.download_size)
    }

    async fn mirror_upload(
        &self,
        _path: &RemotePath,
        _source: &Path,
        _content_type: Option<&str>,
        _condition: RemoteWriteCondition,
        _identity_etag: Option<&str>,
    ) -> rc_core::Result<ObjectInfo> {
        Err(Error::General(
            "test source cannot upload mirror objects".to_string(),
        ))
    }
}

#[derive(Default)]
struct TestRemoteTarget {
    uploads: Mutex<Vec<PathUploadCall>>,
    upload_error: Option<String>,
}

#[async_trait::async_trait]
impl MirrorRemoteTransfer for TestRemoteTarget {
    async fn mirror_head(&self, _path: &RemotePath) -> rc_core::Result<ObjectInfo> {
        Err(Error::General(
            "test target cannot inspect mirror sources".to_string(),
        ))
    }

    async fn mirror_download(
        &self,
        _path: &RemotePath,
        _destination: &Path,
    ) -> rc_core::Result<u64> {
        Err(Error::General(
            "test target cannot download mirror sources".to_string(),
        ))
    }

    async fn mirror_upload(
        &self,
        path: &RemotePath,
        source: &Path,
        content_type: Option<&str>,
        condition: RemoteWriteCondition,
        identity_etag: Option<&str>,
    ) -> rc_core::Result<ObjectInfo> {
        let size = tokio::fs::metadata(source).await?.len();
        self.uploads
            .lock()
            .expect("upload calls lock")
            .push(PathUploadCall {
                key: path.key.clone(),
                content_type: content_type.map(ToOwned::to_owned),
                condition,
                size,
                staging: source.to_path_buf(),
                identity_etag: identity_etag.map(ToOwned::to_owned),
            });
        if let Some(error) = &self.upload_error {
            return Err(Error::Network(error.clone()));
        }
        Ok(ObjectInfo::file(path.key.clone(), size as i64))
    }
}

fn remote_transfer_fixture(
    size: u64,
    content_type: Option<&str>,
) -> (TestRemoteSource, MirrorCopyOperation) {
    let modified = "2026-07-21T04:00:00Z"
        .parse::<Timestamp>()
        .expect("valid transfer timestamp");
    let mut info = ObjectInfo::file("source/large.bin", size as i64);
    info.last_modified = Some(modified);
    info.etag = Some("source-etag".to_string());
    info.content_type = content_type.map(ToOwned::to_owned);
    let source = MirrorEntry {
        relative_path: "large.bin".to_string(),
        location: MirrorLocation::Remote(RemotePath::new("source", "bucket", "source/large.bin")),
        snapshot: MirrorSnapshot {
            size_bytes: Some(size),
            modified: Some(modified),
            etag: Some("source-etag".to_string()),
            identity_etag: None,
        },
    };
    let operation = MirrorCopyOperation {
        relative_path: source.relative_path.clone(),
        source,
        target: MirrorLocation::Remote(RemotePath::new("target", "bucket", "backup/large.bin")),
        target_before: None,
    };
    (
        TestRemoteSource {
            info,
            after_info: None,
            download_size: size,
            head_error: None,
            head_calls: Mutex::new(Vec::new()),
            download_calls: Arc::new(Mutex::new(Vec::new())),
            block_after_download: None,
        },
        operation,
    )
}

#[tokio::test]
async fn large_remote_copy_uses_path_streaming_preserves_metadata_and_cleans_staging() {
    let size = 64 * 1024 * 1024 + 1;
    let (source, operation) = remote_transfer_fixture(size, Some("application/octet-stream"));
    let target = TestRemoteTarget::default();
    let source_path = match &operation.source.location {
        MirrorLocation::Remote(path) => path.clone(),
        MirrorLocation::Local(_) => panic!("expected remote test source"),
    };
    let target_path = match &operation.target {
        MirrorLocation::Remote(path) => path.clone(),
        MirrorLocation::Local(_) => panic!("expected remote test target"),
    };

    let copied = transfer_remote_to_remote(
        &source,
        &target,
        &source_path,
        &target_path,
        &operation,
        RemoteWriteCondition::IfAbsent,
    )
    .await
    .expect("stream remote object through a file");

    assert_eq!(copied, size);
    assert_eq!(
        source
            .head_calls
            .lock()
            .expect("head calls lock")
            .as_slice(),
        ["source/large.bin", "source/large.bin"]
    );
    let uploads = target.uploads.lock().expect("upload calls lock");
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].size, size);
    assert_eq!(
        uploads[0].content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(uploads[0].condition, RemoteWriteCondition::IfAbsent);
    assert_eq!(uploads[0].identity_etag.as_deref(), Some("source-etag"));
    assert!(!uploads[0].staging.exists());
}

#[tokio::test]
async fn remote_head_failure_prevents_download_and_upload() {
    let (mut source, operation) = remote_transfer_fixture(4, None);
    source.head_error = Some("head failed".to_string());
    let target = TestRemoteTarget::default();
    let MirrorLocation::Remote(source_path) = &operation.source.location else {
        panic!("expected remote source")
    };
    let MirrorLocation::Remote(target_path) = &operation.target else {
        panic!("expected remote target")
    };

    let result = transfer_remote_to_remote(
        &source,
        &target,
        source_path,
        target_path,
        &operation,
        RemoteWriteCondition::IfAbsent,
    )
    .await;

    assert!(matches!(result, Err(Error::Network(_))));
    assert!(
        source
            .download_calls
            .lock()
            .expect("download calls lock")
            .is_empty()
    );
    assert!(target.uploads.lock().expect("upload calls lock").is_empty());
}

#[tokio::test]
async fn failed_remote_upload_removes_the_complete_staging_file() {
    let (source, operation) = remote_transfer_fixture(4, None);
    let target = TestRemoteTarget {
        upload_error: Some("upload failed".to_string()),
        ..TestRemoteTarget::default()
    };
    let MirrorLocation::Remote(source_path) = &operation.source.location else {
        panic!("expected remote source")
    };
    let MirrorLocation::Remote(target_path) = &operation.target else {
        panic!("expected remote target")
    };

    let result = transfer_remote_to_remote(
        &source,
        &target,
        source_path,
        target_path,
        &operation,
        RemoteWriteCondition::IfAbsent,
    )
    .await;

    assert!(result.is_err());
    let uploads = target.uploads.lock().expect("upload calls lock");
    assert_eq!(uploads.len(), 1);
    assert!(!uploads[0].staging.exists());
}

#[tokio::test]
async fn remote_source_change_after_download_blocks_upload_and_cleans_staging() {
    let (mut source, operation) = remote_transfer_fixture(4, None);
    let mut changed = source.info.clone();
    changed.etag = Some("changed-etag".to_string());
    source.after_info = Some(changed);
    let target = TestRemoteTarget::default();
    let MirrorLocation::Remote(source_path) = &operation.source.location else {
        panic!("expected remote source")
    };
    let MirrorLocation::Remote(target_path) = &operation.target else {
        panic!("expected remote target")
    };

    let result = transfer_remote_to_remote(
        &source,
        &target,
        source_path,
        target_path,
        &operation,
        RemoteWriteCondition::IfAbsent,
    )
    .await;

    assert!(matches!(result, Err(Error::Conflict(_))));
    assert!(target.uploads.lock().expect("upload calls lock").is_empty());
    let downloads = source.download_calls.lock().expect("download calls lock");
    assert_eq!(downloads.len(), 1);
    assert!(!downloads[0].exists());
}

#[tokio::test]
async fn remote_source_preflight_runs_before_target_identity_check() {
    let (mut source, operation) = remote_transfer_fixture(4, None);
    source.info.etag = Some("replacement-etag".to_string());
    let MirrorLocation::Remote(source_path) = &operation.source.location else {
        panic!("expected remote source")
    };
    let target_checked = Arc::new(Mutex::new(false));
    let target_checked_for_check = Arc::clone(&target_checked);

    let result = source_preflight_then_target(&source, source_path, &operation, || async move {
        *target_checked_for_check.lock().expect("target check lock") = true;
        Ok::<(), Error>(())
    })
    .await;

    assert!(matches!(result, Err(Error::Conflict(_))));
    assert!(!*target_checked.lock().expect("target check lock"));
}

#[tokio::test]
async fn truncated_remote_download_is_rejected_and_cleaned_before_upload() {
    let (mut source, operation) = remote_transfer_fixture(4, None);
    source.download_size = 3;
    let target = TestRemoteTarget::default();
    let MirrorLocation::Remote(source_path) = &operation.source.location else {
        panic!("expected remote source")
    };
    let MirrorLocation::Remote(target_path) = &operation.target else {
        panic!("expected remote target")
    };

    let result = transfer_remote_to_remote(
        &source,
        &target,
        source_path,
        target_path,
        &operation,
        RemoteWriteCondition::IfAbsent,
    )
    .await;

    assert!(matches!(result, Err(Error::Conflict(_))));
    assert!(target.uploads.lock().expect("upload calls lock").is_empty());
    let downloads = source.download_calls.lock().expect("download calls lock");
    assert_eq!(downloads.len(), 1);
    assert!(!downloads[0].exists());
}

#[tokio::test]
async fn cancelling_a_remote_transfer_drops_and_removes_its_staging_file() {
    let (mut source, operation) = remote_transfer_fixture(4, None);
    let started = Arc::new(tokio::sync::Notify::new());
    source.block_after_download = Some(Arc::clone(&started));
    let download_calls = Arc::clone(&source.download_calls);
    let target = TestRemoteTarget::default();
    let source_path = match &operation.source.location {
        MirrorLocation::Remote(path) => path,
        MirrorLocation::Local(_) => panic!("expected remote source"),
    };
    let target_path = match &operation.target {
        MirrorLocation::Remote(path) => path,
        MirrorLocation::Local(_) => panic!("expected remote target"),
    };
    let mut transfer = Box::pin(transfer_remote_to_remote(
        &source,
        &target,
        source_path,
        target_path,
        &operation,
        RemoteWriteCondition::IfAbsent,
    ));
    tokio::select! {
        () = started.notified() => {}
        result = &mut transfer => panic!("transfer unexpectedly completed: {result:?}"),
    }
    drop(transfer);

    let downloads = download_calls.lock().expect("download calls lock");
    assert_eq!(downloads.len(), 1);
    assert!(!downloads[0].exists());
}

#[test]
fn remote_overwrite_requires_the_planned_etag() {
    let mut operation = remote_transfer_fixture(4, None).1;
    operation.target_before = Some(remote_entry(
        "target",
        "backup/large.bin",
        "large.bin",
        "target-etag",
    ));
    assert_eq!(
        remote_write_condition(&operation).expect("target ETag is available"),
        RemoteWriteCondition::IfMatch("target-etag".to_string())
    );

    operation
        .target_before
        .as_mut()
        .expect("planned target")
        .snapshot
        .etag = None;
    assert!(matches!(
        remote_write_condition(&operation),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn remote_entries_without_two_etags_are_never_assumed_equal() {
    let source = remote_entry("source", "root/file.txt", "file.txt", "same");
    let mut target = remote_entry("target", "root/file.txt", "file.txt", "same");
    assert!(source_matches_target(&source, &target, CompareMode::Auto));
    target.snapshot.etag = None;
    assert!(!source_matches_target(&source, &target, CompareMode::Auto));
}

fn remote_entry_with_identity(
    alias: &str,
    key: &str,
    relative: &str,
    etag: &str,
    identity_etag: &str,
) -> MirrorEntry {
    let mut entry = remote_entry(alias, key, relative, etag);
    entry.snapshot.identity_etag = Some(identity_etag.to_string());
    entry
}

#[test]
fn auto_compare_skips_when_source_etag_is_preserved_in_destination_metadata() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "source-etag",
    )]);
    let target = manifest([remote_entry_with_identity(
        "dst",
        "backup/report.txt",
        "report.txt",
        "multipart-etag-1",
        "source-etag",
    )]);

    let plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build identity-aware plan");

    assert!(plan.items.is_empty());
    assert_eq!(plan.summary.skipped, 1);
}

#[test]
fn runtime_identity_shortcut_requires_the_planned_destination_snapshot() {
    let source = remote_entry("src", "source/report.txt", "report.txt", "source-etag");
    let current = remote_entry_with_identity(
        "dst",
        "backup/report.txt",
        "report.txt",
        "multipart-etag-1",
        "source-etag",
    );
    let mut operation = MirrorCopyOperation {
        relative_path: "report.txt".to_string(),
        source: source.clone(),
        target: current.location.clone(),
        target_before: Some(current.clone()),
    };

    assert!(matches!(
        target_disposition_for_current(&operation, Some(&current), CompareMode::Auto),
        Ok(TargetDisposition::AlreadyComplete)
    ));

    let mut replaced = current.clone();
    replaced.snapshot.etag = Some("multipart-etag-2".to_string());
    assert!(matches!(
        target_disposition_for_current(&operation, Some(&replaced), CompareMode::Auto),
        Err(Error::Conflict(_))
    ));

    operation.target_before = None;
    assert!(matches!(
        target_disposition_for_current(&operation, Some(&current), CompareMode::Auto),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn runtime_identity_shortcut_requires_the_current_source_snapshot() {
    let source = remote_entry("src", "source/report.txt", "report.txt", "source-etag");
    let mut current = ObjectInfo::file("source/report.txt", 4);
    current.last_modified = source.snapshot.modified;
    current.etag = source.snapshot.etag.clone();
    assert!(ensure_snapshot_matches(&source, &current).is_ok());

    current.etag = Some("replacement-etag".to_string());
    assert!(matches!(
        ensure_snapshot_matches(&source, &current),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn auto_compare_copies_when_identity_metadata_is_missing() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "source-etag",
    )]);
    let target = manifest([remote_entry(
        "dst",
        "backup/report.txt",
        "report.txt",
        "multipart-etag-1",
    )]);

    let plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build plan without identity metadata");

    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.summary.skipped, 0);
}

#[test]
fn auto_compare_copies_when_identity_metadata_does_not_match() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "source-etag",
    )]);
    let target = manifest([remote_entry_with_identity(
        "dst",
        "backup/report.txt",
        "report.txt",
        "multipart-etag-1",
        "other-etag",
    )]);

    let plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build plan with mismatched identity");

    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.summary.skipped, 0);
}

#[test]
fn size_mismatch_never_skips_regardless_of_compare_mode() {
    let source = remote_entry("src", "source/report.txt", "report.txt", "source-etag");
    let mut target = remote_entry_with_identity(
        "dst",
        "backup/report.txt",
        "report.txt",
        "source-etag",
        "source-etag",
    );
    target.snapshot.size_bytes = Some(8);

    for compare in [CompareMode::Auto, CompareMode::Etag, CompareMode::Size] {
        assert!(
            !source_matches_target(&source, &target, compare),
            "{compare:?} must copy when sizes differ"
        );
        assert!(
            !destination_needs_identity_lookup(&source, &target, compare),
            "{compare:?} must not HeadObject when sizes differ"
        );
    }

    let plan = build_copy_plan(
        &manifest([source]),
        &manifest([target]),
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Auto,
    )
    .expect("build plan for size mismatch");

    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.summary.skipped, 0);
}

#[test]
fn etag_compare_still_copies_when_only_identity_metadata_matches() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "source-etag",
    )]);
    let target = manifest([remote_entry_with_identity(
        "dst",
        "backup/report.txt",
        "report.txt",
        "multipart-etag-1",
        "source-etag",
    )]);

    let plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Etag,
    )
    .expect("build etag-only plan");

    assert_eq!(plan.items.len(), 1);
}

#[test]
fn size_compare_skips_when_sizes_match_even_if_etags_differ() {
    let source = manifest([remote_entry(
        "src",
        "source/report.txt",
        "report.txt",
        "source-etag",
    )]);
    let target = manifest([remote_entry(
        "dst",
        "backup/report.txt",
        "report.txt",
        "multipart-etag-1",
    )]);

    let plan = build_copy_plan(
        &source,
        &target,
        &MirrorEndpointSpec::Remote(RemotePath::new("dst", "bucket", "backup")),
        &TransferSelection::default(),
        true,
        CompareMode::Size,
    )
    .expect("build size-only plan");

    assert!(plan.items.is_empty());
    assert_eq!(plan.summary.skipped, 1);
}

#[test]
fn identity_metadata_is_read_case_insensitively() {
    let metadata = HashMap::from([("Rc-Source-Etag".to_string(), "abc".to_string())]);
    assert_eq!(
        identity_etag_from_metadata(Some(&metadata)).as_deref(),
        Some("abc")
    );
    assert_eq!(
        identity_etag_from_metadata(Some(&HashMap::from([(
            "x-amz-meta-rc-source-etag".to_string(),
            "abc".to_string()
        )])))
        .as_deref(),
        Some("abc")
    );
    assert_eq!(
        identity_etag_from_metadata(Some(&HashMap::from([(
            "rc-source-etag".to_string(),
            String::new()
        )]))),
        None
    );
    assert_eq!(identity_etag_from_metadata(None), None);
}

#[test]
fn destination_identity_lookup_is_limited_to_auto_remote_etag_mismatches() {
    let source = remote_entry("src", "source/file.txt", "file.txt", "source-etag");
    let mismatched = remote_entry("dst", "backup/file.txt", "file.txt", "other-etag");
    let matching = remote_entry("dst", "backup/file.txt", "file.txt", "source-etag");
    let already_enriched = remote_entry_with_identity(
        "dst",
        "backup/file.txt",
        "file.txt",
        "other-etag",
        "source-etag",
    );
    let local_source = local_entry("/src", "file.txt");

    assert!(destination_needs_identity_lookup(
        &source,
        &mismatched,
        CompareMode::Auto
    ));
    assert!(!destination_needs_identity_lookup(
        &source,
        &matching,
        CompareMode::Auto
    ));
    assert!(!destination_needs_identity_lookup(
        &source,
        &already_enriched,
        CompareMode::Auto
    ));
    assert!(!destination_needs_identity_lookup(
        &source,
        &mismatched,
        CompareMode::Etag
    ));
    assert!(!destination_needs_identity_lookup(
        &source,
        &mismatched,
        CompareMode::Size
    ));
    assert!(!destination_needs_identity_lookup(
        &local_source,
        &mismatched,
        CompareMode::Auto
    ));
}

#[test]
fn destination_race_check_ignores_identity_metadata() {
    let planned = snapshot(4, "2026-07-21T04:00:00Z", Some("dest-etag"));
    let current = snapshot_with_identity(
        4,
        "2026-07-21T04:00:00Z",
        Some("dest-etag"),
        Some("source-etag"),
    );
    assert!(planned.content_matches(&current));
    assert_ne!(planned, current);
}

#[tokio::test]
async fn missing_multilevel_local_target_root_is_created_one_directory_at_a_time() {
    let temporary = tempfile::tempdir().expect("create parent directory");
    let existing = temporary.path().join("existing");
    std::fs::create_dir(&existing).expect("create existing ancestor");
    let root = existing.join("a/b/new-root");

    let destination = secure_local_path(&root, "nested/file.txt", true)
        .await
        .expect("create safe target directories");

    assert_eq!(destination, root.join("nested/file.txt"));
    for directory in [
        existing.join("a"),
        existing.join("a/b"),
        root.clone(),
        root.join("nested"),
    ] {
        let metadata = std::fs::symlink_metadata(&directory).expect("created directory metadata");
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
    }
}

#[tokio::test]
async fn plain_relative_multilevel_target_uses_the_current_directory_as_its_ancestor() {
    let current = std::env::current_dir().expect("read current directory");
    let placeholder = tempfile::Builder::new()
        .prefix(".rc-relative-root-")
        .tempdir_in(&current)
        .expect("reserve unique relative root");
    let relative_base = PathBuf::from(placeholder.path().file_name().expect("relative root name"));
    drop(placeholder);
    let root = relative_base.join("foo/bar");

    let result = secure_local_path(&root, "nested/file.txt", true).await;

    let destination = result.expect("create plain relative target tree");
    assert_eq!(destination, root.join("nested/file.txt"));
    assert!(root.join("nested").is_dir());
    std::fs::remove_dir_all(&relative_base).expect("remove relative target tree");
}

#[tokio::test]
async fn atomic_replace_failure_keeps_the_previous_destination() {
    let temporary = tempfile::tempdir().expect("create target directory");
    let destination = temporary.path().join("file.txt");
    let missing_staging = temporary.path().join("missing-stage");
    std::fs::write(&destination, b"old").expect("write previous destination");

    let result = finish_staged_download(
        tempfile::TempPath::try_from_path(missing_staging)
            .expect("take ownership of missing staging path"),
        &destination,
        Ok(3),
        None,
        true,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(
        std::fs::read(&destination).expect("read previous destination"),
        b"old"
    );
}

#[tokio::test]
async fn an_absent_local_target_is_never_clobbered_if_it_appears_before_persist() {
    let temporary = tempfile::tempdir().expect("create target directory");
    let destination = temporary.path().join("file.txt");
    let staging_path = temporary.path().join("staging");
    std::fs::write(&staging_path, b"source").expect("write complete staging file");
    std::fs::write(&destination, b"concurrent").expect("write concurrent destination");

    let result = finish_staged_download(
        tempfile::TempPath::try_from_path(staging_path.clone())
            .expect("take ownership of staging path"),
        &destination,
        Ok(6),
        None,
        false,
    )
    .await;

    assert!(matches!(result, Err(Error::Conflict(_))));
    assert!(!staging_path.exists());
    assert_eq!(
        std::fs::read(&destination).expect("read concurrent destination"),
        b"concurrent"
    );
}

#[test]
fn remote_overlap_is_detected_across_aliases_for_the_same_endpoint() {
    let source = RemotePath::new("source", "bucket", "data/source");
    let nested = RemotePath::new("target", "bucket", "data/source/archive");
    let adjacent = RemotePath::new("target", "bucket", "data/source-archive");

    assert!(
        mirror_locations_overlap(
            &source,
            &nested,
            "https://s3.example.com/",
            "https://S3.EXAMPLE.COM:443"
        )
        .expect("valid mirror roots")
    );
    assert!(
        !mirror_locations_overlap(
            &source,
            &adjacent,
            "https://s3.example.com",
            "https://s3.example.com"
        )
        .expect("valid adjacent mirror roots")
    );
}

#[derive(Parser)]
struct MirrorArgumentParser {
    #[command(flatten)]
    mirror: MirrorArgs,
}

#[test]
fn mirror_arguments_keep_legacy_parallel_alias_and_safe_defaults() {
    let defaults = MirrorArgumentParser::try_parse_from(["test", "source", "target"])
        .expect("parse mirror defaults")
        .mirror;
    assert_eq!(defaults.concurrency, 4);
    assert_eq!(defaults.retry_attempts, 3);
    assert!(!defaults.remove);
    assert!(!defaults.overwrite);

    let legacy = MirrorArgumentParser::try_parse_from([
        "test",
        "source",
        "target",
        "--parallel",
        "7",
        "--skip-errors",
    ])
    .expect("parse legacy aliases")
    .mirror;
    assert_eq!(legacy.concurrency, 7);
    assert!(legacy.continue_on_error);
}

#[test]
fn mirror_output_serialization_preserves_the_public_shape() {
    let output = MirrorOutput {
        source: "src/".to_string(),
        target: "dst/".to_string(),
        copied: 10,
        removed: 2,
        skipped: 5,
        errors: 0,
        dry_run: false,
    };
    let json = serde_json::to_value(output).expect("serialize mirror output");
    assert_eq!(json["copied"], 10);
    assert_eq!(json["removed"], 2);
    assert_eq!(json["dry_run"], false);
}

#[test]
fn human_summary_reports_cancelled_operations() {
    let output = MirrorOutput {
        source: "src/".to_string(),
        target: "dst/".to_string(),
        copied: 1,
        removed: 2,
        skipped: 3,
        errors: 4,
        dry_run: false,
    };

    assert_eq!(
        format_human_summary(&output, 5, 1024),
        "Summary: 1 copied, 2 removed, 3 skipped, 4 errors, 5 cancelled, 1 KiB transferred"
    );
}

#[test]
fn mirror_errors_map_to_distinct_usage_and_conflict_exit_codes() {
    assert_eq!(
        exit_code_for_error(&Error::InvalidPath("bad path".to_string())),
        ExitCode::UsageError
    );
    assert_eq!(
        exit_code_for_error(&Error::Conflict("changed".to_string())),
        ExitCode::Conflict
    );
}

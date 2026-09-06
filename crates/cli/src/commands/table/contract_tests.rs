use clap::Parser;

use super::*;
use crate::commands::{Cli, Commands};
use crate::output::OutputConfig;
use async_trait::async_trait;

struct Api {
    expected: Op,
    fail: bool,
}
#[async_trait]
impl TableCatalogApi for Api {
    async fn catalog(&self, request: &CatalogRequest) -> Result<Value> {
        assert_eq!(request.operation, self.expected);
        if self.fail {
            Err(Error::Auth("denied".into()))
        } else {
            Ok(json!({"metadata":{"snapshots":[{"snapshot-id":1}]}}))
        }
    }
}
fn prepare_command(args: Vec<String>) -> Result<Prepared> {
    match Cli::try_parse_from(args)
        .map_err(|e| Error::Config(e.to_string()))?
        .command
    {
        Commands::Table(command) => prepare_table(command),
        Commands::Admin(super::super::admin::AdminCommands::Table(command)) => {
            prepare_admin(command)
        }
        _ => panic!("not a table command"),
    }
}
#[tokio::test]
async fn catalog_commands_dispatch_success_and_permission_failure() {
    let cases = [
        ("table config a/b", Op::Config),
        ("table warehouse show a/b", Op::WarehouseShow),
        ("admin table warehouse enable a/b", Op::WarehouseEnable),
        (
            "table namespace create a/b/n --property owner=team",
            Op::NamespaceCreate,
        ),
        ("table namespace list a/b", Op::NamespaceList),
        ("table namespace show a/b/n", Op::NamespaceShow),
        ("table namespace exists a/b/n", Op::NamespaceExists),
        (
            "table namespace update a/b/n --set owner=team --remove obsolete",
            Op::NamespaceUpdate,
        ),
        ("table namespace remove a/b/n", Op::NamespaceRemove),
        ("table create a/b/n/t --schema-file FILE", Op::TableCreate),
        (
            "table register a/b/n/t --metadata-location s3://b/m.json",
            Op::TableRegister,
        ),
        (
            "table list a/b/n --page-size 1 --page-token opaque --no-paginate",
            Op::TableList,
        ),
        ("table show a/b/n/t", Op::TableShow),
        ("table exists a/b/n/t", Op::TableExists),
        ("table rename a/b/n/t a/b/n/u", Op::TableRename),
        ("table remove a/b/n/t", Op::TableRemove),
        ("table metadata show a/b/n/t", Op::MetadataShow),
        (
            "table snapshot list a/b/n/t --snapshots refs",
            Op::TableShow,
        ),
        ("table snapshot show a/b/n/t 1", Op::TableShow),
        ("table ref list a/b/n/t", Op::RefList),
        (
            "table ref set a/b/n/t release --type tag --snapshot-id 1 --expected-snapshot-id null --commit-id ref-1",
            Op::RefSet,
        ),
        (
            "table ref remove a/b/n/t release --expected-snapshot-id 1 --commit-id ref-2",
            Op::RefRemove,
        ),
        (
            "table commit a/b/n/t --file COMMIT --commit-id edit-1",
            Op::Commit,
        ),
        ("table view create a/b/n/t --file FILE", Op::ViewCreate),
        ("table view list a/b/n", Op::ViewList),
        ("table view show a/b/n/t", Op::ViewShow),
        ("table view exists a/b/n/t", Op::ViewExists),
        ("table view replace a/b/n/t --file FILE", Op::ViewReplace),
        ("table view remove a/b/n/t", Op::ViewRemove),
        (
            "admin table maintenance plan a/b/n/t --file FILE",
            Op::MaintenancePlan,
        ),
        (
            "admin table maintenance run a/b/n/t --file FILE --yes",
            Op::MaintenanceRun,
        ),
        (
            "admin table maintenance config show a/b/n/t",
            Op::MaintenanceConfigShow,
        ),
        (
            "admin table maintenance config set a/b/n/t --file FILE --yes",
            Op::MaintenanceConfigSet,
        ),
        (
            "admin table maintenance job show a/b/n/t job-1",
            Op::MaintenanceJobShow,
        ),
        (
            "admin table maintenance job heartbeat a/b/n/t job-1 --file FILE --yes",
            Op::JobHeartbeat,
        ),
        (
            "admin table maintenance job quarantine a/b/n/t job-1 --file FILE --yes",
            Op::JobQuarantine,
        ),
        (
            "admin table maintenance scheduler show a/b/n/t",
            Op::SchedulerShow,
        ),
        (
            "admin table maintenance scheduler run a/b/n/t --yes",
            Op::SchedulerRun,
        ),
        (
            "admin table maintenance worker run a/b/n/t --file FILE --yes",
            Op::WorkerRun,
        ),
        ("admin table catalog diagnostics a/b/n/t", Op::Diagnostics),
        ("admin table catalog export a/b/n/t", Op::Export),
        (
            "admin table catalog import a/b/n/t --file FILE --yes",
            Op::Import,
        ),
        ("admin table catalog recover a/b/n/t --yes", Op::Recover),
        (
            "admin table catalog rollback a/b/n/t --file FILE --yes",
            Op::Rollback,
        ),
        (
            "admin table catalog metadata-update a/b/n/t --file FILE --yes",
            Op::MetadataUpdate,
        ),
        (
            "admin table catalog external show a/b/n/t",
            Op::ExternalShow,
        ),
        (
            "admin table catalog external set a/b/n/t --file FILE --yes",
            Op::ExternalSet,
        ),
        (
            "admin table catalog external sync a/b/n/t --file FILE --yes",
            Op::ExternalSync,
        ),
        ("admin table migration status a/b", Op::MigrationStatus),
        ("admin table migration start a/b --yes", Op::MigrationStart),
        (
            "admin table migration cancel a/b --yes",
            Op::MigrationCancel,
        ),
    ];
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("request.json");
    let commit = dir.path().join("commit.json");
    std::fs::write(&file,r#"{"expected-version-token":"v1","expected-metadata-location":"s3://b/m1","version-token":"v1"}"#).unwrap();
    std::fs::write(&commit,r#"{"requirements":[{"type":"assert-ref-snapshot-id","ref":"main","snapshot-id":1}],"updates":[]}"#).unwrap();
    let formatter = Formatter::new(OutputConfig {
        quiet: true,
        ..Default::default()
    });
    for (command, expected) in cases {
        for fail in [false, true] {
            let args = std::iter::once("rc".to_string())
                .chain(command.split_whitespace().map(|s| {
                    match s {
                        "FILE" => file.to_str().unwrap(),
                        "COMMIT" => commit.to_str().unwrap(),
                        s => s,
                    }
                    .to_string()
                }))
                .collect();
            let prepared = prepare_command(args).unwrap_or_else(|e| panic!("{command}: {e}"));
            let code = execute_with_api(prepared, &Api { expected, fail }, &formatter).await;
            assert_eq!(
                code,
                if fail {
                    ExitCode::AuthError
                } else {
                    ExitCode::Success
                },
                "{command}"
            );
        }
    }
}
#[test]
fn catalog_output_matches_v3_schema() {
    let schema: Value =
        serde_json::from_str(include_str!("../../../../../schemas/output_v3.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for value in [
        success_output(Op::TableList, json!({"identifiers":[]})),
        success_output(Op::MaintenanceConfigShow, Value::Null),
        error_output(&Error::Conflict("stale".into())),
        error_output(&Error::UnsupportedFeature("backing".into())),
    ] {
        assert!(validator.is_valid(&value), "{value}");
    }
}
#[test]
fn catalog_standard_commit_rejects_ignored_version_guards() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("commit.json");
    std::fs::write(&file,r#"{"requirements":[{"type":"assert-current-schema-id","current-schema-id":0}],"updates":[]}"#).unwrap();
    let args = vec![
        "rc",
        "table",
        "commit",
        "a/b/n/t",
        "--file",
        file.to_str().unwrap(),
        "--commit-id",
        "c1",
        "--expected-version-token",
        "v1",
        "--expected-metadata-location",
        "s3://b/m1",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert!(
        prepare_command(args)
            .err()
            .unwrap()
            .to_string()
            .contains("Standard updates")
    );
}

#[test]
fn catalog_pointer_commit_requires_conditions_and_rejects_ignored_updates() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pointer.json");
    for (body, valid) in [
        (json!({"new-metadata-location":"s3://b/m2"}), false),
        (
            json!({"new-metadata-location":"s3://b/m2","expected-version-token":"v1","expected-metadata-location":"s3://b/m1"}),
            true,
        ),
        (
            json!({"new-metadata-location":"s3://b/m2","expected-version-token":"v1","expected-metadata-location":"s3://b/m1","updates":[{"action":"set-properties","updates":{"owner":"new"}}]}),
            false,
        ),
    ] {
        std::fs::write(&file, body.to_string()).unwrap();
        let args = [
            "rc",
            "table",
            "commit",
            "a/b/n/t",
            "--file",
            file.to_str().unwrap(),
            "--commit-id",
            "c1",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(prepare_command(args).is_ok(), valid, "{body}");
    }
}

#[test]
fn catalog_namespace_list_accepts_root_and_parent_targets() {
    for (target, namespace) in [
        ("a/warehouse", Vec::<String>::new()),
        ("a/warehouse/sales.eu", vec!["sales".into(), "eu".into()]),
    ] {
        let args = [
            "rc",
            "table",
            "namespace",
            "list",
            target,
            "--page-token",
            "opaque",
            "--no-paginate",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let prepared = prepare_command(args).unwrap();
        assert_eq!(prepared.request.operation, Op::NamespaceList);
        assert_eq!(prepared.request.target.namespace, namespace);
        assert_eq!(prepared.request.page_token.as_deref(), Some("opaque"));
        assert!(prepared.request.single_page);
    }
    for target in [
        "a/warehouse/sales..eu",
        "a/warehouse/sales%2Feu",
        "a/warehouse/sales/table",
    ] {
        let args = ["rc", "table", "namespace", "list", target]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert!(prepare_command(args).is_err());
    }
}

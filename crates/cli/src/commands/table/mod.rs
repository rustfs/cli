//! Catalog commands. Data-file reads and writes remain query-engine operations.

use crate::{exit_code::ExitCode, output::Formatter};
use clap::{Args, Subcommand, ValueEnum};
use rc_core::catalog::{
    CatalogOperation as Op, CatalogRequest, CatalogTarget, ResourceKind as Kind, TableCatalogApi,
};
use rc_core::{Error, Result};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::PathBuf};

#[derive(Debug, Args)]
pub struct TargetArgs {
    /// Catalog resource: alias/warehouse[/namespace[/table]]
    pub target: String,
}
#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Number of entries per server page (all pages are fetched by default)
    #[arg(long, default_value_t=1000, value_parser=clap::value_parser!(u16).range(1..=1000))]
    pub page_size: u16,
    /// Opaque continuation token from an earlier single-page result
    #[arg(long)]
    pub page_token: Option<String>,
    /// Return one page and its next-page-token
    #[arg(long)]
    pub no_paginate: bool,
}
#[derive(Debug, Args)]
pub struct FileArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// JSON request object file; '-' reads stdin (maximum 8 MiB)
    #[arg(long)]
    pub file: PathBuf,
}
#[derive(Debug, Args)]
pub struct AdminFileArgs {
    #[command(flatten)]
    pub input: FileArgs,
    /// Confirm this administrative mutation
    #[arg(long, required = true)]
    pub yes: bool,
}
#[derive(Debug, Args)]
pub struct AdminMutationArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Confirm this administrative mutation
    #[arg(long, required = true)]
    pub yes: bool,
}
#[derive(Debug, Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub schema_file: PathBuf,
    #[arg(long)]
    pub partition_spec_file: Option<PathBuf>,
    #[arg(long)]
    pub sort_order_file: Option<PathBuf>,
    /// Optional location within this warehouse
    #[arg(long)]
    pub location: Option<String>,
    #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u8).range(1..=2))]
    pub format_version: u8,
    /// Table property, repeatable as key=value
    #[arg(long)]
    pub property: Vec<String>,
}
#[derive(Debug, Args)]
pub struct RegisterArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub metadata_location: String,
}
#[derive(Debug, Args)]
pub struct RenameArgs {
    pub source: String,
    /// Destination in the same alias and warehouse
    pub destination: String,
}
#[derive(Debug, Args)]
pub struct NamespaceCreateArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub property: Vec<String>,
}
#[derive(Debug, Args)]
pub struct NamespaceUpdateArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub set: Vec<String>,
    #[arg(long)]
    pub remove: Vec<String>,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Snapshots {
    All,
    Refs,
}
#[derive(Debug, Args)]
pub struct ShowArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long, value_enum, default_value_t=Snapshots::All)]
    pub snapshots: Snapshots,
}
#[derive(Debug, Args)]
pub struct SnapshotShowArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    pub snapshot_id: i64,
}
#[derive(Debug, Args)]
pub struct CommitArgs {
    #[command(flatten)]
    pub input: FileArgs,
    /// Version token read before preparing the mutation
    #[arg(long)]
    pub expected_version_token: Option<String>,
    /// Metadata location read before preparing the mutation
    #[arg(long)]
    pub expected_metadata_location: Option<String>,
    /// Stable identifier for this logical commit; reuse after an uncertain outcome
    #[arg(long)]
    pub commit_id: String,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RefType {
    Branch,
    Tag,
}
#[derive(Debug, Args)]
pub struct RefSetArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    pub name: String,
    #[arg(long)]
    pub snapshot_id: i64,
    #[arg(long = "type", value_enum)]
    pub ref_type: RefType,
    /// Current snapshot ID, or 'null' to require an absent reference
    #[arg(long)]
    pub expected_snapshot_id: String,
    #[arg(long)]
    pub commit_id: String,
    #[arg(long)]
    pub min_snapshots_to_keep: Option<i64>,
    #[arg(long)]
    pub max_snapshot_age_ms: Option<i64>,
    #[arg(long)]
    pub max_ref_age_ms: Option<i64>,
}
#[derive(Debug, Args)]
pub struct RefRemoveArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    pub name: String,
    #[arg(long)]
    pub expected_snapshot_id: i64,
    #[arg(long)]
    pub commit_id: String,
    /// Allow removing a ref with explicit retention; never bypasses main protection
    #[arg(long)]
    pub force: bool,
}
#[derive(Debug, Args)]
pub struct JobArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    pub job: String,
}
#[derive(Debug, Args)]
pub struct JobMutationArgs {
    #[command(flatten)]
    pub input: AdminFileArgs,
    pub job: String,
}

#[derive(Debug, Subcommand)]
pub enum TableCommands {
    /// Discover the catalog configuration for a warehouse
    Config(TargetArgs),
    /// Inspect or enable a table warehouse
    #[command(subcommand)]
    Warehouse(WarehouseCommands),
    /// Manage namespaces and their properties
    #[command(subcommand)]
    Namespace(NamespaceCommands),
    /// Create an Iceberg v1/v2 table; does not write rows
    Create(CreateArgs),
    /// Register an existing metadata file without overwriting a table
    Register(RegisterArgs),
    /// List tables in a namespace
    List(ListArgs),
    /// Load table metadata, without credential bundles
    Show(ShowArgs),
    /// Return success if present, exit 5 if absent
    Exists(TargetArgs),
    /// Rename a table within the same warehouse
    Rename(RenameArgs),
    /// Drop the catalog entry, preserving data files (purge is unsupported)
    Remove(TargetArgs),
    /// Read the current metadata pointer and version token
    #[command(subcommand)]
    Metadata(MetadataCommands),
    /// Inspect snapshot metadata without querying rows
    #[command(subcommand)]
    Snapshot(SnapshotCommands),
    /// Manage snapshot branches and tags
    #[command(subcommand)]
    Ref(RefCommands),
    /// Submit metadata updates with explicit optimistic concurrency conditions
    Commit(CommitArgs),
    /// Manage Iceberg view definitions
    #[command(subcommand)]
    View(ViewCommands),
}
#[derive(Debug, Subcommand)]
pub enum WarehouseCommands {
    Show(TargetArgs),
}
#[derive(Debug, Subcommand)]
pub enum NamespaceCommands {
    Create(NamespaceCreateArgs),
    /// List root namespaces for a warehouse, or direct children of a namespace
    List(ListArgs),
    Show(TargetArgs),
    Exists(TargetArgs),
    Update(NamespaceUpdateArgs),
    Remove(TargetArgs),
}
#[derive(Debug, Subcommand)]
pub enum MetadataCommands {
    Show(TargetArgs),
}
#[derive(Debug, Subcommand)]
pub enum SnapshotCommands {
    List(ShowArgs),
    Show(SnapshotShowArgs),
}
#[derive(Debug, Subcommand)]
pub enum RefCommands {
    List(TargetArgs),
    Set(RefSetArgs),
    Remove(RefRemoveArgs),
}
#[derive(Debug, Subcommand)]
pub enum ViewCommands {
    /// JSON contains schema and view-version; name is derived from target
    Create(FileArgs),
    List(ListArgs),
    Show(TargetArgs),
    Exists(TargetArgs),
    /// JSON contains view requirements/updates plus expected-metadata-location
    Replace(FileArgs),
    Remove(TargetArgs),
}
#[derive(Debug, Subcommand)]
pub enum AdminTableCommands {
    /// Inspect or enable a table warehouse
    #[command(subcommand)]
    Warehouse(AdminWarehouseCommands),
    /// Inspect and run guarded maintenance operations
    #[command(subcommand)]
    Maintenance(MaintenanceCommands),
    /// Inspect, export, import and recover catalog state
    #[command(subcommand)]
    Catalog(CatalogCommands),
    /// Inspect and control backing migration
    #[command(subcommand)]
    Migration(MigrationCommands),
}
#[derive(Debug, Subcommand)]
pub enum AdminWarehouseCommands {
    /// Enable catalog on an existing bucket; create the bucket separately
    Enable(TargetArgs),
}
#[derive(Debug, Subcommand)]
pub enum MaintenanceCommands {
    /// Preview maintenance; overrides all deletion and commit flags to false
    Plan(FileArgs),
    /// Execute only the deletion/commit actions explicitly selected in the JSON
    Run(AdminFileArgs),
    /// Read or update maintenance configuration
    #[command(subcommand)]
    Config(MaintenanceConfigCommands),
    /// Inspect jobs and control leases or quarantine
    #[command(subcommand)]
    Job(JobCommands),
    /// Inspect or run one scheduling pass
    #[command(subcommand)]
    Scheduler(SchedulerCommands),
    /// Run one maintenance worker pass
    #[command(subcommand)]
    Worker(WorkerCommands),
}
#[derive(Debug, Subcommand)]
pub enum MaintenanceConfigCommands {
    Show(TargetArgs),
    Set(AdminFileArgs),
}
#[derive(Debug, Subcommand)]
pub enum JobCommands {
    Show(JobArgs),
    Heartbeat(JobMutationArgs),
    Quarantine(JobMutationArgs),
}
#[derive(Debug, Subcommand)]
pub enum SchedulerCommands {
    Show(TargetArgs),
    Run(AdminMutationArgs),
}
#[derive(Debug, Subcommand)]
pub enum WorkerCommands {
    Run(AdminFileArgs),
}
#[derive(Debug, Subcommand)]
pub enum CatalogCommands {
    Diagnostics(TargetArgs),
    Export(TargetArgs),
    Import(AdminFileArgs),
    Recover(AdminMutationArgs),
    Rollback(AdminFileArgs),
    /// Update metadata pointer using a JSON request with version-token
    MetadataUpdate(AdminFileArgs),
    /// Manage operator-supplied external catalog pointers
    #[command(subcommand)]
    External(ExternalCommands),
}
#[derive(Debug, Subcommand)]
pub enum ExternalCommands {
    Show(TargetArgs),
    Set(AdminFileArgs),
    Sync(AdminFileArgs),
}
#[derive(Debug, Subcommand)]
pub enum MigrationCommands {
    Status(TargetArgs),
    Start(AdminMutationArgs),
    Cancel(AdminMutationArgs),
}

struct Prepared {
    request: CatalogRequest,
    projection: Projection,
}
enum Projection {
    None,
    Snapshots,
    Snapshot(i64),
}
fn prepare(op: Op, args: TargetArgs, kind: Kind) -> Result<Prepared> {
    Ok(Prepared {
        request: CatalogRequest::new(op, CatalogTarget::parse(&args.target, kind)?),
        projection: Projection::None,
    })
}
fn list(op: Op, args: ListArgs, kind: Kind) -> Result<Prepared> {
    let mut prepared = prepare(op, args.target, kind)?;
    prepared.request.page_size = args.page_size;
    prepared.request.page_token = args.page_token;
    prepared.request.single_page = args.no_paginate;
    Ok(prepared)
}
fn read_json(path: &PathBuf) -> Result<Value> {
    let mut bytes = Vec::new();
    let reader: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path)?)
    };
    reader.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(Error::Config("JSON input exceeds 8 MiB".into()));
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Config("Invalid JSON input".into()))
}
fn file(op: Op, args: FileArgs) -> Result<Prepared> {
    let mut prepared = prepare(op, args.target, Kind::Table)?;
    let value = read_json(&args.file)?;
    if !value.is_object() {
        return Err(Error::Config("Request must be a JSON object".into()));
    }
    prepared.request.body = Some(value);
    Ok(prepared)
}
fn properties(values: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for item in values {
        let (key, value) = item
            .split_once('=')
            .filter(|(key, _)| !key.is_empty())
            .ok_or_else(|| Error::Config("Properties must be key=value".into()))?;
        if result.insert(key.into(), value.into()).is_some() {
            return Err(Error::Config("Duplicate property".into()));
        }
    }
    Ok(result)
}
fn require_string(body: &Value, field: &str) -> Result<()> {
    if !body
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return Err(Error::Config(format!("Request requires nonempty {field}")));
    }
    Ok(())
}
fn guard_commit(body: &Value) -> Result<()> {
    require_string(body, "expected-version-token")?;
    require_string(body, "expected-metadata-location")
}
fn show(args: ShowArgs) -> Result<Prepared> {
    let mut p = prepare(Op::TableShow, args.target, Kind::Table)?;
    p.request.snapshots = Some(
        match args.snapshots {
            Snapshots::All => "all",
            Snapshots::Refs => "refs",
        }
        .into(),
    );
    Ok(p)
}

fn prepare_table(command: TableCommands) -> Result<Prepared> {
    match command {
        TableCommands::Config(a) => prepare(Op::Config, a, Kind::Warehouse),
        TableCommands::Warehouse(WarehouseCommands::Show(a)) => {
            prepare(Op::WarehouseShow, a, Kind::Warehouse)
        }
        TableCommands::Namespace(command) => match command {
            NamespaceCommands::List(a) => {
                let kind = if a.target.target.split('/').count() == 2 {
                    Kind::Warehouse
                } else {
                    Kind::Namespace
                };
                list(Op::NamespaceList, a, kind)
            }
            NamespaceCommands::Show(a) => prepare(Op::NamespaceShow, a, Kind::Namespace),
            NamespaceCommands::Exists(a) => prepare(Op::NamespaceExists, a, Kind::Namespace),
            NamespaceCommands::Remove(a) => prepare(Op::NamespaceRemove, a, Kind::Namespace),
            NamespaceCommands::Create(a) => {
                let mut p = prepare(Op::NamespaceCreate, a.target, Kind::Namespace)?;
                p.request.body = Some(
                    json!({"namespace": p.request.target.namespace, "properties": properties(a.property)?}),
                );
                Ok(p)
            }
            NamespaceCommands::Update(a) => {
                let mut p = prepare(Op::NamespaceUpdate, a.target, Kind::Namespace)?;
                let updates = properties(a.set)?;
                if a.remove.iter().any(|key| updates.contains_key(key)) {
                    return Err(Error::Config(
                        "Cannot set and remove the same property".into(),
                    ));
                }
                p.request.body = Some(json!({"updates": updates, "removals": a.remove}));
                Ok(p)
            }
        },
        TableCommands::Create(a) => {
            let mut p = prepare(Op::TableCreate, a.target, Kind::Table)?;
            let mut props = properties(a.property)?;
            if props.contains_key("format-version") {
                return Err(Error::Config(
                    "Use --format-version instead of a format-version property".into(),
                ));
            }
            props.insert("format-version".into(), a.format_version.to_string());
            let mut body = json!({"name": p.request.target.name, "schema": read_json(&a.schema_file)?, "properties": props});
            if let Some(path) = a.partition_spec_file {
                body["partition-spec"] = read_json(&path)?;
            }
            if let Some(path) = a.sort_order_file {
                body["write-order"] = read_json(&path)?;
            }
            if let Some(location) = a.location {
                body["location"] = json!(location);
            }
            p.request.body = Some(body);
            Ok(p)
        }
        TableCommands::Register(a) => {
            let mut p = prepare(Op::TableRegister, a.target, Kind::Table)?;
            p.request.body = Some(
                json!({"name": p.request.target.name, "metadata-location": a.metadata_location}),
            );
            Ok(p)
        }
        TableCommands::List(a) => list(Op::TableList, a, Kind::Namespace),
        TableCommands::Show(a) => show(a),
        TableCommands::Exists(a) => prepare(Op::TableExists, a, Kind::Table),
        TableCommands::Remove(a) => prepare(Op::TableRemove, a, Kind::Table),
        TableCommands::Rename(a) => {
            let mut p = prepare(
                Op::TableRename,
                TargetArgs { target: a.source },
                Kind::Table,
            )?;
            let dest = CatalogTarget::parse(&a.destination, Kind::Table)?;
            let source = &p.request.target;
            if source.alias != dest.alias || source.warehouse != dest.warehouse {
                return Err(Error::Config(
                    "Rename requires the same alias and warehouse".into(),
                ));
            }
            p.request.body = Some(
                json!({"source": {"namespace": source.namespace, "name": source.name}, "destination": {"namespace": dest.namespace, "name": dest.name}}),
            );
            Ok(p)
        }
        TableCommands::Metadata(MetadataCommands::Show(a)) => {
            prepare(Op::MetadataShow, a, Kind::Table)
        }
        TableCommands::Snapshot(SnapshotCommands::List(a)) => {
            let mut p = show(a)?;
            p.projection = Projection::Snapshots;
            Ok(p)
        }
        TableCommands::Snapshot(SnapshotCommands::Show(a)) => {
            let mut p = prepare(Op::TableShow, a.target, Kind::Table)?;
            p.projection = Projection::Snapshot(a.snapshot_id);
            Ok(p)
        }
        TableCommands::Commit(a) => {
            let mut p = file(Op::Commit, a.input)?;
            let body = p.request.body.as_mut().expect("file request has a body");
            if body
                .get("commit-id")
                .is_some_and(|old| old != &json!(a.commit_id))
            {
                return Err(Error::Config(
                    "Conflicting commit-id in file and flags".into(),
                ));
            }
            body["commit-id"] = json!(a.commit_id);
            require_string(body, "commit-id")?;
            if body.get("new-metadata-location").is_some() {
                for (key, value) in [
                    ("expected-version-token", a.expected_version_token),
                    ("expected-metadata-location", a.expected_metadata_location),
                ] {
                    if let Some(value) = value {
                        if body.get(key).is_some_and(|old| old != &json!(value)) {
                            return Err(Error::Config(format!(
                                "Conflicting {key} in file and flags"
                            )));
                        }
                        body[key] = json!(value);
                    }
                }
                if body
                    .get("updates")
                    .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
                {
                    return Err(Error::Config(
                        "Pointer commits cannot include metadata updates".into(),
                    ));
                }
                guard_commit(body)?;
                require_string(body, "new-metadata-location")?;
            } else {
                if a.expected_version_token.is_some()
                    || a.expected_metadata_location.is_some()
                    || body.get("expected-version-token").is_some()
                    || body.get("expected-metadata-location").is_some()
                {
                    return Err(Error::Config("Standard updates use Iceberg requirements; version/location guards require new-metadata-location".into()));
                }
                if !body
                    .get("requirements")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty())
                {
                    return Err(Error::Config(
                        "Standard commit requires explicit Iceberg requirements".into(),
                    ));
                }
                if !body.get("updates").is_some_and(Value::is_array) {
                    return Err(Error::Config(
                        "Standard commit requires an updates array".into(),
                    ));
                }
            }
            Ok(p)
        }
        TableCommands::Ref(command) => match command {
            RefCommands::List(a) => prepare(Op::RefList, a, Kind::Table),
            RefCommands::Set(a) => {
                let mut p = prepare(Op::RefSet, a.target, Kind::Table)?;
                let expected: Value =
                    serde_json::from_str(&a.expected_snapshot_id).map_err(|_| {
                        Error::Config("expected-snapshot-id must be an integer or null".into())
                    })?;
                if !expected.is_null() && expected.as_i64().is_none() {
                    return Err(Error::Config(
                        "expected-snapshot-id must be an integer or null".into(),
                    ));
                }
                let mut body = json!({"snapshot-id":a.snapshot_id,"type":match a.ref_type { RefType::Branch=>"branch", RefType::Tag=>"tag" },"expected-snapshot-id":expected,"commit-id":a.commit_id});
                for (key, value) in [
                    ("min-snapshots-to-keep", a.min_snapshots_to_keep),
                    ("max-snapshot-age-ms", a.max_snapshot_age_ms),
                    ("max-ref-age-ms", a.max_ref_age_ms),
                ] {
                    if let Some(value) = value {
                        body[key] = json!(value);
                    }
                }
                require_string(&body, "commit-id")?;
                p.request.child = Some(a.name);
                p.request.body = Some(body);
                Ok(p)
            }
            RefCommands::Remove(a) => {
                let mut p = prepare(Op::RefRemove, a.target, Kind::Table)?;
                p.request.child = Some(a.name);
                p.request.body = Some(
                    json!({"expected-snapshot-id":a.expected_snapshot_id,"commit-id":a.commit_id,"force":a.force}),
                );
                require_string(p.request.body.as_ref().expect("body"), "commit-id")?;
                Ok(p)
            }
        },
        TableCommands::View(command) => match command {
            ViewCommands::Create(a) => {
                let mut p = file(Op::ViewCreate, a)?;
                let body = p.request.body.as_mut().expect("body");
                if body
                    .get("name")
                    .is_some_and(|name| name != &json!(p.request.target.name))
                {
                    return Err(Error::Config("View name differs from target".into()));
                }
                body["name"] = json!(p.request.target.name);
                Ok(p)
            }
            ViewCommands::List(a) => list(Op::ViewList, a, Kind::Namespace),
            ViewCommands::Show(a) => prepare(Op::ViewShow, a, Kind::Table),
            ViewCommands::Exists(a) => prepare(Op::ViewExists, a, Kind::Table),
            ViewCommands::Remove(a) => prepare(Op::ViewRemove, a, Kind::Table),
            ViewCommands::Replace(a) => {
                let p = file(Op::ViewReplace, a)?;
                require_string(
                    p.request.body.as_ref().expect("body"),
                    "expected-metadata-location",
                )?;
                Ok(p)
            }
        },
    }
}

fn admin_file(op: Op, args: AdminFileArgs) -> Result<Prepared> {
    if !args.yes {
        return Err(Error::Config(
            "Administrative mutation requires --yes".into(),
        ));
    }
    file(op, args.input)
}
fn admin_mutation(op: Op, args: AdminMutationArgs, kind: Kind) -> Result<Prepared> {
    if !args.yes {
        return Err(Error::Config(
            "Administrative mutation requires --yes".into(),
        ));
    }
    prepare(op, args.target, kind)
}
fn prepare_admin(command: AdminTableCommands) -> Result<Prepared> {
    match command {
        AdminTableCommands::Warehouse(AdminWarehouseCommands::Enable(a)) => {
            prepare(Op::WarehouseEnable, a, Kind::Warehouse)
        }
        AdminTableCommands::Migration(c) => match c {
            MigrationCommands::Status(a) => prepare(Op::MigrationStatus, a, Kind::Warehouse),
            MigrationCommands::Start(a) => admin_mutation(Op::MigrationStart, a, Kind::Warehouse),
            MigrationCommands::Cancel(a) => admin_mutation(Op::MigrationCancel, a, Kind::Warehouse),
        },
        AdminTableCommands::Catalog(c) => match c {
            CatalogCommands::Diagnostics(a) => prepare(Op::Diagnostics, a, Kind::Table),
            CatalogCommands::Export(a) => prepare(Op::Export, a, Kind::Table),
            CatalogCommands::Import(a) => admin_file(Op::Import, a),
            CatalogCommands::Recover(a) => admin_mutation(Op::Recover, a, Kind::Table),
            CatalogCommands::Rollback(a) => {
                let p = admin_file(Op::Rollback, a)?;
                require_string(p.request.body.as_ref().expect("body"), "version-token")?;
                Ok(p)
            }
            CatalogCommands::MetadataUpdate(a) => {
                let p = admin_file(Op::MetadataUpdate, a)?;
                require_string(p.request.body.as_ref().expect("body"), "version-token")?;
                Ok(p)
            }
            CatalogCommands::External(c) => match c {
                ExternalCommands::Show(a) => prepare(Op::ExternalShow, a, Kind::Table),
                ExternalCommands::Set(a) => admin_file(Op::ExternalSet, a),
                ExternalCommands::Sync(a) => admin_file(Op::ExternalSync, a),
            },
        },
        AdminTableCommands::Maintenance(c) => match c {
            MaintenanceCommands::Plan(a) => {
                let mut p = file(Op::MaintenancePlan, a)?;
                let body = p.request.body.as_mut().expect("body");
                for key in ["delete", "commit-snapshot-expiration", "commit-compaction"] {
                    body[key] = json!(false);
                }
                Ok(p)
            }
            MaintenanceCommands::Run(a) => admin_file(Op::MaintenanceRun, a),
            MaintenanceCommands::Config(c) => match c {
                MaintenanceConfigCommands::Show(a) => {
                    prepare(Op::MaintenanceConfigShow, a, Kind::Table)
                }
                MaintenanceConfigCommands::Set(a) => admin_file(Op::MaintenanceConfigSet, a),
            },
            MaintenanceCommands::Scheduler(c) => match c {
                SchedulerCommands::Show(a) => prepare(Op::SchedulerShow, a, Kind::Table),
                SchedulerCommands::Run(a) => admin_mutation(Op::SchedulerRun, a, Kind::Table),
            },
            MaintenanceCommands::Worker(WorkerCommands::Run(a)) => admin_file(Op::WorkerRun, a),
            MaintenanceCommands::Job(c) => match c {
                JobCommands::Show(a) => {
                    let mut p = prepare(Op::MaintenanceJobShow, a.target, Kind::Table)?;
                    p.request.child = Some(a.job);
                    Ok(p)
                }
                JobCommands::Heartbeat(a) => {
                    let mut p = admin_file(Op::JobHeartbeat, a.input)?;
                    p.request.child = Some(a.job);
                    Ok(p)
                }
                JobCommands::Quarantine(a) => {
                    let mut p = admin_file(Op::JobQuarantine, a.input)?;
                    p.request.child = Some(a.job);
                    Ok(p)
                }
            },
        },
    }
}

pub async fn execute(command: TableCommands, formatter: &Formatter) -> ExitCode {
    run(prepare_table(command), formatter).await
}
pub async fn execute_admin(command: AdminTableCommands, formatter: &Formatter) -> ExitCode {
    run(prepare_admin(command), formatter).await
}
async fn run(prepared: Result<Prepared>, formatter: &Formatter) -> ExitCode {
    let prepared = match prepared {
        Ok(p) => p,
        Err(e) => return emit_error(&e, formatter),
    };
    let client = rc_core::AliasManager::new()
        .and_then(|aliases| aliases.get(&prepared.request.target.alias))
        .and_then(|alias| rc_s3::AdminClient::new(&alias));
    let client = match client {
        Ok(client) => client,
        Err(error) => return emit_error(&error, formatter),
    };
    execute_with_api(prepared, &client, formatter).await
}
async fn execute_with_api(
    prepared: Prepared,
    api: &dyn TableCatalogApi,
    formatter: &Formatter,
) -> ExitCode {
    let result = async {
        let value = api.catalog(&prepared.request).await?;
        match prepared.projection {
            Projection::None => Ok(value),
            Projection::Snapshots => {
                let snapshots = value
                    .pointer("/metadata/snapshots")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::General("LoadTable response missing snapshots".into()))?;
                Ok(json!({"snapshots":snapshots}))
            }
            Projection::Snapshot(id) => {
                let snapshots = value
                    .pointer("/metadata/snapshots")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::General("LoadTable response missing snapshots".into()))?;
                snapshots
                    .iter()
                    .find(|v| v.get("snapshot-id").and_then(Value::as_i64) == Some(id))
                    .cloned()
                    .ok_or_else(|| Error::NotFound(format!("Snapshot {id}")))
            }
        }
    }
    .await;
    match result {
        Ok(data) => {
            if formatter.is_json() {
                formatter.json(&success_output(prepared.request.operation, data));
            } else {
                formatter.println(
                    &formatter
                        .sanitize_text(&serde_json::to_string_pretty(&data).unwrap_or_default()),
                );
            }
            ExitCode::Success
        }
        Err(error) => emit_error(&error, formatter),
    }
}
fn success_output(operation: Op, result: Value) -> Value {
    json!({"schema_version":3,"type":"table_catalog","status":"success","data":{"operation":operation,"result":result}})
}
fn error_output(error: &Error) -> Value {
    let kind = match error.exit_code() {
        2 => "usage_error",
        3 => "network_error",
        4 => "auth_error",
        5 => "not_found",
        6 => "conflict",
        7 => "unsupported_feature",
        130 => "interrupted",
        _ => "general_error",
    };
    let mut detail = json!({"type":kind,"message":error.to_string(),"retryable":false});
    if error.exit_code() == 7 {
        detail["capability"] = json!("table_catalog");
        detail["server"] = Value::Null;
    }
    json!({"schema_version":3,"type":"table_catalog","status":"error","error":detail})
}
fn emit_error(error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    if formatter.is_json() {
        formatter.json_error(&error_output(error));
    } else {
        formatter.error_with_code(code, &error.to_string());
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TableCommands,
    }
    #[test]
    fn catalog_rejects_unavailable_flags_and_missing_guards() {
        for args in [
            vec!["rc", "remove", "a/b/n/t", "--purge"],
            vec![
                "rc",
                "create",
                "a/b/n/t",
                "--schema-file",
                "s.json",
                "--format-version",
                "3",
            ],
            vec!["rc", "commit", "a/b/n/t", "--file", "x.json"],
        ] {
            assert!(TestCli::try_parse_from(args).is_err());
        }
    }
    #[test]
    fn catalog_rename_cannot_cross_warehouses() {
        let command = TestCli::parse_from(["rc", "rename", "a/b/n/t", "a/c/n/u"]).command;
        assert!(prepare_table(command).is_err());
    }
    #[test]
    fn catalog_preview_clears_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("request.json");
        std::fs::write(
            &path,
            r#"{"delete":true,"commit-compaction":true,"commit-snapshot-expiration":true}"#,
        )
        .unwrap();
        let p = prepare_admin(AdminTableCommands::Maintenance(MaintenanceCommands::Plan(
            FileArgs {
                target: TargetArgs {
                    target: "a/b/n/t".into(),
                },
                file: path,
            },
        )))
        .unwrap();
        let body = p.request.body.unwrap();
        assert_eq!(body["delete"], false);
        assert_eq!(body["commit-compaction"], false);
        assert_eq!(body["commit-snapshot-expiration"], false);
    }
}

#[cfg(test)]
mod contract_tests;

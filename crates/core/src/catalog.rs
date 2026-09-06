//! Table catalog resources and operations, independent of HTTP and storage SDKs.

use crate::{Error, Result};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Warehouse,
    Namespace,
    Table,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTarget {
    pub alias: String,
    pub warehouse: String,
    pub namespace: Vec<String>,
    pub name: Option<String>,
}

impl CatalogTarget {
    pub fn parse(path: &str, kind: ResourceKind) -> Result<Self> {
        let parts: Vec<_> = path.split('/').collect();
        let count = match kind {
            ResourceKind::Warehouse => 2,
            ResourceKind::Namespace => 3,
            ResourceKind::Table => 4,
        };
        if parts.len() != count || parts[0].is_empty() {
            return Err(Error::InvalidPath(
                "Expected alias/warehouse[/namespace[/table]]".into(),
            ));
        }
        validate_warehouse(parts[1])?;
        let namespace = if count >= 3 {
            if parts[2].len() > 512 {
                return Err(Error::InvalidPath("Namespace exceeds 512 bytes".into()));
            }
            parts[2]
                .split('.')
                .map(|segment| {
                    validate_segment(segment)?;
                    Ok(segment.to_owned())
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let name = if count == 4 {
            validate_segment(parts[3])?;
            Some(parts[3].to_owned())
        } else {
            None
        };
        Ok(Self {
            alias: parts[0].into(),
            warehouse: parts[1].into(),
            namespace,
            name,
        })
    }
}

/// Preserve existing bucket names while rejecting URL path syntax.
pub fn validate_warehouse(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || matches!(value, "." | "..")
    {
        return Err(Error::InvalidPath("Invalid warehouse bucket name".into()));
    }
    Ok(())
}

pub fn validate_segment(value: &str) -> Result<()> {
    let boundary = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|b| boundary(b) || b == b'_' || b == b'-')
        || !value.bytes().next().is_some_and(boundary)
        || !value.bytes().last().is_some_and(boundary)
    {
        return Err(Error::InvalidPath("Catalog names must be 1-64 lowercase ASCII letters, digits, '-' or '_', with alphanumeric boundaries".into()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogOperation {
    Config,
    WarehouseShow,
    WarehouseEnable,
    NamespaceCreate,
    NamespaceList,
    NamespaceShow,
    NamespaceExists,
    NamespaceUpdate,
    NamespaceRemove,
    TableCreate,
    TableRegister,
    TableList,
    TableShow,
    TableExists,
    TableRename,
    TableRemove,
    MetadataShow,
    MetadataUpdate,
    Commit,
    RefList,
    RefSet,
    RefRemove,
    ViewCreate,
    ViewList,
    ViewShow,
    ViewExists,
    ViewReplace,
    ViewRemove,
    MaintenancePlan,
    MaintenanceRun,
    MaintenanceConfigShow,
    MaintenanceConfigSet,
    MaintenanceJobShow,
    SchedulerShow,
    SchedulerRun,
    WorkerRun,
    JobHeartbeat,
    JobQuarantine,
    Diagnostics,
    Export,
    Import,
    Recover,
    Rollback,
    ExternalShow,
    ExternalSet,
    ExternalSync,
    MigrationStatus,
    MigrationStart,
    MigrationCancel,
}

impl CatalogOperation {
    pub const fn is_list(self) -> bool {
        matches!(self, Self::NamespaceList | Self::TableList | Self::ViewList)
    }
}

#[derive(Clone, Debug)]
pub struct CatalogRequest {
    pub operation: CatalogOperation,
    pub target: CatalogTarget,
    pub body: Option<Value>,
    /// Reference or maintenance job identifier, encoded as one path segment.
    pub child: Option<String>,
    pub page_size: u16,
    pub page_token: Option<String>,
    pub single_page: bool,
    pub snapshots: Option<String>,
}

impl CatalogRequest {
    pub fn new(operation: CatalogOperation, target: CatalogTarget) -> Self {
        Self {
            operation,
            target,
            body: None,
            child: None,
            page_size: 1000,
            page_token: None,
            single_page: false,
            snapshots: None,
        }
    }
}

#[async_trait]
pub trait TableCatalogApi: Send + Sync {
    /// Execute a catalog operation. Writes are never automatically retried.
    async fn catalog(&self, request: &CatalogRequest) -> Result<Value>;
}

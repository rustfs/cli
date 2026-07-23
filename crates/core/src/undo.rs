//! Pure planning models for safe undo of versioned object operations.

use serde::{Deserialize, Serialize};

use crate::{Error, ObjectVersion, Result};

/// A history-preserving mutation selected by the undo planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoAction {
    /// Remove the current delete marker so the prior data version becomes visible.
    RemoveDeleteMarker {
        /// Exact marker version that may be removed.
        marker_version_id: String,
        /// Data version expected to become current after marker removal.
        revealed_version_id: String,
    },
    /// Copy an existing data version onto the same key as a new version.
    RestoreVersion {
        /// Exact historical data version to copy.
        source_version_id: String,
    },
}

/// One object mutation together with the current-version precondition observed during planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoPlanItem {
    /// Object key within the bucket.
    pub key: String,
    /// Version that must still be current immediately before execution.
    pub expected_latest_version_id: String,
    /// History-preserving action to perform.
    pub action: UndoAction,
}

/// Deterministic collection of object undo actions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoPlan {
    /// Planned actions, ordered by object key.
    pub items: Vec<UndoPlanItem>,
}

impl UndoPlan {
    /// Build a deterministic plan from independently planned objects.
    pub fn new(mut items: Vec<UndoPlanItem>) -> Self {
        items.sort_by(|left, right| left.key.cmp(&right.key));
        Self { items }
    }
}

/// Result state for one planned object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UndoOutcome {
    /// The action was selected but not executed.
    Planned,
    /// The exact delete marker was removed.
    DeleteMarkerRemoved,
    /// A historical data version was copied into a new current version.
    VersionRestored {
        /// Destination version created by the copy, when reported by the backend.
        #[serde(skip_serializing_if = "Option::is_none")]
        created_version_id: Option<String>,
    },
    /// Execution was refused or failed.
    Failed {
        /// Stable error category suitable for structured output.
        error_type: String,
        /// Human-readable failure detail.
        message: String,
        /// Exact version blocked by Object Lock or retention policy, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked_version_id: Option<String>,
    },
}

/// Per-object result that retains the exact plan used for execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoObjectResult {
    /// Object key represented by this result.
    pub key: String,
    /// Planned action and expected-current precondition, when planning succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<UndoPlanItem>,
    /// Planning or execution result.
    pub outcome: UndoOutcome,
}

/// Plan a safe undo for one key from its complete, newest-first version history.
///
/// Without an explicit version, only a latest delete marker over a data version or a latest data
/// version immediately preceded by another data version is reversible. An explicit data version
/// selects that historical version for restoration. An explicit delete marker must be current.
pub fn plan_object_undo(
    key: &str,
    history: &[ObjectVersion],
    explicit_version_id: Option<&str>,
) -> Result<UndoPlanItem> {
    if key.is_empty() {
        return Err(Error::InvalidPath(
            "Undo object key cannot be empty".to_string(),
        ));
    }
    if explicit_version_id.is_some_and(str::is_empty) {
        return Err(Error::InvalidPath(
            "Undo version ID cannot be empty".to_string(),
        ));
    }

    let mut versions = history
        .iter()
        .filter(|version| version.key == key)
        .collect::<Vec<_>>();
    if versions.is_empty() {
        return Err(Error::NotFound(format!(
            "No object version history found for {key}"
        )));
    }

    let latest = exactly_one_latest(key, &versions)?.clone();

    if let Some(version_id) = explicit_version_id {
        let selected = versions
            .iter()
            .copied()
            .find(|version| version.version_id == version_id)
            .ok_or_else(|| Error::VersionNotFound {
                path: key.to_string(),
                version_id: version_id.to_string(),
            })?;

        if !selected.is_delete_marker {
            if selected.is_latest {
                return Err(Error::Conflict(format!(
                    "Version '{version_id}' is already current for {key}"
                )));
            }
            return Ok(UndoPlanItem {
                key: key.to_string(),
                expected_latest_version_id: latest.version_id.clone(),
                action: UndoAction::RestoreVersion {
                    source_version_id: version_id.to_string(),
                },
            });
        }
        if !selected.is_latest {
            return Err(Error::Conflict(format!(
                "Delete marker '{version_id}' is not current for {key}"
            )));
        }
    }

    versions.sort_by_key(|version| std::cmp::Reverse(version.last_modified));
    ensure_unambiguous_order(key, &versions)?;
    let current_index = versions
        .iter()
        .position(|version| version.is_latest)
        .ok_or_else(|| Error::Conflict(format!("No current version found for {key}")))?;
    if current_index != 0 {
        return Err(Error::Conflict(format!(
            "Version history ordering disagrees with the current version for {key}"
        )));
    }
    let previous = versions.get(1).copied().ok_or_else(|| {
        Error::Conflict(format!(
            "Current version has no reversible predecessor for {key}"
        ))
    })?;

    let action = if latest.is_delete_marker {
        if previous.is_delete_marker {
            return Err(Error::Conflict(format!(
                "Current delete marker is preceded by another delete marker for {key}"
            )));
        }
        UndoAction::RemoveDeleteMarker {
            marker_version_id: latest.version_id.clone(),
            revealed_version_id: previous.version_id.clone(),
        }
    } else {
        if previous.is_delete_marker {
            return Err(Error::Conflict(format!(
                "Current data version is preceded by a delete marker for {key}"
            )));
        }
        UndoAction::RestoreVersion {
            source_version_id: previous.version_id.clone(),
        }
    };

    Ok(UndoPlanItem {
        key: key.to_string(),
        expected_latest_version_id: latest.version_id.clone(),
        action,
    })
}

fn exactly_one_latest<'a>(key: &str, versions: &'a [&ObjectVersion]) -> Result<&'a ObjectVersion> {
    let mut latest = versions.iter().copied().filter(|version| version.is_latest);
    let selected = latest
        .next()
        .ok_or_else(|| Error::Conflict(format!("No current version found for {key}")))?;
    if latest.next().is_some() {
        return Err(Error::Conflict(format!(
            "Multiple current versions were reported for {key}"
        )));
    }
    Ok(selected)
}

fn ensure_unambiguous_order(key: &str, versions: &[&ObjectVersion]) -> Result<()> {
    let current = versions
        .first()
        .and_then(|version| version.last_modified)
        .ok_or_else(|| {
            Error::Conflict(format!(
                "Version history is missing modification time for {key}"
            ))
        })?;
    let previous = versions
        .get(1)
        .and_then(|version| version.last_modified)
        .ok_or_else(|| {
            Error::Conflict(format!(
                "Version history is missing a predecessor time for {key}"
            ))
        })?;
    if current == previous
        || versions
            .get(2)
            .and_then(|version| version.last_modified)
            .is_some_and(|next| next == previous)
    {
        return Err(Error::Conflict(format!(
            "Version history ordering is ambiguous for {key}"
        )));
    }
    Ok(())
}

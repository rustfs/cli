//! Typed Object Lock, retention, and legal-hold models.

use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Retention mode enforced by an Object Lock enabled bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetentionMode {
    /// Authorized users may shorten active retention when governance bypass is explicit.
    Governance,
    /// Active retention cannot be shortened or changed to another mode.
    Compliance,
}

impl fmt::Display for RetentionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Governance => "governance",
            Self::Compliance => "compliance",
        })
    }
}

/// Unit used by a bucket's default retention rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetentionDurationUnit {
    /// Calendar days.
    Days,
    /// Calendar years.
    Years,
}

impl fmt::Display for RetentionDurationUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Days => "days",
            Self::Years => "years",
        })
    }
}

/// Positive and unambiguous bucket default retention duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionDuration {
    /// Duration unit.
    pub unit: RetentionDurationUnit,
    /// Positive unit count accepted by the S3 API.
    pub value: i32,
}

impl RetentionDuration {
    /// Create a validated day duration.
    pub fn days(value: i32) -> Result<Self> {
        Self::new(RetentionDurationUnit::Days, value)
    }

    /// Create a validated year duration.
    pub fn years(value: i32) -> Result<Self> {
        Self::new(RetentionDurationUnit::Years, value)
    }

    fn new(unit: RetentionDurationUnit, value: i32) -> Result<Self> {
        if value <= 0 {
            return Err(Error::InvalidPath(
                "Retention duration must be a positive number of days or years".to_string(),
            ));
        }
        Ok(Self { unit, value })
    }
}

/// Default retention rule applied to newly created object versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultRetention {
    /// Default retention mode.
    pub mode: RetentionMode,
    /// Default retention duration.
    pub duration: RetentionDuration,
}

/// Bucket Object Lock configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketObjectLockConfiguration {
    /// Whether Object Lock is enabled for the bucket.
    pub enabled: bool,
    /// Optional default retention rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_retention: Option<DefaultRetention>,
}

/// Retention applied to one object version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRetention {
    /// Retention mode.
    pub mode: RetentionMode,
    /// Absolute UTC instant after which retention expires.
    pub retain_until: Timestamp,
}

/// Legal-hold status applied to one object version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegalHoldStatus {
    /// The object version is not protected by legal hold.
    Off,
    /// The object version is protected by legal hold.
    On,
}

impl LegalHoldStatus {
    /// Return the stable boolean form used by structured output.
    pub const fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

impl fmt::Display for LegalHoldStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::On => "on",
        })
    }
}

/// Version selection and protected-mutation options for Object Lock operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectLockOptions {
    /// Exact object version to read or mutate. `None` selects the current version.
    pub version_id: Option<String>,
    /// Explicitly request governance retention bypass for this mutation.
    pub bypass_governance: bool,
}

impl ObjectLockOptions {
    /// Build options while rejecting ambiguous empty version identifiers.
    pub fn new(version_id: Option<String>, bypass_governance: bool) -> Result<Self> {
        if version_id.as_deref().is_some_and(str::is_empty) {
            return Err(Error::InvalidPath("Version ID cannot be empty".to_string()));
        }
        Ok(Self {
            version_id,
            bypass_governance,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_duration_is_positive_and_unambiguous() {
        assert_eq!(
            RetentionDuration::days(30).expect("positive days"),
            RetentionDuration {
                unit: RetentionDurationUnit::Days,
                value: 30,
            }
        );
        assert_eq!(
            RetentionDuration::years(2).expect("positive years"),
            RetentionDuration {
                unit: RetentionDurationUnit::Years,
                value: 2,
            }
        );
        assert!(RetentionDuration::days(0).is_err());
        assert!(RetentionDuration::years(-1).is_err());
    }

    #[test]
    fn object_lock_options_reject_empty_versions() {
        assert!(ObjectLockOptions::new(Some(String::new()), false).is_err());
        assert_eq!(
            ObjectLockOptions::new(Some("v1".to_string()), true)
                .expect("non-empty version")
                .version_id
                .as_deref(),
            Some("v1")
        );
    }
}

//! Live object notification streaming contracts.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::Result;

/// A boxed stream of live notification frames.
pub type WatchStream = Pin<Box<dyn Stream<Item = Result<WatchFrame>> + Send + 'static>>;

/// Filters and connection settings for one live notification request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRequest {
    /// Optional bucket. `None` watches the whole alias endpoint.
    pub bucket: Option<String>,
    /// S3 event names sent as repeated `events` query parameters.
    pub events: Vec<String>,
    /// Optional decoded object-key prefix filter.
    pub prefix: Option<String>,
    /// Optional decoded object-key suffix filter.
    pub suffix: Option<String>,
    /// RustFS keepalive interval in seconds.
    pub ping_seconds: u64,
}

/// One decoded frame from a live notification stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchFrame {
    /// A server keepalive that must not be shown as an object event.
    KeepAlive,
    /// One object notification record.
    Event(Box<WatchEvent>),
}

/// Optional origin metadata supplied by RustFS for an object event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl WatchSource {
    /// Return whether the server supplied any source metadata.
    pub fn is_empty(&self) -> bool {
        self.event_source.is_none()
            && self.region.is_none()
            && self.principal_id.is_none()
            && self.host.is_none()
            && self.port.is_none()
            && self.user_agent.is_none()
    }
}

/// A version-operation-independent object notification record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEvent {
    pub event_id: Option<String>,
    pub event_name: String,
    pub bucket: String,
    pub key: String,
    pub version_id: Option<String>,
    pub delete_marker: bool,
    pub size_bytes: Option<i64>,
    pub etag: Option<String>,
    pub event_time: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<WatchSource>,
}

/// Backend boundary for opening RustFS-compatible live notification streams.
#[async_trait]
pub trait WatchApi: Send + Sync {
    /// Open a stream using the supplied scope and server-side filters.
    async fn watch(&self, request: &WatchRequest) -> Result<WatchStream>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_metadata_detects_empty_and_populated_values() {
        assert!(WatchSource::default().is_empty());

        let source = WatchSource {
            host: Some("node-1".to_string()),
            ..WatchSource::default()
        };
        assert!(!source.is_empty());
    }

    #[test]
    fn watch_event_serialization_is_independent_of_object_version_operations() {
        let event = WatchEvent {
            event_id: Some("sequence-1".to_string()),
            event_name: "s3:ObjectRemoved:DeleteMarkerCreated".to_string(),
            bucket: "photos".to_string(),
            key: "old.jpg".to_string(),
            version_id: Some("v2".to_string()),
            delete_marker: true,
            size_bytes: None,
            etag: None,
            event_time: "2026-07-21T04:01:00Z"
                .parse()
                .expect("timestamp should parse"),
            source: None,
        };

        let value = serde_json::to_value(event).expect("event should serialize");
        assert_eq!(value["delete_marker"], true);
        assert_eq!(value["version_id"], "v2");
        assert!(value.get("is_latest").is_none());
    }
}

//! RustFS operational endpoints and portable S3 usage scanning.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jiff::Timestamp;
use rc_core::ops::{
    HealthApi, HealthProbe, HealthReport, UsageBucket, UsageFailure, UsageReport, UsageScanApi,
    UsageScanRequest, UsageSnapshotApi, UsageSource,
};
use rc_core::{Error, ListOptions, ObjectStore as _, RemotePath, Result};
use reqwest::Method;
use serde::Deserialize;

use crate::{AdminClient, S3Client};

const S3_PAGE_SIZE: i32 = 1_000;

#[derive(Debug, Default, Deserialize)]
struct HealthPayload {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    ready: Option<bool>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[async_trait]
impl HealthApi for AdminClient {
    async fn check_health(&self, probe: HealthProbe, timeout: Duration) -> Result<HealthReport> {
        if timeout.is_zero() {
            return Err(Error::Config(
                "Health probe timeout must be greater than zero".to_string(),
            ));
        }

        let path = probe.path();
        let url = format!("{}{}", self.endpoint(), path);
        let started = Instant::now();
        let response = self
            .http_client()
            .get(&url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    Error::Network(format!(
                        "{} probe timed out after {} ms",
                        probe.as_str(),
                        timeout.as_millis()
                    ))
                } else {
                    Error::Network(format!("{} probe request failed: {error}", probe.as_str()))
                }
            })?;
        let status_code = response.status().as_u16();
        let http_success = response.status().is_success();
        let body = response.text().await.map_err(|error| {
            Error::Network(format!(
                "Failed to read {} probe response: {error}",
                probe.as_str()
            ))
        })?;
        let payload = serde_json::from_str::<HealthPayload>(&body).unwrap_or_default();
        let healthy = http_success && payload.ready.unwrap_or(true);
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        Ok(HealthReport {
            probe,
            endpoint: self.endpoint().to_string(),
            path: path.to_string(),
            status_code,
            healthy,
            latency_ms,
            status: payload.status,
            service: payload.service,
            server_version: payload.version,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DataUsageInfo {
    #[serde(default, alias = "lastUpdate")]
    last_update: Option<RustfsTimestamp>,
    #[serde(default, alias = "objectsTotalCount")]
    objects_total_count: u64,
    #[serde(default, alias = "versionsTotalCount")]
    versions_total_count: u64,
    #[serde(default, alias = "deleteMarkersTotalCount")]
    delete_markers_total_count: u64,
    #[serde(default, alias = "objectsTotalSize")]
    objects_total_size: u64,
    #[serde(default, alias = "bucketsUsage")]
    buckets_usage: HashMap<String, BucketUsageInfo>,
}

#[derive(Debug, Deserialize)]
struct BucketUsageInfo {
    #[serde(default)]
    size: u64,
    #[serde(default, alias = "objectsCount")]
    objects_count: u64,
    #[serde(default, alias = "versionsCount")]
    versions_count: u64,
    #[serde(default, alias = "deleteMarkersCount")]
    delete_markers_count: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RustfsTimestamp {
    Text(Timestamp),
    SystemTime {
        secs_since_epoch: u64,
        #[serde(default)]
        nanos_since_epoch: u32,
    },
}

impl RustfsTimestamp {
    fn into_timestamp(self) -> Option<Timestamp> {
        match self {
            Self::Text(timestamp) => Some(timestamp),
            Self::SystemTime {
                secs_since_epoch,
                nanos_since_epoch,
            } => {
                let seconds = i64::try_from(secs_since_epoch).ok()?;
                let nanoseconds = i128::from(seconds)
                    .checked_mul(1_000_000_000)?
                    .checked_add(i128::from(nanos_since_epoch))?;
                Timestamp::from_nanosecond(nanoseconds).ok()
            }
        }
    }
}

#[async_trait]
impl UsageSnapshotApi for AdminClient {
    async fn usage_snapshot(&self) -> Result<UsageReport> {
        let response: DataUsageInfo = self
            .request(Method::GET, "/datausageinfo", None, None)
            .await?;
        Ok(snapshot_report(response))
    }
}

fn snapshot_report(response: DataUsageInfo) -> UsageReport {
    let mut buckets = response
        .buckets_usage
        .into_iter()
        .map(|(name, bucket)| UsageBucket {
            name,
            total_bytes: bucket.size,
            object_count: bucket.objects_count,
            version_count: Some(bucket.versions_count),
            delete_marker_count: Some(bucket.delete_markers_count),
            incomplete_upload_count: None,
            incomplete_upload_bytes: None,
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| left.name.cmp(&right.name));

    UsageReport {
        source: UsageSource::ServerSnapshot,
        scope: rc_core::ops::UsageScope::Cluster,
        path: None,
        snapshot_at: response
            .last_update
            .and_then(RustfsTimestamp::into_timestamp),
        total_bytes: response.objects_total_size,
        object_count: response.objects_total_count,
        version_count: Some(response.versions_total_count),
        delete_marker_count: Some(response.delete_markers_total_count),
        incomplete_upload_count: None,
        incomplete_upload_bytes: None,
        buckets,
        partial: false,
        failures: Vec::new(),
    }
}

#[async_trait]
impl UsageScanApi for S3Client {
    async fn scan_usage(&self, request: &UsageScanRequest) -> Result<UsageReport> {
        let requested_bucket = request.bucket.as_deref();
        let mut buckets = if let Some(bucket) = requested_bucket {
            vec![bucket.to_string()]
        } else {
            self.list_buckets()
                .await?
                .into_iter()
                .map(|bucket| bucket.key)
                .collect::<Vec<_>>()
        };
        buckets.sort();

        let mut report = UsageReport::empty(UsageSource::ClientScan, request.scope(), None);
        if request.include_versions {
            report.version_count = Some(0);
            report.delete_marker_count = Some(0);
        }
        if request.include_incomplete_uploads {
            report.incomplete_upload_count = Some(0);
            report.incomplete_upload_bytes = Some(0);
        }

        for bucket in buckets {
            match scan_bucket(self, &bucket, request).await {
                Ok(usage) => report.push_bucket(usage),
                Err(error) if requested_bucket.is_none() => report.push_failure(UsageFailure {
                    bucket,
                    message: error.to_string(),
                }),
                Err(error) => return Err(error),
            }
        }
        report.finish();
        Ok(report)
    }
}

async fn scan_bucket(
    client: &S3Client,
    bucket: &str,
    request: &UsageScanRequest,
) -> Result<UsageBucket> {
    let (completed_bytes, object_count, version_count, delete_marker_count) =
        if request.include_versions {
            scan_versions(client, bucket, request.prefix.as_deref()).await?
        } else {
            let (bytes, objects) =
                scan_current_objects(client, bucket, request.prefix.as_deref()).await?;
            (bytes, objects, None, None)
        };

    let (incomplete_upload_count, incomplete_upload_bytes) = if request.include_incomplete_uploads {
        let (uploads, bytes) =
            scan_incomplete_uploads(client, bucket, request.prefix.as_deref()).await?;
        (Some(uploads), Some(bytes))
    } else {
        (None, None)
    };

    Ok(UsageBucket {
        name: bucket.to_string(),
        total_bytes: completed_bytes.saturating_add(incomplete_upload_bytes.unwrap_or_default()),
        object_count,
        version_count,
        delete_marker_count,
        incomplete_upload_count,
        incomplete_upload_bytes,
    })
}

async fn scan_current_objects(
    client: &S3Client,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<(u64, u64)> {
    let path = RemotePath::new("usage-scan", bucket, prefix.unwrap_or_default());
    let mut continuation_token = None;
    let mut bytes = 0_u64;
    let mut objects = 0_u64;

    loop {
        let page = client
            .list_objects(
                &path,
                ListOptions {
                    max_keys: Some(S3_PAGE_SIZE),
                    continuation_token: continuation_token.clone(),
                    recursive: true,
                    ..Default::default()
                },
            )
            .await?;
        for object in page.items.into_iter().filter(|object| !object.is_dir) {
            objects = objects.saturating_add(1);
            bytes = bytes.saturating_add(non_negative_bytes(object.size_bytes));
        }

        if !page.truncated {
            return Ok((bytes, objects));
        }
        continuation_token = page.continuation_token;
    }
}

async fn scan_versions(
    client: &S3Client,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<(u64, u64, Option<u64>, Option<u64>)> {
    let path = RemotePath::new("usage-scan", bucket, prefix.unwrap_or_default());
    let mut key_marker = None;
    let mut version_id_marker = None;
    let mut bytes = 0_u64;
    let mut current_objects = 0_u64;
    let mut versions = 0_u64;
    let mut delete_markers = 0_u64;

    loop {
        let page = client
            .list_object_versions_page_with_markers(
                &path,
                Some(S3_PAGE_SIZE),
                key_marker.as_deref(),
                version_id_marker.as_deref(),
            )
            .await?;
        for version in page.items {
            if version.is_delete_marker {
                delete_markers = delete_markers.saturating_add(1);
            } else {
                versions = versions.saturating_add(1);
                bytes = bytes.saturating_add(non_negative_bytes(version.size_bytes));
                if version.is_latest {
                    current_objects = current_objects.saturating_add(1);
                }
            }
        }

        if !page.truncated {
            return Ok((bytes, current_objects, Some(versions), Some(delete_markers)));
        }
        let next_key_marker = page.continuation_token.ok_or_else(|| {
            Error::Network(
                "S3 returned a truncated version listing without a key marker".to_string(),
            )
        })?;
        if key_marker.as_deref() == Some(next_key_marker.as_str())
            && version_id_marker == page.version_id_marker
        {
            return Err(Error::Network(
                "S3 returned a truncated version listing without advancing its markers".to_string(),
            ));
        }
        key_marker = Some(next_key_marker);
        version_id_marker = page.version_id_marker;
    }
}

async fn scan_incomplete_uploads(
    client: &S3Client,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<(u64, u64)> {
    let mut key_marker: Option<String> = None;
    let mut upload_id_marker: Option<String> = None;
    let mut uploads = 0_u64;
    let mut bytes = 0_u64;

    loop {
        let mut operation = client
            .inner()
            .list_multipart_uploads()
            .bucket(bucket)
            .max_uploads(S3_PAGE_SIZE);
        if let Some(prefix) = prefix {
            operation = operation.prefix(prefix);
        }
        if let Some(marker) = key_marker.as_deref() {
            operation = operation.key_marker(marker);
        }
        if let Some(marker) = upload_id_marker.as_deref() {
            operation = operation.upload_id_marker(marker);
        }
        let page = operation
            .send()
            .await
            .map_err(|error| map_s3_scan_error("list incomplete multipart uploads", &error))?;
        for upload in page.uploads() {
            let key = upload.key().ok_or_else(|| {
                Error::Network("S3 returned an incomplete upload without an object key".to_string())
            })?;
            let upload_id = upload.upload_id().ok_or_else(|| {
                Error::Network("S3 returned an incomplete upload without an upload ID".to_string())
            })?;
            uploads = uploads.saturating_add(1);
            bytes =
                bytes.saturating_add(scan_uploaded_parts(client, bucket, key, upload_id).await?);
        }

        if !page.is_truncated().unwrap_or(false) {
            return Ok((uploads, bytes));
        }
        let next_key_marker = page.next_key_marker().map(str::to_string);
        let next_upload_id_marker = page.next_upload_id_marker().map(str::to_string);
        if next_key_marker.is_none()
            || (next_key_marker == key_marker && next_upload_id_marker == upload_id_marker)
        {
            return Err(Error::Network(
                "S3 returned a truncated multipart listing without advancing its markers"
                    .to_string(),
            ));
        }
        key_marker = next_key_marker;
        upload_id_marker = next_upload_id_marker;
    }
}

async fn scan_uploaded_parts(
    client: &S3Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<u64> {
    let mut pages = client
        .inner()
        .list_parts()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .into_paginator()
        .send();
    let mut bytes = 0_u64;

    while let Some(page) = pages
        .try_next()
        .await
        .map_err(|error| map_s3_scan_error("list uploaded multipart parts", &error))?
    {
        for part in page.parts() {
            bytes = bytes.saturating_add(non_negative_bytes(part.size()));
        }
    }
    Ok(bytes)
}

fn non_negative_bytes(value: Option<i64>) -> u64 {
    value
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}

fn map_s3_scan_error(context: &str, error: &impl std::fmt::Display) -> Error {
    let message = format!("{context}: {error}");
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("accessdenied")
        || normalized.contains("forbidden")
        || normalized.contains("unauthorized")
    {
        Error::Auth(message)
    } else if normalized.contains("nosuchbucket") || normalized.contains("notfound") {
        Error::NotFound(message)
    } else {
        Error::Network(message)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use rc_core::Alias;

    use super::*;

    struct TestResponse {
        status: &'static str,
        body: &'static str,
        delay: Duration,
    }

    fn start_sequence_server(
        responses: Vec<TestResponse>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept test request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).expect("read test request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8_lossy(&request);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("missing-target")
                    .to_string();
                sender.send(target).expect("capture test request");
                thread::sleep(response.delay);
                let wire = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                let _ = stream.write_all(wire.as_bytes());
            }
        });
        (format!("http://{address}"), receiver, handle)
    }

    fn admin_client(endpoint: &str) -> AdminClient {
        AdminClient::new(&Alias::new("test", endpoint, "access", "secret"))
            .expect("build admin client")
    }

    #[test]
    fn rustfs_system_time_and_bucket_map_convert_to_sorted_snapshot() {
        let raw = serde_json::from_str::<DataUsageInfo>(
            r#"{
                "last_update":{"secs_since_epoch":1700000000,"nanos_since_epoch":9},
                "objects_total_count":3,
                "versions_total_count":5,
                "delete_markers_total_count":2,
                "objects_total_size":99,
                "buckets_usage":{
                    "zeta":{"size":60,"objects_count":2,"versions_count":3,"delete_markers_count":1},
                    "alpha":{"size":39,"objects_count":1,"versions_count":2,"delete_markers_count":1}
                }
            }"#,
        )
        .expect("RustFS beta.10 data usage response should parse");

        let report = snapshot_report(raw);
        assert_eq!(report.source, UsageSource::ServerSnapshot);
        assert_eq!(
            report.snapshot_at.map(Timestamp::as_second),
            Some(1_700_000_000)
        );
        assert_eq!(report.buckets[0].name, "alpha");
        assert_eq!(report.buckets[1].name, "zeta");
        assert_eq!(report.total_bytes, 99);
        assert_eq!(report.version_count, Some(5));
    }

    #[test]
    fn scan_error_mapping_preserves_auth_and_not_found_classes() {
        assert!(matches!(
            map_s3_scan_error("list", &"AccessDenied"),
            Error::Auth(_)
        ));
        assert!(matches!(
            map_s3_scan_error("list", &"NoSuchBucket"),
            Error::NotFound(_)
        ));
        assert!(matches!(
            map_s3_scan_error("list", &"connection reset"),
            Error::Network(_)
        ));
    }

    #[tokio::test]
    async fn health_adapter_reports_healthy_and_not_ready_responses() {
        let (healthy_endpoint, healthy_requests, healthy_handle) = start_sequence_server(vec![
            TestResponse {
                status: "200 OK",
                body: r#"{"status":"ok","ready":true,"service":"rustfs-endpoint","version":"1.0.0-beta.10"}"#,
                delay: Duration::ZERO,
            },
        ]);
        let healthy = admin_client(&healthy_endpoint)
            .check_health(HealthProbe::Liveness, Duration::from_secs(1))
            .await
            .expect("liveness response");
        assert!(healthy.healthy);
        assert_eq!(healthy.status_code, 200);
        assert_eq!(healthy_requests.recv().expect("health target"), "/health");
        healthy_handle.join().expect("healthy server");

        let (ready_endpoint, ready_requests, ready_handle) =
            start_sequence_server(vec![TestResponse {
                status: "503 Service Unavailable",
                body: r#"{"status":"degraded","ready":false}"#,
                delay: Duration::ZERO,
            }]);
        let ready = admin_client(&ready_endpoint)
            .check_health(HealthProbe::Readiness, Duration::from_secs(1))
            .await
            .expect("readiness response");
        assert!(!ready.healthy);
        assert_eq!(ready.status_code, 503);
        assert_eq!(
            ready_requests.recv().expect("ready target"),
            "/health/ready"
        );
        ready_handle.join().expect("readiness server");
    }

    #[tokio::test]
    async fn health_adapter_enforces_total_timeout() {
        let (endpoint, _requests, handle) = start_sequence_server(vec![TestResponse {
            status: "200 OK",
            body: r#"{"status":"ok","ready":true}"#,
            delay: Duration::from_millis(100),
        }]);
        let error = admin_client(&endpoint)
            .check_health(HealthProbe::Liveness, Duration::from_millis(10))
            .await
            .expect_err("delayed response should time out");

        assert!(matches!(
            error,
            Error::Network(message) if message.contains("timed out")
        ));
        handle.join().expect("timeout server");
    }

    #[tokio::test]
    async fn client_scan_paginates_current_objects() {
        const FIRST_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>photos</Name><Prefix></Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>true</IsTruncated>
  <NextContinuationToken>page-2</NextContinuationToken>
  <Contents><Key>a.jpg</Key><LastModified>2026-07-21T00:00:00Z</LastModified><ETag>"a"</ETag><Size>5</Size><StorageClass>STANDARD</StorageClass></Contents>
</ListBucketResult>"#;
        const SECOND_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>photos</Name><Prefix></Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
  <Contents><Key>b.jpg</Key><LastModified>2026-07-21T00:00:00Z</LastModified><ETag>"b"</ETag><Size>7</Size><StorageClass>STANDARD</StorageClass></Contents>
</ListBucketResult>"#;
        let (endpoint, requests, handle) = start_sequence_server(vec![
            TestResponse {
                status: "200 OK",
                body: FIRST_PAGE,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: SECOND_PAGE,
                delay: Duration::ZERO,
            },
        ]);
        let client = S3Client::new(Alias::new("test", &endpoint, "access", "secret"))
            .await
            .expect("build S3 client");
        let report = client
            .scan_usage(&UsageScanRequest {
                bucket: Some("photos".to_string()),
                prefix: None,
                include_versions: false,
                include_incomplete_uploads: false,
            })
            .await
            .expect("scan paginated objects");

        assert_eq!(report.object_count, 2);
        assert_eq!(report.total_bytes, 12);
        let first = requests.recv().expect("first list target");
        let second = requests.recv().expect("second list target");
        assert!(first.contains("list-type=2"));
        assert!(second.contains("continuation-token=page-2"));
        handle.join().expect("object pagination server");
    }

    #[tokio::test]
    async fn client_scan_paginates_versions_and_counts_delete_markers() {
        const FIRST_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>photos</Name><Prefix></Prefix><KeyMarker></KeyMarker><VersionIdMarker></VersionIdMarker>
  <NextKeyMarker>a.jpg</NextKeyMarker><NextVersionIdMarker>v1</NextVersionIdMarker><MaxKeys>1000</MaxKeys><IsTruncated>true</IsTruncated>
  <Version><Key>a.jpg</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest><LastModified>2026-07-21T00:00:00Z</LastModified><ETag>"a2"</ETag><Size>5</Size><StorageClass>STANDARD</StorageClass></Version>
  <DeleteMarker><Key>removed.jpg</Key><VersionId>d1</VersionId><IsLatest>true</IsLatest><LastModified>2026-07-21T00:00:00Z</LastModified></DeleteMarker>
</ListVersionsResult>"#;
        const SECOND_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>photos</Name><Prefix></Prefix><KeyMarker>a.jpg</KeyMarker><VersionIdMarker>v1</VersionIdMarker><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
  <Version><Key>a.jpg</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest><LastModified>2026-07-20T00:00:00Z</LastModified><ETag>"a1"</ETag><Size>7</Size><StorageClass>STANDARD</StorageClass></Version>
</ListVersionsResult>"#;
        let (endpoint, requests, handle) = start_sequence_server(vec![
            TestResponse {
                status: "200 OK",
                body: FIRST_PAGE,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: SECOND_PAGE,
                delay: Duration::ZERO,
            },
        ]);
        let client = S3Client::new(Alias::new("test", &endpoint, "access", "secret"))
            .await
            .expect("build S3 client");
        let report = client
            .scan_usage(&UsageScanRequest {
                bucket: Some("photos".to_string()),
                prefix: None,
                include_versions: true,
                include_incomplete_uploads: false,
            })
            .await
            .expect("scan paginated versions");

        assert_eq!(report.object_count, 1);
        assert_eq!(report.version_count, Some(2));
        assert_eq!(report.delete_marker_count, Some(1));
        assert_eq!(report.total_bytes, 12);
        let first = requests.recv().expect("first version target");
        let second = requests.recv().expect("second version target");
        assert!(first.contains("versions"));
        assert!(second.contains("key-marker=a.jpg"));
        assert!(second.contains("version-id-marker=v1"));
        handle.join().expect("version pagination server");
    }

    #[tokio::test]
    async fn client_scan_paginates_incomplete_uploads_and_parts() {
        const EMPTY_OBJECTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>photos</Name><Prefix></Prefix><KeyCount>0</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated></ListBucketResult>"#;
        const FIRST_UPLOAD_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>photos</Bucket><KeyMarker></KeyMarker><UploadIdMarker></UploadIdMarker><NextKeyMarker>b.bin</NextKeyMarker><NextUploadIdMarker>up-2</NextUploadIdMarker><MaxUploads>1000</MaxUploads><IsTruncated>true</IsTruncated>
  <Upload><Key>a.bin</Key><UploadId>up-1</UploadId><Initiated>2026-07-21T00:00:00Z</Initiated><StorageClass>STANDARD</StorageClass></Upload>
</ListMultipartUploadsResult>"#;
        const SECOND_UPLOAD_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>photos</Bucket><KeyMarker>b.bin</KeyMarker><UploadIdMarker>up-2</UploadIdMarker><MaxUploads>1000</MaxUploads><IsTruncated>false</IsTruncated>
  <Upload><Key>b.bin</Key><UploadId>up-2</UploadId><Initiated>2026-07-21T00:00:00Z</Initiated><StorageClass>STANDARD</StorageClass></Upload>
</ListMultipartUploadsResult>"#;
        const FIRST_PART_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>photos</Bucket><Key>a.bin</Key><UploadId>up-1</UploadId><PartNumberMarker>0</PartNumberMarker><NextPartNumberMarker>1</NextPartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>true</IsTruncated><Part><PartNumber>1</PartNumber><LastModified>2026-07-21T00:00:00Z</LastModified><ETag>"p1"</ETag><Size>5</Size></Part></ListPartsResult>"#;
        const SECOND_PART_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>photos</Bucket><Key>a.bin</Key><UploadId>up-1</UploadId><PartNumberMarker>1</PartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>false</IsTruncated><Part><PartNumber>2</PartNumber><LastModified>2026-07-21T00:00:00Z</LastModified><ETag>"p2"</ETag><Size>7</Size></Part></ListPartsResult>"#;
        const FINAL_PART_PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>photos</Bucket><Key>b.bin</Key><UploadId>up-2</UploadId><PartNumberMarker>0</PartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>false</IsTruncated><Part><PartNumber>1</PartNumber><LastModified>2026-07-21T00:00:00Z</LastModified><ETag>"p3"</ETag><Size>11</Size></Part></ListPartsResult>"#;
        let (endpoint, requests, handle) = start_sequence_server(vec![
            TestResponse {
                status: "200 OK",
                body: EMPTY_OBJECTS,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: FIRST_UPLOAD_PAGE,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: FIRST_PART_PAGE,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: SECOND_PART_PAGE,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: SECOND_UPLOAD_PAGE,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                body: FINAL_PART_PAGE,
                delay: Duration::ZERO,
            },
        ]);
        let client = S3Client::new(Alias::new("test", &endpoint, "access", "secret"))
            .await
            .expect("build S3 client");
        let report = client
            .scan_usage(&UsageScanRequest {
                bucket: Some("photos".to_string()),
                prefix: None,
                include_versions: false,
                include_incomplete_uploads: true,
            })
            .await
            .expect("scan paginated multipart usage");

        assert_eq!(report.incomplete_upload_count, Some(2));
        assert_eq!(report.incomplete_upload_bytes, Some(23));
        assert_eq!(report.total_bytes, 23);
        let targets = (0..6)
            .map(|_| requests.recv().expect("multipart target"))
            .collect::<Vec<_>>();
        assert!(targets[1].contains("uploads"));
        assert!(targets[3].contains("part-number-marker=1"));
        assert!(targets[4].contains("key-marker=b.bin"));
        assert!(targets[4].contains("upload-id-marker=up-2"));
        handle.join().expect("multipart pagination server");
    }
}

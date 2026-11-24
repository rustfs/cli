//! RustFS S3-over-NIXL request and response fields.

use http::{HeaderMap, Method, StatusCode};
use rc_core::{Error, RemotePath, Result};
use rc_s3::data_transport::ControlRequest;
use rustfs_nixl::{RDMA_BYTES_TRANSFERRED_HEADER, RDMA_REPLY_HEADER};

pub(crate) fn request(
    method: Method,
    path: &RemotePath,
    query: Vec<(String, String)>,
    headers: HeaderMap,
) -> ControlRequest {
    ControlRequest {
        method,
        path: path.clone(),
        query,
        headers,
    }
}

pub(crate) fn field<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let first = values
        .next()
        .map(|v| v.to_str())
        .transpose()
        .map_err(|_| Error::General(format!("Invalid {name} response header")))?;
    if values.next().is_some() {
        return Err(Error::General(format!("Duplicate {name} response header")));
    }
    Ok(first)
}

pub(crate) fn receipt(status: StatusCode, headers: &HeaderMap, bytes: u64) -> Result<()> {
    if field(headers, RDMA_REPLY_HEADER)? != Some(status.as_str()) {
        return Err(Error::UnsupportedFeature(
            "The endpoint did not confirm an RDMA transfer".into(),
        ));
    }
    let moved = field(headers, RDMA_BYTES_TRANSFERRED_HEADER)?
        .unwrap_or("0")
        .parse::<u64>()
        .map_err(|_| Error::General("Invalid RDMA transfer count".into()))?;
    if moved != bytes {
        return Err(Error::General(format!(
            "RDMA transferred {moved} of {bytes} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, StatusCode};

    #[test]
    fn empty_http_body_has_a_separate_logical_byte_count() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", "0".parse().unwrap());
        headers.insert("x-amz-rdma-reply", "206".parse().unwrap());
        headers.insert("x-amz-rdma-bytes-transferred", "4".parse().unwrap());
        assert!(receipt(StatusCode::PARTIAL_CONTENT, &headers, 4).is_ok());
        assert!(receipt(StatusCode::OK, &headers, 4).is_err());
        assert!(receipt(StatusCode::PARTIAL_CONTENT, &headers, 5).is_err());
        headers.append("x-amz-rdma-bytes-transferred", "4".parse().unwrap());
        assert!(receipt(StatusCode::PARTIAL_CONTENT, &headers, 4).is_err());
    }

    #[test]
    fn ordinary_s3_success_is_not_a_transfer_receipt() {
        assert!(receipt(StatusCode::OK, &HeaderMap::new(), 0).is_err());
    }
}

#[cfg(test)]
mod native_test;

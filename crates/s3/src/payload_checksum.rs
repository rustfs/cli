//! Streaming verification of logical S3 response checksum headers and trailers.

use std::collections::HashSet;

use aws_smithy_checksums::{
    ChecksumAlgorithm as SdkAlgorithm,
    http::{CHECKSUM_ALGORITHMS_IN_PRIORITY_ORDER, HttpChecksum},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http::HeaderMap;
use rc_core::{ChecksumAlgorithm, Error, ObjectChecksum, Result};

/// Verification result distinguishes composite metadata from a verified body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadChecksumStatus {
    NotDeclared,
    Verified(ChecksumAlgorithm),
    Composite(ChecksumAlgorithm),
}

/// Selects the same preferred supported checksum as the AWS S3 SDK.
///
/// Construct this value from the selected response head before receiving bytes.
/// Finish with the complete logical headers after authenticated trailers arrive.
pub struct PayloadChecksumVerifier {
    selected: Option<Selected>,
}

struct Selected {
    algorithm: ChecksumAlgorithm,
    header: &'static str,
    hasher: Box<dyn HttpChecksum>,
    composite: bool,
    expected: Option<String>,
}

impl PayloadChecksumVerifier {
    pub fn new(headers: &HeaderMap) -> Result<Self> {
        let declared = declared_trailers(headers)?;
        let composite_type = scalar(headers, "x-amz-checksum-type")? == Some("COMPOSITE");
        for name in CHECKSUM_ALGORITHMS_IN_PRIORITY_ORDER {
            let algorithm: SdkAlgorithm = name.parse().map_err(|_| {
                Error::General("The S3 checksum algorithm registry is invalid".to_string())
            })?;
            let hasher = algorithm.into_impl();
            let header = hasher.header_name();
            let expected = scalar(headers, header)?;
            if expected.is_none() && !declared.contains(header) {
                continue;
            }
            let algorithm = core_algorithm(algorithm)?;
            if let Some(value) = expected {
                ObjectChecksum::new_persisted(algorithm, value)?;
            }
            let composite = composite_type || expected.is_some_and(is_composite);
            return Ok(Self {
                selected: Some(Selected {
                    algorithm,
                    header,
                    hasher,
                    composite,
                    expected: expected.map(str::to_string),
                }),
            });
        }
        Ok(Self { selected: None })
    }

    pub fn update(&mut self, bytes: &[u8]) {
        if let Some(selected) = &mut self.selected
            && !selected.composite
        {
            selected.hasher.update(bytes);
        }
    }

    pub fn verify(self, headers: &HeaderMap) -> Result<PayloadChecksumStatus> {
        let Some(selected) = self.selected else {
            return Ok(PayloadChecksumStatus::NotDeclared);
        };
        let expected = scalar(headers, selected.header)?.ok_or_else(|| {
            Error::General("The declared S3 checksum trailer is missing".to_string())
        })?;
        if selected
            .expected
            .as_deref()
            .is_some_and(|original| original != expected)
        {
            return Err(Error::General(
                "The declared S3 checksum changed during payload delivery".to_string(),
            ));
        }
        ObjectChecksum::new_persisted(selected.algorithm, expected)?;
        if selected.composite
            || is_composite(expected)
            || scalar(headers, "x-amz-checksum-type")? == Some("COMPOSITE")
        {
            return Ok(PayloadChecksumStatus::Composite(selected.algorithm));
        }
        let actual = STANDARD.encode(selected.hasher.finalize());
        if actual != expected {
            return Err(Error::General(
                "The selected S3 response checksum does not match its payload".to_string(),
            ));
        }
        Ok(PayloadChecksumStatus::Verified(selected.algorithm))
    }
}

fn core_algorithm(algorithm: SdkAlgorithm) -> Result<ChecksumAlgorithm> {
    match algorithm {
        SdkAlgorithm::Crc32 => Ok(ChecksumAlgorithm::Crc32),
        SdkAlgorithm::Crc32c => Ok(ChecksumAlgorithm::Crc32c),
        SdkAlgorithm::Crc64Nvme => Ok(ChecksumAlgorithm::Crc64Nvme),
        SdkAlgorithm::Sha1 => Ok(ChecksumAlgorithm::Sha1),
        SdkAlgorithm::Sha256 => Ok(ChecksumAlgorithm::Sha256),
        _ => Err(Error::UnsupportedFeature(
            "The S3 response checksum algorithm is unsupported".to_string(),
        )),
    }
}

fn scalar<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let result = values
        .next()
        .map(|value| {
            value
                .to_str()
                .map_err(|_| Error::General("An S3 checksum header is invalid".to_string()))
        })
        .transpose()?;
    if values.next().is_some() {
        return Err(Error::General(
            "An S3 checksum header is ambiguous".to_string(),
        ));
    }
    Ok(result)
}

fn declared_trailers(headers: &HeaderMap) -> Result<HashSet<String>> {
    let mut declared = HashSet::new();
    for name in ["trailer", "x-amz-trailer"] {
        for value in headers.get_all(name) {
            let value = value
                .to_str()
                .map_err(|_| Error::General("The S3 trailer declaration is invalid".to_string()))?;
            for name in value.split(',').map(str::trim) {
                if name.is_empty() {
                    return Err(Error::General(
                        "The S3 trailer declaration is empty".to_string(),
                    ));
                }
                declared.insert(name.to_ascii_lowercase());
            }
        }
    }
    Ok(declared)
}

fn is_composite(value: &str) -> bool {
    value
        .rsplit_once('-')
        .is_some_and(|(_, count)| count.parse::<u32>().is_ok_and(|count| count > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn verifies_split_payload_and_rejects_mismatching_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-checksum-sha256",
            HeaderValue::from_static("LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ="),
        );
        let mut verifier = PayloadChecksumVerifier::new(&headers).unwrap();
        verifier.update(b"he");
        verifier.update(b"llo");
        assert_eq!(
            verifier.verify(&headers).unwrap(),
            PayloadChecksumStatus::Verified(ChecksumAlgorithm::Sha256)
        );
        let mut verifier = PayloadChecksumVerifier::new(&headers).unwrap();
        verifier.update(b"other");
        assert!(verifier.verify(&headers).is_err());
    }

    #[test]
    fn selects_the_sdk_priority_and_validates_a_declared_trailer() {
        let mut headers = HeaderMap::new();
        headers.insert("trailer", HeaderValue::from_static("x-amz-checksum-crc32"));
        let mut verifier = PayloadChecksumVerifier::new(&headers).unwrap();
        verifier.update(b"hello");
        headers.insert("x-amz-checksum-crc32", HeaderValue::from_static("NhCmhg=="));
        headers.insert(
            "x-amz-checksum-sha256",
            HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        );
        assert_eq!(
            verifier.verify(&headers).unwrap(),
            PayloadChecksumStatus::Verified(ChecksumAlgorithm::Crc32)
        );
    }

    #[test]
    fn missing_declared_checksum_and_ambiguous_headers_fail() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-trailer",
            HeaderValue::from_static("x-amz-checksum-crc32"),
        );
        assert!(
            PayloadChecksumVerifier::new(&headers)
                .unwrap()
                .verify(&headers)
                .is_err()
        );
        headers.append("x-amz-checksum-crc32", HeaderValue::from_static("NhCmhg=="));
        headers.append("x-amz-checksum-crc32", HeaderValue::from_static("NhCmhg=="));
        assert!(PayloadChecksumVerifier::new(&headers).is_err());
    }

    #[test]
    fn composite_metadata_is_explicitly_distinct_from_verified_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-checksum-sha256",
            HeaderValue::from_static("LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=-2"),
        );
        let mut verifier = PayloadChecksumVerifier::new(&headers).unwrap();
        verifier.update(b"unrelated data");
        assert_eq!(
            verifier.verify(&headers).unwrap(),
            PayloadChecksumStatus::Composite(ChecksumAlgorithm::Sha256)
        );
    }

    #[test]
    fn absent_checksum_retains_an_explicit_result() {
        let headers = HeaderMap::new();
        assert_eq!(
            PayloadChecksumVerifier::new(&headers)
                .unwrap()
                .verify(&headers)
                .unwrap(),
            PayloadChecksumStatus::NotDeclared
        );
    }
}

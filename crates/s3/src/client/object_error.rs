//! S3 object errors shared by SDK operations and signed control requests.

use super::{ControlResponse, Error, ProvideErrorMetadata, RemotePath, S3Client, SdkError};

struct ObjectErrorFields<'a> {
    status: u16,
    code: Option<&'a str>,
    version_id: Option<&'a str>,
    delete_marker: bool,
}

impl<'a> ObjectErrorFields<'a> {
    fn from_headers(status: u16, header: impl Fn(&'static str) -> Option<&'a str>) -> Self {
        Self {
            status,
            code: header("x-amz-error-code"),
            version_id: header("x-amz-version-id"),
            delete_marker: header("x-amz-delete-marker")
                .is_some_and(|v| v.eq_ignore_ascii_case("true")),
        }
    }

    fn classify(
        &self,
        path: &RemotePath,
        requested_version: Option<&str>,
        service_message: Option<&str>,
        diagnostic: &str,
    ) -> Option<Error> {
        if let Some(error) = crate::auth::temporary_credential_error(self.code) {
            return Some(error);
        }
        let missing = matches!(self.code, Some("NoSuchKey" | "NotFound")) || self.status == 404;
        Some(match (self.status, self.code) {
            (_, Some("InvalidStorageClass")) => Error::UnsupportedFeature(
                "The S3 endpoint rejected the requested storage class".into(),
            ),
            (_, Some("BadDigest" | "InvalidDigest")) => {
                Error::Conflict("The S3 endpoint rejected the supplied object checksum".into())
            }
            (409 | 412, _) | (_, Some("ConditionalRequestConflict" | "PreconditionFailed")) => {
                Error::Conflict(diagnostic.into())
            }
            (401, _) | (_, Some("Unauthorized")) => Error::Auth(diagnostic.into()),
            _ if service_message.is_some_and(S3Client::is_retention_denial) => {
                Error::GovernanceDenied {
                    path: path.to_string(),
                    version_id: requested_version.map(str::to_owned),
                }
            }
            (403, _) | (_, Some("AccessDenied" | "Forbidden")) => Error::Auth(diagnostic.into()),
            _ if self.delete_marker => Error::DeleteMarker {
                path: path.to_string(),
                version_id: self
                    .version_id
                    .or(requested_version)
                    .unwrap_or("unknown")
                    .into(),
            },
            (_, Some("NoSuchVersion")) => Error::VersionNotFound {
                path: path.to_string(),
                version_id: requested_version.unwrap_or("unknown").into(),
            },
            _ if missing => match requested_version {
                Some(version) => Error::VersionNotFound {
                    path: path.to_string(),
                    version_id: version.into(),
                },
                None => Error::NotFound(path.to_string()),
            },
            _ => return None,
        })
    }
}

impl ControlResponse {
    /// Classify an unsuccessful S3 control response using the caller's version selector.
    pub fn object_error(&self, path: &RemotePath, requested_version: Option<&str>) -> Error {
        let fields = ObjectErrorFields::from_headers(self.status.as_u16(), |name| {
            self.headers.get(name)?.to_str().ok()
        });
        let diagnostic = format!("S3 returned HTTP {} for {path}", self.status);
        fields
            .classify(path, requested_version, None, &diagnostic)
            .unwrap_or(Error::General(diagnostic))
    }
}

impl S3Client {
    pub(super) fn map_object_request_error<E>(
        error: &aws_sdk_s3::error::SdkError<E>,
        path: &RemotePath,
        requested_version: Option<&str>,
    ) -> Error
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        let formatted = Self::format_sdk_error(error);
        if let SdkError::ServiceError(service_error) = error {
            let raw = service_error.raw();
            let mut response = ObjectErrorFields::from_headers(raw.status().as_u16(), |name| {
                raw.headers().get(name)
            });
            response.code = service_error.err().code().or(response.code);
            let message = service_error.err().message().unwrap_or(&formatted);
            if let Some(error) =
                response.classify(path, requested_version, Some(message), &formatted)
            {
                return error;
            }
        }

        if Self::is_retention_denial(&formatted) {
            return Error::GovernanceDenied {
                path: path.to_string(),
                version_id: requested_version.map(ToString::to_string),
            };
        }
        if formatted.contains("AccessDenied")
            || formatted.contains("Forbidden")
            || formatted.contains("Unauthorized")
        {
            return Error::Auth(formatted);
        }
        if formatted.contains("NoSuchVersion")
            || (requested_version.is_some()
                && (formatted.contains("NoSuchKey") || formatted.contains("NotFound")))
        {
            return Error::VersionNotFound {
                path: path.to_string(),
                version_id: requested_version.unwrap_or("unknown").to_string(),
            };
        }
        if formatted.contains("NoSuchKey") || formatted.contains("NotFound") {
            return Error::NotFound(path.to_string());
        }
        Error::Network(formatted)
    }
}

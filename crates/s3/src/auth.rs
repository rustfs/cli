use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use rc_core::Error;
use serde::Deserialize;

pub(crate) fn temporary_credential_error(code: Option<&str>) -> Option<Error> {
    match code {
        Some("ExpiredToken") => Some(Error::Auth(
            "Temporary credentials have expired; renew the credentials and session token, then retry".into(),
        )),
        Some("InvalidToken") => Some(Error::Auth(
            "The session token is invalid; renew the credentials and session token, then retry".into(),
        )),
        _ => None,
    }
}

pub(crate) fn sdk_temporary_credential_error<E: ProvideErrorMetadata>(
    error: &SdkError<E>,
) -> Option<Error> {
    let SdkError::ServiceError(service) = error else {
        return None;
    };
    let code = service
        .err()
        .code()
        .or_else(|| service.raw().headers().get("x-amz-error-code"));
    temporary_credential_error(code)
}

pub(crate) fn temporary_credential_body_error(body: &str) -> Option<Error> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "Code", alias = "code")]
        code: String,
    }
    let response = serde_json::from_str::<Response>(body)
        .ok()
        .or_else(|| quick_xml::de::from_str::<Response>(body).ok())?;
    temporary_credential_error(Some(&response.code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_errors_use_exact_codes_and_safe_renewal_messages() {
        for body in [
            r#"{"Code":"ExpiredToken","Message":"secret-token"}"#,
            "<Error><Code>InvalidToken</Code><Message>secret-token</Message></Error>",
        ] {
            let error = temporary_credential_body_error(body).unwrap();
            assert!(matches!(error, Error::Auth(_)));
            assert!(error.to_string().contains("renew"));
            assert!(!error.to_string().contains("secret-token"));
        }
        assert!(
            temporary_credential_body_error(
                r#"{"Code":"InvalidArgument","Message":"ExpiredToken"}"#
            )
            .is_none()
        );
    }
}

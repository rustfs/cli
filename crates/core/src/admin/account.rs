//! Self-service account and two-factor authentication operations.
//!
//! These act on whoever the alias authenticates as: none of them take a target
//! identity, so `rc` cannot use them to touch another account. Managing someone
//! else's credentials goes through the user-management API instead.
//!
//! # Why the CLI never enforces the second factor
//!
//! `rc` signs every request with the alias's long-term access key. That path is
//! not gated by 2FA and must not be: gating it would break every script the
//! moment a human turned 2FA on for their own account, and it would add no
//! protection, because whoever holds the secret key already has full access. The
//! second factor guards session minting — the interactive console login — which
//! `rc` does not use. This is the same division AWS draws.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::Result;

/// Runtime capability required by the self-service account commands.
pub const ACCOUNT_CAPABILITY: &str = "admin.account.info";

/// Runtime capability required by the two-factor commands.
pub const ACCOUNT_MFA_CAPABILITY: &str = "admin.account.mfa";

/// Runtime capability required by the administrative MFA inspection/reset.
pub const USER_MFA_CAPABILITY: &str = "admin.user.mfa";

/// Response bound for account and MFA payloads.
///
/// Generous enough for a QR SVG (a few kilobytes) and a full recovery-code set,
/// tight enough that a misbehaving endpoint cannot stream unbounded data into
/// the CLI.
pub const MAX_ACCOUNT_RESPONSE_BYTES: usize = 256 * 1024;

/// How the calling credential was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityType {
    Root,
    Iam,
    Sts,
    ServiceAccount,
}

impl std::fmt::Display for IdentityType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Root => "root",
            Self::Iam => "iam",
            Self::Sts => "sts",
            Self::ServiceAccount => "service-account",
        })
    }
}

/// Where the identity's long-term secret lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialsSource {
    /// Provisioned from the server process environment; immutable at runtime.
    Env,
    /// Stored in the IAM object store; mutable through the admin API.
    Iam,
}

impl std::fmt::Display for CredentialsSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Env => "env",
            Self::Iam => "iam",
        })
    }
}

/// Which self-service mutations the server will accept for this identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccountMutability {
    #[serde(default)]
    pub password: bool,
    #[serde(default)]
    pub username: bool,
}

/// MFA state reported alongside the account summary.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountMfaSummary {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub pending: bool,
    #[serde(default)]
    pub activated_at: Option<String>,
    #[serde(default)]
    pub recovery_codes_remaining: u32,
    #[serde(default)]
    pub last_verified_at: Option<String>,
    #[serde(default)]
    pub enrollment_available: bool,
    #[serde(default)]
    pub enrollment_blocked_reason: Option<String>,
}

/// The identity behind the alias, as the server describes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    pub access_key: String,
    pub identity_type: IdentityType,
    #[serde(default)]
    pub session_access_key: Option<String>,
    pub is_admin: bool,
    pub status: String,
    #[serde(default)]
    pub member_of: Vec<String>,
    #[serde(default)]
    pub policies: Vec<String>,
    pub credentials_source: CredentialsSource,
    pub mutable: AccountMutability,
    pub mfa: AccountMfaSummary,
}

/// Two-factor state for the calling identity.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MfaStatus {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub pending: bool,
    pub algorithm: String,
    pub digits: u8,
    pub period_seconds: u32,
    #[serde(default)]
    pub activated_at: Option<String>,
    #[serde(default)]
    pub pending_expires_at: Option<String>,
    #[serde(default)]
    pub recovery_codes_remaining: u32,
    #[serde(default)]
    pub last_verified_at: Option<String>,
    #[serde(default)]
    pub enrollment_available: bool,
    #[serde(default)]
    pub enrollment_blocked_reason: Option<String>,
}

/// A started enrollment.
///
/// The shared secret appears here exactly once. `rc` renders it and drops it; it
/// is never written to the alias config or any other file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MfaEnrollment {
    pub secret_base32: String,
    pub otpauth_uri: String,
    /// Server-rendered SVG. Carried for parity with the console; `rc` prints the
    /// terminal form instead.
    #[serde(default)]
    pub qr_svg: String,
    /// Server-rendered Unicode block art, ready to print.
    pub qr_utf8: String,
    pub algorithm: String,
    pub digits: u8,
    pub period_seconds: u32,
    pub expires_at: String,
}

/// A freshly generated recovery-code set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryCodes {
    pub recovery_codes: Vec<String>,
    pub generated_at: String,
}

/// Sessions invalidated by a credential rotation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct PasswordChangeResult {
    #[serde(default)]
    pub sessions_revoked: u32,
}

/// Another identity's two-factor state, for an administrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMfaStatus {
    pub access_key: String,
    pub enabled: bool,
    #[serde(default)]
    pub activated_at: Option<String>,
    #[serde(default)]
    pub recovery_codes_remaining: u32,
}

/// A secret held only long enough to send, then zeroed.
///
/// Wrapped so a password or code cannot survive in a freed allocation, and so
/// `Debug` cannot print it into a log or a panic message.
#[derive(Clone)]
pub struct SecretValue(Zeroizing<String>);

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[async_trait]
pub trait AccountApi: Send + Sync {
    /// Describe the identity the alias authenticates as.
    async fn account_info(&self) -> Result<AccountInfo>;

    /// Rotate the alias identity's own secret key.
    ///
    /// The current secret is required as proof of knowledge; the server rejects
    /// the call without it even though the request is signed.
    async fn account_change_password(
        &self,
        current_secret_key: &SecretValue,
        new_secret_key: &SecretValue,
    ) -> Result<PasswordChangeResult>;
}

#[async_trait]
pub trait AccountMfaApi: Send + Sync {
    async fn account_mfa_status(&self) -> Result<MfaStatus>;

    /// Start (or restart) an enrollment. Does not change the active factor.
    async fn account_mfa_enroll(&self) -> Result<MfaEnrollment>;

    /// Confirm a pending enrollment and receive the first recovery-code set.
    async fn account_mfa_activate(&self, code: &SecretValue) -> Result<RecoveryCodes>;

    /// Turn the factor off. Requires the code and the account password.
    async fn account_mfa_disable(
        &self,
        code: &SecretValue,
        current_secret_key: &SecretValue,
    ) -> Result<()>;

    /// Replace the recovery-code set.
    async fn account_mfa_recovery_codes(&self, code: &SecretValue) -> Result<RecoveryCodes>;
}

#[async_trait]
pub trait UserCredentialApi: Send + Sync {
    /// Reset another identity's secret key, preserving its status and policies.
    async fn set_user_secret_key(
        &self,
        access_key: &str,
        secret_key: &SecretValue,
    ) -> Result<PasswordChangeResult>;

    async fn user_mfa_status(&self, access_key: &str) -> Result<UserMfaStatus>;

    /// Clear another identity's second factor: the break-glass path for a user
    /// who lost both their authenticator and their recovery codes.
    async fn user_mfa_reset(&self, access_key: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_type_renders_the_wire_value() {
        assert_eq!(IdentityType::ServiceAccount.to_string(), "service-account");
        assert_eq!(IdentityType::Root.to_string(), "root");
        assert_eq!(
            serde_json::to_string(&IdentityType::ServiceAccount).expect("serialize"),
            "\"service-account\""
        );
    }

    #[test]
    fn credentials_source_renders_the_wire_value() {
        assert_eq!(CredentialsSource::Env.to_string(), "env");
        assert_eq!(
            serde_json::to_string(&CredentialsSource::Iam).expect("serialize"),
            "\"iam\""
        );
    }

    #[test]
    fn secret_values_never_print_their_contents() {
        let secret = SecretValue::new("super-secret".to_string());

        assert_eq!(format!("{secret:?}"), "SecretValue([REDACTED])");
        assert!(!format!("{secret:?}").contains("super-secret"));
        assert_eq!(secret.expose(), "super-secret");
    }

    #[test]
    fn account_info_decodes_a_minimal_server_response() {
        // Optional fields are absent on a server that has nothing to report;
        // decoding must not require them.
        let decoded: AccountInfo = serde_json::from_str(
            r#"{
                "access_key": "sinan",
                "identity_type": "iam",
                "is_admin": true,
                "status": "enabled",
                "credentials_source": "iam",
                "mutable": {"password": true, "username": false},
                "mfa": {}
            }"#,
        )
        .expect("deserialize");

        assert_eq!(decoded.access_key, "sinan");
        assert!(decoded.mutable.password);
        assert!(!decoded.mfa.enabled);
        assert!(decoded.member_of.is_empty());
    }

    #[test]
    fn mfa_status_decodes_without_optional_timestamps() {
        let decoded: MfaStatus =
            serde_json::from_str(r#"{"algorithm":"SHA1","digits":6,"period_seconds":30}"#)
                .expect("deserialize");

        assert!(!decoded.enabled);
        assert_eq!(decoded.digits, 6);
        assert!(decoded.activated_at.is_none());
    }
}

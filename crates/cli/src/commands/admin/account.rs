//! Self-service account commands: `rc admin account …`
//!
//! `account` manages the identity the alias authenticates as; `user` (in the
//! sibling module) manages somebody else's. That split mirrors the server, where
//! `/account/*` never takes a target and the user endpoints always do, and it
//! means a command cannot accidentally act on the wrong identity.
//!
//! Every command works non-interactively. Secrets and codes come from
//! `--*-from-env` or `--*-file` so nothing sensitive lands on the command line,
//! where it would be visible in shell history and in `ps`. On a terminal, and
//! only when the output is human-readable, the same values may be prompted for
//! instead.

use std::path::PathBuf;

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::{Formatter, qr};
use crate::secret_input::{SecretSource, can_prompt, read_code_interactive};
use rc_core::admin::{
    AccountApi, AccountInfo, AccountMfaApi, CredentialsSource, IdentityType, MfaEnrollment,
    MfaStatus, RecoveryCodes, SecretValue,
};
use rc_core::{Error, Result};

const PASSWD_AFTER_HELP: &str = "\
Examples:
  rc admin account passwd local
  rc admin account passwd local --current-password-from-env OLD_PW --new-password-from-env NEW_PW
  rc admin account passwd local --current-password-file ./old.txt --new-password-file ./new.txt";

const MFA_AFTER_HELP: &str = "\
Examples:
  rc admin account mfa status local
  rc admin account mfa enroll local
  rc admin account mfa activate local --code 123456
  rc admin account mfa recovery-codes local --code-from-env RC_MFA_CODE --output-file ./codes.txt

Note: `rc` signs requests with the alias access key, which two-factor
authentication does not gate. The second factor guards interactive console
logins, so enabling it never breaks a script.";

/// Self-service account subcommands
#[derive(Subcommand, Debug)]
pub enum AccountCommands {
    /// Show the identity this alias authenticates as
    Info(InfoArgs),

    /// Change this identity's own password (S3 secret key)
    #[command(after_help = PASSWD_AFTER_HELP)]
    Passwd(PasswdArgs),

    /// Manage this identity's two-factor authentication
    #[command(subcommand)]
    Mfa(MfaCommands),
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct PasswdArgs {
    /// Alias name of the server
    pub alias: String,

    /// Read the current password from this environment variable
    #[arg(long, value_name = "NAME")]
    pub current_password_from_env: Option<String>,

    /// Read the current password from the first line of this file
    #[arg(long, value_name = "PATH")]
    pub current_password_file: Option<PathBuf>,

    /// Read the new password from this environment variable
    #[arg(long, value_name = "NAME")]
    pub new_password_from_env: Option<String>,

    /// Read the new password from the first line of this file
    #[arg(long, value_name = "PATH")]
    pub new_password_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
#[command(after_help = MFA_AFTER_HELP)]
pub enum MfaCommands {
    /// Show two-factor authentication state
    Status(MfaStatusArgs),

    /// Start enrollment and print the QR code and setup key
    Enroll(MfaEnrollArgs),

    /// Confirm a pending enrollment and print the recovery codes
    Activate(MfaCodeArgs),

    /// Turn off two-factor authentication
    Disable(MfaDisableArgs),

    /// Replace the recovery codes
    #[command(name = "recovery-codes")]
    RecoveryCodes(MfaCodeArgs),
}

#[derive(clap::Args, Debug)]
pub struct MfaStatusArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct MfaEnrollArgs {
    /// Alias name of the server
    pub alias: String,

    /// Do not render the QR code; print only the setup key and URI
    #[arg(long)]
    pub no_qr: bool,
}

#[derive(clap::Args, Debug)]
pub struct MfaCodeArgs {
    /// Alias name of the server
    pub alias: String,

    /// Verification code from the authenticator app, or a recovery code
    #[arg(long, value_name = "CODE")]
    pub code: Option<String>,

    /// Read the verification code from this environment variable
    #[arg(long, value_name = "NAME", conflicts_with = "code")]
    pub code_from_env: Option<String>,

    /// Write the recovery codes to this file instead of stdout
    #[arg(long, value_name = "PATH")]
    pub output_file: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct MfaDisableArgs {
    /// Alias name of the server
    pub alias: String,

    /// Verification code from the authenticator app, or a recovery code
    #[arg(long, value_name = "CODE")]
    pub code: Option<String>,

    /// Read the verification code from this environment variable
    #[arg(long, value_name = "NAME", conflicts_with = "code")]
    pub code_from_env: Option<String>,

    /// Read the account password from this environment variable
    #[arg(long, value_name = "NAME")]
    pub password_from_env: Option<String>,

    /// Read the account password from the first line of this file
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// JSON output shapes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AccountInfoOutput {
    access_key: String,
    identity_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_access_key: Option<String>,
    is_admin: bool,
    status: String,
    credentials_source: String,
    password_mutable: bool,
    username_mutable: bool,
    policies: Vec<String>,
    member_of: Vec<String>,
    mfa_enabled: bool,
    mfa_pending: bool,
    recovery_codes_remaining: u32,
    mfa_enrollment_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    mfa_enrollment_blocked_reason: Option<String>,
}

impl From<AccountInfo> for AccountInfoOutput {
    fn from(info: AccountInfo) -> Self {
        Self {
            access_key: info.access_key,
            identity_type: info.identity_type.to_string(),
            session_access_key: info.session_access_key,
            is_admin: info.is_admin,
            status: info.status,
            credentials_source: info.credentials_source.to_string(),
            password_mutable: info.mutable.password,
            username_mutable: info.mutable.username,
            policies: info.policies,
            member_of: info.member_of,
            mfa_enabled: info.mfa.enabled,
            mfa_pending: info.mfa.pending,
            recovery_codes_remaining: info.mfa.recovery_codes_remaining,
            mfa_enrollment_available: info.mfa.enrollment_available,
            mfa_enrollment_blocked_reason: info.mfa.enrollment_blocked_reason,
        }
    }
}

#[derive(Serialize)]
struct PasswordChangeOutput {
    success: bool,
    /// Absent when the identity could not be read.
    ///
    /// Omitted rather than filled with the alias name: `"access_key": "prod"`
    /// for an alias called `prod` reads as an access key and is not one.
    #[serde(skip_serializing_if = "Option::is_none")]
    access_key: Option<String>,
    sessions_revoked: u32,
    message: String,
}

#[derive(Serialize)]
struct MfaStatusOutput {
    enabled: bool,
    pending: bool,
    algorithm: String,
    digits: u8,
    period_seconds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    activated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_verified_at: Option<String>,
    recovery_codes_remaining: u32,
    enrollment_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    enrollment_blocked_reason: Option<String>,
}

impl From<MfaStatus> for MfaStatusOutput {
    fn from(status: MfaStatus) -> Self {
        Self {
            enabled: status.enabled,
            pending: status.pending,
            algorithm: status.algorithm,
            digits: status.digits,
            period_seconds: status.period_seconds,
            activated_at: status.activated_at,
            last_verified_at: status.last_verified_at,
            recovery_codes_remaining: status.recovery_codes_remaining,
            enrollment_available: status.enrollment_available,
            enrollment_blocked_reason: status.enrollment_blocked_reason,
        }
    }
}

/// The enrollment secret, echoed for a caller that will complete setup itself.
///
/// `qr_svg` is deliberately absent: it is several kilobytes of markup with no
/// use in a terminal pipeline, and including it would make the JSON output
/// unreadable for no gain.
#[derive(Serialize)]
struct MfaEnrollOutput {
    secret_base32: String,
    otpauth_uri: String,
    algorithm: String,
    digits: u8,
    period_seconds: u32,
    expires_at: String,
}

#[derive(Serialize)]
struct RecoveryCodesOutput {
    recovery_codes: Vec<String>,
    generated_at: String,
}

#[derive(Serialize)]
struct MfaOperationOutput {
    success: bool,
    message: String,
}

/// Execute an account subcommand
pub async fn execute(cmd: AccountCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        AccountCommands::Info(args) => execute_info(args, formatter).await,
        AccountCommands::Passwd(args) => execute_passwd(args, formatter).await,
        AccountCommands::Mfa(mfa_cmd) => execute_mfa(mfa_cmd, formatter).await,
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.account_info().await {
        Ok(info) => {
            if formatter.is_json() {
                formatter.json(&AccountInfoOutput::from(info));
            } else {
                print_account_info(&info, formatter);
            }
            ExitCode::Success
        }
        Err(error) => fail(formatter, "Failed to read account information", error),
    }
}

fn print_account_info(info: &AccountInfo, formatter: &Formatter) {
    formatter.println(&format!(
        "Username:    {}",
        formatter.style_name(&info.access_key)
    ));
    formatter.println(&format!("Identity:    {}", info.identity_type));
    formatter.println(&format!(
        "Role:        {}",
        if info.is_admin {
            "administrator"
        } else {
            "user"
        }
    ));
    formatter.println(&format!(
        "Status:      {}",
        formatter.sanitize_text(&info.status)
    ));
    formatter.println(&format!("Credentials: {}", info.credentials_source));

    if let Some(session) = &info.session_access_key {
        formatter.println(&format!(
            "Session key: {}",
            formatter.sanitize_text(session)
        ));
    }
    if !info.policies.is_empty() {
        formatter.println(&format!(
            "Policies:    {}",
            formatter.sanitize_text(&info.policies.join(", "))
        ));
    }
    if !info.member_of.is_empty() {
        formatter.println(&format!(
            "Groups:      {}",
            formatter.sanitize_text(&info.member_of.join(", "))
        ));
    }

    formatter.println(&format!(
        "2FA:         {}",
        if info.mfa.enabled { "on" } else { "off" }
    ));
    if info.mfa.enabled {
        formatter.println(&format!(
            "Recovery:    {} codes remaining",
            info.mfa.recovery_codes_remaining
        ));
    }

    // Say why a mutation is unavailable rather than letting the user discover it
    // by having the request rejected.
    if !info.mutable.password {
        formatter.println("");
        formatter.println("This identity's password cannot be changed here.");
        formatter.println(&format!("  {}", password_immutability_hint(info)));
    }
}

/// Explain an immutable password from the fields the server actually sends.
///
/// There is no per-field reason on the wire: `AccountMutability` is two bools,
/// and `enrollment_blocked_reason` belongs to enrollment. Reading that field
/// here attributed a two-factor restriction to the password, and printed
/// nothing at all in the environment-root case where it is absent — which is
/// the one case a user is most likely to hit.
fn password_immutability_hint(info: &AccountInfo) -> &'static str {
    match (info.credentials_source, info.identity_type) {
        (CredentialsSource::Env, _) => {
            "It is provisioned from the server environment (RUSTFS_ACCESS_KEY / RUSTFS_SECRET_KEY) and cannot be changed while the server is running."
        }
        (_, IdentityType::Sts) => {
            "It is a temporary session credential. Change the password of the identity it was minted from."
        }
        (_, IdentityType::ServiceAccount) => {
            "It is a service account. Change the password of its parent identity."
        }
        _ => "The server reports this identity's secret as read-only.",
    }
}

async fn execute_passwd(args: PasswdArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let interactive = can_prompt(formatter.is_json());
    let current_source = match SecretSource::resolve(
        args.current_password_from_env,
        args.current_password_file,
        interactive,
        "current-password",
    ) {
        Ok(source) => source,
        Err(error) => return usage_failure(formatter, error),
    };
    let new_source = match SecretSource::resolve(
        args.new_password_from_env,
        args.new_password_file,
        interactive,
        "new-password",
    ) {
        Ok(source) => source,
        Err(error) => return usage_failure(formatter, error),
    };

    let current = match current_source.load("Current password: ") {
        Ok(value) => SecretValue::new(value.to_string()),
        Err(error) => return usage_failure(formatter, error),
    };
    let new = match new_source.load("New password: ") {
        Ok(value) => SecretValue::new(value.to_string()),
        Err(error) => return usage_failure(formatter, error),
    };

    match client.account_change_password(&current, &new).await {
        Ok(result) => {
            let message = if result.sessions_revoked > 0 {
                format!(
                    "Password updated. {} session(s) were signed out.",
                    result.sessions_revoked
                )
            } else {
                "Password updated.".to_string()
            };

            if formatter.is_json() {
                // Only the JSON shape names the identity, so the human path does
                // not pay for the extra round-trip. Reporting the identity is a
                // convenience: failing to read it leaves the field out rather
                // than blocking a rotation that already succeeded.
                let access_key = client.account_info().await.ok().map(|info| info.access_key);
                formatter.json(&PasswordChangeOutput {
                    success: true,
                    access_key,
                    sessions_revoked: result.sessions_revoked,
                    message,
                });
            } else {
                formatter.println(&message);
                formatter.println(
                    "Update the alias with `rc alias set` so future commands use the new key.",
                );
            }
            ExitCode::Success
        }
        Err(error) => fail(formatter, "Failed to change the password", error),
    }
}

async fn execute_mfa(cmd: MfaCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        MfaCommands::Status(args) => execute_mfa_status(args, formatter).await,
        MfaCommands::Enroll(args) => execute_mfa_enroll(args, formatter).await,
        MfaCommands::Activate(args) => execute_mfa_activate(args, formatter).await,
        MfaCommands::Disable(args) => execute_mfa_disable(args, formatter).await,
        MfaCommands::RecoveryCodes(args) => execute_mfa_recovery_codes(args, formatter).await,
    }
}

async fn execute_mfa_status(args: MfaStatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.account_mfa_status().await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&MfaStatusOutput::from(status));
            } else {
                formatter.println(&format!(
                    "Two-factor authentication: {}",
                    if status.enabled { "on" } else { "off" }
                ));
                if status.enabled {
                    formatter.println(&format!(
                        "  Algorithm: {} / {} digits / {}s period",
                        formatter.sanitize_text(&status.algorithm),
                        status.digits,
                        status.period_seconds
                    ));
                    if let Some(activated) = &status.activated_at {
                        formatter.println(&format!(
                            "  Enabled on: {}",
                            formatter.sanitize_text(activated)
                        ));
                    }
                    if let Some(last) = &status.last_verified_at {
                        formatter
                            .println(&format!("  Last used:  {}", formatter.sanitize_text(last)));
                    }
                    formatter.println(&format!(
                        "  Recovery:   {} code(s) remaining",
                        status.recovery_codes_remaining
                    ));
                    if status.recovery_codes_remaining == 0 {
                        formatter.println(
                            "  No recovery codes left. Run `rc admin account mfa recovery-codes` to generate a new set.",
                        );
                    }
                }
                if status.pending {
                    formatter.println("  A pending enrollment is waiting for `mfa activate`.");
                }
                if !status.enrollment_available
                    && let Some(reason) = &status.enrollment_blocked_reason
                {
                    formatter.println(&format!(
                        "  Enrollment unavailable: {}",
                        formatter.sanitize_text(reason)
                    ));
                }
            }
            ExitCode::Success
        }
        Err(error) => fail(formatter, "Failed to read two-factor state", error),
    }
}

async fn execute_mfa_enroll(args: MfaEnrollArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.account_mfa_enroll().await {
        Ok(enrollment) => {
            if formatter.is_json() {
                // Moved, not cloned: the branches are exclusive, and the
                // secret should not get a second copy in memory just to be
                // serialized.
                formatter.json(&MfaEnrollOutput {
                    secret_base32: enrollment.secret_base32,
                    otpauth_uri: enrollment.otpauth_uri,
                    algorithm: enrollment.algorithm,
                    digits: enrollment.digits,
                    period_seconds: enrollment.period_seconds,
                    expires_at: enrollment.expires_at,
                });
            } else {
                print_enrollment(&enrollment, args.no_qr, formatter);
            }
            ExitCode::Success
        }
        Err(error) => fail(formatter, "Failed to start two-factor enrollment", error),
    }
}

fn print_enrollment(enrollment: &MfaEnrollment, no_qr: bool, formatter: &Formatter) {
    formatter.println("Two-factor enrollment started.");

    // Only claim there is a code to scan once one has been printed. `--no-qr`, a
    // terminal too narrow for the symbol, and an empty payload all skip it, and
    // telling somebody to scan a code that is not on their screen sends them
    // looking for a rendering bug.
    if qr::print_qr(formatter, &enrollment.qr_utf8, no_qr) {
        formatter
            .println("Scan the code above with your authenticator app, or add the key by hand:");
    } else {
        formatter.println("Add this account to your authenticator app by hand:");
    }

    formatter.println(&format!(
        "Manual setup key: {}",
        group_secret(&formatter.sanitize_text(&enrollment.secret_base32))
    ));
    formatter.println(&format!(
        "Setup URI:        {}",
        formatter.sanitize_text(&enrollment.otpauth_uri)
    ));
    formatter.println(&format!(
        "Parameters:       {} / {} digits / {}s period",
        formatter.sanitize_text(&enrollment.algorithm),
        enrollment.digits,
        enrollment.period_seconds
    ));
    formatter.println(&format!(
        "Expires:          {}",
        formatter.sanitize_text(&enrollment.expires_at)
    ));
    formatter.println("");
    formatter.println("Then confirm with:");
    formatter.println("  rc admin account mfa activate <alias> --code <6-digit code>");
}

/// Group a base32 secret in fours so a human can transcribe it without losing
/// their place. Authenticator apps ignore the spaces.
fn group_secret(secret: &str) -> String {
    secret
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

async fn execute_mfa_activate(args: MfaCodeArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    if let Err(exit) = ensure_output_path_is_free(args.output_file.as_deref(), formatter) {
        return exit;
    }

    let code = match resolve_code(args.code, args.code_from_env, formatter) {
        Ok(code) => code,
        Err(exit) => return exit,
    };

    match client.account_mfa_activate(&code).await {
        Ok(codes) => emit_recovery_codes(codes, args.output_file, formatter, true),
        Err(error) => fail(formatter, "Failed to confirm the enrollment", error),
    }
}

async fn execute_mfa_recovery_codes(args: MfaCodeArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    if let Err(exit) = ensure_output_path_is_free(args.output_file.as_deref(), formatter) {
        return exit;
    }

    let code = match resolve_code(args.code, args.code_from_env, formatter) {
        Ok(code) => code,
        Err(exit) => return exit,
    };

    match client.account_mfa_recovery_codes(&code).await {
        Ok(codes) => emit_recovery_codes(codes, args.output_file, formatter, false),
        Err(error) => fail(formatter, "Failed to generate recovery codes", error),
    }
}

async fn execute_mfa_disable(args: MfaDisableArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    // Validate every flag before resolving the code. Resolving may prompt, and a
    // TOTP code is single-use against a 30-second window: burning one only to
    // then report that `--password-from-env` and `--password-file` conflict
    // costs the user a wait they did not need.
    let interactive = can_prompt(formatter.is_json());
    let password_source = match SecretSource::resolve(
        args.password_from_env,
        args.password_file,
        interactive,
        "password",
    ) {
        Ok(source) => source,
        Err(error) => return usage_failure(formatter, error),
    };

    let code = match resolve_code(args.code, args.code_from_env, formatter) {
        Ok(code) => code,
        Err(exit) => return exit,
    };

    let password = match password_source.load("Account password: ") {
        Ok(value) => SecretValue::new(value.to_string()),
        Err(error) => return usage_failure(formatter, error),
    };

    match client.account_mfa_disable(&code, &password).await {
        Ok(()) => {
            let message = "Two-factor authentication is off.".to_string();
            if formatter.is_json() {
                formatter.json(&MfaOperationOutput {
                    success: true,
                    message,
                });
            } else {
                formatter.println(&message);
            }
            ExitCode::Success
        }
        Err(error) => fail(
            formatter,
            "Failed to turn off two-factor authentication",
            error,
        ),
    }
}

/// Resolve a verification code from a flag, an environment variable, or a prompt.
fn resolve_code(
    inline: Option<String>,
    from_env: Option<String>,
    formatter: &Formatter,
) -> std::result::Result<SecretValue, ExitCode> {
    if let Some(code) = inline {
        let code = code.trim().to_string();
        if code.is_empty() {
            return Err(usage_failure(
                formatter,
                Error::InvalidPath("The verification code is empty".to_string()),
            ));
        }
        return Ok(SecretValue::new(code));
    }

    if let Some(name) = from_env {
        return match SecretSource::resolve(Some(name), None, false, "code")
            .and_then(|source| source.load(""))
        {
            Ok(value) => Ok(SecretValue::new(value.to_string())),
            Err(error) => Err(usage_failure(formatter, error)),
        };
    }

    if !can_prompt(formatter.is_json()) {
        return Err(usage_failure(
            formatter,
            Error::InvalidPath(
                "Provide the verification code with --code or --code-from-env when running non-interactively or with --json"
                    .to_string(),
            ),
        ));
    }

    match read_code_interactive("Verification code: ") {
        Ok(value) => Ok(SecretValue::new(value.to_string())),
        Err(error) => Err(usage_failure(formatter, error)),
    }
}

/// Reject an occupied output path *before* the server issues a set.
///
/// `write_recovery_codes` will not clobber an existing file, and by the time it
/// runs the server has already activated or rotated: the set it refuses to
/// write is the only copy that will ever exist, and the previous set is already
/// invalid. Checking here costs a syscall; checking there costs the codes.
fn ensure_output_path_is_free(
    output_file: Option<&std::path::Path>,
    formatter: &Formatter,
) -> std::result::Result<(), ExitCode> {
    let Some(path) = output_file else {
        return Ok(());
    };
    // `symlink_metadata`, not `exists`: a dangling symlink reports absent but
    // still makes the `create_new` open fail with `AlreadyExists`.
    if std::fs::symlink_metadata(path).is_ok() {
        // Through `fail`, not `usage_failure`, so this reports the same exit
        // code as `write_recovery_codes` refusing the very same path later.
        return Err(fail(
            formatter,
            "Cannot write the recovery codes",
            Error::Conflict(format!(
                "{} already exists; choose another path",
                path.display()
            )),
        ));
    }
    Ok(())
}

/// Print or write a recovery-code set.
///
/// These exist in plaintext exactly once. The file path refuses to clobber an
/// existing file, and if the write fails anyway the set is printed instead of
/// being dropped: a file the operator has to re-create is a nuisance, a set
/// nobody ever saw is a locked-out account.
fn emit_recovery_codes(
    codes: RecoveryCodes,
    output_file: Option<PathBuf>,
    formatter: &Formatter,
    activated: bool,
) -> ExitCode {
    let Some(path) = output_file else {
        return print_recovery_codes(&codes, formatter, activated);
    };

    match write_recovery_codes(&path, &codes.recovery_codes) {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&MfaOperationOutput {
                    success: true,
                    message: format!("Recovery codes written to {}", path.display()),
                });
            } else {
                formatter.println(&format!(
                    "{} recovery code(s) written to {} (mode 0600).",
                    codes.recovery_codes.len(),
                    path.display()
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            // The server has already issued this set and invalidated any
            // previous one, so it exists nowhere but in this process. Print it
            // before reporting the failure. In `--json` mode the codes go to
            // stdout and the error to stderr, so a script gets both and the
            // exit code still says the file was not written.
            print_recovery_codes(&codes, formatter, activated);
            fail(
                formatter,
                &format!(
                    "The codes above were NOT written to {}; store them now",
                    path.display()
                ),
                error,
            )
        }
    }
}

fn print_recovery_codes(codes: &RecoveryCodes, formatter: &Formatter, activated: bool) -> ExitCode {
    if formatter.is_json() {
        // Not sanitized: `serde_json` escapes control characters correctly, and
        // a consumer needs the value the server sent. Escaping is a terminal
        // concern, so it belongs on the human path below and nowhere else.
        formatter.json(&RecoveryCodesOutput {
            recovery_codes: codes.recovery_codes.clone(),
            generated_at: codes.generated_at.clone(),
        });
        return ExitCode::Success;
    }

    if activated {
        formatter.println("Two-factor authentication is now on.");
    } else {
        formatter.println("Previous recovery codes no longer work.");
    }
    formatter.println("");
    formatter.println("Save these recovery codes. They are shown only once:");
    for (index, code) in codes.recovery_codes.iter().enumerate() {
        formatter.println(&format!(
            "  {:2}. {}",
            index + 1,
            formatter.sanitize_text(code)
        ));
    }
    formatter.println("");
    formatter.println("Each code works once. Store them somewhere only you can reach.");
    ExitCode::Success
}

/// Write recovery codes to a new file with owner-only permissions.
fn write_recovery_codes(path: &std::path::Path, codes: &[String]) -> Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    // `create_new` so an existing file is never silently overwritten: it may
    // hold the only copy of a previous set.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Conflict(format!(
                "{} already exists; choose another path",
                path.display()
            ))
        } else {
            Error::Io(error)
        }
    })?;

    for code in codes {
        writeln!(file, "{code}").map_err(Error::Io)?;
    }
    file.flush().map_err(Error::Io)?;
    Ok(())
}

/// Report a failed operation and return its exit code.
///
/// `Formatter::fail` rather than `error`: the latter builds the descriptor from
/// a bare message, so the `--json` error envelope carried `code: null` while
/// every other command reported the real one.
fn fail(formatter: &Formatter, context: &str, error: Error) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    formatter.fail(code, &format!("{context}: {error}"))
}

fn usage_failure(formatter: &Formatter, error: Error) -> ExitCode {
    formatter.fail(ExitCode::UsageError, &error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: AccountCommands,
    }

    #[test]
    fn info_parses_with_only_an_alias() {
        let cli = TestCli::parse_from(["account", "info", "local"]);
        match cli.command {
            AccountCommands::Info(args) => assert_eq!(args.alias, "local"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn passwd_accepts_both_secrets_from_the_environment() {
        // The automation path: nothing sensitive on the command line.
        let cli = TestCli::parse_from([
            "account",
            "passwd",
            "local",
            "--current-password-from-env",
            "OLD",
            "--new-password-from-env",
            "NEW",
        ]);
        match cli.command {
            AccountCommands::Passwd(args) => {
                assert_eq!(args.current_password_from_env.as_deref(), Some("OLD"));
                assert_eq!(args.new_password_from_env.as_deref(), Some("NEW"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn a_code_cannot_be_given_twice() {
        // `--code` and `--code-from-env` together is ambiguous, so clap rejects
        // it rather than letting one silently win.
        let result = TestCli::try_parse_from([
            "account",
            "mfa",
            "activate",
            "local",
            "--code",
            "123456",
            "--code-from-env",
            "RC_CODE",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn enroll_accepts_suppressing_the_qr() {
        let cli = TestCli::parse_from(["account", "mfa", "enroll", "local", "--no-qr"]);
        match cli.command {
            AccountCommands::Mfa(MfaCommands::Enroll(args)) => {
                assert!(args.no_qr);
                assert_eq!(args.alias, "local");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn recovery_codes_accepts_an_output_file() {
        let cli = TestCli::parse_from([
            "account",
            "mfa",
            "recovery-codes",
            "local",
            "--code",
            "123456",
            "--output-file",
            "/tmp/codes.txt",
        ]);
        match cli.command {
            AccountCommands::Mfa(MfaCommands::RecoveryCodes(args)) => {
                assert_eq!(args.output_file, Some(PathBuf::from("/tmp/codes.txt")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn secrets_are_grouped_for_transcription() {
        assert_eq!(group_secret("JBSWY3DPEHPK3PXP"), "JBSW Y3DP EHPK 3PXP");
        assert_eq!(group_secret(""), "");
    }

    #[test]
    fn writing_recovery_codes_refuses_to_clobber_an_existing_file() {
        // That file may hold the only copy of a previous set.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("codes.txt");

        write_recovery_codes(&path, &["AAAA-BBBB".to_string()]).expect("first write");
        let error = write_recovery_codes(&path, &["CCCC-DDDD".to_string()])
            .expect_err("second write must fail");

        assert!(matches!(error, Error::Conflict(_)), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "AAAA-BBBB\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_code_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("codes.txt");
        write_recovery_codes(&path, &["AAAA-BBBB".to_string()]).expect("write");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "unexpected mode {:o}", mode & 0o777);
    }
}

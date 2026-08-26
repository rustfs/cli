use rc_core::{Error, Result, SseCustomerKey};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SecretLocator {
    File(PathBuf),
    Environment(String),
}

impl fmt::Debug for SecretLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(_) => formatter.write_str("SecretLocator::File([REDACTED])"),
            Self::Environment(_) => formatter.write_str("SecretLocator::Environment([REDACTED])"),
        }
    }
}

pub(crate) fn resolve_secret_locator(
    file: Option<PathBuf>,
    environment: Option<String>,
) -> Result<Option<SecretLocator>> {
    let locator = match (file, environment) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(Error::InvalidPath(
                "Select either an SSE-C key file or a named environment variable, not both"
                    .to_string(),
            ));
        }
        (Some(path), None) => SecretLocator::File(path),
        (None, Some(name)) => SecretLocator::Environment(name),
    };
    locator.validate()?;
    Ok(Some(locator))
}

impl SecretLocator {
    /// Validate a locator without opening a file or reading an environment value.
    ///
    /// Dry-run paths use this structural validation and must not load key material.
    fn validate(&self) -> Result<()> {
        match self {
            Self::File(path) if path.as_os_str().is_empty() => Err(Error::InvalidPath(
                "SSE-C key file path cannot be empty".to_string(),
            )),
            Self::Environment(name) if !valid_environment_name(name) => Err(Error::InvalidPath(
                "SSE-C key environment variable name is invalid".to_string(),
            )),
            _ => Ok(()),
        }
    }

    pub(crate) fn load_customer_key(&self) -> Result<SseCustomerKey> {
        self.validate()?;
        let bytes = match self {
            Self::File(path) => read_protected_file(path, &SSE_C_KEY_FILE)?,
            Self::Environment(name) => read_environment_key(name)?,
        };
        if bytes.len() != 32 {
            return Err(Error::InvalidPath(
                "SSE-C keys must contain exactly 32 bytes".to_string(),
            ));
        }
        SseCustomerKey::new(bytes.as_slice().to_vec())
    }
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// How a file holding secret material is read.
///
/// An SSE-C key and an account password are different shapes of secret, and one
/// reader serving both silently truncated the longer one. Everything that
/// differs between them lives here, so a caller cannot inherit a length bound,
/// a hardening rule, or an error message meant for the other.
struct ProtectedFileSpec {
    /// Most bytes to read from the file.
    read_limit: usize,
    /// Report a file longer than `read_limit` instead of returning the prefix.
    ///
    /// Off for the SSE-C key, whose caller checks for exactly 32 bytes and has
    /// its own wording for a file that is not: reading 33 and letting that check
    /// speak keeps its message unchanged. On wherever nothing downstream
    /// verifies the length, which is where a silent prefix becomes a password
    /// the operator does not know.
    reject_oversize: bool,
    /// Noun used in error messages, so a password failure never mentions SSE-C.
    subject: &'static str,
    /// Require a regular file, not a symlink, with no group or other permission.
    ///
    /// On for an SSE-C key: long-lived encryption material, placed by the
    /// operator, where a symlink or a readable mode is worth refusing. Off for
    /// an account password, which is routinely a Kubernetes secret mount —
    /// those are symlinks into `..data/` and are group-readable inside the
    /// container by default, so the strict rule rejects an ordinary deployment.
    owner_only_regular_file: bool,
}

/// A 32-byte SSE-C customer key.
///
/// Reads 33 so `load_customer_key` can distinguish "exactly 32" from "more than
/// that" and report it in the words it always has.
const SSE_C_KEY_FILE: ProtectedFileSpec = ProtectedFileSpec {
    read_limit: 33,
    reject_oversize: false,
    subject: "SSE-C key file",
    owner_only_regular_file: true,
};

/// A password or secret key. S3 sets no maximum secret-key length, so this is a
/// sanity bound on a file meant to hold a single line, not a protocol limit.
const ACCOUNT_SECRET_FILE: ProtectedFileSpec = ProtectedFileSpec {
    read_limit: 4096,
    reject_oversize: true,
    subject: "secret file",
    owner_only_regular_file: false,
};

fn read_protected_file(path: &Path, spec: &ProtectedFileSpec) -> Result<Zeroizing<Vec<u8>>> {
    let subject = spec.subject;

    // `symlink_metadata` when a symlink is a hard error, plain `metadata` when
    // one is allowed: either way the identity check below compares against the
    // file that was actually opened.
    let path_metadata = if spec.owner_only_regular_file {
        std::fs::symlink_metadata(path)
    } else {
        std::fs::metadata(path)
    }
    // No article here, unlike its siblings: `tests/sse_customer.rs` pins this
    // exact string, and rewording another command's error is not this change's
    // business.
    .map_err(|_| Error::InvalidPath(format!("Failed to inspect {subject}")))?;
    if !path_metadata.is_file() {
        return Err(Error::InvalidPath(if spec.owner_only_regular_file {
            format!("The {subject} must be a regular file, not a symlink")
        } else {
            format!("The {subject} must be a regular file")
        }));
    }

    let file = File::open(path)
        .map_err(|_| Error::InvalidPath(format!("Failed to open the {subject}")))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| Error::InvalidPath(format!("Failed to inspect the opened {subject}")))?;
    if !file_metadata.is_file() {
        return Err(Error::InvalidPath(format!(
            "The {subject} must remain a regular file while opening"
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(Error::InvalidPath(format!(
                "The {subject} changed while being opened"
            )));
        }
        if spec.owner_only_regular_file && file_metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(format!(
                "The {subject} cannot grant group or other permissions"
            )));
        }
    }

    read_bounded(file, spec)
}

fn read_environment_key(name: &str) -> Result<Zeroizing<Vec<u8>>> {
    let value = std::env::var(name).map_err(|_| {
        Error::InvalidPath(
            "SSE-C key environment variable is missing or is not valid Unicode".to_string(),
        )
    })?;
    Ok(Zeroizing::new(value.into_bytes()))
}

/// Read at most `read_limit`, and fail rather than truncate when there is more.
///
/// Truncating is the dangerous outcome: an operator who fed a 40-character
/// secret key to `--new-password-file` would have set a password consisting of
/// its first 33 bytes, with nothing anywhere saying so.
fn read_bounded(reader: impl Read, spec: &ProtectedFileSpec) -> Result<Zeroizing<Vec<u8>>> {
    let subject = spec.subject;
    // One past the bound when oversize is an error, so a file exactly at the
    // bound is not mistaken for one that ran over it.
    let probe = if spec.reject_oversize {
        spec.read_limit.saturating_add(1)
    } else {
        spec.read_limit
    };
    let mut bytes = Zeroizing::new(Vec::with_capacity(probe));
    reader
        .take(probe as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidPath(format!("Failed to read the {subject}")))?;
    if spec.reject_oversize && bytes.len() > spec.read_limit {
        return Err(Error::InvalidPath(format!(
            "The {subject} is larger than {} bytes",
            spec.read_limit
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::Error;
    use std::io::Write as _;

    fn write_key_file(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create key file");
        file.write_all(contents).expect("write key file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
                .expect("protect key file");
        }
        file
    }

    #[test]
    fn locator_requires_exactly_one_source_and_validates_without_reading() {
        assert!(
            resolve_secret_locator(None, None)
                .expect("absent secret is valid")
                .is_none()
        );
        assert!(matches!(
            resolve_secret_locator(
                Some(PathBuf::from("key.bin")),
                Some("RC_TEST_KEY".to_string())
            ),
            Err(Error::InvalidPath(_))
        ));
        assert!(matches!(
            resolve_secret_locator(None, Some("bad=name".to_string())),
            Err(Error::InvalidPath(_))
        ));

        let missing = PathBuf::from("dry-run-does-not-read-this-key");
        assert_eq!(
            resolve_secret_locator(Some(missing.clone()), None)
                .expect("dry-run validation must not inspect the file"),
            Some(SecretLocator::File(missing))
        );
    }

    #[test]
    fn file_loader_requires_exactly_32_bytes_without_leaking_contents() {
        for contents in [&b"short secret"[..], &[b'x'; 33][..]] {
            let file = write_key_file(contents);
            let locator = SecretLocator::File(file.path().to_path_buf());
            let error = locator
                .load_customer_key()
                .expect_err("wrong-length key must fail");

            assert!(matches!(error, Error::InvalidPath(_)));
            assert!(!error.to_string().contains("short secret"));
            assert!(!error.to_string().contains(&"x".repeat(33)));
        }

        let raw = b"0123456789abcdef0123456789abcdef";
        let file = write_key_file(raw);
        let key = SecretLocator::File(file.path().to_path_buf())
            .load_customer_key()
            .expect("32-byte key");
        assert_eq!(key.expose_secret(), raw);
    }

    #[test]
    fn missing_environment_value_is_redacted() {
        let name = "RC_SSE_C_TEST_VALUE_THAT_MUST_NOT_EXIST";
        let locator = resolve_secret_locator(None, Some(name.to_string()))
            .expect("valid environment locator")
            .expect("locator exists");
        let error = locator
            .load_customer_key()
            .expect_err("missing environment value");

        assert!(matches!(error, Error::InvalidPath(_)));
        assert!(!error.to_string().contains(name));
        assert!(!error.to_string().contains("customer-key-material"));
    }

    #[test]
    fn locator_debug_never_contains_loaded_secret() {
        let raw = b"0123456789abcdef0123456789abcdef";
        let file = write_key_file(raw);
        let locator = SecretLocator::File(file.path().to_path_buf());
        let debug = format!("{locator:?}");

        assert!(!debug.contains("0123456789abcdef"));
        assert!(debug.contains("File"));

        let environment =
            SecretLocator::Environment("CUSTOMER_KEY_MATERIAL_THAT_LOOKS_SECRET".to_string());
        let debug = format!("{environment:?}");
        assert!(!debug.contains("CUSTOMER_KEY_MATERIAL_THAT_LOOKS_SECRET"));
        assert!(debug.contains("Environment"));
    }

    #[cfg(unix)]
    #[test]
    fn file_loader_rejects_symlinks_and_group_or_other_permissions() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let file = write_key_file(b"0123456789abcdef0123456789abcdef");
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o640))
            .expect("make key file too broad");
        let permission_error = SecretLocator::File(file.path().to_path_buf())
            .load_customer_key()
            .expect_err("broad permissions must fail");
        assert!(matches!(permission_error, Error::InvalidPath(_)));

        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
            .expect("protect key file");
        let link_dir = tempfile::TempDir::new().expect("create symlink directory");
        let link = link_dir.path().join("key-link");
        symlink(file.path(), &link).expect("create key symlink");
        let symlink_error = SecretLocator::File(link)
            .load_customer_key()
            .expect_err("symlink must fail");
        assert!(matches!(symlink_error, Error::InvalidPath(_)));
    }
}

// ---------------------------------------------------------------------------
// Account credentials and second-factor codes
// ---------------------------------------------------------------------------

/// Where a password or verification code comes from.
///
/// Every source except an interactive prompt is explicit, because a command that
/// silently prompts is a command that hangs in CI. `--*-from-env` and `--*-file`
/// exist so automation never has to put a secret on the command line, where it
/// would land in the shell history and in `ps` output.
#[derive(Debug, Clone)]
pub(crate) enum SecretSource {
    /// Read from a named environment variable.
    Environment(String),
    /// Read from the first line of a file.
    File(PathBuf),
    /// Prompt on the terminal, with echo off.
    Prompt,
}

impl SecretSource {
    /// Pick a source from the mutually exclusive flags.
    ///
    /// Interactive prompting is only offered when there is a terminal to prompt
    /// on and the output is human-readable; otherwise the caller is told which
    /// flag to pass instead of being left to hang.
    pub(crate) fn resolve(
        from_env: Option<String>,
        from_file: Option<PathBuf>,
        interactive_allowed: bool,
        what: &str,
    ) -> Result<Self> {
        match (from_env, from_file) {
            (Some(_), Some(_)) => Err(Error::InvalidPath(format!(
                "Select either an environment variable or a file for the {what}, not both"
            ))),
            (Some(name), None) => {
                if !valid_environment_name(&name) {
                    return Err(Error::InvalidPath(format!(
                        "Environment variable name for the {what} is invalid"
                    )));
                }
                Ok(Self::Environment(name))
            }
            (None, Some(path)) => {
                if path.as_os_str().is_empty() {
                    return Err(Error::InvalidPath(format!(
                        "File path for the {what} cannot be empty"
                    )));
                }
                Ok(Self::File(path))
            }
            (None, None) => {
                if interactive_allowed {
                    Ok(Self::Prompt)
                } else {
                    Err(Error::InvalidPath(format!(
                        "Provide the {what} with --{what}-from-env or --{what}-file when running non-interactively or with --json"
                    )))
                }
            }
        }
    }

    /// Load the value, prompting with `prompt` when this is [`Self::Prompt`].
    pub(crate) fn load(&self, prompt: &str) -> Result<Zeroizing<String>> {
        match self {
            Self::Environment(name) => {
                let value = std::env::var(name).map_err(|_| {
                    Error::InvalidPath(format!("Environment variable '{name}' is not set"))
                })?;
                let value = Zeroizing::new(value.trim_end_matches(['\r', '\n']).to_string());
                if value.is_empty() {
                    return Err(Error::InvalidPath(format!(
                        "Environment variable '{name}' is empty"
                    )));
                }
                Ok(value)
            }
            Self::File(path) => {
                let bytes = read_protected_file(path, &ACCOUNT_SECRET_FILE)?;
                // Borrow the zeroizing buffer rather than copying out of it:
                // `String::from_utf8(bytes.to_vec())` would leave two further
                // copies of the secret in memory with nothing to wipe them.
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| Error::InvalidPath("The secret file must be UTF-8".to_string()))?;
                // First line only: an editor-written file usually has a trailing
                // newline, and a stray second line is more likely a mistake than
                // part of the secret.
                let value = Zeroizing::new(
                    text.lines()
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches(['\r', ' ', '\t'])
                        .to_string(),
                );
                if value.is_empty() {
                    return Err(Error::InvalidPath("The secret file is empty".to_string()));
                }
                Ok(value)
            }
            Self::Prompt => {
                let term = console::Term::stderr();
                // Prompt on stderr so stdout stays clean for piping.
                term.write_str(prompt).map_err(Error::Io)?;
                let value = term.read_secure_line().map_err(Error::Io)?;
                let value = Zeroizing::new(value);
                if value.is_empty() {
                    return Err(Error::Interrupted("No value was entered".to_string()));
                }
                Ok(value)
            }
        }
    }
}

/// Read a verification code, which is not secret enough to hide but is still
/// kept out of the command line by default.
pub(crate) fn read_code_interactive(prompt: &str) -> Result<Zeroizing<String>> {
    let term = console::Term::stderr();
    term.write_str(prompt).map_err(Error::Io)?;
    // Echoed, unlike a password: a TOTP code is short-lived, and hiding it only
    // makes transcription errors harder to spot.
    let value = term.read_line().map_err(Error::Io)?;
    let value = Zeroizing::new(value.trim().to_string());
    if value.is_empty() {
        return Err(Error::Interrupted("No code was entered".to_string()));
    }
    Ok(value)
}

/// Whether prompting is possible and appropriate.
pub(crate) fn can_prompt(is_json: bool) -> bool {
    use std::io::IsTerminal;
    !is_json && std::io::stdin().is_terminal()
}

#[cfg(test)]
mod account_secret_tests {
    use super::*;
    use std::io::Write as _;

    fn write_secret_file(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create secret file");
        file.write_all(contents).expect("write secret file");
        file
    }

    #[test]
    fn a_secret_longer_than_the_sse_c_bound_is_read_whole() {
        // The regression this guards: the reader was shared with SSE-C and
        // stopped at 33 bytes, so a 40-character secret key silently became a
        // password made of its first 33 bytes.
        let secret = "A".repeat(40);
        let file = write_secret_file(secret.as_bytes());
        let loaded = SecretSource::File(file.path().to_path_buf())
            .load("")
            .expect("a 40-byte secret must load");

        assert_eq!(loaded.as_str(), secret);
    }

    #[test]
    fn an_oversized_secret_file_is_rejected_rather_than_truncated() {
        let file = write_secret_file(&vec![b'A'; ACCOUNT_SECRET_FILE.read_limit + 1]);
        let error = SecretSource::File(file.path().to_path_buf())
            .load("")
            .expect_err("an oversized file must fail");

        assert!(matches!(error, Error::InvalidPath(_)), "{error:?}");
        assert!(error.to_string().contains("larger than"), "{error}");
        // Never the contents, however long.
        assert!(!error.to_string().contains("AAAA"), "{error}");
    }

    #[test]
    fn secret_file_errors_never_mention_sse_c() {
        // Somebody changing a password should not be told about an SSE-C key.
        let missing = SecretSource::File(PathBuf::from("no-such-secret-file"))
            .load("")
            .expect_err("a missing file must fail");
        assert!(!missing.to_string().contains("SSE-C"), "{missing}");

        let empty = write_secret_file(b"");
        let error = SecretSource::File(empty.path().to_path_buf())
            .load("")
            .expect_err("an empty file must fail");
        assert!(!error.to_string().contains("SSE-C"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_group_readable_secret_file_loads() {
        // The shape Kubernetes mounts a secret in: a symlink into `..data/`,
        // group-readable inside the container. The SSE-C rules reject both.
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let file = write_secret_file(b"projected-secret\n");
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644))
            .expect("relax permissions the way a projected volume does");

        let link_dir = tempfile::TempDir::new().expect("create link directory");
        let link = link_dir.path().join("password");
        symlink(file.path(), &link).expect("create the secret symlink");

        let loaded = SecretSource::File(link)
            .load("")
            .expect("a projected secret must load");
        assert_eq!(loaded.as_str(), "projected-secret");
    }

    #[test]
    fn only_the_first_line_of_a_secret_file_is_used() {
        let file = write_secret_file(b"the-secret\nnot-part-of-it\n");
        let loaded = SecretSource::File(file.path().to_path_buf())
            .load("")
            .expect("load first line");
        assert_eq!(loaded.as_str(), "the-secret");
    }
}

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
            Self::File(path) => read_protected_key_file(path)?,
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

fn read_protected_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Error::InvalidPath("Failed to inspect SSE-C key file".to_string()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(Error::InvalidPath(
            "SSE-C key input must be a regular file, not a symlink".to_string(),
        ));
    }

    let file = File::open(path)
        .map_err(|_| Error::InvalidPath("Failed to open SSE-C key file".to_string()))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| Error::InvalidPath("Failed to inspect opened SSE-C key file".to_string()))?;
    if !file_metadata.is_file() {
        return Err(Error::InvalidPath(
            "SSE-C key input must remain a regular file while opening".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(Error::InvalidPath(
                "SSE-C key file changed while being opened".to_string(),
            ));
        }
        if file_metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(
                "SSE-C key file cannot grant group or other permissions".to_string(),
            ));
        }
    }

    read_exact_key(file)
}

fn read_environment_key(name: &str) -> Result<Zeroizing<Vec<u8>>> {
    let value = std::env::var(name).map_err(|_| {
        Error::InvalidPath(
            "SSE-C key environment variable is missing or is not valid Unicode".to_string(),
        )
    })?;
    Ok(Zeroizing::new(value.into_bytes()))
}

fn read_exact_key(reader: impl Read) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(33));
    reader
        .take(33)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidPath("Failed to read SSE-C key file".to_string()))?;
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

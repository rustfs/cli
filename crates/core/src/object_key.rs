//! Object-key normalization and local-name safety.
//!
//! S3 keys may contain characters that some local filesystems reject. Traversal
//! and control-character checks always apply. Windows filename rules apply only
//! when a key is being materialized onto a local filesystem that needs them.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// How a relative object key should be validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKeyPolicy {
    /// Security checks only: relative `/` paths, no traversal, no control characters.
    ///
    /// Use this for remote-to-remote work and for local destinations on Unix-like
    /// filesystems, where characters such as `:` are legal in file names.
    Logical,
    /// Also reject names that cannot be created portably on Windows filesystems.
    WindowsPortable,
}

impl ObjectKeyPolicy {
    /// Policy for writing an object key onto a local filesystem.
    ///
    /// Windows destinations always use [`Self::WindowsPortable`]. Other platforms
    /// stay on [`Self::Logical`] unless the caller requests portable names.
    pub fn for_local_destination(force_portable: bool) -> Self {
        if force_portable || cfg!(windows) {
            Self::WindowsPortable
        } else {
            Self::Logical
        }
    }

    /// Policy for remote object keys. S3 does not use Windows filename rules.
    pub const fn for_remote_destination() -> Self {
        Self::Logical
    }
}

/// Normalize a source-relative object key.
///
/// The result uses `/` separators, rejects traversal, and optionally applies
/// Windows filename portability rules.
pub fn normalize_relative_key(value: &str, policy: ObjectKeyPolicy) -> Result<String> {
    if value.starts_with(['/', '\\']) || value.contains('\\') {
        return Err(Error::InvalidPath(format!(
            "Object key must be relative and use '/' separators: {value}"
        )));
    }

    let mut normalized = Vec::new();
    for component in value.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(Error::InvalidPath(
                "Object keys must not contain traversal components".to_string(),
            ));
        }
        validate_key_component(component, policy)?;
        normalized.push(component);
    }

    if normalized.is_empty() {
        return Err(Error::InvalidPath(
            "Object key does not contain a file name".to_string(),
        ));
    }

    Ok(normalized.join("/"))
}

/// Strip `prefix` from `key` and return a relative local path.
///
/// Traversal and other unsafe components are rejected before any filesystem
/// join so a single hostile key cannot escape the destination root.
pub fn relative_local_path_from_key(
    key: &str,
    prefix: &str,
    policy: ObjectKeyPolicy,
) -> Result<PathBuf> {
    let relative = key
        .strip_prefix(prefix)
        .ok_or_else(|| Error::InvalidPath(format!("key is outside requested prefix '{prefix}'")))?
        .trim_start_matches('/');
    let normalized = normalize_relative_key(relative, policy)?;
    Ok(normalized.split('/').collect())
}

fn validate_key_component(component: &str, policy: ObjectKeyPolicy) -> Result<()> {
    if component.chars().any(char::is_control) {
        return Err(Error::InvalidPath(format!(
            "Object key component contains a control character: {component}"
        )));
    }

    if policy != ObjectKeyPolicy::WindowsPortable {
        return Ok(());
    }

    if component
        .chars()
        .any(|character| matches!(character, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        || component.ends_with(['.', ' '])
    {
        return Err(Error::InvalidPath(format!(
            "Object key component is not portable: {component}"
        )));
    }

    let stem = component.split('.').next().unwrap_or_default();
    let stem = stem.to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err(Error::InvalidPath(format!(
            "Object key uses a reserved device name: {component}"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_policy_accepts_colon_keys() {
        assert_eq!(
            normalize_relative_key(
                "fake/deadbeef/19f6abd9af4:19f6abe0e77:499628ff",
                ObjectKeyPolicy::Logical
            )
            .expect("colon key is valid on Unix"),
            "fake/deadbeef/19f6abd9af4:19f6abe0e77:499628ff"
        );
    }

    #[test]
    fn logical_policy_accepts_question_and_asterisk_in_names() {
        assert_eq!(
            normalize_relative_key("logs/what?.txt", ObjectKeyPolicy::Logical).expect("valid"),
            "logs/what?.txt"
        );
        assert_eq!(
            normalize_relative_key("logs/star*.txt", ObjectKeyPolicy::Logical).expect("valid"),
            "logs/star*.txt"
        );
    }

    #[test]
    fn windows_policy_rejects_colon_and_reserved_names() {
        for value in [
            "safe:stream",
            "nested/bad?.txt",
            "CON.txt",
            "com1.log",
            "trailing.",
            "trailing ",
        ] {
            let error =
                normalize_relative_key(value, ObjectKeyPolicy::WindowsPortable).expect_err(value);
            assert!(
                error.to_string().contains("portable")
                    || error.to_string().contains("reserved device name"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn both_policies_reject_traversal_and_control_characters() {
        for policy in [ObjectKeyPolicy::Logical, ObjectKeyPolicy::WindowsPortable] {
            for value in [
                "../secret",
                "nested/../../secret",
                "/absolute",
                "nested\\escaped",
                "nested/control\u{0007}.txt",
            ] {
                assert!(
                    normalize_relative_key(value, policy).is_err(),
                    "policy {policy:?} accepted {value}"
                );
            }
        }
    }

    #[test]
    fn local_destination_policy_is_logical_on_non_windows_by_default() {
        let policy = ObjectKeyPolicy::for_local_destination(false);
        if cfg!(windows) {
            assert_eq!(policy, ObjectKeyPolicy::WindowsPortable);
        } else {
            assert_eq!(policy, ObjectKeyPolicy::Logical);
        }
        assert_eq!(
            ObjectKeyPolicy::for_local_destination(true),
            ObjectKeyPolicy::WindowsPortable
        );
        assert_eq!(
            ObjectKeyPolicy::for_remote_destination(),
            ObjectKeyPolicy::Logical
        );
    }

    #[test]
    fn relative_local_path_preserves_nested_colon_keys() {
        let path = relative_local_path_from_key(
            "loki/fake/deadbeef/19f6abd9af4:19f6abe0e77:499628ff",
            "loki/",
            ObjectKeyPolicy::Logical,
        )
        .expect("colon key should map onto a Unix path");

        assert_eq!(
            path,
            PathBuf::from("fake")
                .join("deadbeef")
                .join("19f6abd9af4:19f6abe0e77:499628ff")
        );
    }

    #[test]
    fn relative_local_path_rejects_keys_outside_prefix() {
        let error =
            relative_local_path_from_key("other/file.txt", "loki/", ObjectKeyPolicy::Logical)
                .expect_err("outside prefix");
        assert!(error.to_string().contains("outside requested prefix"));
    }

    #[test]
    fn relative_local_path_rejects_colon_keys_when_portable() {
        assert!(
            relative_local_path_from_key(
                "loki/fake/deadbeef/19f6abd9af4:19f6abe0e77:499628ff",
                "loki/",
                ObjectKeyPolicy::WindowsPortable
            )
            .is_err()
        );
    }

    #[test]
    fn normalize_relative_key_rejects_empty_or_dot_only_keys() {
        for policy in [ObjectKeyPolicy::Logical, ObjectKeyPolicy::WindowsPortable] {
            for value in ["", ".", "./", "//"] {
                assert!(
                    normalize_relative_key(value, policy).is_err(),
                    "policy {policy:?} accepted {value:?}"
                );
            }
        }
    }

    #[test]
    fn relative_local_path_rejects_traversal_after_prefix() {
        assert!(
            relative_local_path_from_key(
                "reports/../../escaped",
                "reports/",
                ObjectKeyPolicy::Logical
            )
            .is_err()
        );
    }
}

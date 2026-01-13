//! Path parsing and resolution
//!
//! Handles parsing of remote paths in the format: alias/bucket[/key]
//! Local paths are passed through as-is.

use crate::error::{Error, Result};

/// A parsed remote path pointing to an S3 location
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePath {
    /// Alias name
    pub alias: String,
    /// Bucket name
    pub bucket: String,
    /// Object key (empty for bucket root)
    pub key: String,
    /// Whether the path ends with a slash (directory semantics)
    pub is_dir: bool,
}

impl RemotePath {
    /// Create a new RemotePath
    pub fn new(
        alias: impl Into<String>,
        bucket: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        let key = key.into();
        let is_dir = key.ends_with('/') || key.is_empty();
        Self {
            alias: alias.into(),
            bucket: bucket.into(),
            key,
            is_dir,
        }
    }

    /// Get the full path as a string (alias/bucket/key)
    pub fn to_full_path(&self) -> String {
        if self.key.is_empty() {
            format!("{}/{}", self.alias, self.bucket)
        } else {
            format!("{}/{}/{}", self.alias, self.bucket, self.key)
        }
    }

    /// Get the parent path (one level up)
    pub fn parent(&self) -> Option<Self> {
        if self.key.is_empty() {
            // At bucket level, no parent within the remote context
            None
        } else {
            let key = self.key.trim_end_matches('/');
            match key.rfind('/') {
                Some(pos) => Some(Self {
                    alias: self.alias.clone(),
                    bucket: self.bucket.clone(),
                    key: format!("{}/", &key[..pos]),
                    is_dir: true,
                }),
                None => Some(Self {
                    alias: self.alias.clone(),
                    bucket: self.bucket.clone(),
                    key: String::new(),
                    is_dir: true,
                }),
            }
        }
    }

    /// Join a child path component
    pub fn join(&self, child: &str) -> Self {
        let base = self.key.trim_end_matches('/');
        let key = if base.is_empty() {
            child.to_string()
        } else {
            format!("{base}/{child}")
        };
        let is_dir = child.ends_with('/');
        Self {
            alias: self.alias.clone(),
            bucket: self.bucket.clone(),
            key,
            is_dir,
        }
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_full_path())
    }
}

/// Parsed path that can be either local or remote
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPath {
    /// Local filesystem path
    Local(std::path::PathBuf),
    /// Remote S3 path
    Remote(RemotePath),
}

impl ParsedPath {
    /// Check if this is a remote path
    pub fn is_remote(&self) -> bool {
        matches!(self, ParsedPath::Remote(_))
    }

    /// Check if this is a local path
    pub fn is_local(&self) -> bool {
        matches!(self, ParsedPath::Local(_))
    }

    /// Get the remote path if this is a remote path
    pub fn as_remote(&self) -> Option<&RemotePath> {
        match self {
            ParsedPath::Remote(p) => Some(p),
            ParsedPath::Local(_) => None,
        }
    }

    /// Get the local path if this is a local path
    pub fn as_local(&self) -> Option<&std::path::PathBuf> {
        match self {
            ParsedPath::Local(p) => Some(p),
            ParsedPath::Remote(_) => None,
        }
    }
}

/// Parse a path string into a ParsedPath
///
/// Remote paths have the format: alias/bucket[/key]
/// Local paths are anything that:
/// - Starts with / (absolute path)
/// - Starts with ./ or ../ (relative path)
/// - Contains no / (could be local file in current directory)
/// - Or doesn't match the alias/bucket pattern
pub fn parse_path(path: &str) -> Result<ParsedPath> {
    // Empty path is invalid
    if path.is_empty() {
        return Err(Error::InvalidPath("Path cannot be empty".into()));
    }

    // Absolute paths are local
    if path.starts_with('/') {
        return Ok(ParsedPath::Local(std::path::PathBuf::from(path)));
    }

    // Explicit relative paths are local
    if path.starts_with("./") || path.starts_with("../") {
        return Ok(ParsedPath::Local(std::path::PathBuf::from(path)));
    }

    // Windows absolute paths
    #[cfg(windows)]
    if path.len() >= 2 && path.chars().nth(1) == Some(':') {
        return Ok(ParsedPath::Local(std::path::PathBuf::from(path)));
    }

    // Try to parse as remote path
    let parts: Vec<&str> = path.splitn(3, '/').collect();

    match parts.len() {
        // Just alias name - invalid for most operations but could be valid for 'alias list'
        1 => {
            // Treat as local path if it doesn't look like an alias
            // In Phase 1, we'll validate against known aliases
            if parts[0].contains('.') || parts[0].contains('\\') {
                Ok(ParsedPath::Local(std::path::PathBuf::from(path)))
            } else {
                // Could be just an alias, return as remote path with empty bucket
                // This will be validated later against actual aliases
                Err(Error::InvalidPath(format!(
                    "Path '{path}' is incomplete. Use format: alias/bucket[/key]"
                )))
            }
        }
        // alias/bucket
        2 => {
            let alias = parts[0];
            let bucket = parts[1];

            // Validate alias name (alphanumeric, underscore, hyphen)
            if !is_valid_alias_name(alias) {
                return Ok(ParsedPath::Local(std::path::PathBuf::from(path)));
            }

            // Validate bucket name
            if bucket.is_empty() {
                return Err(Error::InvalidPath("Bucket name cannot be empty".into()));
            }

            Ok(ParsedPath::Remote(RemotePath::new(alias, bucket, "")))
        }
        // alias/bucket/key
        3 => {
            let alias = parts[0];
            let bucket = parts[1];
            let key = parts[2];

            if !is_valid_alias_name(alias) {
                return Ok(ParsedPath::Local(std::path::PathBuf::from(path)));
            }

            if bucket.is_empty() {
                return Err(Error::InvalidPath("Bucket name cannot be empty".into()));
            }

            Ok(ParsedPath::Remote(RemotePath::new(alias, bucket, key)))
        }
        _ => unreachable!(),
    }
}

/// Check if a string is a valid alias name
fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_remote_path() {
        let path = parse_path("minio/bucket/file.txt").unwrap();
        assert!(path.is_remote());

        let remote = path.as_remote().unwrap();
        assert_eq!(remote.alias, "minio");
        assert_eq!(remote.bucket, "bucket");
        assert_eq!(remote.key, "file.txt");
        assert!(!remote.is_dir);
    }

    #[test]
    fn test_parse_remote_path_dir() {
        let path = parse_path("minio/bucket/dir/").unwrap();
        let remote = path.as_remote().unwrap();
        assert_eq!(remote.key, "dir/");
        assert!(remote.is_dir);
    }

    #[test]
    fn test_parse_remote_path_bucket_only() {
        let path = parse_path("minio/bucket").unwrap();
        let remote = path.as_remote().unwrap();
        assert_eq!(remote.alias, "minio");
        assert_eq!(remote.bucket, "bucket");
        assert_eq!(remote.key, "");
        assert!(remote.is_dir);
    }

    #[test]
    fn test_parse_local_absolute_path() {
        let path = parse_path("/home/user/file.txt").unwrap();
        assert!(path.is_local());
        assert_eq!(
            path.as_local().unwrap().to_str().unwrap(),
            "/home/user/file.txt"
        );
    }

    #[test]
    fn test_parse_local_relative_path() {
        let path = parse_path("./file.txt").unwrap();
        assert!(path.is_local());

        let path = parse_path("../file.txt").unwrap();
        assert!(path.is_local());
    }

    #[test]
    fn test_parse_empty_path() {
        let result = parse_path("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_alias_only() {
        let result = parse_path("minio");
        assert!(result.is_err());
    }

    #[test]
    fn test_remote_path_parent() {
        let path = RemotePath::new("minio", "bucket", "a/b/c.txt");
        let parent = path.parent().unwrap();
        assert_eq!(parent.key, "a/b/");

        let parent = parent.parent().unwrap();
        assert_eq!(parent.key, "a/");

        let parent = parent.parent().unwrap();
        assert_eq!(parent.key, "");

        assert!(parent.parent().is_none());
    }

    #[test]
    fn test_remote_path_join() {
        let path = RemotePath::new("minio", "bucket", "");
        let child = path.join("dir/");
        assert_eq!(child.key, "dir/");
        assert!(child.is_dir);

        let file = child.join("file.txt");
        assert_eq!(file.key, "dir/file.txt");
        assert!(!file.is_dir);
    }

    #[test]
    fn test_remote_path_display() {
        let path = RemotePath::new("minio", "bucket", "key/file.txt");
        assert_eq!(path.to_string(), "minio/bucket/key/file.txt");
    }

    #[test]
    fn test_local_path_with_dots() {
        // Files like "file.txt" in current directory should be local
        let path = parse_path("some.file.txt");
        assert!(path.is_ok());
        assert!(path.unwrap().is_local());
    }
}

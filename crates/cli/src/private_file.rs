//! Writing a file that holds something only its owner should read.
//!
//! Two commands write one: `admin config export` and the recovery-code output of
//! `admin account mfa`. Both need the same three properties — created rather
//! than opened, mode `0600` from the moment it exists, and never a silent
//! overwrite — and had them written out twice.
//!
//! What the callers do *not* share is how they classify the refusal, so that
//! stays with them: an export that will not clobber a file is an ordinary
//! failure, while a recovery-code set that cannot be written is a conflict the
//! operator has to resolve before there is anywhere to put the only copy.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use rc_core::Result;

/// Create `path` with owner-only permissions and write `contents`.
///
/// `create_new`, so an existing file is never truncated: the caller cannot know
/// that what is already there is expendable. The mode is set in the open flags
/// rather than afterwards, so the file is never briefly readable by anyone else.
///
/// Returns the underlying [`std::io::Error`] as-is — including
/// [`std::io::ErrorKind::AlreadyExists`] — for the caller to classify.
pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    file.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_file_is_left_alone() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join("private.txt");
        std::fs::write(&path, "existing").expect("create existing file");

        let error =
            write_private_file(&path, b"replacement").expect_err("must not overwrite the file");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read existing file"),
            "existing"
        );
        // The kind is the caller's to interpret, so it must survive the trip.
        assert!(
            matches!(
                error,
                rc_core::Error::Io(ref io)
                    if io.kind() == std::io::ErrorKind::AlreadyExists
            ),
            "{error:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join("private.txt");
        write_private_file(&path, b"secret").expect("write the file");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "secret");
    }
}

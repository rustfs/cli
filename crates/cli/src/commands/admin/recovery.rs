//! Output and durable local receipts shared by recovery commands.

use std::io::{Read, Write};
use std::path::Path;

use rc_core::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Serialize)]
pub(super) struct Output<'a, T: Serialize> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'a str,
    status: &'a str,
    message: &'a str,
    data: &'a T,
}

pub(super) fn output<T: Serialize>(
    kind: &str,
    status: &str,
    message: &str,
    data: &T,
    formatter: &Formatter,
) {
    if formatter.is_json() {
        formatter.json(&Output {
            schema_version: 3,
            output_type: kind,
            status,
            message,
            data,
        });
    } else {
        formatter.println(message);
    }
}

pub(super) fn error(kind: &str, error: &Error, unknown: bool, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = error.to_string();
    if formatter.is_json() {
        formatter.json_error(&serde_json::json!({
            "schema_version": 3, "type": kind, "status": if unknown { "unknown" } else { "error" },
            "error": { "message": message, "exit_code": code.as_i32(), "automatic_retry": false }
        }));
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

pub(super) fn read_json<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::InvalidPath("expected a regular JSON file".into()));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::InvalidPath(
            "JSON file exceeds the command's size limit".into(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::InvalidPath("invalid recovery JSON file".into()))
}

/// Reserve staging space before creating a remote job. Never clobber an existing receipt.
pub(super) fn prepare_receipt(path: &Path) -> Result<tempfile::NamedTempFile> {
    match path.symlink_metadata() {
        Ok(_) => {
            return Err(Error::Conflict(
                "receipt already exists; select a new path".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(tempfile::NamedTempFile::new_in(parent(path))?)
}

pub(super) fn save_receipt<T: Serialize>(
    mut file: tempfile::NamedTempFile,
    path: &Path,
    value: &T,
) -> Result<()> {
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent(path))?.sync_all()?;
    Ok(())
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
pub(super) mod test_support;

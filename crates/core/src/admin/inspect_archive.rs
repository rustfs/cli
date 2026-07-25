//! Encrypted RustFS diagnostic archive contracts and local verification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use async_trait::async_trait;
use rand::rngs::OsRng;
use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

use crate::{Error, Result};

pub const INSPECT_ARCHIVE_CAPABILITY: &str = "admin.diagnostics.inspect-archive";
pub const INSPECT_ARCHIVE_ROUTE: &str = "/rustfs/admin/v4/inspect/archive";
pub const INSPECT_ARCHIVE_CONTENT_TYPE: &str = "application/vnd.rustfs.inspect-archive.v1";
pub const INSPECT_ARCHIVE_ENCRYPTION: &str = "RSA-OAEP-SHA256+AES-256-GCM-CHUNKED";
pub const INSPECT_ARCHIVE_COMPLETION: &str = "authenticated-final-record-required";
pub const INSPECT_ARCHIVE_VERSION: u16 = 1;
pub const MAX_INSPECT_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_INSPECT_ARCHIVE_DURATION: Duration = Duration::from_secs(30);
pub const MAX_INSPECT_ARCHIVE_METADATA_BYTES_PER_DRIVE: usize = 4 * 1024 * 1024;

const FORMAT_MAGIC: &[u8; 8] = b"RFSINSP1";
const ARCHIVE_CHUNK_SIZE: usize = 64 * 1024;
const RECORD_DATA: u8 = 1;
const RECORD_FINAL: u8 = 2;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Exact capability fields required before invoking the archive route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectArchiveCapabilityContract {
    pub state: super::RuntimeCapabilityStatus,
    pub route: String,
    pub archive_version: u16,
    pub content_type: String,
    pub encryption: String,
    pub completion_contract: String,
    pub max_bytes: usize,
    pub max_duration_secs: u64,
    pub max_metadata_bytes_per_drive: usize,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl InspectArchiveCapabilityContract {
    /// Validate the server advertisement against the only contract understood by this client.
    pub fn validate(&self) -> Result<()> {
        if self.state.availability() != super::CapabilityAvailability::Available {
            return Err(Error::UnsupportedFeature(
                self.state
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Diagnostic archive capability is unavailable".to_string()),
            ));
        }
        if self.route != INSPECT_ARCHIVE_ROUTE
            || self.archive_version != INSPECT_ARCHIVE_VERSION
            || self.content_type != INSPECT_ARCHIVE_CONTENT_TYPE
            || self.encryption != INSPECT_ARCHIVE_ENCRYPTION
            || self.completion_contract != INSPECT_ARCHIVE_COMPLETION
        {
            return Err(Error::UnsupportedFeature(
                "RustFS advertised an incompatible diagnostic archive contract".to_string(),
            ));
        }
        if self.max_bytes == 0 || self.max_bytes > MAX_INSPECT_ARCHIVE_BYTES {
            return Err(Error::RequestRejected(
                "RustFS advertised an invalid diagnostic archive byte limit".to_string(),
            ));
        }
        if self.max_duration_secs == 0
            || self.max_duration_secs > MAX_INSPECT_ARCHIVE_DURATION.as_secs()
        {
            return Err(Error::RequestRejected(
                "RustFS advertised an invalid diagnostic archive duration limit".to_string(),
            ));
        }
        if self.max_metadata_bytes_per_drive == 0
            || self.max_metadata_bytes_per_drive > MAX_INSPECT_ARCHIVE_METADATA_BYTES_PER_DRIVE
        {
            return Err(Error::RequestRejected(
                "RustFS advertised an invalid diagnostic archive metadata limit".to_string(),
            ));
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.max_duration_secs)
    }
}

/// Opaque RSA private key used only for local archive decryption.
pub struct InspectArchiveKey {
    private_key: RsaPrivateKey,
    public_key_pem: String,
}

impl InspectArchiveKey {
    /// Generate a fresh 2048-bit ephemeral key.
    pub fn generate() -> Result<Self> {
        let private_key = RsaPrivateKey::new(&mut OsRng, 2048)
            .map_err(|_| Error::General("Failed to generate diagnostic archive key".to_string()))?;
        Self::from_private_key(private_key)
    }

    /// Parse caller-provided PKCS#8 PEM private-key material.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self> {
        let private_key = RsaPrivateKey::from_pkcs8_pem(pem).map_err(|_| {
            Error::InvalidPath(
                "Diagnostic archive private key must be valid PKCS#8 RSA PEM".to_string(),
            )
        })?;
        Self::from_private_key(private_key)
    }

    fn from_private_key(private_key: RsaPrivateKey) -> Result<Self> {
        let bits = private_key.size().saturating_mul(8);
        if !(2048..=8192).contains(&bits) {
            return Err(Error::InvalidPath(
                "Diagnostic archive RSA private key must be between 2048 and 8192 bits".to_string(),
            ));
        }
        let public_key_pem = RsaPublicKey::from(&private_key)
            .to_public_key_pem(LineEnding::LF)
            .map_err(|_| {
                Error::General("Failed to derive diagnostic archive public key".to_string())
            })?;
        Ok(Self {
            private_key,
            public_key_pem,
        })
    }

    pub fn public_key_pem(&self) -> &str {
        &self.public_key_pem
    }
}

/// Sanitized transport request. It intentionally has no `Debug` implementation.
pub struct InspectArchiveTransportRequest {
    pub bucket: String,
    pub object: String,
    pub public_key_pem: String,
    pub max_bytes: usize,
    pub timeout: Duration,
}

/// Encrypted response staged in private temporary storage.
pub struct EncryptedInspectArchive {
    pub file: NamedTempFile,
    pub bytes: u64,
}

/// Transport boundary implemented by the RustFS Admin API adapter.
#[async_trait]
pub trait InspectArchiveApi: Send + Sync {
    async fn inspect_archive_capability(&self) -> Result<InspectArchiveCapabilityContract>;

    async fn download_inspect_archive(
        &self,
        request: InspectArchiveTransportRequest,
        temporary_directory: &Path,
    ) -> Result<EncryptedInspectArchive>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectArchiveManifest {
    pub archive_version: u16,
    pub drive_count: usize,
    pub artifact_paths: Vec<String>,
    pub target_identifiers_included: bool,
    pub raw_object_data_included: bool,
    pub raw_metadata_included: bool,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Fully authenticated and structurally validated plaintext archive.
pub struct VerifiedInspectArchive {
    file: NamedTempFile,
    pub manifest: InspectArchiveManifest,
    pub encrypted_bytes: u64,
    pub plaintext_bytes: u64,
    pub plaintext_sha256: String,
}

/// Safe metadata returned after atomic publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedInspectArchive {
    pub path: PathBuf,
    pub archive_version: u16,
    pub drive_count: usize,
    pub encrypted_bytes: u64,
    pub plaintext_bytes: u64,
    pub plaintext_sha256: String,
}

/// Cooperative cancellation shared with blocking decryption and validation.
#[derive(Clone, Default)]
pub struct InspectArchiveCancellation {
    cancelled: Arc<AtomicBool>,
}

impl InspectArchiveCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

fn crypto_error(message: &str) -> Error {
    Error::General(format!("Diagnostic archive verification failed: {message}"))
}

fn read_exact(reader: &mut impl Read, bytes: &mut [u8], message: &str) -> Result<()> {
    reader.read_exact(bytes).map_err(|_| crypto_error(message))
}

fn encryption_aad(record_type: u8, counter: u32) -> [u8; 13] {
    let mut aad = [0_u8; 13];
    aad[..8].copy_from_slice(FORMAT_MAGIC);
    aad[8] = record_type;
    aad[9..].copy_from_slice(&counter.to_be_bytes());
    aad
}

/// Decrypt, authenticate, and validate an encrypted archive without publishing plaintext.
pub fn decrypt_and_validate_inspect_archive(
    encrypted: EncryptedInspectArchive,
    key: &InspectArchiveKey,
    temporary_directory: &Path,
    max_plaintext_bytes: usize,
) -> Result<VerifiedInspectArchive> {
    decrypt_and_validate_inspect_archive_with_cancel(
        encrypted,
        key,
        temporary_directory,
        max_plaintext_bytes,
        MAX_INSPECT_ARCHIVE_METADATA_BYTES_PER_DRIVE,
        max_plaintext_bytes,
        &InspectArchiveCancellation::default(),
    )
}

pub fn decrypt_and_validate_inspect_archive_with_cancel(
    encrypted: EncryptedInspectArchive,
    key: &InspectArchiveKey,
    temporary_directory: &Path,
    max_plaintext_bytes: usize,
    max_metadata_bytes_per_drive: usize,
    max_unpacked_bytes: usize,
    cancellation: &InspectArchiveCancellation,
) -> Result<VerifiedInspectArchive> {
    let mut reader =
        BufReader::new(encrypted.file.reopen().map_err(|_| {
            Error::InvalidPath("Failed to open staged diagnostic archive".to_string())
        })?);
    let mut fixed_header = [0_u8; 24];
    read_exact(&mut reader, &mut fixed_header, "truncated encrypted header")?;
    if &fixed_header[..8] != FORMAT_MAGIC {
        return Err(crypto_error("invalid encrypted header"));
    }
    if u16::from_be_bytes([fixed_header[8], fixed_header[9]]) != INSPECT_ARCHIVE_VERSION {
        return Err(crypto_error("unsupported archive version"));
    }
    let chunk_size = usize::try_from(u32::from_be_bytes(
        fixed_header[10..14]
            .try_into()
            .map_err(|_| crypto_error("invalid chunk size"))?,
    ))
    .map_err(|_| crypto_error("invalid chunk size"))?;
    if chunk_size != ARCHIVE_CHUNK_SIZE {
        return Err(crypto_error("unexpected encrypted chunk size"));
    }
    let wrapped_len = usize::from(u16::from_be_bytes([fixed_header[14], fixed_header[15]]));
    if wrapped_len == 0 || wrapped_len > 1024 {
        return Err(crypto_error("invalid wrapped key length"));
    }
    let nonce_prefix: [u8; 8] = fixed_header[16..24]
        .try_into()
        .map_err(|_| crypto_error("invalid nonce prefix"))?;
    let mut wrapped_key = vec![0_u8; wrapped_len];
    read_exact(
        &mut reader,
        &mut wrapped_key,
        "truncated wrapped encryption key",
    )?;
    let data_key = Zeroizing::new(
        key.private_key
            .decrypt(Oaep::new::<Sha256>(), &wrapped_key)
            .map_err(|_| crypto_error("RSA key unwrap failed"))?,
    );
    let cipher = Aes256Gcm::new_from_slice(&data_key)
        .map_err(|_| crypto_error("invalid decrypted data key"))?;

    let mut plaintext = NamedTempFile::new_in(temporary_directory).map_err(|_| {
        Error::InvalidPath("Failed to create private diagnostic archive staging file".to_string())
    })?;
    protect_private_file(plaintext.as_file())?;
    let mut digest = Sha256::new();
    let mut plaintext_bytes = 0_usize;
    let mut expected_counter = 0_u32;
    let final_digest = loop {
        if cancellation.is_cancelled() {
            return Err(Error::Interrupted(
                "Diagnostic archive verification was cancelled".to_string(),
            ));
        }
        let mut record_header = [0_u8; 9];
        read_exact(
            &mut reader,
            &mut record_header,
            "authenticated final record is missing",
        )?;
        let record_type = record_header[0];
        let counter = u32::from_be_bytes(
            record_header[1..5]
                .try_into()
                .map_err(|_| crypto_error("invalid record counter"))?,
        );
        if counter != expected_counter {
            return Err(crypto_error("record counter is not contiguous"));
        }
        let ciphertext_len = usize::try_from(u32::from_be_bytes(
            record_header[5..9]
                .try_into()
                .map_err(|_| crypto_error("invalid record length"))?,
        ))
        .map_err(|_| crypto_error("invalid record length"))?;
        let maximum = match record_type {
            RECORD_DATA => ARCHIVE_CHUNK_SIZE + 16,
            RECORD_FINAL => 32 + 16,
            _ => return Err(crypto_error("unknown encrypted record type")),
        };
        if ciphertext_len < 16 || ciphertext_len > maximum {
            return Err(crypto_error(
                "encrypted record length is outside the contract",
            ));
        }
        let mut ciphertext = vec![0_u8; ciphertext_len];
        read_exact(&mut reader, &mut ciphertext, "truncated encrypted record")?;
        let mut nonce = [0_u8; 12];
        nonce[..8].copy_from_slice(&nonce_prefix);
        nonce[8..].copy_from_slice(&counter.to_be_bytes());
        let decoded = cipher
            .decrypt(
                (&nonce).into(),
                Payload {
                    msg: &ciphertext,
                    aad: &encryption_aad(record_type, counter),
                },
            )
            .map_err(|_| crypto_error("record authentication failed"))?;
        match record_type {
            RECORD_DATA => {
                plaintext_bytes = plaintext_bytes
                    .checked_add(decoded.len())
                    .ok_or_else(|| crypto_error("plaintext byte accounting overflow"))?;
                if plaintext_bytes > max_plaintext_bytes
                    || plaintext_bytes > MAX_INSPECT_ARCHIVE_BYTES
                {
                    return Err(crypto_error("plaintext archive exceeds the client limit"));
                }
                digest.update(&decoded);
                plaintext.write_all(&decoded).map_err(|_| {
                    Error::InvalidPath(
                        "Failed to write private diagnostic archive staging file".to_string(),
                    )
                })?;
                expected_counter = expected_counter
                    .checked_add(1)
                    .ok_or_else(|| crypto_error("record counter exhausted"))?;
            }
            RECORD_FINAL => {
                if decoded.len() != 32 || decoded.as_slice() != digest.clone().finalize().as_slice()
                {
                    return Err(crypto_error("final plaintext digest mismatch"));
                }
                break decoded;
            }
            _ => unreachable!(),
        }
    };
    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .map_err(|_| crypto_error("failed to inspect encrypted completion"))?
        != 0
    {
        return Err(crypto_error("records follow the authenticated completion"));
    }
    plaintext
        .as_file_mut()
        .sync_all()
        .map_err(|_| Error::InvalidPath("Failed to sync diagnostic archive staging file".into()))?;
    plaintext
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|_| crypto_error("failed to rewind plaintext archive"))?;
    let manifest = validate_plaintext_archive(
        plaintext.as_file_mut(),
        plaintext_bytes,
        max_metadata_bytes_per_drive,
        max_unpacked_bytes,
        cancellation,
    )?;

    Ok(VerifiedInspectArchive {
        file: plaintext,
        manifest,
        encrypted_bytes: encrypted.bytes,
        plaintext_bytes: u64::try_from(plaintext_bytes)
            .map_err(|_| crypto_error("plaintext byte accounting overflow"))?,
        plaintext_sha256: hex::encode(final_digest),
    })
}

fn validate_plaintext_archive(
    file: &mut File,
    expected_plaintext_bytes: usize,
    max_metadata_bytes_per_drive: usize,
    max_unpacked_bytes: usize,
    cancellation: &InspectArchiveCancellation,
) -> Result<InspectArchiveManifest> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| crypto_error("failed to inspect plaintext archive"))?;
    let mut archive = tar::Archive::new(file);
    let mut entries = archive
        .entries()
        .map_err(|_| crypto_error("plaintext tar is malformed"))?;
    let Some(first) = entries.next() else {
        return Err(crypto_error("plaintext tar is empty"));
    };
    let mut first = first.map_err(|_| crypto_error("plaintext tar is malformed"))?;
    if !first.header().entry_type().is_file()
        || first
            .path()
            .map_err(|_| crypto_error("manifest path is malformed"))?
            .as_ref()
            != Path::new("manifest.json")
        || first.size() > MAX_MANIFEST_BYTES
    {
        return Err(crypto_error("manifest entry is invalid"));
    }
    let mut manifest_bytes = Vec::new();
    first
        .read_to_end(&mut manifest_bytes)
        .map_err(|_| crypto_error("manifest entry is truncated"))?;
    let manifest: InspectArchiveManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| crypto_error("manifest JSON is invalid"))?;
    validate_manifest(&manifest)?;
    let mut unpacked_bytes = manifest_bytes.len();
    if unpacked_bytes > max_unpacked_bytes {
        return Err(crypto_error("unpacked archive exceeds the client limit"));
    }

    let expected = manifest
        .artifact_paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for entry in entries {
        if cancellation.is_cancelled() {
            return Err(Error::Interrupted(
                "Diagnostic archive verification was cancelled".to_string(),
            ));
        }
        let mut entry = entry.map_err(|_| crypto_error("plaintext tar is malformed"))?;
        if !entry.header().entry_type().is_file()
            || entry.size()
                > u64::try_from(max_metadata_bytes_per_drive)
                    .map_err(|_| crypto_error("invalid drive metadata limit"))?
        {
            return Err(crypto_error("drive artifact entry is invalid"));
        }
        let path = entry
            .path()
            .map_err(|_| crypto_error("drive artifact path is malformed"))?
            .to_string_lossy()
            .into_owned();
        if !expected.contains(&path) || !observed.insert(path) {
            return Err(crypto_error("plaintext tar contains an unexpected entry"));
        }
        let mut artifact = Vec::new();
        entry
            .read_to_end(&mut artifact)
            .map_err(|_| crypto_error("drive artifact entry is truncated"))?;
        unpacked_bytes = unpacked_bytes
            .checked_add(artifact.len())
            .ok_or_else(|| crypto_error("unpacked byte accounting overflow"))?;
        if unpacked_bytes > max_unpacked_bytes {
            return Err(crypto_error("unpacked archive exceeds the client limit"));
        }
        let value: serde_json::Value = serde_json::from_slice(&artifact)
            .map_err(|_| crypto_error("drive artifact JSON is invalid"))?;
        if !value.is_object() {
            return Err(crypto_error("drive artifact JSON is not an object"));
        }
    }
    if observed != expected {
        return Err(crypto_error("plaintext tar is missing drive artifacts"));
    }
    if archive
        .into_inner()
        .metadata()
        .map_err(|_| crypto_error("failed to inspect plaintext archive"))?
        .len()
        != u64::try_from(expected_plaintext_bytes)
            .map_err(|_| crypto_error("plaintext byte accounting overflow"))?
    {
        return Err(crypto_error(
            "plaintext archive length changed during verification",
        ));
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &InspectArchiveManifest) -> Result<()> {
    if manifest.archive_version != INSPECT_ARCHIVE_VERSION
        || manifest.target_identifiers_included
        || manifest.raw_object_data_included
        || manifest.raw_metadata_included
        || manifest.drive_count != manifest.artifact_paths.len()
    {
        return Err(crypto_error("manifest safety contract is invalid"));
    }
    let expected = (0..manifest.drive_count)
        .map(|index| format!("drives/{index:04}.json"))
        .collect::<Vec<_>>();
    if manifest.artifact_paths != expected {
        return Err(crypto_error("manifest artifact paths are invalid"));
    }
    Ok(())
}

fn protect_private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| {
                Error::InvalidPath("Failed to protect diagnostic archive staging file".to_string())
            })?;
    }
    Ok(())
}

/// Reject output directories containing symbolic links or parent traversal.
pub fn validate_inspect_archive_output_directory(directory: &Path) -> Result<()> {
    let mut current = if directory.is_absolute() {
        PathBuf::new()
    } else {
        PathBuf::from(".")
    };
    let mut normal_depth = 0_usize;
    for component in directory.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component.as_os_str());
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(Error::InvalidPath(
                    "Diagnostic archive output directory cannot contain parent traversal"
                        .to_string(),
                ));
            }
            Component::Normal(part) => {
                normal_depth += 1;
                current.push(part);
                let metadata = std::fs::symlink_metadata(&current).map_err(|_| {
                    Error::InvalidPath(
                        "Diagnostic archive output directory does not exist or is inaccessible"
                            .to_string(),
                    )
                })?;
                // macOS exposes system roots such as /var and /tmp as root-level aliases.
                let platform_root_alias = directory.is_absolute() && normal_depth == 1;
                if metadata.file_type().is_symlink() && !platform_root_alias {
                    return Err(Error::InvalidPath(
                        "Diagnostic archive output directory cannot contain symbolic links"
                            .to_string(),
                    ));
                }
            }
        }
    }
    let metadata = std::fs::symlink_metadata(directory).map_err(|_| {
        Error::InvalidPath(
            "Diagnostic archive output directory does not exist or is inaccessible".to_string(),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(Error::InvalidPath(
            "Diagnostic archive output directory cannot be a symbolic link".to_string(),
        ));
    }
    if !metadata.is_dir() {
        return Err(Error::InvalidPath(
            "Diagnostic archive output directory does not exist".to_string(),
        ));
    }
    Ok(())
}

/// Atomically publish a verified archive without replacing an existing destination.
pub fn publish_inspect_archive(
    verified: VerifiedInspectArchive,
    destination: &Path,
) -> Result<PublishedInspectArchive> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    validate_inspect_archive_output_directory(parent)?;
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(Error::Conflict(
            "Diagnostic archive output already exists".to_string(),
        ));
    }
    let VerifiedInspectArchive {
        file,
        manifest,
        encrypted_bytes,
        plaintext_bytes,
        plaintext_sha256,
    } = verified;
    file.persist_noclobber(destination).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Conflict("Diagnostic archive output already exists".to_string())
        } else {
            Error::InvalidPath("Failed to atomically publish diagnostic archive output".to_string())
        }
    })?;
    #[cfg(unix)]
    if File::open(parent)
        .and_then(|directory| directory.sync_all())
        .is_err()
    {
        let _ = std::fs::remove_file(destination);
        return Err(Error::InvalidPath(
            "Failed to sync diagnostic archive output directory".to_string(),
        ));
    }
    Ok(PublishedInspectArchive {
        path: destination.to_path_buf(),
        archive_version: manifest.archive_version,
        drive_count: manifest.drive_count,
        encrypted_bytes,
        plaintext_bytes,
        plaintext_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::Key;
    use rsa::pkcs8::DecodePublicKey;
    use tempfile::tempdir;

    fn manifest(raw_metadata: bool) -> InspectArchiveManifest {
        InspectArchiveManifest {
            archive_version: 1,
            drive_count: 1,
            artifact_paths: vec!["drives/0000.json".to_string()],
            target_identifiers_included: false,
            raw_object_data_included: false,
            raw_metadata_included: raw_metadata,
            extra: BTreeMap::new(),
        }
    }

    fn tar_fixture(manifest: &InspectArchiveManifest) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut output);
            for (path, bytes) in [
                (
                    "manifest.json",
                    serde_json::to_vec(manifest).expect("serialize manifest"),
                ),
                (
                    "drives/0000.json",
                    br#"{"drive_index":0,"status":"ok"}"#.to_vec(),
                ),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o600);
                header.set_mtime(0);
                header.set_size(bytes.len() as u64);
                header.set_cksum();
                archive
                    .append_data(&mut header, path, bytes.as_slice())
                    .expect("append tar entry");
            }
            archive.finish().expect("finish tar");
        }
        output
    }

    fn encrypt_fixture(key: &InspectArchiveKey, plaintext: &[u8]) -> Vec<u8> {
        let public =
            RsaPublicKey::from_public_key_pem(key.public_key_pem()).expect("parse public key");
        let data_key = [7_u8; 32];
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&data_key));
        let wrapped = public
            .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &data_key)
            .expect("wrap key");
        let nonce_prefix = [9_u8; 8];
        let mut output = Vec::new();
        output.extend_from_slice(FORMAT_MAGIC);
        output.extend_from_slice(&INSPECT_ARCHIVE_VERSION.to_be_bytes());
        output.extend_from_slice(&(ARCHIVE_CHUNK_SIZE as u32).to_be_bytes());
        output.extend_from_slice(&(wrapped.len() as u16).to_be_bytes());
        output.extend_from_slice(&nonce_prefix);
        output.extend_from_slice(&wrapped);

        for (counter, chunk) in plaintext.chunks(ARCHIVE_CHUNK_SIZE).enumerate() {
            append_encrypted_record(
                &mut output,
                &cipher,
                &nonce_prefix,
                RECORD_DATA,
                counter as u32,
                chunk,
            );
        }
        append_encrypted_record(
            &mut output,
            &cipher,
            &nonce_prefix,
            RECORD_FINAL,
            plaintext.len().div_ceil(ARCHIVE_CHUNK_SIZE) as u32,
            &Sha256::digest(plaintext),
        );
        output
    }

    fn append_encrypted_record(
        output: &mut Vec<u8>,
        cipher: &Aes256Gcm,
        nonce_prefix: &[u8; 8],
        record_type: u8,
        counter: u32,
        plaintext: &[u8],
    ) {
        let mut nonce = [0_u8; 12];
        nonce[..8].copy_from_slice(nonce_prefix);
        nonce[8..].copy_from_slice(&counter.to_be_bytes());
        let ciphertext = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: plaintext,
                    aad: &encryption_aad(record_type, counter),
                },
            )
            .expect("encrypt record");
        output.push(record_type);
        output.extend_from_slice(&counter.to_be_bytes());
        output.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
        output.extend_from_slice(&ciphertext);
    }

    fn staged(bytes: &[u8], directory: &Path) -> EncryptedInspectArchive {
        let mut file = NamedTempFile::new_in(directory).expect("create encrypted staging");
        file.write_all(bytes).expect("write encrypted staging");
        EncryptedInspectArchive {
            file,
            bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn complete_archive_decrypts_validates_and_publishes_privately() {
        let directory = tempdir().expect("temp directory");
        let key = InspectArchiveKey::generate().expect("key");
        let plaintext = tar_fixture(&manifest(false));
        let encrypted = encrypt_fixture(&key, &plaintext);
        let verified = decrypt_and_validate_inspect_archive(
            staged(&encrypted, directory.path()),
            &key,
            directory.path(),
            MAX_INSPECT_ARCHIVE_BYTES,
        )
        .expect("verified archive");
        assert_eq!(verified.manifest.drive_count, 1);
        assert_eq!(verified.plaintext_bytes, plaintext.len() as u64);
        let destination = directory.path().join("inspect.tar");
        let published =
            publish_inspect_archive(verified, &destination).expect("publish verified archive");
        assert_eq!(published.path, destination);
        assert_eq!(std::fs::read(&destination).expect("read output"), plaintext);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(destination)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }

    #[test]
    fn corruption_truncation_and_missing_completion_leave_no_plaintext() {
        let directory = tempdir().expect("temp directory");
        let key = InspectArchiveKey::generate().expect("key");
        let plaintext = tar_fixture(&manifest(false));
        let encrypted = encrypt_fixture(&key, &plaintext);
        let cases = [
            encrypted[..encrypted.len() - 1].to_vec(),
            {
                let mut corrupt = encrypted.clone();
                let last = corrupt.last_mut().expect("ciphertext");
                *last ^= 1;
                corrupt
            },
            encrypted[..encrypted.len() - 57].to_vec(),
        ];
        for bytes in cases {
            let before = std::fs::read_dir(directory.path())
                .expect("read directory")
                .count();
            let error = decrypt_and_validate_inspect_archive(
                staged(&bytes, directory.path()),
                &key,
                directory.path(),
                MAX_INSPECT_ARCHIVE_BYTES,
            )
            .err()
            .expect("invalid archive");
            assert!(error.to_string().contains("verification failed"));
            let after = std::fs::read_dir(directory.path())
                .expect("read directory")
                .count();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn unsafe_manifest_limit_and_cancellation_are_rejected() {
        let directory = tempdir().expect("temp directory");
        let key = InspectArchiveKey::generate().expect("key");
        let unsafe_tar = tar_fixture(&manifest(true));
        let error = decrypt_and_validate_inspect_archive(
            staged(&encrypt_fixture(&key, &unsafe_tar), directory.path()),
            &key,
            directory.path(),
            MAX_INSPECT_ARCHIVE_BYTES,
        )
        .err()
        .expect("unsafe manifest");
        assert!(error.to_string().contains("manifest safety"));

        let valid_tar = tar_fixture(&manifest(false));
        let error = decrypt_and_validate_inspect_archive(
            staged(&encrypt_fixture(&key, &valid_tar), directory.path()),
            &key,
            directory.path(),
            valid_tar.len() - 1,
        )
        .err()
        .expect("plaintext limit");
        assert!(error.to_string().contains("client limit"));

        let cancellation = InspectArchiveCancellation::default();
        cancellation.cancel();
        let error = decrypt_and_validate_inspect_archive_with_cancel(
            staged(&encrypt_fixture(&key, &valid_tar), directory.path()),
            &key,
            directory.path(),
            MAX_INSPECT_ARCHIVE_BYTES,
            MAX_INSPECT_ARCHIVE_METADATA_BYTES_PER_DRIVE,
            MAX_INSPECT_ARCHIVE_BYTES,
            &cancellation,
        )
        .err()
        .expect("cancelled verification");
        assert!(matches!(error, Error::Interrupted(_)));

        let error = decrypt_and_validate_inspect_archive_with_cancel(
            staged(&encrypt_fixture(&key, &valid_tar), directory.path()),
            &key,
            directory.path(),
            MAX_INSPECT_ARCHIVE_BYTES,
            MAX_INSPECT_ARCHIVE_METADATA_BYTES_PER_DRIVE,
            1,
            &InspectArchiveCancellation::default(),
        )
        .err()
        .expect("unpacked limit");
        assert!(error.to_string().contains("unpacked archive"));
    }

    #[test]
    fn publish_refuses_overwrite_and_removes_staging_file() {
        let directory = tempdir().expect("temp directory");
        let key = InspectArchiveKey::generate().expect("key");
        let plaintext = tar_fixture(&manifest(false));
        let verified = decrypt_and_validate_inspect_archive(
            staged(&encrypt_fixture(&key, &plaintext), directory.path()),
            &key,
            directory.path(),
            MAX_INSPECT_ARCHIVE_BYTES,
        )
        .expect("verified archive");
        let staging = verified.file.path().to_path_buf();
        let destination = directory.path().join("existing.tar");
        std::fs::write(&destination, b"keep").expect("existing output");
        let error = publish_inspect_archive(verified, &destination).expect_err("no overwrite");
        assert!(matches!(error, Error::Conflict(_)));
        assert_eq!(
            std::fs::read(destination).expect("existing output"),
            b"keep"
        );
        assert!(!staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn output_directory_and_broken_destination_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temp directory");
        let real = directory.path().join("real");
        std::fs::create_dir(&real).expect("real directory");
        let child = real.join("child");
        std::fs::create_dir(&child).expect("real child directory");
        let linked = directory.path().join("linked");
        symlink(&real, &linked).expect("directory symlink");
        assert!(validate_inspect_archive_output_directory(&linked).is_err());
        assert!(validate_inspect_archive_output_directory(&linked.join("child")).is_err());

        let destination = real.join("archive.tar");
        symlink(real.join("missing"), &destination).expect("broken destination symlink");
        let key = InspectArchiveKey::generate().expect("key");
        let plaintext = tar_fixture(&manifest(false));
        let verified = decrypt_and_validate_inspect_archive(
            staged(&encrypt_fixture(&key, &plaintext), &real),
            &key,
            &real,
            MAX_INSPECT_ARCHIVE_BYTES,
        )
        .expect("verified archive");
        assert!(matches!(
            publish_inspect_archive(verified, &destination),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn capability_and_private_key_contracts_fail_closed() {
        let supported = InspectArchiveCapabilityContract {
            state: super::super::RuntimeCapabilityStatus {
                state: super::super::RuntimeCapabilityState::Supported,
                reason: None,
                extra: BTreeMap::new(),
            },
            route: INSPECT_ARCHIVE_ROUTE.to_string(),
            archive_version: 1,
            content_type: INSPECT_ARCHIVE_CONTENT_TYPE.to_string(),
            encryption: INSPECT_ARCHIVE_ENCRYPTION.to_string(),
            completion_contract: INSPECT_ARCHIVE_COMPLETION.to_string(),
            max_bytes: MAX_INSPECT_ARCHIVE_BYTES,
            max_duration_secs: 30,
            max_metadata_bytes_per_drive: 4 * 1024 * 1024,
            extra: BTreeMap::new(),
        };
        supported.validate().expect("supported contract");
        let mut incompatible = supported;
        incompatible.encryption = "future".to_string();
        assert!(matches!(
            incompatible.validate(),
            Err(Error::UnsupportedFeature(_))
        ));
        assert!(InspectArchiveKey::from_pkcs8_pem("not-a-key").is_err());
    }
}

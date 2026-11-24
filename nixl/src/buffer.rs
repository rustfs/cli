//! Backend registrations used by the shared object transfer pipeline.

use rc_core::{Error, RdmaBackend, Result};
use rustfs_nixl::{NixlConfig, RegisteredClientWindow};
use std::fs::File;
#[cfg(feature = "cuobject")]
use std::os::unix::fs::FileExt;

pub(super) const STREAM_CHUNK: usize = 256 * 1024;

pub(super) enum Buffer {
    Nixl(RegisteredClientWindow),
    #[cfg(feature = "cuobject")]
    CuObject(crate::nvidia::HostBuffer),
}

pub(super) fn register(
    backend: RdmaBackend,
    size: usize,
    count: usize,
    _budget: &crate::pool::Budget,
) -> Result<Vec<Buffer>> {
    if backend == RdmaBackend::CuObject {
        #[cfg(feature = "cuobject")]
        return crate::nvidia::register(size, count, _budget);
        #[cfg(not(feature = "cuobject"))]
        return Err(Error::UnsupportedFeature(
            "The cuObject backend is not installed".into(),
        ));
    }
    let id = getrandom::u64()
        .map_err(|_| Error::General("Could not generate a NIXL agent ID".into()))?;
    let name = format!("rc-{}-{id:016x}", std::process::id());
    let windows = RegisteredClientWindow::register(&NixlConfig {
        agent_name: name,
        chunk_bytes: size,
        pool_bytes: count * size,
        ..NixlConfig::default()
    })
    .map_err(nixl_error)?;
    Ok(windows.into_iter().map(Buffer::Nixl).collect())
}

pub(super) fn nixl_error(error: rustfs_nixl::NixlError) -> Error {
    Error::General(format!("NIXL/UCX: {error}"))
}

pub(super) enum ReceivedChunks<'a> {
    Nixl {
        window: &'a mut RegisteredClientWindow,
        offset: usize,
        len: usize,
        scratch: Vec<u8>,
    },
    #[cfg(feature = "cuobject")]
    CuObject(Option<&'a [u8]>),
}

impl ReceivedChunks<'_> {
    pub(super) fn next_chunk(&mut self) -> Result<Option<&[u8]>> {
        match self {
            Self::Nixl {
                window,
                offset,
                len,
                scratch,
            } => {
                let count = (*len - *offset).min(scratch.len());
                if count == 0 {
                    return Ok(None);
                }
                let bytes = &mut scratch[..count];
                window.read_at(*offset, bytes).map_err(nixl_error)?;
                *offset += count;
                Ok(Some(bytes))
            }
            #[cfg(feature = "cuobject")]
            Self::CuObject(bytes) => Ok(bytes.take()),
        }
    }
}

impl Buffer {
    pub(super) fn received(&mut self, len: usize) -> Result<ReceivedChunks<'_>> {
        match self {
            Self::Nixl(chunk) => Ok(ReceivedChunks::Nixl {
                window: chunk,
                offset: 0,
                len,
                scratch: vec![0; len.min(STREAM_CHUNK)],
            }),
            #[cfg(feature = "cuobject")]
            Self::CuObject(host) => Ok(ReceivedChunks::CuObject(Some(&host.writable()?[..len]))),
        }
    }

    pub(super) fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Nixl(chunk) => chunk.write_at(offset, bytes).map_err(nixl_error),
            #[cfg(feature = "cuobject")]
            Self::CuObject(host) => {
                host.writable()?[offset..offset + bytes.len()].copy_from_slice(bytes);
                Ok(())
            }
        }
    }

    pub(super) fn read_file(&mut self, file: &File, offset: u64, len: usize) -> Result<()> {
        match self {
            Self::Nixl(chunk) => chunk.read_file_at(file, offset, 0, len).map_err(nixl_error),
            #[cfg(feature = "cuobject")]
            Self::CuObject(host) => Ok(file.read_exact_at(&mut host.writable()?[..len], offset)?),
        }
    }

    pub(super) fn write_file(&mut self, file: &File, offset: u64, len: usize) -> Result<()> {
        match self {
            Self::Nixl(chunk) => chunk
                .write_file_at(file, offset, 0, len)
                .map_err(nixl_error),
            #[cfg(feature = "cuobject")]
            Self::CuObject(host) => Ok(file.write_all_at(&host.writable()?[..len], offset)?),
        }
    }
}

//! Backend registrations used by the shared object transfer pipeline.

use base64::{Engine, engine::general_purpose::STANDARD};
use http::HeaderValue;
use rc_core::{Error, RdmaBackend, Result};
use rustfs_nixl::{NixlConfig, NixlServer, StagingChunk};

pub(super) enum Buffer {
    Nixl {
        chunk: StagingChunk,
        agent: HeaderValue,
        metadata: HeaderValue,
    },
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
    let agent = NixlServer::new(&NixlConfig {
        agent_name: name.clone(),
        chunk_bytes: size,
        pool_bytes: count * size,
        ..NixlConfig::default()
    })
    .map_err(nixl_error)?;
    let mut metadata =
        HeaderValue::from_str(&STANDARD.encode(agent.local_metadata().map_err(nixl_error)?))
            .map_err(|_| Error::General("Invalid NIXL agent metadata".into()))?;
    metadata.set_sensitive(true);
    let name = HeaderValue::from_str(&name).expect("ASCII agent ID");
    (0..count)
        .map(|_| {
            let chunk = agent.try_reserve().ok_or_else(|| {
                Error::General("NIXL registration has no available window".into())
            })?;
            Ok(Buffer::Nixl {
                chunk,
                agent: name.clone(),
                metadata: metadata.clone(),
            })
        })
        .collect()
}

fn nixl_error(error: rustfs_nixl::NixlError) -> Error {
    Error::General(format!("NIXL/UCX: {error}"))
}

impl Buffer {
    pub(super) fn writable(&mut self) -> Result<&mut [u8]> {
        match self {
            Self::Nixl { chunk, .. } => chunk.spare_mut().map_err(nixl_error),
            #[cfg(feature = "cuobject")]
            Self::CuObject(buffer) => buffer.writable(),
        }
    }
}

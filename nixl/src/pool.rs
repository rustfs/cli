//! Registered host memory and ownership while the server can access it.

use base64::{Engine, engine::general_purpose::STANDARD};
use http::HeaderValue;
use rc_core::{Error, Result};
use rustfs_nixl::{NixlConfig, NixlServer, StagingChunk};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, SemaphorePermit};

pub(crate) const WINDOW: usize = rc_s3::multipart::DEFAULT_PART_SIZE as usize;
pub(crate) const SMALL_WINDOW: usize = 1024 * 1024;
// Two windows allow the next network read to overlap the previous file write.
static AGENTS: Semaphore = Semaphore::const_new(8);
static QUARANTINE: Mutex<Vec<(StagingChunk, Arc<Pool>)>> = Mutex::new(Vec::new());

pub(crate) struct Pool {
    agent: Arc<NixlServer>,
    pub(crate) name: HeaderValue,
    pub(crate) metadata: HeaderValue,
    _budget: SemaphorePermit<'static>,
}

impl Pool {
    pub(crate) fn open(window: usize) -> Result<Arc<Self>> {
        let budget = AGENTS
            .try_acquire()
            .map_err(|_| Error::General("NIXL registered memory budget exhausted".into()))?;
        let id = getrandom::u64()
            .map_err(|_| Error::General("Could not generate a NIXL agent ID".into()))?;
        let name = format!("rc-{}-{id:016x}", std::process::id());
        let agent = NixlServer::new(&NixlConfig {
            agent_name: name.clone(),
            chunk_bytes: window,
            pool_bytes: 2 * window,
            ..NixlConfig::default()
        })
        .map_err(native_error)?;
        let mut metadata =
            HeaderValue::from_str(&STANDARD.encode(agent.local_metadata().map_err(native_error)?))
                .map_err(|_| Error::General("Invalid NIXL agent metadata".into()))?;
        metadata.set_sensitive(true);
        Ok(Arc::new(Self {
            agent,
            name: HeaderValue::from_str(&name).expect("ASCII agent ID"),
            metadata,
            _budget: budget,
        }))
    }

    pub(crate) async fn reserve(&self) -> Result<StagingChunk> {
        self.agent
            .reserve(Duration::from_secs(60))
            .await
            .map_err(native_error)
    }
}

pub(crate) fn native_error(error: rustfs_nixl::NixlError) -> Error {
    Error::General(format!("NIXL/UCX: {error}"))
}

/// A cancelled or interrupted HTTPS exchange gives no proof that peer DMA has
/// finished. Such windows retain their registration and its budget until exit.
pub(crate) struct Exposed {
    chunk: Option<StagingChunk>,
    pool: Arc<Pool>,
}

impl Exposed {
    pub(crate) fn new(chunk: StagingChunk, pool: Arc<Pool>) -> Self {
        Self {
            chunk: Some(chunk),
            pool,
        }
    }

    pub(crate) fn finished(mut self) -> StagingChunk {
        self.chunk
            .take()
            .expect("exposed window is owned until completion")
    }
}

impl Drop for Exposed {
    fn drop(&mut self) {
        if let Some(chunk) = self.chunk.take() {
            QUARANTINE
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((chunk, self.pool.clone()));
        }
    }
}

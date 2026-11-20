//! Registered host memory and ownership while the server can access it.

use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderMap, HeaderValue};
use rc_core::{Error, Result};
use rustfs_nixl::{NixlConfig, NixlServer, StagingChunk};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, SemaphorePermit, watch};

pub(crate) const WINDOW: usize = rc_s3::multipart::DEFAULT_PART_SIZE as usize;
pub(crate) const SMALL_WINDOW: usize = 1024 * 1024;
// Two windows allow the next network read to overlap the previous file write.
static AGENTS: Semaphore = Semaphore::const_new(8);
static QUARANTINE: Mutex<Vec<Window>> = Mutex::new(Vec::new());
const WINDOWS_PER_POOL: usize = 2;

/// One active registration per S3 client. A larger request upgrades the pool;
/// outstanding windows keep their original registration alive.
#[derive(Default)]
pub(crate) struct Pools {
    active: AsyncMutex<Option<Arc<Pool>>>,
}

impl Pools {
    pub(crate) async fn reserve(&self, required: u64) -> Result<Window> {
        let size = if required <= SMALL_WINDOW as u64 {
            SMALL_WINDOW
        } else {
            WINDOW
        };
        loop {
            let pool = {
                let mut active = self.active.lock().await;
                if let Some(pool) = active.as_ref()
                    && pool.agent.chunk_bytes() >= size
                    && *pool.lost.borrow() < WINDOWS_PER_POOL
                {
                    pool.clone()
                } else {
                    let pool = tokio::task::spawn_blocking(move || Pool::open(size))
                        .await
                        .map_err(|_| {
                            Error::General("NIXL agent startup was interrupted".into())
                        })??;
                    *active = Some(pool.clone());
                    pool
                }
            };
            let mut lost = pool.lost.subscribe();
            tokio::select! {
                biased;
                _ = lost.wait_for(|count| *count == WINDOWS_PER_POOL) => continue,
                result = pool.agent.reserve(Duration::from_secs(60)) => {
                    return Ok(Window { chunk: result.map_err(native_error)?, pool });
                }
            }
        }
    }
}

struct Pool {
    lost: watch::Sender<usize>,
    agent: Arc<NixlServer>,
    name: HeaderValue,
    metadata: HeaderValue,
    _budget: SemaphorePermit<'static>,
}

impl Pool {
    fn open(window: usize) -> Result<Arc<Self>> {
        let budget = AGENTS
            .try_acquire()
            .map_err(|_| Error::General("NIXL registered memory budget exhausted".into()))?;
        let id = getrandom::u64()
            .map_err(|_| Error::General("Could not generate a NIXL agent ID".into()))?;
        let name = format!("rc-{}-{id:016x}", std::process::id());
        let agent = NixlServer::new(&NixlConfig {
            agent_name: name.clone(),
            chunk_bytes: window,
            pool_bytes: WINDOWS_PER_POOL * window,
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
            lost: watch::Sender::new(0),
            _budget: budget,
        }))
    }
}

pub(crate) fn native_error(error: rustfs_nixl::NixlError) -> Error {
    Error::General(format!("NIXL/UCX: {error}"))
}

pub(crate) struct Window {
    pub(crate) chunk: StagingChunk,
    pool: Arc<Pool>,
}

impl Window {
    pub(crate) fn bind(&self, headers: &mut HeaderMap, len: usize) -> Result<()> {
        crate::wire::bind(
            headers,
            &self.pool.name,
            &self.pool.metadata,
            self.chunk.addr(),
            len,
        )
    }

    pub(crate) fn expose(self) -> Exposed {
        Exposed(Some(self))
    }
}

/// Until a receipt proves completion, cancellation must retain the address
/// and its registration. The process registration budget also covers this list.
pub(crate) struct Exposed(Option<Window>);

impl Exposed {
    pub(crate) fn finished(mut self) -> Window {
        self.0.take().expect("owned NIXL window")
    }
}

impl Drop for Exposed {
    fn drop(&mut self) {
        if let Some(window) = self.0.take() {
            window.pool.lost.send_modify(|lost| *lost += 1);
            QUARANTINE
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uncertain_windows_wake_waiters_and_cannot_be_reissued() {
        let pools = Pools::default();
        let first = pools.reserve(32).await.unwrap();
        let second = pools.reserve(32).await.unwrap();
        let old = first.pool.clone();
        assert!(Arc::ptr_eq(&old, &second.pool));
        let waiting = pools.reserve(32);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut waiting)
                .await
                .is_err()
        );
        drop(first.expose());
        drop(second.expose());
        let replacement = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&old, &replacement.pool));
        let returned = replacement.expose().finished();
        drop(returned);
        assert!(old.agent.try_reserve().is_none());
    }
}

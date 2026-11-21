//! Bounded registration and exclusive ownership until a transfer completes.

use crate::buffer::{self, Buffer};
use http::{HeaderMap, Method};
use rc_core::{Error, RdmaBackend, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, SemaphorePermit, watch};

pub(crate) const WINDOW: usize = rc_s3::multipart::DEFAULT_PART_SIZE as usize;
const SMALL_WINDOW: usize = 1024 * 1024;
const WINDOWS_PER_POOL: usize = 2;
static AGENTS: Semaphore = Semaphore::const_new(8);
static QUARANTINE: Mutex<Vec<Window>> = Mutex::new(Vec::new());

#[derive(Default)]
pub(crate) struct Pools {
    backend: RdmaBackend,
    active: AsyncMutex<Option<Arc<Pool>>>,
}

impl Pools {
    pub(crate) fn new(backend: RdmaBackend) -> Self {
        Self {
            backend,
            active: AsyncMutex::default(),
        }
    }

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
                    && pool.size >= size
                    && *pool.lost.borrow() < WINDOWS_PER_POOL
                {
                    pool.clone()
                } else {
                    let backend = self.backend;
                    let pool = tokio::task::spawn_blocking(move || Pool::open(backend, size))
                        .await
                        .map_err(|_| {
                            Error::General("RDMA registration was interrupted".into())
                        })??;
                    *active = Some(pool.clone());
                    pool
                }
            };
            if let Some(window) = pool.reserve().await? {
                return Ok(window);
            }
        }
    }
}

struct Pool {
    size: usize,
    free: Mutex<Vec<Buffer>>,
    available: Semaphore,
    lost: watch::Sender<usize>,
    _budget: SemaphorePermit<'static>,
}

impl Pool {
    fn open(backend: RdmaBackend, size: usize) -> Result<Arc<Self>> {
        let budget = AGENTS
            .try_acquire()
            .map_err(|_| Error::General("RDMA registered memory budget exhausted".into()))?;
        let buffers = buffer::register(backend, size, WINDOWS_PER_POOL)?;
        Ok(Arc::new(Self {
            size,
            free: Mutex::new(buffers),
            available: Semaphore::new(WINDOWS_PER_POOL),
            lost: watch::Sender::new(0),
            _budget: budget,
        }))
    }

    async fn reserve(self: &Arc<Self>) -> Result<Option<Window>> {
        let mut lost = self.lost.subscribe();
        tokio::select! {
            biased;
            _ = lost.wait_for(|count| *count == WINDOWS_PER_POOL) => Ok(None),
            result = tokio::time::timeout(Duration::from_secs(60), self.available.acquire()) => {
                let permit = result
                    .map_err(|_| Error::General("Timed out waiting for a registered window".into()))?
                    .expect("registration semaphore remains open");
                let buffer = self.free.lock().unwrap_or_else(|e| e.into_inner())
                    .pop().expect("each permit owns a free window");
                permit.forget();
                Ok(Some(Window { buffer: Some(buffer), pool: self.clone(), len: 0 }))
            }
        }
    }
}

pub(crate) struct Window {
    buffer: Option<Buffer>,
    pool: Arc<Pool>,
    len: usize,
}
impl Window {
    pub(crate) fn writable(&mut self) -> Result<&mut [u8]> {
        self.buffer.as_mut().expect("owned window").writable()
    }
    pub(crate) fn received(&mut self) -> Result<&[u8]> {
        let len = self.len;
        self.writable()?
            .get(..len)
            .ok_or_else(|| Error::General("Received length exceeds the registered window".into()))
    }

    pub(crate) fn finish_read(&mut self, len: usize) -> Result<()> {
        if len > self.pool.size {
            return Err(Error::General(
                "Received length exceeds the registered window".into(),
            ));
        }
        self.len = len;
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn expose(
        self,
        _method: &Method,
        len: usize,
        headers: &mut HeaderMap,
    ) -> Result<Exposed> {
        let mut exposed = Exposed(Some(self));
        let buffer = exposed.window_mut().buffer.as_mut().expect("owned window");
        let result = match buffer {
            Buffer::Nixl {
                chunk,
                agent,
                metadata,
            } => crate::wire::bind(headers, agent, metadata, chunk.addr(), len),
            #[cfg(feature = "cuobject")]
            Buffer::CuObject(buffer) => buffer.describe(_method, len, headers),
        };
        if let Err(error) = result {
            // The token has not left this process. Release it before reuse.
            exposed.finished()?;
            return Err(error);
        }
        Ok(exposed)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            self.pool
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(buffer);
            self.pool.available.add_permits(1);
        }
    }
}

pub(crate) struct Exposed(Option<Window>);
impl Exposed {
    fn window_mut(&mut self) -> &mut Window {
        self.0.as_mut().expect("owned transfer")
    }

    pub(crate) fn finished(mut self) -> Result<Window> {
        #[cfg(feature = "cuobject")]
        if let Some(Buffer::CuObject(host)) = self.window_mut().buffer.as_mut() {
            host.release_token()?;
        }
        Ok(self.0.take().expect("owned transfer"))
    }
}
impl Drop for Exposed {
    fn drop(&mut self) {
        if let Some(window) = self.0.take() {
            window.pool.lost.send_modify(|lost| *lost += 1);
            QUARANTINE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uncertain_windows_wake_waiters_and_cannot_be_reissued() {
        check_window_lifetime(RdmaBackend::Nixl).await;
    }

    #[cfg(feature = "cuobject")]
    #[tokio::test]
    #[ignore = "requires an NVIDIA cuObject client SDK and an active RDMA device"]
    async fn cuobject_token_survives_cancellation_and_timeout() {
        check_window_lifetime(RdmaBackend::CuObject).await;
    }

    fn observe(exposed: Exposed) -> (Exposed, Option<Arc<std::sync::atomic::AtomicUsize>>) {
        #[cfg(feature = "cuobject")]
        {
            let mut exposed = exposed;
            let release = match exposed.window_mut().buffer.as_mut() {
                Some(Buffer::CuObject(host)) => {
                    assert!(host.writable().is_err());
                    assert!(
                        host.describe(&Method::GET, 1, &mut HeaderMap::new())
                            .is_err()
                    );
                    Some(host.releases.clone())
                }
                _ => None,
            };
            (exposed, release)
        }
        #[cfg(not(feature = "cuobject"))]
        (exposed, None)
    }

    async fn check_window_lifetime(backend: RdmaBackend) {
        let pools = Pools::new(backend);
        let first = pools.reserve(32).await.unwrap();
        let second = pools.reserve(32).await.unwrap();
        let old = first.pool.clone();
        assert!(Arc::ptr_eq(&old, &second.pool));
        let waiting = old.reserve();
        tokio::pin!(waiting);
        tokio::select! {
            biased;
            _ = &mut waiting => panic!("both windows are checked out"),
            _ = std::future::ready(()) => {}
        }
        let mut headers = HeaderMap::new();
        let (first, first_releases) =
            observe(first.expose(&Method::GET, 32, &mut headers).unwrap());
        #[cfg(feature = "cuobject")]
        if backend == RdmaBackend::CuObject {
            assert!(headers[crate::nvidia::TOKEN_HEADER].is_sensitive());
        }
        let exchanging = |exposed: Exposed| async move {
            std::future::pending::<()>().await;
            exposed.finished()
        };
        let (started, ready) = tokio::sync::oneshot::channel();
        let cancelled = tokio::spawn(async move {
            started.send(()).unwrap();
            exchanging(first).await
        });
        ready.await.unwrap();
        cancelled.abort();
        assert!(matches!(cancelled.await, Err(e) if e.is_cancelled()));
        let mut put_headers = HeaderMap::new();
        let second = second.expose(&Method::PUT, 32, &mut put_headers).unwrap();
        let (second, second_releases) = observe(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), exchanging(second))
                .await
                .is_err()
        );
        tokio::select! {
            biased;
            result = waiting => assert!(result.unwrap().is_none()),
            _ = std::future::ready(()) => panic!("retiring a pool must wake its waiters immediately"),
        }
        #[cfg(feature = "cuobject")]
        if backend == RdmaBackend::CuObject {
            let retained = QUARANTINE.lock().unwrap();
            for advertised in [
                &headers[crate::nvidia::TOKEN_HEADER],
                &put_headers[crate::nvidia::TOKEN_HEADER],
            ] {
                assert!(retained.iter().any(|window| Arc::ptr_eq(&window.pool, &old)
                    && matches!(&window.buffer, Some(Buffer::CuObject(host)) if host.holds_token(advertised))));
            }
        }
        let replacement = pools.reserve(32).await.unwrap();
        assert!(!Arc::ptr_eq(&old, &replacement.pool));
        let fresh_pool = replacement.pool.clone();
        let (completed, completed_releases) = observe(
            replacement
                .expose(&Method::GET, 32, &mut HeaderMap::new())
                .unwrap(),
        );
        let returned = completed.finished().unwrap();
        if let Some(count) = &completed_releases {
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        drop(returned);
        if backend == RdmaBackend::CuObject {
            for counter in [first_releases, second_releases] {
                assert_eq!(
                    counter.unwrap().load(std::sync::atomic::Ordering::SeqCst),
                    0
                );
            }
            assert_eq!(
                completed_releases
                    .unwrap()
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        }
        assert_eq!(fresh_pool.available.available_permits(), WINDOWS_PER_POOL);
        assert_eq!(old.available.available_permits(), 0);
    }
}

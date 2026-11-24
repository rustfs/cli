//! Bounded registration and exclusive ownership until a transfer completes.

use crate::buffer::{self, Buffer};
use http::{HeaderMap, Method};
use rc_core::{Error, RdmaBackend, Result};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, SemaphorePermit, watch};

const WINDOW_WAIT: Duration = Duration::from_secs(60);
pub(crate) const WINDOW: usize = rc_s3::multipart::DEFAULT_PART_SIZE as usize;
const SMALL_WINDOW: usize = 1024 * 1024;
const WINDOWS_PER_POOL: usize = 2;
static AGENTS: Semaphore = Semaphore::const_new(8);
// Keep the exposed registration and its budget, so the retired pool can free its idle buffers.
pub(super) type Budget = Arc<SemaphorePermit<'static>>;
static QUARANTINE: Mutex<Vec<(Buffer, Budget)>> = Mutex::new(Vec::new());

fn window_size(required: u64) -> usize {
    let required = required.clamp(SMALL_WINDOW as u64, WINDOW as u64) as usize;
    required.next_power_of_two().min(WINDOW)
}

#[derive(Default)]
struct ActivePool {
    pool: Option<Arc<Pool>>,
    // A failed wide registration keeps this size and larger sizes on the original
    // two-window policy for this transport's lifetime.
    narrow_from: Option<usize>,
}

pub(crate) struct Pools {
    backend: RdmaBackend,
    active: AsyncMutex<ActivePool>,
    ordinary_windows: Arc<Semaphore>,
}

impl Default for Pools {
    fn default() -> Self {
        Self::new(RdmaBackend::default())
    }
}

impl Pools {
    pub(crate) fn new(backend: RdmaBackend) -> Self {
        Self {
            backend,
            active: AsyncMutex::default(),
            ordinary_windows: Arc::new(Semaphore::new(WINDOWS_PER_POOL)),
        }
    }

    pub(crate) async fn reserve(&self, required: u64) -> Result<Window> {
        self.reserve_with_capacity(required, WINDOWS_PER_POOL, WINDOW_WAIT, Pool::open)
            .await
    }

    pub(crate) async fn reserve_read(&self, required: u64, object_size: u64) -> Result<Window> {
        let capacity = if self.backend == RdmaBackend::Nixl
            && required == object_size
            && matches!(object_size, 4_194_304 | 8_388_608 | 16_777_216)
        {
            4
        } else {
            WINDOWS_PER_POOL
        };
        self.reserve_with_capacity(required, capacity, WINDOW_WAIT, Pool::open)
            .await
    }

    async fn reserve_with_capacity(
        &self,
        required: u64,
        capacity: usize,
        mut wait: Duration,
        open: fn(RdmaBackend, usize, usize, Budget) -> Result<Arc<Pool>>,
    ) -> Result<Window> {
        let size = window_size(required);
        let ordinary = if self.backend == RdmaBackend::Nixl && capacity == WINDOWS_PER_POOL {
            let started = Instant::now();
            let permit = tokio::time::timeout(wait, self.ordinary_windows.clone().acquire_owned())
                .await
                .map_err(|_| Error::General("Timed out waiting for a registered window".into()))?
                .expect("ordinary window semaphore remains open");
            wait = wait.saturating_sub(started.elapsed());
            Some(permit)
        } else {
            None
        };
        loop {
            let pool = {
                let mut active = self.active.lock().await;
                let capacity = if active.narrow_from.is_some_and(|limit| size >= limit) {
                    WINDOWS_PER_POOL
                } else {
                    capacity
                };
                let compatible = active
                    .pool
                    .as_ref()
                    .filter(|pool| pool.size >= size && *pool.lost.borrow() < pool.capacity)
                    .cloned();
                if let Some(pool) = compatible.as_ref().filter(|pool| pool.capacity >= capacity) {
                    pool.clone()
                } else if let Ok(permit) = AGENTS.try_acquire() {
                    let backend = self.backend;
                    let (opened, wide_failed) = tokio::task::spawn_blocking(move || {
                        let budget = Arc::new(permit);
                        match open(backend, size, capacity, budget.clone()) {
                            Err(wide) if capacity > WINDOWS_PER_POOL => {
                                let narrow = match compatible {
                                    Some(pool) => Ok(pool),
                                    None => open(backend, size, WINDOWS_PER_POOL, budget).map_err(|narrow| {
                                        Error::General(format!("Could not register four RDMA windows ({wide}) or two ({narrow})"))
                                    }),
                                };
                                (narrow, true)
                            }
                            result => (result, false),
                        }
                    })
                    .await
                    .map_err(|_| Error::General("RDMA registration was interrupted".into()))?;
                    if wide_failed {
                        active.narrow_from =
                            Some(active.narrow_from.map_or(size, |limit| limit.min(size)));
                    }
                    let pool = opened?;
                    active.pool = Some(pool.clone());
                    pool
                } else if let Some(pool) = compatible {
                    // A wider read registration is optional when the process already owns eight.
                    pool.clone()
                } else {
                    return Err(Error::General(
                        "RDMA registered memory budget exhausted".into(),
                    ));
                }
            };
            if let Some(mut window) = pool.reserve(wait).await? {
                window._ordinary = ordinary;
                return Ok(window);
            }
        }
    }
}

struct Pool {
    size: usize,
    capacity: usize,
    free: Mutex<Vec<Buffer>>,
    available: Semaphore,
    lost: watch::Sender<usize>,
    budget: Budget,
}

impl Pool {
    fn open(
        backend: RdmaBackend,
        size: usize,
        capacity: usize,
        budget: Budget,
    ) -> Result<Arc<Self>> {
        let buffers = buffer::register(backend, size, capacity, &budget)?;
        Ok(Arc::new(Self {
            size,
            capacity,
            free: Mutex::new(buffers),
            available: Semaphore::new(capacity),
            lost: watch::Sender::new(0),
            budget,
        }))
    }

    async fn reserve(self: &Arc<Self>, wait: Duration) -> Result<Option<Window>> {
        let mut lost = self.lost.subscribe();
        tokio::select! {
            biased;
            _ = lost.wait_for(|count| *count == self.capacity) => Ok(None),
            result = tokio::time::timeout(wait, self.available.acquire()) => {
                let permit = result
                    .map_err(|_| Error::General("Timed out waiting for a registered window".into()))?
                    .expect("registration semaphore remains open");
                let buffer = self.free.lock().unwrap_or_else(|e| e.into_inner())
                    .pop().expect("each permit owns a free window");
                permit.forget();
                Ok(Some(Window { buffer: Some(buffer), pool: self.clone(), len: 0, _ordinary: None }))
            }
        }
    }
}

pub(crate) struct Window {
    _ordinary: Option<OwnedSemaphorePermit>,
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
        if let Some(mut window) = self.0.take() {
            let buffer = window.buffer.take().expect("owned transfer");
            window.pool.lost.send_modify(|lost| *lost += 1);
            QUARANTINE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((buffer, Arc::clone(&window.pool.budget)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_registration_preserves_both_causes_and_suppresses_wide_retries() {
        static OPENS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        fn reject(_: RdmaBackend, _: usize, capacity: usize, _: Budget) -> Result<Arc<Pool>> {
            OPENS.lock().unwrap().push(capacity);
            Err(Error::General(format!("denied {capacity} windows")))
        }
        let pools = Pools::new(RdmaBackend::Nixl);
        let both = pools
            .reserve_with_capacity(8 << 20, 4, WINDOW_WAIT, reject)
            .await;
        assert!(
            matches!(both, Err(Error::General(e)) if e.contains("denied 4 windows") && e.contains("denied 2 windows"))
        );
        for size in [8 << 20, 16 << 20] {
            let failed = pools
                .reserve_with_capacity(size, 4, WINDOW_WAIT, reject)
                .await;
            assert!(matches!(failed, Err(Error::General(e)) if e == "denied 2 windows"));
        }
        let smaller = pools
            .reserve_with_capacity(4 << 20, 4, WINDOW_WAIT, reject)
            .await;
        assert!(smaller.is_err());
        assert_eq!(*OPENS.lock().unwrap(), [4, 2, 2, 2, 4, 2]);
        assert!(pools.active.lock().await.pool.is_none());
    }

    #[tokio::test]
    #[ignore = "requires the native NIXL runtime and an active RDMA device"]
    async fn failed_wide_registration_reuses_or_creates_a_real_two_window_pool() {
        static OPENS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        fn reject_wide(
            backend: RdmaBackend,
            size: usize,
            capacity: usize,
            budget: Budget,
        ) -> Result<Arc<Pool>> {
            OPENS.lock().unwrap().push(capacity);
            if capacity == 4 {
                Err(Error::General("simulated registration limit".to_owned()))
            } else {
                Pool::open(backend, size, capacity, budget)
            }
        }
        let size = 16 << 20;
        for existing in [false, true] {
            OPENS.lock().unwrap().clear();
            let pools = Pools::new(RdmaBackend::Nixl);
            let mut old = if existing {
                Some(pools.reserve(size).await.unwrap())
            } else {
                None
            };
            if let Some(window) = old.as_mut() {
                window.writable().unwrap()[0] = 27;
            }
            let read = pools
                .reserve_with_capacity(size, 4, WINDOW_WAIT, reject_wide)
                .await
                .unwrap();
            assert_eq!(read.pool.capacity, 2);
            let baseline = read.pool.clone();
            if let Some(window) = old.as_mut() {
                assert!(Arc::ptr_eq(&window.pool, &baseline));
                assert_eq!(window.writable().unwrap()[0], 27);
            }
            drop((old, read));
            let next = pools
                .reserve_with_capacity(size, 4, WINDOW_WAIT, reject_wide)
                .await
                .unwrap();
            assert!(Arc::ptr_eq(&baseline, &next.pool));
            let other = pools.reserve(size).await.unwrap();
            assert!(Arc::ptr_eq(&baseline, &other.pool));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), pools.reserve_read(size, size))
                    .await
                    .is_err()
            );
            let calls = OPENS.lock().unwrap();
            assert_eq!(
                calls.as_slice(),
                if existing { &[4][..] } else { &[4, 2][..] }
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires the native NIXL runtime and an active RDMA device"]
    async fn full_object_reads_share_one_pool_with_two_ordinary_windows() {
        let size = 4 << 20;
        let pools = Pools::new(RdmaBackend::Nixl);
        let mut old_put = pools.reserve(size).await.unwrap();
        assert_eq!(old_put.pool.capacity, 2);
        old_put.writable().unwrap()[0] = 37;
        let mut reads = Vec::new();
        for _ in 0..4 {
            reads.push(pools.reserve_read(size, size).await.unwrap());
        }
        assert!(
            reads
                .iter()
                .all(|w| w.pool.capacity == 4 && Arc::ptr_eq(&reads[0].pool, &w.pool))
        );
        let mut addresses: Vec<_> = reads
            .iter_mut()
            .map(|w| w.writable().unwrap().as_ptr())
            .collect();
        addresses.sort();
        addresses.dedup();
        assert_eq!(addresses.len(), 4);
        assert_eq!(old_put.writable().unwrap()[0], 37);
        assert!(!Arc::ptr_eq(&reads[0].pool, &old_put.pool));
        let expiry = Duration::from_millis(30);
        assert!(
            tokio::time::timeout(expiry, pools.reserve_read(size, size))
                .await
                .is_err()
        );
        let active = Arc::downgrade(&reads[0].pool);
        reads.clear();
        let second_put = pools.reserve(size).await.unwrap();
        assert!(Arc::ptr_eq(&second_put.pool, &active.upgrade().unwrap()));
        assert!(
            tokio::time::timeout(expiry, pools.reserve(size))
                .await
                .is_err()
        );
        let exhausted = pools
            .reserve_with_capacity(size, WINDOWS_PER_POOL, expiry, Pool::open)
            .await;
        assert!(
            matches!(exhausted, Err(Error::General(msg)) if msg == "Timed out waiting for a registered window")
        );
        drop(old_put);
        let partial_read = pools.reserve_read(size, 32 << 20).await.unwrap();
        assert!(
            tokio::time::timeout(expiry, pools.reserve(size))
                .await
                .is_err()
        );
        drop((partial_read, second_put));
        for size in [8 << 20, 16 << 20] {
            let w = pools.reserve_read(size, size).await.unwrap();
            assert_eq!(w.pool.capacity, 4);
            assert_eq!(w.pool.size, size as usize);
        }
        drop(pools);
        assert!(active.upgrade().is_none());
    }

    #[tokio::test]
    #[ignore = "requires exclusive access to the native registration budget"]
    async fn quota_preserves_existing_buffers_and_quarantine_retires_all_four() {
        let size = 4 << 20;
        let pools = Pools::new(RdmaBackend::Nixl);
        let ordinary = pools.reserve(size).await.unwrap();
        let first = Arc::downgrade(&ordinary.pool);
        drop(ordinary);
        let permits = AGENTS.available_permits();
        let held = AGENTS
            .try_acquire_many(permits.try_into().unwrap())
            .unwrap();
        let fallback = pools.reserve_read(size, size).await.unwrap();
        assert_eq!(fallback.pool.capacity, 2);
        assert!(Arc::ptr_eq(&first.upgrade().unwrap(), &fallback.pool));
        assert!(pools.reserve_read(size * 2, size * 2).await.is_err());
        drop((fallback, held));
        let mut exposed = Vec::new();
        for _ in 0..4 {
            let window = pools.reserve_read(size, size).await.unwrap();
            exposed.push(Exposed(Some(window)));
        }
        let old = Arc::clone(&exposed[0].0.as_ref().unwrap().pool);
        assert!(first.upgrade().is_none());
        let old_addresses: Vec<_> = exposed
            .iter_mut()
            .map(|x| x.window_mut().writable().unwrap().as_ptr() as usize)
            .collect();
        exposed.pop();
        assert_eq!(*old.lost.borrow(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), pools.reserve_read(size, size))
                .await
                .is_err()
        );
        drop(exposed);
        assert_eq!(*old.lost.borrow(), 4);
        let mut fresh = pools.reserve_read(size, size).await.unwrap();
        assert!(!Arc::ptr_eq(&fresh.pool, &old));
        assert!(!old_addresses.contains(&(fresh.writable().unwrap().as_ptr() as usize)));
        let retained = QUARANTINE.lock().unwrap();
        assert_eq!(
            retained
                .iter()
                .filter(|(_, budget)| Arc::ptr_eq(budget, &old.budget))
                .count(),
            4
        );
    }
    #[test]
    fn registrations_use_a_bounded_size_class() {
        assert_eq!(window_size(0), SMALL_WINDOW);
        assert_eq!(window_size(SMALL_WINDOW as u64), SMALL_WINDOW);
        assert_eq!(window_size(3 * SMALL_WINDOW as u64), 4 * SMALL_WINDOW);
        assert_eq!(window_size(u64::MAX), WINDOW);
    }

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
        let waiting = old.reserve(WINDOW_WAIT);
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
                assert!(retained.iter().any(|(buffer, budget)| Arc::ptr_eq(budget, &old.budget)
                    && matches!(buffer, Buffer::CuObject(host) if host.holds_token(advertised))));
            }
        }
        let replacement = pools.reserve(3 * SMALL_WINDOW as u64).await.unwrap();
        assert_eq!(replacement.pool.size, 4 * SMALL_WINDOW);
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

        // Retiring a pool with one uncertain transfer must free its idle sibling.
        let window = pools.reserve(32).await.unwrap();
        let pool = Arc::downgrade(&fresh_pool);
        let budget = Arc::downgrade(&fresh_pool.budget);
        drop(
            window
                .expose(&Method::GET, 32, &mut HeaderMap::new())
                .unwrap(),
        );
        drop(fresh_pool);
        drop(pools);
        assert!(pool.upgrade().is_none());
        assert!(budget.upgrade().is_some());
    }
}

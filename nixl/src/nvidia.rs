//! Direct NVIDIA client registrations for the shared S3 transfer pipeline.

use crate::buffer::Buffer;
use http::{HeaderMap, HeaderValue, Method};
use rc_core::{Error, Result};
use rustfs_cuobject_client::{Client, Operation, Window};

pub(super) const TOKEN_HEADER: &str = "x-amz-rdma-token";

fn sdk_error(error: std::io::Error) -> Error {
    Error::General(format!("NVIDIA cuObject: {error}"))
}

pub(super) fn register(
    size: usize,
    count: usize,
    budget: &crate::pool::Budget,
) -> Result<Vec<Buffer>> {
    let client = Client::open().map_err(sdk_error)?;
    (0..count)
        .map(|_| {
            let raw = client.register(size, budget.clone()).map_err(sdk_error)?;
            Ok(Buffer::CuObject(HostBuffer {
                raw,
                #[cfg(test)]
                releases: Default::default(),
            }))
        })
        .collect()
}

pub(super) struct HostBuffer {
    raw: Window,
    #[cfg(test)]
    pub(super) releases: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl HostBuffer {
    pub(super) fn writable(&mut self) -> Result<&mut [u8]> {
        self.raw.writable().map_err(sdk_error)
    }

    pub(super) fn describe(
        &mut self,
        method: &Method,
        len: usize,
        headers: &mut HeaderMap,
    ) -> Result<()> {
        let operation = match *method {
            Method::GET => Operation::Get,
            Method::PUT => Operation::Put,
            _ => return Err(Error::General("Unsupported cuObject operation".into())),
        };
        let token = self.raw.acquire_token(operation, len).map_err(sdk_error)?;
        let mut value = HeaderValue::from_bytes(token)
            .map_err(|_| Error::General("Invalid NVIDIA cuObject token".into()))?;
        value.set_sensitive(true);
        headers.insert(TOKEN_HEADER, value);
        Ok(())
    }

    pub(super) fn release_token(&mut self) -> Result<()> {
        #[cfg(test)]
        let active = !self.raw.active_token().is_empty();
        self.raw.release_after_completion().map_err(sdk_error)?;
        #[cfg(test)]
        if active {
            self.releases
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn holds_token(&self, advertised: &HeaderValue) -> bool {
        self.raw.active_token() == advertised.as_bytes()
    }
}

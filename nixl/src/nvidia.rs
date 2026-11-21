//! Direct NVIDIA client registrations for the shared S3 transfer pipeline.

use crate::buffer::Buffer;
use http::{HeaderMap, HeaderValue, Method};
use rc_core::{Error, Result};

pub(super) const TOKEN_HEADER: &str = "x-amz-rdma-token";

#[expect(
    unsafe_code,
    reason = "private SDK declarations and native window thread ownership"
)]
mod sdk;
use sdk::ffi;

fn sdk_error(error: cxx::Exception) -> Error {
    Error::General(format!("NVIDIA cuObject: {error}"))
}

pub(super) fn register(size: usize, count: usize) -> Result<Vec<Buffer>> {
    let client = ffi::make_client().map_err(sdk_error)?;
    (0..count)
        .map(|_| {
            let raw = ffi::register_window(client.clone(), size).map_err(sdk_error)?;
            Ok(Buffer::CuObject(HostBuffer {
                raw,
                #[cfg(test)]
                releases: Default::default(),
            }))
        })
        .collect()
}

pub(super) struct HostBuffer {
    raw: cxx::UniquePtr<ffi::NativeWindow>,
    #[cfg(test)]
    pub(super) releases: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl HostBuffer {
    pub(super) fn writable(&mut self) -> Result<&mut [u8]> {
        self.raw.pin_mut().writable().map_err(sdk_error)
    }

    pub(super) fn describe(
        &mut self,
        method: &Method,
        len: usize,
        headers: &mut HeaderMap,
    ) -> Result<()> {
        let for_get = match *method {
            Method::GET => true,
            Method::PUT => false,
            _ => return Err(Error::General("Unsupported cuObject operation".into())),
        };
        self.raw
            .pin_mut()
            .acquire(len, for_get)
            .map_err(sdk_error)?;
        let mut value = HeaderValue::from_bytes(self.raw.active_token())
            .map_err(|_| Error::General("Invalid NVIDIA cuObject token".into()))?;
        value.set_sensitive(true);
        headers.insert(TOKEN_HEADER, value);
        Ok(())
    }

    pub(super) fn release_token(&mut self) -> Result<()> {
        #[cfg(test)]
        let active = !self.raw.active_token().is_empty();
        self.raw.pin_mut().release_token().map_err(sdk_error)?;
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

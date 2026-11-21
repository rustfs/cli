//! NVIDIA DC token ownership and registered host memory.

use crate::buffer::Buffer;
use crate::pool::Window;
use cuobject::{Client, ClientBuffer, ClientToken, Operation};
use http::{HeaderMap, HeaderValue, Method};
use rc_core::{Error, Result};
use std::sync::Arc;

fn sdk_error(error: cuobject::Error) -> Error {
    Error::General(format!("NVIDIA cuObject: {error}"))
}

pub(super) fn register(size: usize, count: usize) -> Result<Vec<Buffer>> {
    let client = Arc::new(Client::new().map_err(sdk_error)?);
    (0..count)
        .map(|_| {
            let buffer = ClientBuffer::new(client.clone(), size).map_err(sdk_error)?;
            Ok(Buffer::CuObject(HostBuffer(buffer)))
        })
        .collect()
}

pub(super) struct HostBuffer(ClientBuffer<Arc<Client>>);

type Token<'a> = ClientToken<'a, Arc<Client>>;

// Dropping the token releases its SDK QP reference. Keep it with the memory
// registration through cancellation, timeout, and any unconfirmed receipt.
self_cell::self_cell!(
    pub(crate) struct Exposure {
        owner: Window,
        #[covariant]
        dependent: Token,
    }
);

impl HostBuffer {
    #[expect(
        unsafe_code,
        reason = "the SDK exposes registered host memory through raw slices"
    )]
    pub(super) fn writable(&mut self) -> Result<&mut [u8]> {
        // SAFETY: ClientBuffer::new owns this entire host mapping. A Window
        // gives the caller exclusive access. Exposing it moves the Window
        // and keeps the token alive until the peer confirms completion or
        // declines the transfer. An uncertain transfer is never reissued.
        unsafe { self.0.slice_mut(0, self.0.len()) }.map_err(sdk_error)
    }

    pub(super) fn describe(
        &self,
        method: &Method,
        len: usize,
        headers: &mut HeaderMap,
    ) -> Result<Token<'_>> {
        let operation = match *method {
            Method::GET => Operation::Get,
            Method::PUT => Operation::Put,
            _ => return Err(Error::General("Unsupported cuObject operation".into())),
        };
        let token = self.0.token(0, len, operation).map_err(sdk_error)?;
        let mut value = HeaderValue::from_str(token.as_str())
            .map_err(|_| Error::General("Invalid NVIDIA cuObject token".into()))?;
        value.set_sensitive(true);
        headers.insert(cuobject::TOKEN_HEADER, value);
        Ok(token)
    }
}

//! Registered host windows for NVIDIA cuObject transfers.
#![cfg(feature = "client")]
#![forbid(unsafe_code)]

use rustfs_cuobject_client_sys as sys;
use std::io;

/// A native client shared by the windows it registers.
pub struct Client(sys::ClientHandle);

impl Client {
    pub fn open() -> io::Result<Self> {
        sys::make_client().map(Self).map_err(io::Error::other)
    }

    /// Registers a host mapping that keeps its native client alive.
    pub fn register(&self, bytes: usize) -> io::Result<Window> {
        sys::register_window(self.0.clone(), bytes)
            .map(Window)
            .map_err(io::Error::other)
    }
}

/// The operation for which a remote endpoint can use a token.
pub enum Operation {
    Get,
    Put,
}

/// A registered host buffer. Dropping a window with an active or uncertain
/// token keeps its native mapping and registration alive.
pub struct Window(sys::WindowHandle);

impl Window {
    /// Borrows the registered bytes when the window has no active token.
    pub fn writable(&mut self) -> io::Result<&mut [u8]> {
        self.0.pin_mut().writable().map_err(io::Error::other)
    }

    /// Exposes the first `length` bytes for one transfer and excludes host access.
    pub fn acquire_token(&mut self, operation: Operation, length: usize) -> io::Result<&[u8]> {
        self.0
            .pin_mut()
            .acquire(length, matches!(operation, Operation::Get))
            .map_err(io::Error::other)?;
        Ok(self.active_token())
    }

    pub fn active_token(&self) -> &[u8] {
        self.0.active_token()
    }

    /// Releases the token after confirmed completion or proof that DMA never
    /// started. An uncertain exchange must keep the token and window together.
    pub fn release_after_completion(&mut self) -> io::Result<()> {
        self.0.pin_mut().release_token().map_err(io::Error::other)
    }
}

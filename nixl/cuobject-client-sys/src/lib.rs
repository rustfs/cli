//! Native bindings for NVIDIA cuObject client registrations.
#![cfg(feature = "client")]
#![deny(unsafe_code)]

#[expect(
    unsafe_code,
    reason = "NVIDIA SDK declarations and native window thread ownership"
)]
mod sdk;

pub use sdk::ffi::{make_client, register_window};
pub type ClientHandle = cxx::SharedPtr<sdk::ffi::Client>;
pub type WindowHandle = cxx::UniquePtr<sdk::ffi::NativeWindow>;

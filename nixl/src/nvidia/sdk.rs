//! Private interface to the NVIDIA client and its registered host windows.

#[cxx::bridge(namespace = "rc::cuobject")]
pub(super) mod ffi {
    // SAFETY: every mutable view borrows its pinned window. An outstanding
    // token excludes host access until the SDK confirms its release.
    unsafe extern "C++" {
        include!("native/cuobject.h");
        type Client;
        type NativeWindow;

        fn make_client() -> Result<SharedPtr<Client>>;
        fn register_window(
            client: SharedPtr<Client>,
            size: usize,
        ) -> Result<UniquePtr<NativeWindow>>;
        fn writable(self: Pin<&mut NativeWindow>) -> Result<&mut [u8]>;
        fn acquire(self: Pin<&mut NativeWindow>, length: usize, for_get: bool) -> Result<()>;
        fn active_token(self: &NativeWindow) -> &[u8];
        fn release_token(self: Pin<&mut NativeWindow>) -> Result<()>;
    }
}

// SAFETY: transferring ownership between workers neither moves the mapping nor
// exposes it twice. The native client serializes SDK calls shared by windows.
unsafe impl Send for ffi::NativeWindow {}

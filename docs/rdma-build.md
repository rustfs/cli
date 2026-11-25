# Object data over RDMA

## Build the native client

Build on Linux with Rust 1.99 or later, libclang, a C++ compiler and the
NIXL libraries, headers and UCX plugin used by the RustFS server. Set
`NIXL_PREFIX` to the native installation before building. At runtime set
`LD_LIBRARY_PATH` to include its library directory and `NIXL_PLUGIN_DIR` to
its plugin directory. Set `UCX_NET_DEVICES` to select client network devices
when needed. The native manifest pins the public RustFS library revision, and its lockfile
pins the corresponding NIXL bindings.

```sh
./scripts/build-rdma.sh --release
export PATH="$PWD/nixl/target/release:$PATH"
rc --data-transport rdma cp model.bin local/models/model.bin
rc --data-transport rdma cp local/models/model.bin downloaded.bin
```

The executable is `nixl/target/release/rc` with Cargo's default target directory.
The build script forwards Cargo options. Set `RUSTFS_RDMA_TOOLCHAIN` to select
an installed Rust toolchain or `CARGO_BUILD_JOBS` to limit parallelism.

Configure a verified HTTPS alias and start RustFS with its `nixl` Cargo
feature and `RUSTFS_NIXL_ENABLE=true`. For a private CA, pass
`--ca-bundle /path/to/ca.pem` to `rc alias set`. Set `RC_DATA_TRANSPORT=auto`
or `rdma` to reuse the selection across commands. The default is `http`. Signed control requests use the alias connection and
read timeouts, including the wait for each transfer receipt.

### Choose the RDMA backend

The native executable uses NIXL/UCX by default. To add NVIDIA cuObject
to the same executable, install the NVIDIA cuObject **client** SDK,
cuFile development libraries, libibverbs development headers and CUDA headers
for the target architecture.
Set `CUOBJCLIENT_ROOT` to the SDK prefix and `CUDA_HOME` to the CUDA toolkit.
The `rustfs-cuobject-client-sys` crate builds the C++17 adapter against `cuobjclient.h` and links the NVIDIA SDK. The RDMA provider accesses registered windows and tokens through `rustfs-cuobject-client`.
The SDK's `libcuobjclient`, `libcufile` and RDMA plugin must be available
at runtime. Set `CUOBJ_ENV_PATH_JSON` to the NVIDIA client configuration
that selects the application RDMA device.

```sh
./scripts/build-rdma.sh --release --features cuobject
rc --data-transport rdma --rdma-backend cuobject put model.bin lab/models/model.bin
rc --data-transport rdma --rdma-backend cuobject get lab/models/model.bin downloaded.bin
cmp model.bin downloaded.bin
rc --data-transport rdma --rdma-backend nixl get other/models/model.bin from-nixl.bin
```

Configure `lab` as a verified HTTPS alias for a RustFS server built with
`--features cuobject` and `RUSTFS_CUOBJECT_ENABLE=true`. Use an alias for a
NIXL-enabled server with the `nixl` backend. `RC_RDMA_BACKEND` sets the
backend for subsequent commands. Request headers in the `x-amz-rdma-*` and
`x-rustfs-nixl-*` namespaces are reserved for the selected provider. The `rdma` policy requires the selected
backend. The `auto` policy allows a safe S3 fallback when that backend is
unavailable. The default `http` policy transfers the payload through S3.

### Object transfers

File uploads use positional reads directly into two reusable registered
windows. Each completed part frees a window for the next part, and S3 receives
the completion list in part-number order. A new pool sizes each window to the
next power of two that fits the requested part or range, with a 1 MiB minimum
and a 64 MiB maximum. For example, a 4 MiB range registers two 4 MiB windows.
A client can reuse its existing larger pool for subsequent small requests.
For large downloads, NIXL and cuObject issue up to two adjacent range GETs
concurrently. Each request uses the caller's version selector and `If-Match`.
The CLI checks each response against the version observed by HEAD and the requested range,
consumes the results in byte order, and writes directly to the destination
stream or temporary file. File downloads publish the temporary file after
verifying the transfer count and any applicable object checksum.
A verified receipt releases the window. A local rejection before sending or
an authenticated S3 `PreconditionFailed` response to an `If-Match` GET also
releases it. For the latter, the client requires HTTP 412, a bounded S3
error document and no RDMA response headers. An ambiguous response, a
missing receipt, cancellation or timeout retains the token and window until
the process exits.
When both windows are unavailable for this reason, waiting operations use a
fresh pool within the process registration budget. Retiring a cuObject pool
releases its idle registrations. Each quarantined window keeps its own
registration and the pool's budget reservation. A native registration also
keeps this reservation if the SDK cannot confirm deregistration.
For the rest of the process, an ordinary HTTP response without an RDMA
receipt disables further probes for the same endpoint, backend and operation
(GET or UploadPart). Concurrent requests share the initial discovery.

RustFS can interpret a declined external PUT as an ordinary empty PUT.
The client uploads nonempty objects into unpublished multipart parts and
completes the upload only after RustFS confirms every part. This requires
multipart permissions and produces multipart ETags even for a single part.
In `auto` mode a one-byte probe of the new upload precedes reading the
source. A normal HTTP response to that probe aborts the temporary upload
and selects the ordinary S3 path. Empty uploads use ordinary S3. Uploads
with an explicit S3 checksum, SSE-C, a storage class or a required part
size above 64 MiB use HTTP in `auto` and report an error in `rdma`.

To run the real two-agent memory transfer test, use
`cargo test --manifest-path nixl/Cargo.toml --locked`. For an S3 command
round trip, set `RC` to the native executable and supply disposable buckets
in `RDMA_BUCKET` and `HTTP_BUCKET` on a native and an HTTP-only endpoint.
Run `RC_RDMA_BACKEND=cuobject nixl/tests/interop.sh` on an NVIDIA client
or `RC_RDMA_BACKEND=nixl nixl/tests/interop.sh` on a NIXL client. The
script accepts `NIXL_BUCKET` as an alias for `RDMA_BUCKET`. For NIXL,
`UCX_TLS=tcp,self` exercises the protocol on a machine without an RDMA adapter.

On a cuObject client with an active RDMA device, run the buffer lifetime
test with the same SDK and device configuration used by the CLI:

```sh
cargo test --locked --manifest-path nixl/Cargo.toml --features cuobject \
  cuobject_token_survives_cancellation_and_timeout -- --ignored
```

### Test failed cuObject deregistration

On a Linux RDMA host with the client SDK installed, build the test-only SDK
override and run the registration lifetime test in its own process.

```sh
c++ -std=c++17 -fPIC -shared -I"$CUOBJCLIENT_ROOT/include" -I"$CUDA_HOME/include" \
  nixl/cuobject-client/tests/failed_deregistration.cc -o /tmp/rc-test-failed-deregistration.so
LD_PRELOAD=/tmp/rc-test-failed-deregistration.so cargo test --manifest-path nixl/Cargo.toml \
  -p rustfs-cuobject-client --features client --test registration_budget -- --ignored
```

## Embed the Rust client

Add the SDK from the CLI repository to your application's `Cargo.toml`.
Choose `nixl`, `cuobject`, or both. Each feature requires
the corresponding native libraries described above. The default is `nixl`.

```toml
[dependencies]
rustfs-rdma-client = { git = "https://github.com/rustfs/cli", default-features = false, features = ["cuobject"] }
```

For a local checkout, replace the `git` key with `path = "../cli/nixl/rdma-client"`.

The following function accepts the endpoint configuration and object key from
the application. Run it on a Tokio runtime. Reuse a provider for clients that
should share endpoint capability probes, and reuse each client across transfers.

```rust
use rustfs_rdma_client::{Alias, DataTransportMode, ObjectStore, RdmaBackend, RdmaProvider, RemotePath, Result};

async fn download(alias: Alias, path: &RemotePath) -> Result<Vec<u8>> {
    let provider = RdmaProvider::new(RdmaBackend::CuObject)?;
    let client = provider.client(alias, DataTransportMode::Rdma).await?;
    client.get_object(path).await
}
```

`RdmaProvider::new` returns `Error::UnsupportedFeature` for a backend
omitted from the build. Native memory registration happens on demand during
transfer. Use `DataTransportMode::Auto` to permit safe HTTP fallback.
Both backends transfer through SDK-managed host memory.

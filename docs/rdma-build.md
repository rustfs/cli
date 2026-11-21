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
the completion list in part-number order. For transfers of up to 1 MiB, the client can register two 1 MiB
windows. Larger transfers use 64 MiB windows, which also serve subsequent
small requests on the same client. File downloads write directly into the temporary
destination file. A range GET for the next window can proceed while the
previous window is being written. All requests keep the caller's version selector. Each GET uses
`If-Match` and checks the version from HEAD before delivering that range. Downloads publish the temporary file
after verifying the transfer count and any applicable object checksum.
A verified receipt releases the window. A local rejection before sending or
an authenticated S3 `PreconditionFailed` response to an `If-Match` GET also
releases it. For the latter, the client requires HTTP 412, a bounded S3
error document and no RDMA response headers. An ambiguous response, a
missing receipt, cancellation or timeout retains the token and window until
the process exits.
When both windows are unavailable for this reason, waiting operations use a
fresh pool within the process registration budget.

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

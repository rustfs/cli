# Object data over RDMA

## RustFS native NIXL/UCX

Build on Linux with Rust 1.99 or later, libclang, a C++ compiler and the
NIXL libraries, headers and UCX plugin used by the RustFS server. Set
`NIXL_PREFIX` to the native installation before building. At runtime set
`LD_LIBRARY_PATH` to include its library directory and `NIXL_PLUGIN_DIR` to
its plugin directory. Set `UCX_NET_DEVICES` to select client network devices
when needed. The native package pins the same `rustfs-nixl` and
`nixl-sys` sources as the server.

```sh
cargo build --manifest-path nixl/Cargo.toml --release --locked
export PATH="$PWD/nixl/target/release:$PATH"
rc --data-transport rdma cp model.bin local/models/model.bin
rc --data-transport rdma cp local/models/model.bin downloaded.bin
```

Configure a verified HTTPS alias and start RustFS with its `nixl` Cargo
feature and `RUSTFS_NIXL_ENABLE=true`. For a private CA, pass
`--ca-bundle /path/to/ca.pem` to `rc alias set`. Set `RC_DATA_TRANSPORT=auto`
or `rdma` to reuse the selection across commands. The default is `http`.

File uploads use positional reads directly into two reusable registered
windows. For transfers of up to 1 MiB, the client can register two 1 MiB
windows. Larger transfers use 64 MiB windows, which also serve subsequent
small requests on the same client. File downloads write directly into the temporary
destination file. A range GET for the next window can proceed while the
previous window is being written. The client selects a version and uses
`If-Match` to keep ranges consistent. Downloads publish the temporary file
after verifying the transfer count and any applicable object checksum.

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
in `NIXL_BUCKET` and `HTTP_BUCKET` on enabled and disabled endpoints. Run
`nixl/tests/interop.sh` with those variables. `UCX_TLS=tcp,self` exercises
the protocol on a machine without an RDMA network adapter.

## Private gateway protocol

The normal workspace builds without access to the private transport repository:

```sh
cargo build --release -p rustfs-cli
```

The optional build uses an independent manifest. Cargo resolves optional
dependencies even when their feature is disabled, so the normal workspace does
not declare a private path or Git dependency.

An authorized developer enables the private build with:

```sh
git submodule update --init vendor/rustfs-ucx
./scripts/build-rdma.sh --release
```

The equivalent Cargo command is:

```sh
source vendor/rustfs-ucx/scripts/build-env.sh
cp -n Cargo.lock rdma/Cargo.lock
cargo +1.98.1 build --manifest-path rdma/Cargo.toml --features rdma --release -j 2
```

The helper seeds a missing opt-in lockfile from the reviewed public CLI lockfile.
Cargo then adds private dependencies; an existing opt-in lockfile is preserved.

The default output is `rdma/target/release/rc`. `RUSTFS_RDMA_TOOLCHAIN` selects
another installed toolchain satisfying the private dependency's MSRV, and
`CARGO_BUILD_JOBS` selects build parallelism. The submodule commit is pinned by
the parent repository; updating it is an explicit dependency update.

Missing source causes the build helper to stop before invoking Cargo. Fetching
the private submodule requires repository authorization. The ordinary build does
not initialize it. The private startup entry installs the object data provider
and then runs the same public CLI launcher.

`--data-transport http|auto|rdma` selects object payload policy. Its default is
`http`; management and metadata requests retain their existing transport.

The public S3 client retains request options, signing, multipart management,
file publication and retry decisions. `rc-s3::data_transport` exposes the typed
provider and bounded signed-control interfaces. Transport implementation source
is compiled from the private submodule.

The private provider discovers a matching gateway through a signed HTTPS HEAD
request before allocating transport resources or submitting an object write.
An endpoint which does not advertise the supported protocol version receives no
external-payload write. The provider then coordinates one signed S3 operation,
its external data, and its verified final result.

Automatic selection may use HTTP only before the data operation is accepted or
supplied IO is polled. Explicit RDMA selection reports unavailable capability.
An unknown upload outcome cannot trigger a generic business retry. Multipart
creation, completion, and abort retain their existing S3 HTTPS implementation.

The optional executable also defaults to HTTP. Use `--data-transport rdma` to
require the provider, or `--data-transport auto` to allow pre-admission fallback.
`RUSTFS_RDMA_OPERATION_TIMEOUT_SECONDS` sets the finite external operation
deadline, with a default of 3600 seconds. The private repository documents its
platform and runtime installation requirements. Successful interface tests do
not establish hardware throughput or latency.

Install the matching native dependencies from the pinned private engine's
guide on both endpoints. Capability negotiation precedes external transfer
admission. Ambiguous write results use authenticated queries for the original
operation within its deadline; they do not resubmit the object or part.

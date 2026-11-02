# Optional external object data build

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

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
cargo +1.98.1 build --manifest-path rdma/Cargo.toml --features rdma --release -j 2
```

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

The current provider reports a typed pre-admission unavailability error because
the data engine is not connected. HTTP remains the default. Automatic selection
may use HTTP only before any request is accepted or supplied IO is polled;
explicit RDMA selection reports the error. Successful port tests do not establish
device transfer, an operational S3 extension or a throughput result.

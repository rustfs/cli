# Object data over RDMA

## RustFS native NIXL/UCX

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

File uploads use positional reads directly into two reusable registered
windows. For transfers of up to 1 MiB, the client can register two 1 MiB
windows. Larger transfers use 64 MiB windows, which also serve subsequent
small requests on the same client. File downloads write directly into the temporary
destination file. A range GET for the next window can proceed while the
previous window is being written. All requests keep the caller's version selector. Each GET uses
`If-Match` and checks the version from HEAD before delivering that range. Downloads publish the temporary file
after verifying the transfer count and any applicable object checksum.
A window with an uncertain transfer stays registered until the process exits.
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
in `NIXL_BUCKET` and `HTTP_BUCKET` on enabled and disabled endpoints. Run
`nixl/tests/interop.sh` with those variables. `UCX_TLS=tcp,self` exercises
the protocol on a machine without an RDMA network adapter.

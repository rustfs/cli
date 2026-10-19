# Object data transport ports

The default executable uses HTTP for all S3 object payloads. The global
`--data-transport http|auto|rdma` option selects payload policy without changing
the existing `ObjectStore` interface. Management requests, multipart creation,
completion, and abort continue to use the established S3 implementation.

`rc-core` owns transport selection and business outcome classification.
`rc-s3::data_transport` owns the optional provider ports, expressed with existing
S3 read/write options and borrowed asynchronous IO. A separately built executable
can install an `ObjectDataTransportFactory` before calling `rustfs_cli::run()`.
The common launcher preserves command dispatch, output, exit codes, and the
existing worker-thread configuration.

The provider receives a host-owned `ControlExecutor` for bounded, signed HTTPS
exchanges. Credentials, endpoint addressing, custom headers, and TLS remain in
the S3 client. The executor sends each exchange once and preserves response
status and duplicate headers. The provider owns acceptance proof and any
reconciliation required after an ambiguous response.

External control requires an authenticated principal and verified HTTPS. Object
paths and query values use the S3 encoding rules, with canonical path
normalization disabled and single URI encoding during SigV4 signing. The
encoding and signature are checked against actual AWS SDK requests for reserved
characters, Unicode, repeated slashes, and selected version IDs.

Object reads to vectors, writers, and temporary files use the same provider
entry point. Files retain atomic replacement only after verified data and writer
flush. Existing version, checksum, encryption, and Range options are forwarded.
Object writes and multipart parts retain their existing option validation and
checksum verification; conditional file writes retain HTTP-managed multipart
completion. No existing S3 options may be silently omitted by a provider.

`auto` permits HTTP fallback only when the provider proves nonacceptance and the
host observes that the supplied IO was untouched. Partial output and unknown
write results never trigger automatic replay. External failures keep typed
outcome state through redaction and multipart cleanup; generic timeout-string
retry logic does not retry them. `rdma` reports an explicit failure when no
provider or usable capability exists.

After a successful external PUT, a failed persisted-checksum HEAD cannot cause
the object write to replay. Multipart transfers record whether an external part
succeeded. Uncertain completion responses retain an unknown outcome and upload
identity; post-completion verification failures retain a confirmed failure with
their original diagnostic cause. These failures remain outside generic business
retry logic.

Native provider code is outside the default workspace dependency graph. The
independent opt-in build and private dependency instructions are described in
the project build documentation.

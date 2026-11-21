# Object data transport ports

The default executable uses HTTP for all S3 object payloads. The global
`--data-transport http|auto|rdma` option selects payload policy without changing
the existing `ObjectStore` interface. Management requests, multipart creation,
completion, and abort continue to use the established S3 implementation.

`rc-core` owns transport selection and business outcome classification.
`rc-s3::data_transport` owns the optional provider ports, expressed with existing
S3 read/write options, registered file transfers, and borrowed asynchronous IO.
The Linux native executable registers an `ObjectDataTransportFactory` for
each compiled RDMA backend before calling `rustfs_cli::run()`. Backend
selection (`--rdma-backend nixl|cuobject`) and fallback policy are independent.
Both backends use the same multipart publication, version selection, file IO,
checksum verification and receipt handling.
The common launcher preserves command dispatch, output, exit codes, and the
existing worker-thread configuration.

The NIXL backend supplies agent metadata and a registered buffer description.
The NVIDIA cuObject backend uses a private CXX bridge to register host memory with
`cuObjClient` and acquire a direction-specific token for each transfer. The window can be reused after
a verified receipt or proof that DMA never started. An uncertain transfer retains both resources
within the process registration budget. Both use the host-owned `ControlExecutor`. The S3 client signs and sends a bodyless
HTTPS request on the shared S3 HTTP client using the alias timeout policy. It returns the status and headers directly,
including duplicate headers. The provider verifies the RustFS transfer receipt
and byte count. Credentials, endpoint addressing, custom headers, and TLS
remain in the S3 client.

External control requires an authenticated principal and verified HTTPS. Object
paths and query values use the S3 encoding rules, with canonical path
normalization disabled and single URI encoding during SigV4 signing. The
encoding and signature are checked against actual AWS SDK requests for reserved
characters, Unicode, repeated slashes, and selected version IDs.

Object reads to vectors and writers share the streaming provider entry point.
For file downloads the provider can write registered memory directly to a
temporary file. The client verifies the transfer before replacing the
destination. Existing version, checksum, encryption, and Range options are
forwarded.
Object writes and multipart parts retain their existing option validation and
checksum verification; conditional file writes retain HTTP-managed multipart
completion. No existing S3 options may be silently omitted by a provider.

`auto` permits HTTP fallback only when the provider proves nonacceptance and the
host observes that the supplied IO was untouched. Partial output and unknown
write results never trigger automatic replay. External failures keep typed
outcome state through redaction and multipart cleanup; generic timeout-string
retry logic does not retry them. `rdma` reports an explicit failure when no
provider or usable capability exists.

Nonempty native uploads use unpublished multipart parts. The client publishes
the object after all part receipts and ETags have been verified. An uncertain
completion retains the upload ID and an unknown outcome. A failed checksum
verification after completion cannot replay the write. Empty objects use S3
directly.

The [RDMA build guide](../rdma-build.md) describes the Linux executable,
native dependencies, and runtime setup.

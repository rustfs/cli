# rc - Rust S3 CLI Client

[![CI](https://github.com/rustfs/cli/actions/workflows/ci.yml/badge.svg)](https://github.com/rustfs/cli/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE-APACHE)
[![Crates.io](https://img.shields.io/crates/v/rustfs-cli.svg)](https://crates.io/crates/rustfs-cli)
[![Docs.rs](https://docs.rs/rustfs-cli/badge.svg)](https://docs.rs/rustfs-cli)

A S3-compatible command-line client written in Rust.

Migrating from MinIO `mc`? See the
[mc compatibility and server-blocker matrix](docs/reference/rc/mc-compatibility.md).

## Features

- 🚀 **High Performance** - Written in Rust with async concurrent operations
- 🔧 **S3 Compatible** - Data operations support RustFS, MinIO, AWS S3, and other S3-compatible services
- 📦 **Cross-Platform** - Supports Linux, macOS, and Windows
- 🎨 **Friendly Output** - Human-readable and JSON format output
- 🔒 **Secure** - Secure credential storage, no sensitive data in logs

## Installation

### Binary Download

Download the appropriate binary for your platform from the [Releases](https://github.com/rustfs/cli/releases) page.
On Linux, use the default `linux-amd64` / `linux-arm64` artifacts for maximum compatibility (`musl` static build).
If you specifically need glibc-linked builds, use `linux-amd64-gnu` / `linux-arm64-gnu`.

### Homebrew (macOS/Linux)

```bash
brew install rustfs/tap/rc
```

### Scoop (Windows)

```powershell
scoop bucket add rustfs https://github.com/rustfs/scoop-bucket
scoop install rustfs/rc
```

### Cargo

```bash
cargo install rustfs-cli
```

### Docker

```bash
# Show help
docker run --rm rustfs/rc:latest --help

# Run a command with a local RustFS instance
docker run --rm --network host rustfs/rc:latest \
  alias set local http://localhost:9000 accesskey secretkey
```

### Build from Source

```bash
git clone https://github.com/rustfs/cli.git
cd cli
cargo build --release
```

## Quick Start

### Configure Aliases

```bash
# Add local S3 service
rc alias set local http://localhost:9000 accesskey secretkey

# Add AWS S3
rc alias set s3 https://s3.amazonaws.com AKIAIOSFODNN7EXAMPLE wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

# List all aliases
rc alias list
```

### Basic Operations

```bash
# List buckets
rc ls local/

# Create bucket
rc mb local/my-bucket

# Upload file
rc cp ./file.txt local/my-bucket/

# Download file
rc cp local/my-bucket/file.txt ./

# View object info
rc stat local/my-bucket/file.txt

# Delete object
rc rm local/my-bucket/file.txt

# Delete bucket
rc rb local/my-bucket
```

### Advanced Operations

```bash
# Recursively copy directory
rc cp -r ./local-dir/ local/bucket/remote-dir/

# Mirror between the local filesystem and RustFS
rc mirror ./local-dir/ local/bucket/backup/

# Find objects
rc find local/bucket --name "*.txt" --newer 1d

# List anonymous access rules
rc anonymous list local/bucket

# Set anonymous access level
rc anonymous set public local/bucket/public

# Generate download link
rc share download local/bucket/file.txt --expire 24h

# View directory tree
rc tree local/bucket -L 3
```

Bulk copies accept one or more sources followed by a directory or remote-prefix target:

```bash
rc cp ./january.csv ./february.csv local/reports/ --concurrency 8 --summary
rc cp -r ./reports/ local/archive/ --include '*.csv' --exclude 'private-*' --newer-than 7d
rc cp -r ./reports/ local/archive/ --rate-limit 10MiB/s --retry-attempts 5 --continue-on-error
```

Direction-safe `mc` compatibility commands reuse the same copy planner:

```bash
rc get local/reports/report.json ./report.json
rc put ./report.json local/reports/
```

When include rules are present, a path must match at least one of them. Exclude rules are applied
after include rules and always win, regardless of flag order. Age filters compare source metadata in
UTC: newer/older boundaries are strict, while `--rewind` includes the specified boundary. Rate,
concurrency, and retry settings are shared across the full command. Any failed item leaves the
aggregate command exit code non-zero even when `--continue-on-error` is used. An empty selection
succeeds by default; pass `--fail-empty` when automation should receive a not-found exit code.

### Admin Operations (IAM)

```bash
# List users
rc admin user list local/

# Add a new user
rc admin user add local/ newuser secretpassword

# Create a policy
rc admin policy create local/ readonly --file policy.json

# Attach policy to user
rc admin policy attach local/ readonly --user newuser

# Create a service account (access_key + secret_key)
rc admin service-account create local/ AKIAIOSFODNN7EXAMPLE wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

# Create a service account with a policy file
rc admin service-account create local/ SAKEY123 SASECRET123 --policy ./service-account-policy.json

# Create a service account with inline policy JSON
rc admin service-account create local/ SAKEY123 SASECRET123 --policy-json '{"Version":"2012-10-17","Statement":[]}'

# Update selected fields on an existing service account
rc admin service-account update local/ SAKEY123 --policy ./service-account-policy.json --description "Automation access"

# Inspect any access key and resolve whether it belongs to a user, service account, or STS credential
rc admin access-key info local/ AKIAIOSFODNN7EXAMPLE
rc admin access-key info local/ AKIAIOSFODNN7EXAMPLE --json

# Manage bucket event notifications
rc event add local/my-bucket arn:aws:sns:us-east-1:123456789012:topic --event 's3:ObjectCreated:*'
rc event list local/my-bucket
rc event remove local/my-bucket arn:aws:sns:us-east-1:123456789012:topic

# Manage bucket CORS configuration
rc bucket cors list local/my-bucket
rc bucket cors get local/my-bucket
rc bucket cors set local/my-bucket cors.xml
cat cors.xml | rc bucket cors set local/my-bucket -
rc bucket cors set local/my-bucket --file cors.json
rc cors remove local/my-bucket
```

### Lifecycle (ILM) Operations

```bash
# Add lifecycle rule: expire objects after 30 days with prefix filter
rc ilm rule add local/my-bucket --expiry-days 30 --prefix "logs/"

# Add lifecycle rule: transition to remote tier after 90 days
rc ilm rule add local/my-bucket --transition-days 90 --storage-class WARM

# List lifecycle rules
rc ilm rule list local/my-bucket

# Edit an existing rule
rc ilm rule edit local/my-bucket --id rule-abc123 --expiry-days 60

# Remove a specific rule or all rules
rc ilm rule remove local/my-bucket --id rule-abc123
rc ilm rule remove local/my-bucket --all

# Export/import lifecycle configuration (JSON)
rc ilm rule export local/my-bucket > lifecycle.json
rc ilm rule import local/my-bucket lifecycle.json

# Manage remote storage tiers
rc ilm tier add rustfs WARM local --endpoint http://remote:9000 --access-key ak --secret-key sk --bucket warm-bucket
rc ilm tier list local
rc ilm tier info WARM local
rc ilm tier remove WARM local --force

# Restore a transitioned (archived) object
rc ilm restore local/my-bucket/archived-file.dat --days 7
```

### Bucket Replication

```bash
# Replication requires versioning on both source and destination buckets
rc version enable local/my-bucket
rc version enable remote/target-bucket

# Configure a remote alias with the destination RustFS endpoint URL.
# rc normalizes the remote target endpoint to the host:port form expected by
# the RustFS admin API when creating replication targets.
rc alias set remote http://remote:9000 ACCESS_KEY SECRET_KEY

# Add a replication rule
rc replicate add local/my-bucket \
  --remote-bucket remote/target-bucket \
  --priority 1 \
  --replicate delete,delete-marker,existing-objects

# Allow self-signed or otherwise untrusted target certificates for this
# replication target only.
rc replicate add local/my-bucket \
  --remote-bucket remote/target-bucket \
  --replicate delete,delete-marker,existing-objects \
  --insecure

# Upload a local PEM CA bundle so RustFS can trust a private CA when it
# connects to the remote replication target.
rc replicate add local/my-bucket \
  --remote-bucket remote/target-bucket \
  --replicate delete,delete-marker,existing-objects \
  --ca-cert ./private-ca.pem

# List replication rules
rc replicate list local/my-bucket

# View replication status/metrics
rc replicate status local/my-bucket

# Update a replication rule
rc replicate update local/my-bucket --id rule-1 --priority 2

# Remove replication rules
rc replicate remove local/my-bucket --id rule-1
rc replicate remove local/my-bucket --all

# Export/import replication configuration (JSON)
rc replicate export local/my-bucket > replication.json
rc replicate import local/my-bucket replication.json
```

### Admin Operations (Cluster)

```bash
# Cluster information
rc admin info cluster local
rc admin info server local
rc admin info disk local --offline

# Aggregate background heal status
rc admin heal status local

# Root recursive manual heal
rc admin heal start local --scan-mode deep
rc admin heal status local --client-token <TOKEN_FROM_START>
rc admin heal stop local --client-token <TOKEN_FROM_START>

# Bucket manual heal
rc admin heal start local --bucket mybucket --scan-mode deep
rc admin heal status local --bucket mybucket --client-token <TOKEN_FROM_START>
rc admin heal stop local --bucket mybucket --client-token <TOKEN_FROM_START>

# Global force stop
rc admin heal stop local

# Pool expansion and decommission workflows
rc admin pool list local
rc admin pool status local 1 --by-id
rc admin expand start local
rc admin expand status local
rc admin expand stop local
rc admin decommission start local '/data/pool1/disk{1...4}'
rc admin decommission status local '/data/pool1/disk{1...4}'
rc admin decommission cancel local 1 --by-id
rc admin decommission clear local 1 --by-id

# Rebalance data after adding server pools
rc admin rebalance start local
rc admin rebalance status local
rc admin rebalance stop local

# Site replication across clusters (peer sites given as alias names)
rc admin replicate add site1 site2
rc admin replicate info site1
rc admin replicate status site1
rc admin replicate remove site1 --all

# Service control (restart/stop perform a graceful shutdown;
# a process manager such as systemd relaunches after restart)
rc admin service restart local
rc admin service stop local

# JSON output
rc admin info cluster local --json
rc admin heal status local --json
rc admin rebalance status local --json
```

### Operational Health and Usage

```bash
# Public liveness and dependency-readiness probes
rc ping local
rc ready local --timeout 2

# Prefer the RustFS background-scanner snapshot
rc du local

# Explicitly permit a portable paginated S3 fallback
rc du local/photos/2026/ --fallback --versions
rc du local/photos --fallback --incomplete
```

`rc du` never starts the potentially expensive client scan after an unsupported or unauthorized admin request unless `--fallback` is present. See the [operational utilities reference](docs/reference/rc/ops.md) for count, staleness, and partial-result semantics.

## Command Overview

For full command documentation, see the [`rc` command reference](docs/reference/rc/README.md).

| Command       | Description                                                                  |
|---------------|------------------------------------------------------------------------------|
| `alias`       | Manage storage service aliases                                               |
| `admin`       | Manage IAM users, policies, groups, service accounts, and cluster operations |
| `ls`          | List buckets or objects                                                      |
| `mb`          | Make bucket                                                                  |
| `rb`          | Remove bucket                                                                |
| `cp`          | Copy objects                                                                 |
| `mv`          | Move objects                                                                 |
| `rm`          | Remove objects                                                               |
| `cat`         | Display object contents                                                      |
| `head`        | Display first N lines of object                                              |
| `stat`        | Display object metadata                                                      |
| `find`        | Find objects                                                                 |
| `anonymous`   | Manage anonymous access to buckets and objects                               |
| `diff`        | Compare two locations                                                        |
| `mirror`      | Mirror local and S3-compatible directory trees                               |
| `tree`        | Tree view display                                                            |
| `share`       | Generate presigned URLs                                                      |
| `event`       | Manage bucket event notifications                                            |
| `cors`        | Manage bucket CORS configuration                                             |
| `pipe`        | Upload from stdin                                                            |
| `version`     | Manage bucket versioning                                                     |
| `tag`         | Manage bucket and object tags                                                |
| `quota`       | Manage bucket quota                                                          |
| `ilm`         | Manage lifecycle rules, storage tiers, and object restore                    |
| `replicate`   | Manage bucket replication                                                    |
| `watch`       | Stream live RustFS object notifications                                      |
| `completions` | Generate shell completion scripts                                            |
| `ping`        | Check service liveness and round-trip latency                                |
| `ready`       | Check service dependency readiness                                           |
| `du`          | Report server-snapshot or explicitly permitted client-scan usage             |

### Admin Subcommands

| Command                 | Description                                                                           |
|-------------------------|---------------------------------------------------------------------------------------|
| `admin user`            | Manage IAM users (add, remove, list, info, enable, disable)                           |
| `admin policy`          | Manage IAM policies (create, remove, list, info, attach)                              |
| `admin group`           | Manage IAM groups (add, remove, list, info, enable, disable, add-members, rm-members) |
| `admin service-account` | Manage service accounts (create, update, remove, list, info)                          |
| `admin access-key`      | Inspect access key identity and metadata (info)                                       |
| `admin info`            | Display cluster information (cluster, server, disk)                                   |
| `admin heal`            | Manage cluster healing operations (status, start, stop)                               |
| `admin pool`            | List pools and inspect expansion/decommission status                                  |
| `admin expand`          | Manage post-expansion data rebalancing (start, status, stop)                          |
| `admin decommission`    | Manage server pool decommissioning (start, status, cancel, clear)                     |
| `admin rebalance`       | Manage post-expansion rebalancing (start, status, stop)                               |

### ILM Subcommands

| Command           | Description                              |
|-------------------|------------------------------------------|
| `ilm rule add`    | Add a lifecycle rule to a bucket         |
| `ilm rule edit`   | Edit an existing lifecycle rule          |
| `ilm rule list`   | List lifecycle rules on a bucket         |
| `ilm rule remove` | Remove lifecycle rules from a bucket     |
| `ilm rule export` | Export lifecycle configuration as JSON   |
| `ilm rule import` | Import lifecycle configuration from JSON |
| `ilm tier add`    | Add a remote storage tier                |
| `ilm tier edit`   | Edit tier credentials                    |
| `ilm tier list`   | List all configured storage tiers        |
| `ilm tier info`   | Show details for a specific tier         |
| `ilm tier remove` | Remove a storage tier                    |
| `ilm restore`     | Restore a transitioned (archived) object |

### Replicate Subcommands

| Command            | Description                                |
|--------------------|--------------------------------------------|
| `replicate add`    | Add a new replication rule                 |
| `replicate update` | Update an existing replication rule        |
| `replicate list`   | List replication rules for a bucket        |
| `replicate status` | Show replication status and metrics        |
| `replicate remove` | Remove replication rules                   |
| `replicate export` | Export replication configuration as JSON   |
| `replicate import` | Import replication configuration from JSON |

## Output Format

### Human-Readable (default)

```bash
rc ls local/bucket
[2024-01-15 10:30:00]     0B dir/
[2024-01-15 10:30:00] 1.2MiB file.txt
```

### JSON Format

The versioned schemas and migration guidance are documented in the [JSON output contracts](docs/reference/rc/output.md). Existing command output remains on its documented v1 or v2 contract; new command families adopt v3 explicitly.

```bash
rc ls local/bucket --json
```

```json
{
  "items": [
    {
      "key": "dir/",
      "is_dir": true
    },
    {
      "key": "file.txt",
      "size_bytes": 1258291,
      "size_human": "1.2 MiB",
      "is_dir": false
    }
  ],
  "truncated": false
}
```

## Shell Completion

Generate and install shell completion scripts:

### Bash

```bash
rc completions bash > ~/.bash_completion.d/rc
# Or add to .bashrc:
# source <(rc completions bash)
```

### Zsh

```bash
rc completions zsh > ~/.zfunc/_rc
# Ensure ~/.zfunc is in your fpath (add to .zshrc):
# fpath=(~/.zfunc $fpath)
# autoload -Uz compinit && compinit
```

### Fish

```bash
rc completions fish > ~/.config/fish/completions/rc.fish
```

### PowerShell

```powershell
rc completions powershell >> $PROFILE
```

## Configuration

Configuration file is located at `~/.config/rc/config.toml`:

```toml
schema_version = 1

[defaults]
output = "human"
color = "auto"
progress = true

[[aliases]]
name = "local"
endpoint = "http://localhost:9000"
access_key = "accesskey"
secret_key = "secretkey"
region = "us-east-1"
```

## Exit Codes

| Code | Description                     |
|------|---------------------------------|
| 0    | Success                         |
| 1    | General error                   |
| 2    | Usage/path error                |
| 3    | Network error (retryable)       |
| 4    | Authentication/permission error |
| 5    | Resource not found              |
| 6    | Conflict/precondition failed    |
| 7    | Feature not supported           |
| 130  | Interrupted (Ctrl+C)            |

## Compatibility

### Supported Backends

These tiers describe S3-compatible data operations. The `rc admin` commands use the RustFS Admin API and do not support MinIO Admin API endpoints.

| Backend             | Tier        | Description                       |
|---------------------|-------------|-----------------------------------|
| RustFS              | Tier 1      | S3 and Admin APIs supported       |
| MinIO               | Tier 2      | S3 operations supported           |
| AWS S3              | Tier 3      | Best effort S3 support            |
| Other S3-compatible | Best Effort | No compatibility guarantee        |

### Minimum Rust Version

- Rust 1.92 or higher (Edition 2024)

## Development

### Build

```bash
cargo build --workspace
```

### Test

```bash
# Unit tests
cargo test --workspace

# Integration tests (requires S3-compatible backend)
docker compose -f docker/docker-compose.yml up -d
cargo test --workspace --features integration
docker compose -f docker/docker-compose.yml down
```

### Lint

```bash
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
```

## Contributing

Contributions are welcome! Please read [AGENTS.md](AGENTS.md) for development guidelines.

## License

This project is dual-licensed under MIT or Apache-2.0. See [LICENSE-MIT](LICENSE-MIT)
and [LICENSE-APACHE](LICENSE-APACHE).

## Acknowledgments

- [MinIO Client (mc)](https://github.com/minio/mc) - Inspiration for CLI design
- [aws-sdk-s3](https://crates.io/crates/aws-sdk-s3) - AWS S3 SDK for Rust

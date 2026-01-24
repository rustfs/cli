# rc - Rust S3 CLI Client

[![CI](https://github.com/rustfs/cli/actions/workflows/ci.yml/badge.svg)](https://github.com/rustfs/cli/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

A S3-compatible command-line client written in Rust.

## Features

- 🚀 **High Performance** - Written in Rust with async concurrent operations
- 🔧 **S3 Compatible** - Supports RustFS, MinIO, AWS S3, and other S3-compatible services
- 📦 **Cross-Platform** - Supports Linux, macOS, and Windows
- 🎨 **Friendly Output** - Human-readable and JSON format output
- 🔒 **Secure** - Secure credential storage, no sensitive data in logs

## Installation

### Binary Download

Download the appropriate binary for your platform from the [Releases](https://github.com/rustfs/cli/releases) page.

### Homebrew (macOS/Linux)

```bash
brew install rustfs/tap/rc
```

### Cargo

```bash
cargo install rustfs-cli
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

# Mirror between S3 locations
rc mirror local/bucket1/ local/bucket2/

# Find objects
rc find local/bucket --name "*.txt" --newer 1d

# Generate download link
rc share download local/bucket/file.txt --expire 24h

# View directory tree
rc tree local/bucket -L 3
```

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

# Create a service account
rc admin service-account add local/ myuser
```

### Admin Operations (Cluster)

```bash
# Cluster information
rc admin info cluster local
rc admin info server local
rc admin info disk local --offline

# Heal operations
rc admin heal status local
rc admin heal start local --bucket mybucket --scan-mode deep
rc admin heal start local --dry-run
rc admin heal stop local

# JSON output
rc admin info cluster local --json
rc admin heal status local --json
```

## Command Overview

| Command | Description |
|---------|-------------|
| `alias` | Manage storage service aliases |
| `admin` | Manage IAM users, policies, groups, service accounts, and cluster operations |
| `ls` | List buckets or objects |
| `mb` | Make bucket |
| `rb` | Remove bucket |
| `cp` | Copy objects |
| `mv` | Move objects |
| `rm` | Remove objects |
| `cat` | Display object contents |
| `head` | Display first N lines of object |
| `stat` | Display object metadata |
| `find` | Find objects |
| `diff` | Compare two locations |
| `mirror` | Mirror sync between S3 locations |
| `tree` | Tree view display |
| `share` | Generate presigned URLs |
| `pipe` | Upload from stdin |
| `version` | Manage bucket versioning |
| `tag` | Manage object tags |
| `completions` | Generate shell completion scripts |

### Admin Subcommands

| Command | Description |
|---------|-------------|
| `admin user` | Manage IAM users (add, remove, list, info, enable, disable) |
| `admin policy` | Manage IAM policies (create, remove, list, info, attach, detach) |
| `admin group` | Manage IAM groups (add, remove, list, info, member) |
| `admin service-account` | Manage service accounts (add, remove, list, info, edit) |
| `admin info` | Display cluster information (cluster, server, disk) |
| `admin heal` | Manage cluster healing operations (status, start, stop) |

## Output Format

### Human-Readable (default)

```bash
rc ls local/bucket
[2024-01-15 10:30:00]     0B dir/
[2024-01-15 10:30:00] 1.2MiB file.txt
```

### JSON Format

```bash
rc ls local/bucket --json
```

```json
{
  "items": [
    {"key": "dir/", "is_dir": true},
    {"key": "file.txt", "size_bytes": 1258291, "size_human": "1.2 MiB", "is_dir": false}
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

| Code | Description |
|------|-------------|
| 0 | Success |
| 1 | General error |
| 2 | Usage/path error |
| 3 | Network error (retryable) |
| 4 | Authentication/permission error |
| 5 | Resource not found |
| 6 | Conflict/precondition failed |
| 7 | Feature not supported |
| 130 | Interrupted (Ctrl+C) |

## Compatibility

### Supported Backends

| Backend | Tier | Description |
|---------|------|-------------|
| RustFS | Tier 1 | Fully supported |
| MinIO | Tier 2 | Fully supported |
| AWS S3 | Tier 3 | Best effort support |
| Other S3-compatible | Best Effort | No guarantee |

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

This project is dual-licensed under MIT or Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

## Acknowledgments

- [MinIO Client (mc)](https://github.com/minio/mc) - Inspiration for CLI design
- [aws-sdk-s3](https://crates.io/crates/aws-sdk-s3) - AWS S3 SDK for Rust

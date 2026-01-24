# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking

- Updated JSON output schema to `schemas/output_v2.json` to cover admin cluster info and heal outputs

### Added

- Shell completion generation command (`rc completions <shell>`)
  - Supports bash, zsh, fish, and powershell
- Golden test infrastructure using insta for snapshot testing
- `RC_CONFIG_DIR` environment variable for custom config location
- MIT and Apache-2.0 license files
- Admin cluster commands (`rc admin info` and `rc admin heal`)

### Changed

- Updated minimum supported Rust version (MSRV) to 1.92

## [0.1.0] - 2026-01-13

### Added

#### Phase 1: Core Infrastructure
- Exit code system with 9 defined codes for different error scenarios
- Configuration management with TOML format and schema versioning
- Alias management for S3-compatible storage endpoints
- Path parsing for local and remote (S3) paths
- ObjectStore trait for storage abstraction
- S3 client wrapper using aws-sdk-s3

#### Phase 2: Basic Commands
- `alias` - Manage storage service aliases (set, list, remove)
- `ls` - List buckets and objects with pagination support
- `mb` - Create buckets
- `rb` - Remove buckets
- `cat` - Display object contents
- `head` - Display first N lines of an object
- `stat` - Show object metadata

#### Phase 3: Transfer Commands
- `cp` - Copy objects (local↔S3, S3↔S3)
- `mv` - Move objects (copy + delete source)
- `rm` - Remove objects with batch delete support
- `pipe` - Stream stdin to an object
- Multipart upload support for large files
- Progress bar with indicatif
- Retry mechanism with exponential backoff

#### Phase 4: Advanced Commands
- `find` - Find objects with filters (--name, --larger, --smaller, --newer, --older)
- `diff` - Compare two S3 locations
- `mirror` - Incremental sync with --remove, --overwrite, --dry-run
- `tree` - Display objects in tree format with depth control
- `share` - Generate presigned URLs for download/upload

#### Phase 5: Optional Commands
- `version` - Manage bucket versioning (enable, suspend, info, list)
- `tag` - Manage object tags (list, set, remove)
- Capability detection for backend feature support

#### Output & Formatting
- Human-readable output format (default)
- JSON output format with `--json` flag
- Colored output with `--no-color` option
- Progress bar with `--no-progress` option
- Quiet mode with `--quiet` option

#### CI/CD
- GitHub Actions CI workflow with multi-platform testing
- GitHub Actions Release workflow for automated releases
- Integration test support with RustFS backend
- Golden test support for output format verification

### Security

- Secure credential storage in config file (600 permissions on Unix)
- No sensitive data logged in error messages

[Unreleased]: https://github.com/rustfs/cli/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rustfs/cli/releases/tag/v0.1.0

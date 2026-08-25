#!/usr/bin/env bash
#
# Regression tests for object-key path safety (#338).
#
# Usage:
#   ./scripts/regression/object-key-safety.sh
#
# These tests do not require a running S3 backend. They cover:
#   - Unix destinations accepting ':' in object keys (Loki chunks)
#   - Windows-portable mode still rejecting reserved names
#   - Traversal and control-character rejection on every policy
#   - CLI help/parse contract for --portable-names
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_error() { echo -e "${RED}[FAIL]${NC} $*"; }

run_tests() {
    local description="$1"
    shift
    log_info "$description"
    if "$@"; then
        log_success "$description"
    else
        log_error "$description"
        return 1
    fi
}

log_info "Object-key safety regression suite"

run_tests "core object-key policy unit tests" \
    cargo test -p rc-core --lib -- object_key::

run_tests "mirror path-policy unit tests" \
    cargo test -p rustfs-cli --lib -- \
    logical_relative_paths_accept_colon_object_keys \
    windows_portable_relative_paths_reject_reserved_names \
    local_logical_target_maps_colon_object_keys \
    windows_portable_local_target_rejects_colon_object_keys \
    relative_paths_are_normalized_and_traversal_is_rejected

run_tests "copy download path-policy unit tests" \
    cargo test -p rustfs-cli --lib -- \
    download_relative_path_preserves_safe_nested_keys \
    download_relative_path_rejects_traversal_and_absolute_keys \
    download_relative_path_accepts_colon_keys_on_logical_destinations \
    download_relative_path_rejects_colon_keys_when_portable_names_requested

run_tests "CLI parse contract" \
    cargo test -p rustfs-cli --lib -- cli_accepts_portable_names_on_mirror_and_copy

run_tests "CLI help contract" \
    cargo test -p rustfs-cli --test help_contract

log_success "Object-key safety regression suite passed"

#!/usr/bin/env bash
#
# Regression tests for cross-alias S3-to-S3 copy.
#
# Usage:
#   ./scripts/regression/cross-alias-copy.sh
#
# These tests do not require a running S3 backend. They cover:
#   - Planning clients for both source and destination aliases
#   - Same-alias remote copies still using one client alias
#   - Recursive dry-run and live copies across aliases without CopyObject
#   - Single-object overwrite using download then upload
#   - Source user metadata forwarded on the destination upload
#   - Storage-class rejection using the upload multipart threshold
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

log_info "Cross-alias copy regression suite"

run_tests "copy planner unit tests" \
    cargo test -p rustfs-cli --lib -- \
    planned_client_aliases_include_both_sides_of_a_cross_alias_copy \
    planned_client_aliases_keep_same_alias_remote_copy_on_one_alias \
    storage_class_plan_rejects_cross_alias_multipart_uploads \
    piped_copy_preserves_source_content_type_unless_replaced \
    piped_copy_preserves_source_user_metadata_unless_replaced

run_tests "cross-alias copy integration tests" \
    cargo test -p rustfs-cli --test recursive_remote_copy -- \
    cross_alias_recursive_dry_run_plans_without_server_side_copy \
    cross_alias_copy_downloads_then_uploads_without_copy_source \
    cross_alias_recursive_copy_downloads_then_uploads_without_copy_source \
    cross_alias_copy_forwards_source_user_metadata_on_upload \
    recursive_same_alias_copy_paginates_and_emits_deterministic_plan

log_success "Cross-alias copy regression suite passed"

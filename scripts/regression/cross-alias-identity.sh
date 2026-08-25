#!/usr/bin/env bash
#
# Regression tests for shared source-identity semantics across commands.
#
# Usage:
#   ./scripts/regression/cross-alias-identity.sh
#
# These tests do not require a running S3 backend. They cover:
#   - The shared x-amz-meta-rc-source-etag helper read/write contract
#   - Cross-alias cp recording the source ETag (including --metadata-directive replace)
#   - Cross-alias mv streaming through the client and deleting the source only on success
#   - rc diff --compare auto|etag|size agreeing with rc mirror --compare
#   - The migration path: cp --recursive across aliases, then mirror --compare auto skips
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

log_info "Cross-alias identity regression suite"

run_tests "shared source-identity helper unit tests" \
    cargo test -p rustfs-cli --lib -- commands::object_identity::

run_tests "cp identity metadata unit tests" \
    cargo test -p rustfs-cli --lib -- \
    piped_copy_records_the_source_etag_as_identity_metadata \
    piped_copy_records_identity_even_when_metadata_is_replaced \
    piped_copy_omits_identity_when_the_source_has_no_etag \
    piped_copy_preserves_source_content_type_unless_replaced \
    piped_copy_preserves_source_user_metadata_unless_replaced

run_tests "diff compare-mode unit tests" \
    cargo test -p rustfs-cli --lib -- commands::diff::

run_tests "mirror identity unit tests still hold" \
    cargo test -p rustfs-cli --lib -- \
    auto_compare_skips_when_source_etag_is_preserved_in_destination_metadata \
    auto_compare_copies_when_identity_metadata_is_missing \
    auto_compare_copies_when_identity_metadata_does_not_match \
    size_mismatch_never_skips_regardless_of_compare_mode \
    destination_race_check_ignores_identity_metadata

run_tests "cross-alias cp, mv, and diff integration tests" \
    cargo test -p rustfs-cli --test recursive_remote_copy -- \
    cross_alias_copy_records_the_source_etag_for_later_incremental_runs \
    cross_alias_move_uploads_then_deletes_the_source \
    cross_alias_move_keeps_the_source_when_the_upload_fails \
    diff_auto_compare_treats_a_recorded_source_identity_as_same \
    diff_etag_compare_ignores_the_recorded_identity_without_head \
    mirror_auto_compare_skips_objects_a_cross_alias_copy_already_migrated

run_tests "CLI help contract includes diff --compare" \
    cargo test -p rustfs-cli --test help_contract -- top_level_command_help_contract

log_success "Cross-alias identity regression suite passed"

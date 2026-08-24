#!/usr/bin/env bash
#
# Regression tests for incremental mirror identity (#342).
#
# Usage:
#   ./scripts/regression/mirror-identity.sh
#
# These tests do not require a running S3 backend. They cover:
#   - Auto compare skipping recopies when destination metadata preserves the
#     source ETag after a multipart re-upload
#   - ETag compare still treating a changed stored ETag as different
#   - Size compare ignoring ETag differences
#   - CLI help/parse contract for --compare
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

log_info "Mirror identity regression suite"

run_tests "mirror identity unit tests" \
    cargo test -p rustfs-cli --lib -- \
    auto_compare_skips_when_source_etag_is_preserved_in_destination_metadata \
    auto_compare_copies_when_identity_metadata_is_missing \
    auto_compare_copies_when_identity_metadata_does_not_match \
    size_mismatch_never_skips_regardless_of_compare_mode \
    etag_compare_still_copies_when_only_identity_metadata_matches \
    size_compare_skips_when_sizes_match_even_if_etags_differ \
    identity_metadata_is_read_case_insensitively \
    destination_identity_lookup_is_limited_to_auto_remote_etag_mismatches \
    destination_race_check_ignores_identity_metadata \
    large_remote_copy_uses_path_streaming_preserves_metadata_and_cleans_staging \
    remote_entries_without_two_etags_are_never_assumed_equal \
    equivalent_destination_is_restart_safe_and_not_planned_again \
    changed_destination_requires_overwrite

run_tests "mirror identity planner integration tests" \
    cargo test -p rustfs-cli --test mirror_planner -- \
    remote_to_remote_auto_compare_skips_when_destination_preserves_source_etag \
    remote_to_remote_etag_compare_recopies_when_stored_etags_differ \
    remote_to_remote_auto_compare_recopies_when_destination_identity_is_missing \
    remote_to_remote_auto_compare_recopies_when_destination_identity_mismatches \
    remote_to_remote_auto_compare_recopies_when_sizes_differ_without_head \
    remote_to_remote_dry_run_reads_both_manifests_without_mutation

run_tests "CLI help contract includes --compare" \
    cargo test -p rustfs-cli --test help_contract -- top_level_command_help_contract

log_success "Mirror identity regression suite passed"

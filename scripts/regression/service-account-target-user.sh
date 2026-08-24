#!/usr/bin/env bash
#
# Regression tests for service-account create --user / targetUser (#340).
#
# Usage:
#   ./scripts/regression/service-account-target-user.sh
#
# These tests do not require a running RustFS backend. They cover:
#   - CreateServiceAccountRequest serializing targetUser only when set
#   - CLI --user forwarding into the admin create body
#   - Existing create-without-user requests omitting targetUser
#   - Help contract for admin service-account create --user
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

log_info "Service-account targetUser regression suite"

run_tests "core request serialization" \
    cargo test -p rc-core --lib -- \
    test_create_service_account_request_includes_expiration \
    test_create_service_account_request_serializes_target_user \
    test_create_service_account_request_deserializes_target_user

run_tests "CLI request mapping" \
    cargo test -p rustfs-cli --lib -- \
    test_build_create_request_forwards_target_user \
    test_build_create_request_omits_empty_target_user \
    test_build_create_request_uses_access_key_as_default_name \
    test_build_create_request_keeps_explicit_name

run_tests "admin create integration" \
    cargo test -p rustfs-cli --test admin_service_account -- \
    service_account_create_sends_target_user_for_another_parent \
    service_account_create_omits_target_user_when_parent_is_the_caller \
    service_account_create_omits_empty_user_flag_from_request_body \
    service_account_create_accepts_inline_policy_json

run_tests "CLI help contract includes create --user" \
    cargo test -p rustfs-cli --test help_contract -- nested_subcommand_help_contract

log_success "Service-account targetUser regression suite passed"

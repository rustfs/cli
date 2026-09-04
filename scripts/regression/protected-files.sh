#!/usr/bin/env bash
#
# Regression tests for the protected-files marker check (#359).
#
# Usage:
#   ./scripts/regression/protected-files.sh
#
# These build synthetic git histories in a temp directory and run
# scripts/check-protected-files.sh against them. No network and no backend.
#
# The case that motivated the file: a page renamed inside docs/reference/rc/.
# With git's rename detection on, `--name-only` reports only the destination,
# the per-file diff then sees an addition with zero deletions, and a moved page
# passes as "additive only" — a reader's link breaks with no marker demanded.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CHECK="$PROJECT_ROOT/scripts/check-protected-files.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_error() { echo -e "${RED}[FAIL]${NC} $*"; }

failures=0

# Build a repo with a `base` branch and a checked-out `pr` branch, then run the
# mutation given as the remaining arguments.
setup_repo() {
    local repo="$1"
    shift
    rm -rf "$repo"
    mkdir -p "$repo/docs/reference/rc" "$repo/schemas"
    cd "$repo"
    git init -q -b base .
    git config user.email regression@example.com
    git config user.name regression
    printf 'line %s\n' 1 2 3 4 5 > docs/reference/rc/existing.md
    echo '{"version":1}' > schemas/output_v1.json
    git add -A
    git commit -qm base
    git switch -qc pr
    "$@"
    git add -A
    git commit -qm change
}

# Runs the check with the given PR body and asserts the exit status.
expect_status() {
    local description="$1" expected="$2" body="$3"
    local actual=0
    PR_BODY="$body" bash "$CHECK" base > /dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        log_success "$description"
    else
        log_error "$description (expected exit $expected, got $actual)"
        failures=$((failures + 1))
    fi
}

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

log_info "A renamed reference page demands the marker"
setup_repo "$workdir/rename" \
    git mv docs/reference/rc/existing.md docs/reference/rc/moved.md
expect_status "rename without marker is rejected" 1 "Just moving a page."
expect_status "rename with marker is accepted" 0 "BREAKING: the page moved."

log_info "A new reference page still needs no marker"
setup_repo "$workdir/addition" \
    cp docs/reference/rc/existing.md docs/reference/rc/added.md
expect_status "pure addition is accepted" 0 "Documents a new command."

log_info "An edited reference page demands the marker"
setup_repo "$workdir/edit" \
    sed -i.bak '2d' docs/reference/rc/existing.md
expect_status "removed line without marker is rejected" 1 "Tidying up."

log_info "A deleted reference page demands the marker"
setup_repo "$workdir/delete" \
    git rm -q docs/reference/rc/existing.md
expect_status "deletion without marker is rejected" 1 "No longer relevant."

log_info "A protected file renamed out of its directory still demands the marker"
setup_repo "$workdir/escape" \
    git mv schemas/output_v1.json schemas/output_v1.json.old
expect_status "renamed schema without marker is rejected" 1 "Archiving the schema."

cd "$PROJECT_ROOT"
if [ "$failures" -ne 0 ]; then
    log_error "$failures check(s) failed"
    exit 1
fi
log_success "All protected-files regression checks passed"

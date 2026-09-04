#!/usr/bin/env bash
#
# Enforce the Breaking Change process for protected files (AGENTS.md § 3).
#
# Usage:
#   PR_BODY="$(cat)" ./scripts/check-protected-files.sh <base-ref>
#
# Exits 0 when every protected change is covered, 1 when a change needs the
# BREAKING marker and the PR description does not carry one. Lives in a script
# rather than inline in the workflow so scripts/regression/protected-files.sh
# can exercise it against synthetic histories.

set -euo pipefail

BASE="${1:?usage: check-protected-files.sh <base-ref>}"
PR_BODY="${PR_BODY-}"

# Any change to these needs the marker: a new exit code, a new config field or
# a new schema property extends a contract even when nothing existing moves.
PROTECTED_FILES=(
  "schemas/output_v1.json"
  "crates/cli/src/exit_code.rs"
  "crates/core/src/config.rs"
)

# The command reference is a contract too, but adding a section to it is how a
# new command gets documented — required by AGENTS.md § 3 of the Breaking Change
# process, in fact. Demanding the marker for that makes every additive PR claim
# to be breaking, so these paths need it only when existing lines move: a
# rewritten sentence, a removed flag, a deleted or renamed page.
ADDITIVE_OK_FILES=(
  "docs/reference/rc/"
)

# Rename detection off from the start. With it on, a pure rename reports only
# the destination path; the per-file diff below then sees an addition with zero
# deletions and a moved page passes as "additive only". It also hides a
# protected file renamed out of its directory, which would leave nothing under
# the protected prefix to match.
CHANGED_FILES=$(git diff --name-only --no-renames "$BASE"...HEAD)

# Paths under a protected prefix, one per line.
matched_paths() {
  local protected_path="${1%/}"
  printf '%s\n' "$CHANGED_FILES" \
    | grep -E "^$(printf '%s' "$protected_path" | sed 's/[.[\*^$]/\\&/g')(/|$)" || true
}

require_marker() {
  local subject="$1"
  echo "::warning::Protected file modified: $subject"
  echo "This change requires the Breaking Change process. See AGENTS.md."
  if ! grep -q "BREAKING" <<< "$PR_BODY"; then
    echo "::error::Protected file $subject modified without BREAKING marker in PR description"
    return 1
  fi
}

status=0

for file in "${PROTECTED_FILES[@]}"; do
  if [ -n "$(matched_paths "$file")" ]; then
    require_marker "$file" || status=1
  fi
done

for file in "${ADDITIVE_OK_FILES[@]}"; do
  paths=$(matched_paths "$file")
  [ -n "$paths" ] || continue

  removed=0
  count=0
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    count=$((count + 1))
    deleted=$(
      git diff --numstat --no-renames "$BASE"...HEAD -- "$path" \
        | awk '{ total += $2 } END { print total + 0 }'
    )
    removed=$((removed + deleted))
  done <<< "$paths"

  if [ "$removed" -gt 0 ]; then
    require_marker "$file ($removed line(s) changed or removed)" || status=1
  else
    echo "$file changed by addition only ($count file(s)); no marker required"
  fi
done

if [ "$status" -ne 0 ]; then
  exit 1
fi
echo "Protected files check passed"

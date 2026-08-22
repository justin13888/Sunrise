#!/usr/bin/env bash
#
# Shared runner for the repository's grep-based source gates.
#
# Usage: grep-gate.sh <name> <ERE pattern> <dir>...
#
# Why this exists rather than an inline `if grep ...; then fail; fi`:
#
#   `if` suppresses errexit for its condition, and grep exits 1 for "no match"
#   but 2 for "bad path" and 127 for "not installed". An inline `if` therefore
#   takes the else branch — printing OK and passing the job — for *every*
#   failure mode, not just the clean one. Both of this repo's gates were
#   written that way, so neither had ever enforced anything: one searched
#   paths that do not exist, and the other invoked `rg`, which is not
#   installed on GitHub's ubuntu-latest runners.
#
# This script distinguishes the three outcomes explicitly, and treats
# "the gate could not run" as a failure rather than a pass.
#
# Comment lines are filtered out, so prose that merely *names* a banned
# construct (see the `rand::random::<f64>()` example in
# crates/sunrise-sync/src/backoff.rs) does not red the build.
set -uo pipefail

name="$1"; pattern="$2"; shift 2

dirs=()
for d in "$@"; do [ -d "$d" ] && dirs+=("$d"); done
if [ ${#dirs[@]} -eq 0 ]; then
  echo "::error::$name: none of the search paths exist; the gate cannot run."
  exit 1
fi

raw=$(grep -rEn --include='*.rs' -- "$pattern" "${dirs[@]}"); rc=$?
if [ "$rc" -gt 1 ]; then
  echo "::error::$name: grep exited $rc; the gate could not run."
  exit 1
fi

if [ "$rc" -eq 0 ] && [ -n "$raw" ]; then
  hits=$(printf '%s\n' "$raw" | grep -vE ':[0-9]+:[[:space:]]*(//|/\*|\*)' || true)
  if [ -n "$hits" ]; then
    printf '%s\n' "$hits"
    echo "::error::$name: violations listed above."
    exit 1
  fi
fi

echo "OK: $name clean (searched: ${dirs[*]})."

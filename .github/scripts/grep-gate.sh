#!/usr/bin/env bash
#
# Shared runner for the repository's grep-based source gates.
#
# Usage: grep-gate.sh [--allowlist FILE] <name> <ERE pattern> <dir>...
#
# --allowlist FILE excuses named hits. One entry per line, three tab-separated
# fields: the path as grep prints it, the source line with its leading and
# trailing whitespace trimmed, and the reason it is not a violation. Blank
# lines and lines starting `#` are comments. An entry is matched on path and
# line text, never on a line number, so it survives edits above it and dies
# with the line it excuses. Three things fail the gate: an entry with an empty
# field (a justification is the point of the file), an entry that matches no
# hit (a stale excuse is a hole waiting for a new line to fill it), and a
# missing file (an allowlist that cannot be read excuses nothing, and must not
# be read as excusing everything).
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

allowlist=""
if [ "${1:-}" = "--allowlist" ]; then
  allowlist="${2:-}"; shift 2
fi

name="$1"; pattern="$2"; shift 2

if [ -n "$allowlist" ]; then
  if [ ! -f "$allowlist" ]; then
    echo "::error::$name: allowlist $allowlist does not exist; the gate cannot run."
    exit 1
  fi
  malformed=$(awk -F'\t' '
    /^[[:space:]]*(#|$)/ { next }
    NF != 3 || $1 == "" || $2 ~ /^[[:space:]]*$/ || $3 ~ /^[[:space:]]*$/ {
      print FILENAME ":" FNR ": " $0
    }' "$allowlist")
  if [ -n "$malformed" ]; then
    printf '%s\n' "$malformed"
    echo "::error::$name: allowlist entries above are not path<TAB>line<TAB>reason."
    exit 1
  fi
fi

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

hits=""
if [ "$rc" -eq 0 ] && [ -n "$raw" ]; then
  hits=$(printf '%s\n' "$raw" | grep -vE ':[0-9]+:[[:space:]]*(//|/\*|\*)' || true)
fi

stale=""
excused=0
if [ -n "$allowlist" ]; then
  # Split the allowlist off the hits. Both passes key a hit the way an entry
  # is written: `path:line:text` -> `path<TAB>trimmed text`.
  split=$(printf '%s\n' "$hits" | awk -F'\t' -v allowfile="$allowlist" '
    function key(line,   i, rest, j, text) {
      i = index(line, ":"); rest = substr(line, i + 1)
      j = index(rest, ":"); text = substr(rest, j + 1)
      sub(/^[[:space:]]+/, "", text); sub(/[[:space:]]+$/, "", text)
      return substr(line, 1, i - 1) "\t" text
    }
    # Keyed on the file name rather than `NR == FNR`, which an empty
    # allowlist would make true for every hit as well.
    FILENAME == allowfile {
      if ($0 !~ /^[[:space:]]*(#|$)/) { allowed[$1 "\t" $2] = FNR }
      next
    }
    NF == 0 { next }
    (key($0) in allowed) { used[key($0)] = 1; next }
    { print "HIT " $0 }
    END {
      for (k in allowed) if (!(k in used)) print "STALE " allowfile ":" allowed[k] ": " k
      for (k in used) n++
      print "EXCUSED " n + 0
    }
  ' "$allowlist" -)
  hits=$(printf '%s\n' "$split" | sed -n 's/^HIT //p')
  stale=$(printf '%s\n' "$split" | sed -n 's/^STALE //p')
  excused=$(printf '%s\n' "$split" | sed -n 's/^EXCUSED //p')
fi

if [ -n "$hits" ]; then
  printf '%s\n' "$hits"
  echo "::error::$name: violations listed above."
fi
if [ -n "$stale" ]; then
  printf '%s\n' "$stale"
  echo "::error::$name: allowlist entries above match no line; delete them."
fi
if [ -n "$hits" ] || [ -n "$stale" ]; then
  exit 1
fi

if [ -n "$allowlist" ]; then
  echo "OK: $name clean, $excused allowlisted (searched: ${dirs[*]})."
else
  echo "OK: $name clean (searched: ${dirs[*]})."
fi

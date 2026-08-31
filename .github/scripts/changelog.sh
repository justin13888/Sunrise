#!/usr/bin/env bash
#
# Render a commit range as a Markdown changelog, grouped by Conventional Commit
# type.
#
# Usage: changelog.sh <from-ref> [<to-ref>]
#        changelog.sh "" v0.2.0     # no predecessor: the whole history
#
# Why this and not a changelog generator: the repository has no CHANGELOG.md
# and no git-cliff or release-plz config, but every commit in it is already a
# Conventional Commit ("feat(macos): …", "refactor(tui)!: …"). That convention
# is the source of truth the repo actually maintains, so the release notes read
# it rather than introducing a second one that has to be kept in sync.
#
# The output is prepended to GitHub's own generated notes, which supply the PR
# and contributor links a commit log cannot see.
set -euo pipefail

from="${1:-}"
to="${2:-HEAD}"
range=$([ -n "$from" ] && echo "${from}..${to}" || echo "$to")

# hash <US> subject. A unit separator cannot occur in a subject line, so this
# round-trips without quoting games.
US=$'\x1f'
commits=$(git log --no-merges --reverse --format="%H${US}%s" "$range")

# Recognised types, as one alternation reused by both the per-type sections and
# the catch-all below. Keeping a single definition is what stops a commit from
# being listed twice or not at all.
known='feat|fix|perf|refactor|docs|test|build|ci|chore|style|revert'

render() { # reads hash<US>subject lines on stdin, writes markdown list items
  while IFS="$US" read -r hash subject; do
    [ -n "$hash" ] || continue
    printf -- '- %s (%s)\n' "$subject" "${hash:0:7}"
  done
}

section() { # <heading> <ERE matched against the subject>
  local heading="$1" pattern="$2" body
  body=$(printf '%s\n' "$commits" | grep -E "${US}${pattern}" | render || true)
  [ -n "$body" ] || return 0
  printf '### %s\n\n%s\n\n' "$heading" "$body"
}

# Breaking changes lead, and also appear under their own type below. That
# duplication is deliberate: a reader scanning for "what will break" and a
# reader scanning "what is new" are different readers.
section "Breaking changes" "(${known})(\([^)]*\))?!:"
section "Features"         'feat(\([^)]*\))?!?:'
section "Fixes"            'fix(\([^)]*\))?!?:'
section "Performance"      'perf(\([^)]*\))?!?:'
section "Refactors"        'refactor(\([^)]*\))?!?:'
section "Documentation"    'docs(\([^)]*\))?!?:'
section "Tests"            'test(\([^)]*\))?!?:'
section "Build and CI"     '(build|ci)(\([^)]*\))?!?:'
section "Chores"           '(chore|style|revert)(\([^)]*\))?!?:'

# Whatever did not parse as a Conventional Commit. Listing it rather than
# dropping it keeps the changelog honest about what actually shipped.
other=$(printf '%s\n' "$commits" | grep -vE "${US}(${known})(\([^)]*\))?!?:" | render || true)
[ -n "$other" ] && printf '### Other\n\n%s\n\n' "$other"

exit 0

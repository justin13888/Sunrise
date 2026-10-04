#!/usr/bin/env bash
#
# Build the cross-version harness's baseline driver at a pinned commit or tag.
#
# Usage: crates/sunrise-e2e/baseline-driver/build-baseline.sh <ref>
#
# Prints the absolute path of the built binary on its last line of stdout, so
# a caller can run
#
#     SUNRISE_BASELINE_DRIVER=$(crates/sunrise-e2e/baseline-driver/build-baseline.sh d9566ade | tail -n1) \
#       cargo test -p sunrise-e2e --test cross_version_convergence -- --ignored
#
# What it does, and why each step is that step (ADR-0057):
#
# 1. Extracts the ref with `git archive` into
#    `target/sunrise-baseline/<ref>/src/`. Not a `git worktree`, which would
#    register itself in this clone's metadata; an extracted tree is plain
#    files under the ignored `target/`.
# 2. Copies this directory in as `crates/sunrise-baseline-driver`. The
#    baseline's own workspace globs `crates/*`, so the driver becomes one of
#    its members and every `workspace = true` dependency resolves to the
#    baseline's version.
# 3. Builds it against the baseline's own `Cargo.lock`, so the driver links
#    the exact dependency graph that commit had. Adding a member is the only
#    lock change cargo makes, which is why this is not `--locked`.
# 4. Stamps the full commit id into the binary (`SUNRISE_BASELINE_REF`), so
#    the harness learns which baseline it is talking to from the binary itself
#    and matches it against `KNOWN_GAPS` whatever spelling the ref was given in.
#
# The ref must be present locally. A shallow CI checkout has neither the tag
# nor the commit; the CI job fetches the one it needs before calling this.

set -euo pipefail

ref="${1:?usage: build-baseline.sh <ref>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
target_root="${CARGO_TARGET_DIR:-$repo/target}"
tree="$target_root/sunrise-baseline/$ref/src"
build="$target_root/sunrise-baseline/$ref/target"

if ! commit="$(git -C "$repo" rev-parse --verify --quiet "$ref^{commit}")"; then
  echo "build-baseline: $ref is not in this clone; fetch it first, e.g." >&2
  echo "  git fetch --depth=1 origin $ref" >&2
  exit 1
fi

# Re-extract every time: a tree left over from an earlier driver revision would
# build the old driver source against the right baseline.
rm -rf "$tree"
mkdir -p "$tree"
git -C "$repo" archive --format=tar "$commit" | tar -x -C "$tree"

if [ -e "$tree/crates/sunrise-baseline-driver" ]; then
  echo "build-baseline: $ref already has a crates/sunrise-baseline-driver; refusing to overwrite it" >&2
  exit 1
fi
mkdir -p "$tree/crates/sunrise-baseline-driver"
cp "$here/Cargo.toml" "$tree/crates/sunrise-baseline-driver/Cargo.toml"
cp -R "$here/src" "$tree/crates/sunrise-baseline-driver/src"

# The baseline's own target directory, never this checkout's: two workspaces
# that share package names must not share build output. It sits beside the
# extracted tree rather than inside it, so re-extracting does not throw away
# the build and a CI cache can hold it.
(
  cd "$tree"
  SUNRISE_BASELINE_REF="$commit" CARGO_TARGET_DIR="$build" \
    cargo build --quiet -p sunrise-baseline-driver >&2
)

bin="$build/debug/sunrise-baseline-driver"
if [ ! -x "$bin" ]; then
  echo "build-baseline: cargo reported success but $bin is missing" >&2
  exit 1
fi
echo "$bin"

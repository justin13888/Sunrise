#!/usr/bin/env bash
# Fetch the pinned Sparkle command-line tools and prove they are the right ones.
#
# `sign_update` is what turns a `.dmg` and an `appcast.xml` into things an
# installed copy of Sunrise will accept. It is not in the SPM package the app
# links -- that ships the framework -- so the release workflow downloads the
# tools tarball from the Sparkle project's own GitHub Release.
#
# Three things make that safe enough to put in a release path:
#
#   1. The version is pinned here, not resolved.
#   2. The tarball's SHA-256 is pinned here too and checked before anything is
#      extracted, so a replaced asset fails rather than signs.
#   3. The pin is asserted equal to `apps/apple/project.yml`'s `exactVersion`
#      for the Sparkle package. Signing a feed with a different release of the
#      tooling than the framework that will verify it is exactly the kind of
#      skew that shows up as "updates stopped working" months later.
#
# Bumping Sparkle is therefore a three-line commit -- the two constants below
# and the `exactVersion` in project.yml -- and the gate fails loudly if only
# some of them move.
#
# Usage: sparkle-tools.sh <destination-directory>
# Prints the path to the extracted `bin` directory on stdout.
#
# See docs/11-adr/0038-macos-update-feed.md.
set -euo pipefail

SPARKLE_VERSION="2.10.0"
# Of https://github.com/sparkle-project/Sparkle/releases/download/2.10.0/Sparkle-2.10.0.tar.xz
SPARKLE_SHA256="c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c"

dest="${1:?usage: sparkle-tools.sh <destination-directory>}"

# The Sparkle package block in project.yml, so the assertion cannot be
# satisfied by an `exactVersion` belonging to some other package added later.
pinned=$(awk '
  /^packages:/            { in_packages = 1; next }
  in_packages && /^[^ ]/  { in_packages = 0 }
  in_packages && /^  Sparkle:/ { in_sparkle = 1; next }
  in_sparkle && /^  [^ ]/ { in_sparkle = 0 }
  in_sparkle && $1 == "exactVersion:" { gsub(/"/, "", $2); print $2; exit }
' apps/apple/project.yml)

if [ "$pinned" != "$SPARKLE_VERSION" ]; then
  echo "::error title=Sparkle pin mismatch::apps/apple/project.yml pins the Sparkle framework at '${pinned}' and .github/scripts/sparkle-tools.sh pins the signing tools at '${SPARKLE_VERSION}'. The tool that signs the appcast and the framework that verifies it must be the same release. Update both."
  exit 1
fi

mkdir -p "$dest"
tarball="$dest/Sparkle-${SPARKLE_VERSION}.tar.xz"
curl --fail --silent --show-error --location \
  --retry 3 --retry-connrefused \
  -o "$tarball" \
  "https://github.com/sparkle-project/Sparkle/releases/download/${SPARKLE_VERSION}/Sparkle-${SPARKLE_VERSION}.tar.xz"

actual=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
if [ "$actual" != "$SPARKLE_SHA256" ]; then
  echo "::error title=Sparkle tools checksum mismatch::Sparkle-${SPARKLE_VERSION}.tar.xz hashed ${actual}, expected ${SPARKLE_SHA256}. Nothing was extracted and nothing is signed."
  exit 1
fi

tar -xJf "$tarball" -C "$dest"
test -x "$dest/bin/sign_update"
echo "$dest/bin"

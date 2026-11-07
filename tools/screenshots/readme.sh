#!/usr/bin/env bash
# Regenerate the two screenshots at the top of the README:
#
#   docs/assets/screenshots/ios.png    the iPhone app's Today tab
#   docs/assets/screenshots/macos.png  the Mac app's Today window
#
# Run it on a Mac from anywhere in the repository:
#
#   tools/screenshots/readme.sh
#
# It renders nothing itself. `mise run apple-shots` walks both apps through
# their XCUITest screenshot walks (`ScreenshotWalkTests`), which start from a
# fresh vault and type the same five captures through the app's own capture
# bar, so the picture is the app a user would see and not a mock of it. This
# script pins what the walk leaves to the machine, runs it, and copies the
# walk's `02-today` frame for each platform into place.
#
# What is pinned, so two runs on two Macs differ only where the app does:
#
# - the simulator: the one `mise.toml` names as `ios_sim`, nothing booted ad hoc;
# - its status bar: 9:41, full Wi-Fi and cellular, a charged battery, rather
#   than the minute and charge level the run happened at;
# - its appearance: light.
#
# What is not pinned: the date the Today screen heads itself with (the walk's
# captures are `^today` and `^tomorrow`, so the tasks are the same on any
# day), and the Mac's own appearance, which is the host's system setting and
# not this script's to change.
#
# The Mac half is an XCUITest, so `apple-shots` needs developer mode and, on
# the first run from a checkout, an automation prompt accepted; it says so
# itself. The task leaves every other frame in `out/shots/`.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

if [ "$(uname -s)" != Darwin ]; then
  echo "tools/screenshots/readme.sh needs a Mac with Xcode" >&2
  exit 1
fi

# The simulator name lives in mise.toml's [env]; reading it from there keeps
# this script and `apple-shots` on one device.
sim="$(mise env --json | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["ios_sim"])')"

xcrun simctl boot "$sim" 2>/dev/null || true
xcrun simctl bootstatus "$sim" -b >/dev/null
# The override lasts until it is cleared or the device shuts down, so clear it
# on the way out: a developer's simulator should not be left reading 9:41.
trap 'xcrun simctl status_bar "$sim" clear 2>/dev/null || true' EXIT
xcrun simctl status_bar "$sim" override \
  --time 9:41 \
  --dataNetwork wifi --wifiMode active --wifiBars 3 \
  --cellularMode active --cellularBars 4 \
  --batteryState charged --batteryLevel 100
xcrun simctl ui "$sim" appearance light

mise run apple-shots

mkdir -p docs/assets/screenshots
for platform in ios macos; do
  frame="out/shots/$platform/02-today.png"
  if [ ! -f "$frame" ]; then
    echo "the $platform walk wrote no 02-today frame; see out/shots/$platform.xcresult" >&2
    exit 1
  fi
  cp "$frame" "docs/assets/screenshots/$platform.png"
done
echo "docs/assets/screenshots/ios.png and docs/assets/screenshots/macos.png"

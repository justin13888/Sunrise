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
# - the simulator: one device named `ios_sim` in `mise.toml`, chosen by UDID
#   and handed to `apple-shots`, so the device pinned is the one photographed;
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
name="$(mise env --json | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["ios_sim"])')"

# The device by UDID, picked the way `mise.toml`'s ios-app task picks it: a
# Mac with several iOS runtimes carries one "$name" per runtime, and a name
# does not say which of them `xcodebuild` tests on, so pinning by name could
# pin a device nobody photographs. The pick is the newest runtime whose
# major.minor the installed simulator SDK can build for. `apple-shots` tests on
# the same device because `ios_sim_udid` is passed through to it below.
sdk="$(xcrun --sdk iphonesimulator --show-sdk-version)"
sim="$(xcrun simctl list devices available -j | python3 -I -c '
import json, re, sys
name, sdk = sys.argv[1], tuple(int(p) for p in sys.argv[2].split(".")[:2])
found = []
for runtime, devices in json.load(sys.stdin)["devices"].items():
    m = re.search(r"\.iOS-(\d+)-(\d+)(?:-(\d+))?$", runtime)
    if not m or tuple(int(p) for p in m.groups()[:2]) > sdk:
        continue
    version = tuple(int(p or 0) for p in m.groups())
    found += [(version, d["udid"]) for d in devices if d["name"] == name]
if not found:
    sys.exit("no available iOS simulator named %r at or below SDK %s" % (name, sys.argv[2]))
print(max(found)[1])
' "$name" "$sdk")"
echo "pinning and photographing $name ($sim), iOS Simulator SDK $sdk" >&2

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

# See the note in readme.xcconfig: without it the macOS walk never starts.
XCODE_XCCONFIG_FILE="$root/tools/screenshots/readme.xcconfig" ios_sim_udid="$sim" \
  mise run apple-shots

# The macOS frame is only safe to commit once the walk photographs the app's
# window. While `ScreenshotWalk.swift` still captures with `app.screenshot()`,
# macOS returns the whole display (menu bar, desktop, Dock, other windows), and
# copying that frame would put the operator's screen at a path meant to be
# committed. Until the capture is scoped to `app.windows.firstMatch`, the macOS
# frame stays in out/shots/ and is not copied.
walk=apps/apple/UITestSupport/ScreenshotWalk.swift
platforms=ios
if grep -q 'windows\.firstMatch\.screenshot()' "$walk"; then
  platforms="ios macos"
else
  echo "not copying the macOS frame: $walk captures the whole display, not the app's window" >&2
fi

mkdir -p docs/assets/screenshots
for platform in $platforms; do
  frame="out/shots/$platform/02-today.png"
  if [ ! -f "$frame" ]; then
    echo "the $platform walk wrote no 02-today frame; see out/shots/$platform.xcresult" >&2
    exit 1
  fi
  cp "$frame" "docs/assets/screenshots/$platform.png"
  echo "docs/assets/screenshots/$platform.png"
done
if [ "$platforms" = ios ]; then
  exit 1
fi

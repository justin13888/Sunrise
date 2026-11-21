#!/usr/bin/env bash
# Run one command against a throwaway loopback `sunrise-server` (#1242).
#
#   tools/apple-ui-test-relay.sh COMMAND [ARG...]
#   tools/apple-ui-test-relay.sh --build
#
# The restore-from-code XCUITests (`RestoreFromCodeUITests`, #458) found an
# account on a relay and restore it on a second device. They read the relay's
# base URL from `SUNRISE_UI_TEST_RELAY` in the test runner's environment, and
# skip when it is unset. `xcodebuild` sets a runner variable from
# `TEST_RUNNER_<NAME>` in its own environment, never from a build setting, so
# this script starts a relay, exports `TEST_RUNNER_SUNRISE_UI_TEST_RELAY` for
# COMMAND alone, and stops the relay when COMMAND ends, whatever its outcome.
# It exits with COMMAND's status.
#
# A fresh relay per call, and never a reused one. The self-host `NullVerifier`
# maps every bearer to one account whose recovery blob is write-once, so a
# relay seeded by an earlier run refuses the next founding with
# `409 RECOVERY_BLOB_EXISTS` (see `SunriseUITestCase.relayVariable`). Each call
# therefore gets its own port and its own data directory, under the
# repository's gitignored `target/apple-ui-test-relay/`, removed on exit.
#
# The relay binds `127.0.0.1` only: the `NullVerifier` accepts any bearer, and
# the server refuses to bind anything but loopback without OIDC configured.
#
# The binary is `SUNRISE_UI_TEST_RELAY_BIN` when that is set, an absolute path
# or one relative to the repository root; CI sets it to the binary the
# `apple-xcframework` job built and handed over, because the `ios-app` job
# compiles no Rust. Unset, the script builds `sunrise-server` with cargo.
# `--build` does only that build and prints the binary's path, which is how
# CI produces the artefact, so the build is defined here once.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
  echo "apple-ui-test-relay: $1" >&2
  exit 1
}

# Where cargo writes, asked rather than assumed, as `apple-xcframework` in
# mise.toml does and for the same reason: CARGO_TARGET_DIR and
# `build.target-dir` both move it.
build_relay() {
  (cd "$root" && cargo build -q -p sunrise-server --bin sunrise-server) >&2
  local target_dir
  target_dir=$(cd "$root" && cargo metadata --format-version 1 --no-deps \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
  echo "$target_dir/debug/sunrise-server"
}

if [ $# -eq 0 ]; then
  fail "usage: $0 COMMAND [ARG...] | $0 --build"
fi
if [ "$1" = "--build" ]; then
  build_relay
  exit 0
fi

bin="${SUNRISE_UI_TEST_RELAY_BIN:-}"
if [ -z "$bin" ]; then
  bin="$(build_relay)"
elif [ "${bin#/}" = "$bin" ]; then
  bin="$root/$bin"
fi
[ -x "$bin" ] || fail "$bin is not an executable sunrise-server"

work="$root/target/apple-ui-test-relay"
mkdir -p "$work"
run="$(mktemp -d "$work/run.XXXXXX")"
relay_pid=""
# shellcheck disable=SC2329 # invoked by the EXIT trap below
cleanup() {
  if [ -n "$relay_pid" ]; then
    kill "$relay_pid" 2>/dev/null || true
    wait "$relay_pid" 2>/dev/null || true
  fi
  rm -rf "$run"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# A port the kernel says is free now. Another process could take it before the
# relay binds; the readiness check below then fails naming the relay, rather
# than the tests skipping or talking to something else.
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
config="$run/sunrise.toml"
mkdir -p "$run/data"
printf '[server]\nlisten = "127.0.0.1:%s"\n\n[storage]\ndata_dir = "%s"\n' \
  "$port" "$run/data" >"$config"

SUNRISE_CONFIG="$config" "$bin" >"$run/relay.log" 2>&1 &
relay_pid=$!

# Ready when its own healthcheck answers, which probes the address the config
# binds. Bounded: a relay that dies or never listens fails here in thirty
# seconds, with its log, instead of every relay test failing after it.
ready_limit=30
started=$SECONDS
until SUNRISE_CONFIG="$config" "$bin" healthcheck >/dev/null 2>&1; do
  if ! kill -0 "$relay_pid" 2>/dev/null; then
    cat "$run/relay.log" >&2
    fail "sunrise-server exited before it answered on 127.0.0.1:$port"
  fi
  if [ $((SECONDS - started)) -ge "$ready_limit" ]; then
    cat "$run/relay.log" >&2
    fail "sunrise-server did not answer on 127.0.0.1:$port within ${ready_limit}s"
  fi
  sleep 0.2
done

url="http://127.0.0.1:$port"
echo "apple-ui-test-relay: sunrise-server at $url (data under $run)" >&2
status=0
TEST_RUNNER_SUNRISE_UI_TEST_RELAY="$url" "$@" || status=$?
if [ "$status" -ne 0 ]; then
  echo "--- last 40 lines of the relay log ---" >&2
  tail -n 40 "$run/relay.log" >&2 || true
fi
exit "$status"

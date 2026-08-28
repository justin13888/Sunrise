# Sunrise developer task runner.
# All project commands live here; git hooks (lefthook.yaml) call these recipes.
# Run `just` or `just --list` to see everything available.

# Show available recipes
default:
    @just --list

# Install JS dependencies and git hooks
[group('setup')]
setup:
    bun install
    lefthook install

# --- JavaScript / TypeScript (Bun + Biome + tsc) ---

# Lint & format check, no writes (Biome)
[group('js')]
check:
    bun run check

# Lint & format with autofix (Biome)
[group('js')]
fix:
    bun run check:fix

# Strict CI lint check, no writes (Biome)
[group('js')]
ci:
    bun run ci

# Type-check every workspace package
[group('js')]
typecheck:
    bun run typecheck

# Run the JS/TS test suite once
[group('js')]
test:
    bun run test:run

# Run the JS/TS test suite with coverage
[group('js')]
test-coverage:
    bun run test:coverage

# --- Rust (Cargo) ---

# Check Rust formatting, no writes
[group('rust')]
rust-fmt-check:
    cargo fmt --all -- --check

# Format Rust code in place
[group('rust')]
rust-fmt:
    cargo fmt --all

# Lint Rust with Clippy (warnings denied)
[group('rust')]
rust-clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Type-check the Rust workspace
[group('rust')]
rust-check:
    cargo check --workspace --all-targets

# Run the Rust test suite
[group('rust')]
rust-test:
    cargo test --workspace --all-targets

# Reject crates that no shipping binary can reach (the CI gate, run locally)
[group('rust')]
orphan-crates:
    .github/scripts/orphan-crate-gate.py

# Run the Criterion benchmark suite (submit / query_today / fts / ws_handshake)
[group('rust')]
bench:
    cargo bench -p sunrise-bench

# Run the benches, then merge the results into bench/baseline.json for this platform
[group('rust')]
bench-baseline:
    cargo bench -p sunrise-bench
    cargo run -p sunrise-bench --bin baseline

# --- macOS client (Swift / UniFFI) ---

# The UniFFI-exported crate, its underscored cargo lib name, the framework, and
# the slices to build. Add `x86_64-apple-darwin` here (and `rustup target add`
# it) for a universal binary; `lipo` below already handles more than one.
ffi_crate  := "sunrise-core-bindings"
ffi_lib    := "sunrise_core_bindings"
ffi_fw     := "SunriseCore"
ffi_slices := "aarch64-apple-darwin"

# Deployment target. Must equal `options.deploymentTarget.macOS` in
# apps/macos/project.yml; see the note in `macos-xcframework`.
macos_target := "26.0"

# Build the Swift bindings + SunriseCore.xcframework the macOS app links
[group('macos')]
macos-xcframework:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf build out/{{ffi_fw}}.xcframework
    mkdir -p build/headers build/macos out/swift

    # 1. One static + dynamic lib per slice. `--library` binding generation
    #    reads the .dylib, so both crate-types are load-bearing.
    #
    #    `MACOSX_DEPLOYMENT_TARGET` must match the app's, or every object file
    #    in the archive draws an `ld` warning about being built for a newer
    #    macOS than it is linked against — hundreds of them, from the C
    #    dependencies (sqlite, zstd, aws-lc), drowning every real diagnostic.
    export MACOSX_DEPLOYMENT_TARGET="{{macos_target}}"
    for t in {{ffi_slices}}; do cargo build -p {{ffi_crate}} --release --target "$t"; done

    # 2. Bindings, generated from the built library — no .udl, no build.rs.
    #    `--locked` is not optional: the generator lives outside the workspace
    #    precisely so its cargo-platform pin survives, and `cargo build` without
    #    it would re-resolve to a version that needs rustc 1.91.
    first=$(echo {{ffi_slices}} | awk '{print $1}')
    cargo build --locked --release --manifest-path tools/uniffi-bindgen/Cargo.toml
    tools/uniffi-bindgen/target/release/uniffi-bindgen generate \
      --library "target/$first/release/lib{{ffi_lib}}.dylib" \
      --language swift --out-dir out/swift

    # 3. The module map. UniFFI's own is unusable inside an xcframework: it is
    #    named <lib>FFI.modulemap, which Xcode does not look for, and it emits
    #    `use Darwin` / `use _Builtin_stdbool` / `use _Builtin_stdint` lines
    #    that fail to resolve there. Rewriting it is the fix; the module name
    #    must stay <lib>FFI to match the generated `import`.
    cp "out/swift/{{ffi_lib}}FFI.h" build/headers/
    printf 'module %sFFI {\n    header "%sFFI.h"\n    export *\n}\n' \
      {{ffi_lib}} {{ffi_lib}} > build/headers/module.modulemap

    # 4. One fat static lib, then package it.
    libs=""
    for t in {{ffi_slices}}; do libs="$libs target/$t/release/lib{{ffi_lib}}.a"; done
    lipo -create $libs -output "build/macos/lib{{ffi_lib}}.a"
    xcodebuild -create-xcframework \
      -library "build/macos/lib{{ffi_lib}}.a" \
      -headers build/headers \
      -output "out/{{ffi_fw}}.xcframework"
    echo "out/{{ffi_fw}}.xcframework + out/swift/{{ffi_lib}}.swift"

# Generate the Xcode project, build the macOS app, and run its tests.
#
# `xcodegen` needs the generated Swift bindings to exist before it can add them
# to the target, so the xcframework is built first even though the project's
# own `SunriseFFI` target would rebuild it. `.xcodeproj` is gitignored: this
# recipe is the only supported way to get one.
[group('macos')]
macos-app: macos-xcframework
    #!/usr/bin/env bash
    set -euo pipefail
    cd apps/macos
    xcodegen generate --quiet
    swiftlint lint --strict --quiet --config .swiftlint.yml
    xcodebuild test \
      -project Sunrise.xcodeproj \
      -scheme Sunrise \
      -destination 'platform=macOS,arch=arm64' \
      -quiet \
      CODE_SIGNING_ALLOWED=NO

# Drive the real window (XCUITest); needs `sudo DevToolsSecurity -enable` once
[group('macos')]
macos-uitest: macos-xcframework
    #!/usr/bin/env bash
    set -euo pipefail
    # Separate from `macos-app` because it needs one thing a build must not do
    # for you. A macOS UI test takes control of another process, and the system
    # kills the runner ("signal kill before establishing connection") unless
    # developer mode is on — a one-time change to the machine's security
    # posture. Signing is left on, too: the runner will not launch
    # ad-hoc-unsigned.
    if ! DevToolsSecurity -status | grep -q enabled; then
      echo "developer mode is off; run: sudo DevToolsSecurity -enable" >&2
      exit 1
    fi
    cd apps/macos
    xcodegen generate --quiet
    xcodebuild test \
      -project Sunrise.xcodeproj \
      -scheme Sunrise \
      -destination 'platform=macOS,arch=arm64' \
      -only-testing:SunriseUITests \
      -skip-testing:SunriseTests \
      -quiet

# Generate the Xcode project and open it. Everyday development entry point.
[group('macos')]
macos-open: macos-xcframework
    cd apps/macos && xcodegen generate --quiet && open Sunrise.xcodeproj

# --- Release artifacts ---

# Same Dockerfile and the same build args the release workflow uses, so a
# failure here is a failure there. Note the size of the job: a release build of
# the workspace's C dependencies (SQLCipher, ring, zstd) inside a fresh
# container wants ~10 GB of container storage and tens of minutes cold.

# Build the sunrise-server image that the release workflow publishes to GHCR
[group('release')]
docker-build tag="sunrise-server:dev":
    docker build \
      --build-arg VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)" \
      --build-arg VCS_REF="$(git rev-parse --short HEAD)" \
      -t {{tag}} .

# --- Aggregates (mirror the git hooks; handy to run by hand) ---

# Everything the pre-commit hook runs
[group('hooks')]
pre-commit: fix typecheck rust-fmt-check rust-clippy

# Everything the pre-push hook runs
[group('hooks')]
pre-push: ci typecheck test rust-test

# Full local validation: Biome CI + typecheck + coverage
[group('hooks')]
validate:
    bun run validate

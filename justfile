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

# Sunrise

Managed by Bun workspace. Core logic must be isolated to make it possible to deterministically unit test in isolation. UI is modern, efficient, and thoughtfully laid out.

## Gates

The README's [Development](README.md#development) section is the reference; `mise.toml` holds every task and `lefthook.yaml` only calls them.

- A fresh checkout needs `mise install`, then `mise run setup` (`bun install` and the git hooks).
- The pre-commit hook fixes Biome and runs the typecheck when the staged files reach them, `rust-fmt-check` and `rust-clippy-workspace` for staged Rust, and `rust-clippy-fuzz-staged` when `.github/scripts/fuzz-closure-touched.py` says the staged files reach `fuzz/` (including `Cargo.toml`, `Cargo.lock`, `.cargo/`, `rust-toolchain*` or `clippy.toml`, not only staged Rust).
- The pre-push hook runs `mise run ci`, `mise run typecheck`, `mise run rust-fmt-check` and `mise run rust-clippy-workspace` over the whole tree, and no test suite.
- CI on a pull request runs only the fast checks (format, lint, typecheck, `cargo-deny` and the small gate jobs); the test suites run after the merge, on `master`. Run the tests a change reaches before pushing: `mise run validate` (Biome CI, typecheck, JS/TS coverage) and `mise run rust-test`, or `mise run test-changed` and `mise run rust-test-changed` for only what the push can affect. `mise run pre-push` runs the hook's checks and both suites whole.

CI does not run the Apple jobs on pull requests. Before merging a change that reaches what the Apple apps are built from (the path list is in the README's Apple section), run `mise run apple-app` on a Mac at the pull request's head. It is the same task CI runs after the merge.


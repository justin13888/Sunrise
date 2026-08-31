//! `uniffi-bindgen` — the binding generator, quarantined from the workspace.
//!
//! See this crate's `Cargo.toml` for why it is not a workspace member. It is a
//! build tool: it never ships, and nothing in `crates/` depends on it.
//!
//! Driven by `just apple-xcframework`, in `--library` mode (bindings are read
//! out of the built `.dylib`, so there is no `.udl` file and no `build.rs`).

fn main() {
    uniffi::uniffi_bindgen_main();
}

//! The workaround in `init.rs` has an expiry date, and this is it.
//!
//! `pin_interest_cache` exists to defeat a defect in `tracing-core`, not in
//! this workspace: while exactly one `Dispatch` is registered process-wide,
//! `Dispatchers::rebuilder` returns `Rebuilder::JustOne`, whose `for_each`
//! calls `dispatcher::get_default` — the *registering thread's* default
//! subscriber — instead of folding over the registry. A thread with no
//! subscriber therefore caches `Interest::never()` for a callsite another
//! thread's capture is about to emit into. `#185` records the diagnosis and
//! `tests/interest_cache.rs` pins the fix.
//!
//! A workaround for someone else's bug has exactly one failure mode that a
//! test suite cannot otherwise see: the bug gets fixed and the workaround
//! stays forever, because nobody was watching. So this test watches. It fails
//! the moment the locked `tracing-core` moves off the version the defect was
//! confirmed in, which is the moment somebody should go and look.
//!
//! # State of the upstream fix, as of this commit
//!
//! **Not fixed.** `tracing-core 0.1.36` is the newest release on crates.io and
//! is what `Cargo.lock` pins. The `v0.1.x` release branch of `tokio-rs/tracing`
//! still carries the `Rebuilder::JustOne` shortcut, at the same line numbers as
//! the published 0.1.36 source (`tracing-core/src/callsite.rs`, `enum Rebuilder`
//! and `Dispatchers::rebuilder`), so there is no unreleased fix waiting either.
//! The workaround stays; this test is what retires it.
//!
//! # Why the lock file rather than a version constant
//!
//! There is no way for a crate to ask the compiler what version of a
//! dependency it was built against — `env!("CARGO_PKG_VERSION")` answers for
//! this crate only, and a `build.rs` would have to shell out to `cargo
//! metadata` to learn any more. `Cargo.lock` is committed, is the thing that
//! actually changes when someone runs `cargo update`, and is one file read.

use std::path::PathBuf;

/// The `tracing-core` release the `Rebuilder::JustOne` defect was confirmed in
/// and the workaround was written against.
///
/// Bumping this is a deliberate act. See the failure message below for what to
/// check first.
const CONFIRMED_BUGGY: &str = "0.1.36";

/// The workspace root: this crate's manifest directory, up two.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/<name> always has two ancestors")
        .to_path_buf()
}

/// The `version` recorded for `name` in a `Cargo.lock`.
///
/// A lock file is a flat list of `[[package]]` tables whose first two keys are
/// always `name` and `version`, in that order, written by cargo rather than by
/// hand. Scanning for the name and taking the next `version = ` line is
/// therefore exact here, and needs no TOML parser in this crate's dev
/// dependencies.
fn locked_version(lock: &str, name: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() != format!("name = \"{name}\"") {
            continue;
        }
        for following in lines.by_ref() {
            if let Some(rest) = following.trim().strip_prefix("version = ") {
                return Some(rest.trim_matches('"').to_owned());
            }
            if following.trim().starts_with("[[") {
                break;
            }
        }
    }
    None
}

#[test]
fn the_interest_cache_workaround_is_still_needed() {
    let lock_path = workspace_root().join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", lock_path.display()));
    let locked = locked_version(&lock, "tracing-core")
        .expect("Cargo.lock records a version for tracing-core");

    assert_eq!(
        locked, CONFIRMED_BUGGY,
        "tracing-core moved from {CONFIRMED_BUGGY} to {locked}, and this workspace \
         carries a workaround for a tracing-core defect (#185).\n\
         \n\
         Check tracing-core {locked}'s `src/callsite.rs`. If `Rebuilder::JustOne` \
         is gone — or its `for_each` no longer calls `dispatcher::get_default` — \
         the defect is fixed upstream: delete `pin_interest_cache`, the `Inert` \
         subscriber and its `INTEREST_PIN` from `crates/sunrise-log/src/init.rs`, \
         delete `crates/sunrise-log/tests/interest_cache.rs`, and delete this \
         file. If the shortcut is still there, the workaround is still load-bearing: \
         update CONFIRMED_BUGGY to {locked} and say in the commit body that you \
         looked."
    );
}

#[test]
fn the_version_scan_reads_the_package_it_was_asked_for() {
    // Shapes cargo actually writes, including a package that precedes the one
    // asked for and one that shares a prefix with it.
    let lock = "\
[[package]]
name = \"tracing\"
version = \"0.1.44\"

[[package]]
name = \"tracing-core\"
version = \"0.1.36\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"

[[package]]
name = \"tracing-subscriber\"
version = \"0.3.23\"
";
    assert_eq!(locked_version(lock, "tracing").as_deref(), Some("0.1.44"));
    assert_eq!(
        locked_version(lock, "tracing-core").as_deref(),
        Some("0.1.36")
    );
    assert_eq!(
        locked_version(lock, "tracing-subscriber").as_deref(),
        Some("0.3.23")
    );
    assert_eq!(locked_version(lock, "nonesuch"), None);
}

#[test]
fn a_package_table_without_a_version_does_not_borrow_the_next_ones() {
    // Defensive: if a `[[package]]` ever lacked a `version`, taking "the next
    // `version = ` line in the file" would silently report the following
    // package's. The scan stops at the next table header instead.
    let lock = "\
[[package]]
name = \"broken\"

[[package]]
name = \"tracing-core\"
version = \"0.1.36\"
";
    assert_eq!(locked_version(lock, "broken"), None);
    assert_eq!(
        locked_version(lock, "tracing-core").as_deref(),
        Some("0.1.36")
    );
}

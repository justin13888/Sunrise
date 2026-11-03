//! One resolved seed for every randomised harness in this workspace.
//!
//! `docs/10-cross-cutting/testing.md` documents `SUNRISE_FUZZ_SEED` as *the*
//! seed convention. Until this crate existed the convention had exactly one
//! consumer — the chaos harness — and the property tests had a different
//! reproduction story: proptest's persistence file and nothing else. A failing
//! CI run therefore handed you a seed one half of the suite could not use, and
//! a counterexample the other half never printed (#119).
//!
//! This crate is the single reader. Set `SUNRISE_FUZZ_SEED` and every
//! randomised harness in the workspace draws from it:
//!
//! ```text
//! SUNRISE_FUZZ_SEED=0x5352_5f43_4841_4f53   the chaos harness's RNG stream
//!                                           every proptest's ProptestConfig
//! ```
//!
//! # What a seed does and does not buy
//!
//! A seed reproduces the **run**: the same cases, in the same order, drawn
//! from the same stream. proptest's persistence file reproduces a **specific
//! minimal case** — the shrunken counterexample, which is the thing a failing
//! CI run actually hands you and which no seed can regenerate once the
//! generator's shape changes. Both exist and neither replaces the other; see
//! `.github/scripts/proptest-persistence-gate.py` for the file half.
//!
//! # The fallbacks differ, on purpose
//!
//! When `SUNRISE_FUZZ_SEED` is unset the two harnesses fall back differently,
//! because they are asking for different things:
//!
//! * **The chaos harness** falls back to [`DEFAULT_FUZZ_SEED`], a fixed
//!   constant, so a chaos run reproduces out of the box and two developers see
//!   the same fault schedule. That was already its documented behaviour and
//!   this crate does not change it.
//! * **Property tests** fall back to a fresh random seed drawn once per
//!   process — which is proptest's own default behaviour, and is kept because
//!   pinning a constant here would mean every CI run forever explores the same
//!   256 cases out of an infinite space. The seed is **announced** instead
//!   ([`proptest_rng_seed`]), so a failing run tells you what to export.
//!
//! One variable, one parser, one announcement. The value in the announcement
//! is the one thing to set.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// This crate's entire output is a line telling a developer how to reproduce a
// failing run, and the only channel a test harness has for that is stderr.
// `print_stderr` is denied workspace-wide because a shipping binary's stderr
// is an ingest surface (`docs/10-cross-cutting/logging.md`); nothing here
// ships.
#![allow(clippy::print_stderr)]

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

use proptest::test_runner::{Config as ProptestConfig, RngSeed};

/// The environment variable naming the seed, for every harness in the
/// workspace.
const ENV_FUZZ_SEED: &str = "SUNRISE_FUZZ_SEED";

/// Seed the chaos harness uses when `SUNRISE_FUZZ_SEED` is unset and no explicit
/// seed is supplied. Fixed so that harness is reproducible out of the box.
pub const DEFAULT_FUZZ_SEED: u64 = 0x5352_5f43_4841_4f53; // "SR_CHAOS"

/// Parse one [`ENV_FUZZ_SEED`] value.
///
/// Per `docs/10-cross-cutting/testing.md` the convention is hex, but a plain
/// decimal integer is also accepted, and a leading `0x`/`0X` forces hex. A
/// bare `42` is therefore decimal 42 and a bare `deadbeef` is hex — the
/// decimal reading is tried first and hex is the fallback, so the two never
/// disagree about a string that could be read either way.
///
/// Returns `None` for anything that is neither.
///
/// Pure over its argument: no environment, no process state. This is the whole
/// of the parsing rule and it is unit-tested on its own.
#[must_use]
fn parse_seed(raw: &str) -> Option<u64> {
    let t = raw.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>()
            .ok()
            .or_else(|| u64::from_str_radix(t, 16).ok())
    }
}

/// Resolve the seed from `SUNRISE_FUZZ_SEED`, or `None` when it is unset or
/// unparseable.
#[must_use]
pub fn seed_from_env() -> Option<u64> {
    parse_seed(&std::env::var(ENV_FUZZ_SEED).ok()?)
}

/// Which seed a `ProptestConfig` should carry, given the three inputs.
///
/// Pure, so the precedence rule is testable without touching the process
/// environment — which is the only part of this that anyone will need to argue
/// about later.
///
/// * `native` is what proptest itself resolved from `PROPTEST_RNG_SEED`
///   (`RngSeed::Fixed` when that variable was set and parsed,
///   `RngSeed::Random` otherwise). **It wins.** proptest's own dial is the one
///   a proptest user reaches for first, and silently overriding it from this
///   crate would make that dial appear broken; the same argument
///   `op_envelope_proptest.rs` makes about `PROPTEST_CASES`.
/// * `sunrise` is [`seed_from_env`] — the workspace-wide convention.
/// * `fallback` is used when neither variable is set.
#[must_use]
fn choose_rng_seed(native: RngSeed, sunrise: Option<u64>, fallback: u64) -> RngSeed {
    match (native, sunrise) {
        (RngSeed::Fixed(n), _) | (RngSeed::Random, Some(n)) => RngSeed::Fixed(n),
        (RngSeed::Random, None) => RngSeed::Fixed(fallback),
    }
}

/// A seed drawn once for this process, for when nothing pinned one.
///
/// `RandomState` is seeded from the operating system's entropy on first use,
/// which is exactly the property wanted and is reached without a `rand`
/// dependency — see this crate's manifest for why `rand` is the wrong tool
/// here specifically.
fn process_seed() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| RandomState::new().build_hasher().finish())
}

/// The seed every property test in this workspace runs under, as a
/// [`RngSeed`], announced on stderr.
///
/// Drop it into a `ProptestConfig` literal:
///
/// ```
/// use proptest::prelude::ProptestConfig;
///
/// let config = ProptestConfig {
///     cases: sunrise_test_seed::proptest_cases(256),
///     rng_seed: sunrise_test_seed::proptest_rng_seed(),
///     ..ProptestConfig::default()
/// };
/// ```
///
/// # Why this announces on every call rather than once per process
///
/// `cargo test` captures a test's output and prints it only when that test
/// fails. A once-per-process line would be captured by whichever test happened
/// to call first and would then be invisible in every *other* test's failure
/// report — including the one you are actually reading. The seed itself is
/// resolved once per process, so every announcement in a run names the
/// same value; only the printing repeats, and only into buffers that are
/// discarded unless something fails.
#[must_use]
pub fn proptest_rng_seed() -> RngSeed {
    let chosen = choose_rng_seed(
        ProptestConfig::default().rng_seed,
        seed_from_env(),
        process_seed(),
    );
    if let RngSeed::Fixed(value) = chosen {
        announce("proptest", value);
    }
    chosen
}

/// The environment variable choosing how deep every property test runs.
///
/// `full` runs each block at the count its call site names — the count CI
/// runs, and the one `.github/workflows/ci.yml` asks for. Unset, or `local`,
/// caps every block at [`LOCAL_PROPTEST_CASES`] so an ordinary `cargo test`
/// stays cheap. Any other value panics: a typo in CI must fail loudly rather
/// than quietly shrink the run to the local count.
pub const ENV_PROPTEST_DEPTH: &str = "SUNRISE_PROPTEST_DEPTH";

/// The most cases any property test runs when [`ENV_PROPTEST_DEPTH`] is not
/// `full` and `PROPTEST_CASES` is unset.
///
/// 48 is the largest count the cheap blocks under
/// `crates/sunrise-core/src/engine/tests/` already ran, so those run exactly
/// as many cases locally as in CI, and only the expensive blocks (128 to
/// 2048) are cut.
pub const LOCAL_PROPTEST_CASES: u32 = 48;

/// What [`ENV_PROPTEST_DEPTH`] asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// Cap every block at [`LOCAL_PROPTEST_CASES`].
    Local,
    /// Run every block at its call site's count.
    Full,
}

/// Parse one [`ENV_PROPTEST_DEPTH`] value; `None` for anything but `local`
/// or `full`. Pure over its argument.
fn parse_depth(raw: &str) -> Option<Depth> {
    match raw.trim() {
        "local" => Some(Depth::Local),
        "full" => Some(Depth::Full),
        _ => None,
    }
}

/// How many cases a block whose call site names `full` should run.
///
/// Pure, so the precedence rule is testable without the process environment:
///
/// * `native` is `PROPTEST_CASES`, parsed. **It wins**, for the reason
///   [`choose_rng_seed`] gives about proptest's own seed variable: it is the
///   dial a proptest user reaches for first, and a block that ignored it
///   would make it look broken.
/// * Otherwise `Full` runs `full` and `Local` runs at most
///   [`LOCAL_PROPTEST_CASES`].
fn choose_cases(native: Option<u32>, depth: Depth, full: u32) -> u32 {
    match (native, depth) {
        (Some(n), _) => n,
        (None, Depth::Full) => full,
        (None, Depth::Local) => full.min(LOCAL_PROPTEST_CASES),
    }
}

/// The case count for a `proptest!` block whose CI count is `full`.
///
/// Every block that names a count takes it from here rather than from a
/// literal, so one dial sets the depth of the whole suite:
///
/// ```
/// use proptest::prelude::ProptestConfig;
///
/// let config = ProptestConfig {
///     cases: sunrise_test_seed::proptest_cases(2048),
///     rng_seed: sunrise_test_seed::proptest_rng_seed(),
///     ..ProptestConfig::default()
/// };
/// ```
///
/// `PROPTEST_CASES` wins when it is set to a number; then
/// [`ENV_PROPTEST_DEPTH`]`=full` runs `full`; otherwise the count is capped at
/// [`LOCAL_PROPTEST_CASES`].
///
/// # Panics
///
/// When [`ENV_PROPTEST_DEPTH`] is set to anything but `local` or `full`.
#[must_use]
pub fn proptest_cases(full: u32) -> u32 {
    let native = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok());
    let depth = match std::env::var_os(ENV_PROPTEST_DEPTH) {
        None => Depth::Local,
        Some(raw) => raw.to_str().and_then(parse_depth).unwrap_or_else(|| {
            panic!(
                "{ENV_PROPTEST_DEPTH}={} is neither `local` nor `full`",
                raw.display()
            )
        }),
    };
    choose_cases(native, depth, full)
}

/// Print `seed` and how to pin it, for `harness`.
///
/// Takes a plain `u64` rather than a [`RngSeed`] so that the chaos harness —
/// whose RNG is `rand_chacha`, not proptest's — announces through the same
/// line. One grep over a CI log then finds the reproduction for either half of
/// the suite.
pub fn announce(harness: &str, seed: u64) {
    eprintln!("{harness}: RNG seed {seed:#018x} — reproduce with {ENV_FUZZ_SEED}={seed:#018x}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seed_reads_decimal_hex_and_prefixed_hex() {
        assert_eq!(parse_seed("42"), Some(42));
        assert_eq!(parse_seed("0xff"), Some(255));
        assert_eq!(parse_seed("0XFF"), Some(255));
        assert_eq!(parse_seed("deadbeef"), Some(0xdead_beef));
        assert_eq!(parse_seed("  0x10  "), Some(16));
        assert_eq!(parse_seed(""), None);
        assert_eq!(parse_seed("not a number"), None);
        assert_eq!(parse_seed("0xzz"), None);
    }

    #[test]
    fn a_string_readable_as_both_is_decimal() {
        // "10" is 10 in decimal and 16 in hex. The decimal reading wins, which
        // is what the documented "hex, but decimal also accepted" rule means.
        assert_eq!(parse_seed("10"), Some(10));
        // And forcing hex is what `0x` is for.
        assert_eq!(parse_seed("0x10"), Some(16));
    }

    #[test]
    fn proptests_own_variable_outranks_the_workspace_one() {
        assert_eq!(
            choose_rng_seed(RngSeed::Fixed(7), Some(9), 11),
            RngSeed::Fixed(7)
        );
    }

    #[test]
    fn the_workspace_variable_outranks_the_fallback() {
        assert_eq!(
            choose_rng_seed(RngSeed::Random, Some(9), 11),
            RngSeed::Fixed(9)
        );
    }

    #[test]
    fn with_nothing_set_the_fallback_is_used_and_is_still_fixed() {
        // Fixed, not Random: an unpinned run must still know its own seed, or
        // the announcement has nothing to announce.
        assert_eq!(
            choose_rng_seed(RngSeed::Random, None, 11),
            RngSeed::Fixed(11)
        );
    }

    #[test]
    fn parse_depth_reads_local_and_full_and_nothing_else() {
        assert_eq!(parse_depth("local"), Some(Depth::Local));
        assert_eq!(parse_depth(" full\n"), Some(Depth::Full));
        assert_eq!(parse_depth(""), None);
        assert_eq!(parse_depth("FULL"), None);
        assert_eq!(parse_depth("ci"), None);
    }

    #[test]
    fn proptests_own_case_count_outranks_the_depth() {
        assert_eq!(choose_cases(Some(5), Depth::Full, 2048), 5);
        assert_eq!(choose_cases(Some(5000), Depth::Local, 2048), 5000);
    }

    #[test]
    fn full_depth_runs_the_call_sites_count() {
        assert_eq!(choose_cases(None, Depth::Full, 2048), 2048);
        assert_eq!(choose_cases(None, Depth::Full, 24), 24);
    }

    #[test]
    fn local_depth_caps_only_the_expensive_blocks() {
        assert_eq!(choose_cases(None, Depth::Local, 2048), LOCAL_PROPTEST_CASES);
        assert_eq!(
            choose_cases(None, Depth::Local, LOCAL_PROPTEST_CASES),
            LOCAL_PROPTEST_CASES
        );
        assert_eq!(choose_cases(None, Depth::Local, 24), 24);
    }

    /// The only test that touches `PROPTEST_CASES` or
    /// [`ENV_PROPTEST_DEPTH`], so every case lives in this one function for
    /// the reason [`seed_from_env_reads_the_variable`] gives.
    #[test]
    fn proptest_cases_reads_both_variables() {
        std::env::remove_var("PROPTEST_CASES");
        std::env::remove_var(ENV_PROPTEST_DEPTH);
        assert_eq!(proptest_cases(2048), LOCAL_PROPTEST_CASES);
        std::env::set_var(ENV_PROPTEST_DEPTH, "full");
        assert_eq!(proptest_cases(2048), 2048);
        std::env::set_var("PROPTEST_CASES", "7");
        assert_eq!(proptest_cases(2048), 7);
        // An unparseable `PROPTEST_CASES` is ignored, as proptest ignores it.
        std::env::set_var("PROPTEST_CASES", "many");
        assert_eq!(proptest_cases(2048), 2048);
        std::env::remove_var("PROPTEST_CASES");
        std::env::set_var(ENV_PROPTEST_DEPTH, "ful");
        assert!(std::panic::catch_unwind(|| proptest_cases(2048)).is_err());
        std::env::remove_var(ENV_PROPTEST_DEPTH);
    }

    #[test]
    fn the_process_seed_is_drawn_once() {
        assert_eq!(process_seed(), process_seed());
    }

    /// The one test in this crate that touches the process environment, and
    /// therefore the only one that may not run beside another that reads it.
    /// Keeping every case in a single test function is what keeps that true
    /// under `cargo test`'s thread pool.
    #[test]
    fn seed_from_env_reads_the_variable() {
        std::env::set_var(ENV_FUZZ_SEED, "42");
        assert_eq!(seed_from_env(), Some(42));
        std::env::set_var(ENV_FUZZ_SEED, "0xff");
        assert_eq!(seed_from_env(), Some(255));
        std::env::set_var(ENV_FUZZ_SEED, "deadbeef");
        assert_eq!(seed_from_env(), Some(0xdead_beef));
        std::env::set_var(ENV_FUZZ_SEED, "not a seed");
        assert_eq!(seed_from_env(), None);
        std::env::remove_var(ENV_FUZZ_SEED);
        assert_eq!(seed_from_env(), None);
    }
}

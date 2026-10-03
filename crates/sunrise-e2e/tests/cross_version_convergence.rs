//! Merging a vault across client versions never breaks and never loses data
//! (#326, ADR-0057, `docs/02-domain/schema-versioning.md` §Compatibility
//! testing).
//!
//! Two runs of one property over generated scenarios:
//!
//! - **baseline against `HEAD`** — replica A is `sunrise-core` as a release tag
//!   shipped it, in the driver process `SUNRISE_BASELINE_DRIVER` names. Ignored
//!   by a plain `cargo test`, because the driver is built from another tree;
//!   the `Cross-version merge` CI job builds it and runs this with
//!   `--ignored`. Without the variable the ignored tests fail rather than pass
//!   vacuously.
//! - **`HEAD` against `HEAD`** — the control. It runs everywhere, and it is
//!   what keeps the harness honest: the reference the model computes has to be
//!   the one `HEAD` produces, so the only violations a `HEAD`-only run may show
//!   are `HEAD`'s own.
//!
//! A violation a known gap explains is an expected failure naming its issue;
//! the table is `sunrise_e2e::cross_version::gaps::KNOWN_GAPS`. Each gap also
//! has a fixed scenario below that must still reproduce it, so an entry cannot
//! outlive its defect.

#![allow(clippy::missing_panics_doc)]

use std::path::PathBuf;
use std::sync::Mutex;

use proptest::prelude::*;
use sunrise_e2e::cross_version::baseline::DRIVER_ENV;
use sunrise_e2e::cross_version::events::install_event_capture;
use sunrise_e2e::cross_version::future::FutureWrite;
use sunrise_e2e::cross_version::gaps::{expected_gaps, KnownGap};
use sunrise_e2e::cross_version::{run, Mode, Report, Side, Step};

/// One run at a time in this binary: the `HEAD` cores share one captured log,
/// and a run reads it as its own.
static SERIAL: Mutex<()> = Mutex::new(());

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime")
}

fn execute(mode: &Mode, steps: &[Step]) -> Report {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        install_event_capture(),
        "another subscriber owns this process's log, so HEAD-side corruption would go unseen"
    );
    runtime().block_on(run(mode, steps))
}

fn baseline_mode() -> Mode {
    let path = std::env::var_os(DRIVER_ENV).unwrap_or_else(|| {
        panic!(
            "{DRIVER_ENV} is not set. Build the driver with \
             `crates/sunrise-e2e/baseline-driver/build-baseline.sh <tag>` and point \
             {DRIVER_ENV} at the path it prints."
        )
    });
    Mode::Baseline(PathBuf::from(path))
}

fn describe(report: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = format!("baseline {:?}\n", report.baseline);
    for c in &report.violations {
        let _ = match c.issue {
            Some(issue) => writeln!(out, "  expected (#{issue}): {:?}", c.violation),
            None => writeln!(out, "  UNEXPLAINED: {:?}", c.violation),
        };
    }
    out
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::A), Just(Side::B)]
}

fn future_write() -> impl Strategy<Value = FutureWrite> {
    proptest::sample::select(FutureWrite::ALL.to_vec())
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (side(), any::<u8>(), proptest::option::of(1u8..=5))
            .prop_map(|(on, title, priority)| Step::Create { on, title, priority }),
        3 => (side(), any::<usize>(), any::<u8>())
            .prop_map(|(on, task, title)| Step::SetTitle { on, task, title }),
        2 => (side(), any::<usize>(), proptest::option::of(1u8..=5))
            .prop_map(|(on, task, priority)| Step::SetPriority { on, task, priority }),
        1 => (side(), any::<usize>()).prop_map(|(on, task)| Step::Delete { on, task }),
        2 => (any::<usize>(), future_write(), side())
            .prop_map(|(task, write, first)| Step::Future { task, write, first }),
        2 => Just(Step::Settle),
    ]
}

/// A scenario always starts with a task on each side and a settle, so the
/// steps that follow have something to act on.
fn scenario() -> impl Strategy<Value = Vec<Step>> {
    proptest::collection::vec(step(), 1..14).prop_map(|rest| {
        let mut steps = vec![
            Step::Create {
                on: Side::A,
                title: 0,
                priority: None,
            },
            Step::Create {
                on: Side::B,
                title: 1,
                priority: Some(3),
            },
            Step::Settle,
        ];
        steps.extend(rest);
        steps
    })
}

/// How many cases a property runs: `SUNRISE_CROSS_VERSION_CASES`, else
/// `default`. Each case boots a relay and three vaults, so the default is
/// small and CI raises it where it has the minutes.
fn cases(default: u32) -> u32 {
    std::env::var("SUNRISE_CROSS_VERSION_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

proptest! {
    // `Direct`, not the `SourceParallel` default: nothing above a `tests/` file
    // holds a `lib.rs` or `main.rs`, so the default warns and drops the
    // counterexample beside this source instead. See
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        cases: cases(4),
        max_shrink_iters: 64,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/cross_version_convergence.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// The control: `HEAD` against `HEAD` merges with no violation but
    /// `HEAD`'s own known gaps.
    #[test]
    fn head_and_head_merge_without_loss_or_break(steps in scenario()) {
        let report = execute(&Mode::HeadOnly, &steps);
        prop_assert!(report.unexplained().is_empty(), "{}", describe(&report));
    }

    /// The property: the baseline and `HEAD` merge with no violation but the
    /// known gaps of that baseline.
    #[test]
    #[ignore = "needs SUNRISE_BASELINE_DRIVER; the Cross-version merge CI job runs it"]
    fn baseline_and_head_merge_without_loss_or_break(steps in scenario()) {
        let report = execute(&baseline_mode(), &steps);
        prop_assert!(report.unexplained().is_empty(), "{}", describe(&report));
    }
}

/// The fixed scenario that reproduces `gap`.
fn reproduction(gap: &KnownGap) -> Vec<Step> {
    let open = vec![
        Step::Create {
            on: Side::A,
            title: 0,
            priority: None,
        },
        Step::Settle,
    ];
    let tail = match gap.issue {
        // Two writers set different fields of one task with neither having
        // seen the other: entity-level last-writer-wins keeps one whole side.
        // The newer writer is one of them because the harness holds its op
        // back from A until the settle, which makes the two writes concurrent
        // by construction rather than by a race with the relay.
        319 => vec![
            Step::Future {
                task: 0,
                write: FutureWrite::Field,
                first: Side::B,
            },
            Step::SetTitle {
                on: Side::A,
                task: 0,
                title: 9,
            },
            Step::Settle,
        ],
        320 => vec![Step::Future {
            task: 0,
            write: FutureWrite::OpKind,
            first: Side::A,
        }],
        321 => vec![Step::Future {
            task: 0,
            write: FutureWrite::EnumValue,
            first: Side::A,
        }],
        322 => vec![Step::Future {
            task: 0,
            write: FutureWrite::NestedKind,
            first: Side::A,
        }],
        // B takes the newer state; the baseline drops it, then edits the task
        // a settle later, and its edit carries the state it still believes.
        324 => vec![
            Step::Future {
                task: 0,
                write: FutureWrite::EnumValue,
                first: Side::B,
            },
            Step::Settle,
            Step::SetPriority {
                on: Side::A,
                task: 0,
                priority: Some(2),
            },
            Step::Settle,
        ],
        other => panic!("known gap #{other} has no reproduction; add one here"),
    };
    open.into_iter().chain(tail).collect()
}

fn assert_gaps_reproduce(mode: &Mode, baseline: Option<&str>) {
    let gaps = expected_gaps(baseline);
    assert!(
        !gaps.is_empty() || baseline.is_some(),
        "the control run expects #319"
    );
    for gap in gaps {
        let report = execute(mode, &reproduction(&gap));
        assert!(
            report.issues().contains(&gap.issue),
            "known gap #{} ({}) no longer reproduces. If its fix has reached this \
             run, remove its entry from KNOWN_GAPS.\n{}",
            gap.issue,
            gap.summary,
            describe(&report)
        );
        assert!(
            report.unexplained().is_empty(),
            "the reproduction of #{} broke the invariant in a way no known gap \
             explains:\n{}",
            gap.issue,
            describe(&report)
        );
    }
}

/// Every gap the control run expects still happens at `HEAD`.
#[test]
fn head_known_gaps_still_reproduce() {
    assert_gaps_reproduce(&Mode::HeadOnly, None);
}

/// Every gap expected of the baseline still happens against it.
#[test]
#[ignore = "needs SUNRISE_BASELINE_DRIVER; the Cross-version merge CI job runs it"]
fn baseline_known_gaps_still_reproduce() {
    let mode = baseline_mode();
    // Ask the driver which tag it is, through a run that does nothing.
    let probe = execute(&mode, &[]);
    let tag = probe
        .baseline
        .clone()
        .expect("a baseline run reports its tag");
    assert!(
        probe.unexplained().is_empty(),
        "an empty scenario broke the invariant:\n{}",
        describe(&probe)
    );
    assert_gaps_reproduce(&mode, Some(&tag));
}

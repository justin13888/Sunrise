//! The violations the harness already knows about, each naming its issue.
//!
//! A violation the classifier attributes to an issue in this table is an
//! expected failure: it is reported, and the property still passes. Anything
//! else fails the property. An entry leaves this table when its fix reaches
//! the baseline it is scoped to — for an entry scoped to every run, when its
//! issue closes — and `known_gaps_still_reproduce` is what notices the day an
//! entry stops being true: it runs each entry's fixed scenario and fails if
//! the violation is gone.
//!
//! Note what "fixed at `HEAD`" does and does not buy. #320, #321 and #322 are
//! closed: `HEAD` parks an unknown op kind, keeps an unknown enum value and
//! keeps an unknown nested kind. A baseline cut before a fix never gets it,
//! because a build that has shipped never changes. So against such a baseline
//! the entry stays for as long as that baseline is in the matrix, and it
//! leaves when the matrix moves past the fix.

/// Which runs an expected failure applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapScope {
    /// Every run, `HEAD` against `HEAD` included: the defect is in `HEAD`.
    Every,
    /// Runs against one of these baselines, by full commit id: the defect is
    /// in that build.
    Baselines(&'static [&'static str]),
}

/// One known way the invariant is violated today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownGap {
    /// The issue that tracks it.
    pub issue: u32,
    /// Which runs it applies to.
    pub scope: GapScope,
    /// What goes wrong, in one line.
    pub summary: &'static str,
}

/// The ADR-0042 floor: `d9566ade`, the merge of #371, which withdrew the
/// pre-release licence to break older builds. It is the oldest build the
/// invariant binds, and it predates the fixes for #320, #321 and #322.
pub const FLOOR: &str = "d9566ade714bf49714ea1b034625a5b6a1792984";

/// The baselines that predate the parking fix (#320), the enum fix (#321) and
/// the nested-unknowns fix (#322).
const BEFORE_PARKING: &[&str] = &[FLOOR];

/// Every expected failure. Ordered by issue.
pub const KNOWN_GAPS: &[KnownGap] = &[
    KnownGap {
        issue: 319,
        scope: GapScope::Every,
        summary: "entity-level last-writer-wins: of two concurrent writes to different \
                  fields of one task, one side's field is lost",
    },
    KnownGap {
        issue: 320,
        scope: GapScope::Baselines(BEFORE_PARKING),
        summary: "the baseline does not keep an op of an unknown kind: it classes it as \
                  corruption and drops it",
    },
    KnownGap {
        issue: 321,
        scope: GapScope::Baselines(BEFORE_PARKING),
        summary: "the baseline does not keep an unknown enum value: it stores the value's \
                  fallback in its place, or drops the op",
    },
    KnownGap {
        issue: 322,
        scope: GapScope::Baselines(BEFORE_PARKING),
        summary: "the baseline does not keep an unknown nested kind: it drops the op that \
                  carries it, or the value",
    },
    KnownGap {
        issue: 324,
        scope: GapScope::Baselines(BEFORE_PARKING),
        summary: "the baseline keeps writing a task after failing to keep a newer value on \
                  it, and its write replaces that value on every replica",
    },
];

impl KnownGap {
    /// Whether this gap is expected in a run against `baseline` (`None` for a
    /// `HEAD`-against-`HEAD` run).
    #[must_use]
    pub fn applies_to(&self, baseline: Option<&str>) -> bool {
        match self.scope {
            GapScope::Every => true,
            GapScope::Baselines(refs) => baseline.is_some_and(|b| refs.contains(&b)),
        }
    }
}

/// The gaps expected in a run against `baseline`.
#[must_use]
pub fn expected_gaps(baseline: Option<&str>) -> Vec<KnownGap> {
    KNOWN_GAPS
        .iter()
        .copied()
        .filter(|g| g.applies_to(baseline))
        .collect()
}

//! The verdicts: a run against the recorded baseline (`--check`), and a head
//! against a base taken on the same machine (`--compare`, #474). Both read
//! metrics through [`super::collect_metrics`] and judge them with [`compare`].

use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use super::collect_metrics;

/// Compare `metrics` against the recorded baseline for `platform`.
///
/// Every metric here is a cost, a latency or a resident size, so *higher is
/// worse* and only upward moves are regressions. A metric with no recorded
/// baseline (`null`, or a platform we have never recorded) is reported and
/// skipped rather than treated as a pass or a failure — there is nothing to
/// compare against, and silently passing would let an unrecorded platform
/// drift forever.
pub(super) fn check_against(
    baseline_path: &Path,
    platform: &str,
    metrics: &[(String, f64)],
    tolerance_pct: f64,
) -> ExitCode {
    let Ok(text) = std::fs::read_to_string(baseline_path) else {
        println!("baseline: cannot read {}", baseline_path.display());
        return ExitCode::FAILURE;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        println!("baseline: {} is not valid JSON", baseline_path.display());
        return ExitCode::FAILURE;
    };
    let recorded = doc.get("platforms").and_then(|p| p.get(platform));

    let verdict = compare(
        |key| recorded.and_then(|r| r.get(key)).and_then(Value::as_f64),
        &format!("no baseline for {platform}"),
        metrics,
        Some(tolerance_pct),
    );
    if verdict.compared == 0 {
        println!(
            "baseline: nothing comparable for {platform}; not failing on an empty comparison."
        );
        return ExitCode::SUCCESS;
    }
    verdict.exit(tolerance_pct)
}

/// `baseline --compare BASE_DIR HEAD_DIR [TOLERANCE_PCT]`: compare two
/// Criterion output directories taken on one machine, the base's against the
/// head's, so the machine cancels out of the comparison.
///
/// The head must be complete, as `--check` requires: a row whose bench did not
/// run there, or a directory no row names, fails. The base need not be, since
/// a bench the head adds has no base to have run in; its metrics are reported
/// and skipped. With no tolerance (absent, or an empty argument) every move is
/// printed and nothing fails on one, which is how an A/A run, base and head at
/// one commit, measures the spread a tolerance is chosen from.
pub(super) fn compare_main(args: &[String]) -> ExitCode {
    let (Some(base_dir), Some(head_dir)) = (args.get(1), args.get(2)) else {
        println!("usage: baseline --compare BASE_CRITERION_DIR HEAD_CRITERION_DIR [TOLERANCE_PCT]");
        return ExitCode::FAILURE;
    };
    let tolerance_pct = match args.get(3).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => match raw.parse::<f64>() {
            Ok(pct) if pct.is_finite() && pct >= 0.0 => Some(pct),
            _ => {
                println!("baseline: tolerance `{raw}` is not a non-negative percentage");
                return ExitCode::FAILURE;
            }
        },
    };
    println!("baseline: comparing {base_dir} (base) with {head_dir} (head)");

    let base = collect_metrics(Path::new(base_dir));
    for dir in &base.missing {
        println!("baseline: the base did not run {dir}; its metrics are not compared");
    }
    let head = collect_metrics(Path::new(head_dir));
    for dir in &head.missing {
        println!("baseline: missing {head_dir}/{dir}/new/sample.json — that bench did not run");
    }
    for dir in &head.unknown {
        println!("baseline: unknown bench directory {head_dir}/{dir} — no row in BENCHES reads it");
    }

    let verdict = compare(
        |key| base.metrics.iter().find(|(k, _)| k == key).map(|(_, v)| *v),
        "not in the base",
        &head.metrics,
        tolerance_pct,
    );
    if let Some((key, pct)) = &verdict.widest {
        println!(
            "baseline: widest move {pct:+.1}% ({key}) over {} metric(s)",
            verdict.compared
        );
    }
    if !head.missing.is_empty() || !head.unknown.is_empty() {
        println!(
            "::error::baseline: the head has {} missing and {} unknown bench director(ies); see above",
            head.missing.len(),
            head.unknown.len()
        );
        return ExitCode::FAILURE;
    }
    let Some(tolerance_pct) = tolerance_pct else {
        println!("baseline: no tolerance given; reported, not judged.");
        return ExitCode::SUCCESS;
    };
    verdict.exit(tolerance_pct)
}

/// What one comparison found.
#[derive(Debug, Default)]
struct Verdict {
    /// Metrics that had a positive previous value to compare against.
    compared: usize,
    /// `(key, previous, current, delta_pct)` for each move over the tolerance.
    regressions: Vec<(String, f64, f64, f64)>,
    /// The largest move in either direction, `(key, delta_pct)`.
    widest: Option<(String, f64)>,
}

impl Verdict {
    /// Print the outcome and turn it into an exit status.
    fn exit(&self, tolerance_pct: f64) -> ExitCode {
        if self.regressions.is_empty() {
            println!(
                "baseline: {} metric(s) within {tolerance_pct:.0}% tolerance.",
                self.compared
            );
            return ExitCode::SUCCESS;
        }
        for (key, prev, cur, pct) in &self.regressions {
            println!("::error::{key} regressed {pct:+.1}% ({prev:.3} -> {cur:.3})");
        }
        ExitCode::FAILURE
    }
}

/// Compare each of `metrics` against the value `previous` gives for its key,
/// printing one line per metric.
///
/// Every metric is a cost, a latency or a resident size, so *higher is worse*
/// and only upward moves are regressions. A metric with no previous value is
/// printed with `skipped_because` and skipped. With no tolerance, every move is
/// printed and none is a regression.
fn compare(
    previous: impl Fn(&str) -> Option<f64>,
    skipped_because: &str,
    metrics: &[(String, f64)],
    tolerance_pct: Option<f64>,
) -> Verdict {
    let mut out = Verdict::default();
    for (key, current) in metrics {
        let Some(previous) = previous(key) else {
            println!("  {key}: {skipped_because} — skipped");
            continue;
        };
        if previous <= 0.0 {
            println!("  {key}: baseline is not positive ({previous}) — skipped");
            continue;
        }
        out.compared += 1;
        let delta_pct = (current - previous) / previous * 100.0;
        if out
            .widest
            .as_ref()
            .is_none_or(|(_, w)| delta_pct.abs() > w.abs())
        {
            out.widest = Some((key.clone(), delta_pct));
        }
        let verdict = match tolerance_pct {
            Some(t) if delta_pct > t => {
                out.regressions
                    .push((key.clone(), previous, *current, delta_pct));
                "REGRESSION"
            }
            Some(t) if delta_pct < -t => "improved",
            Some(_) => "ok",
            None => "reported",
        };
        println!("  {key}: {previous:.3} -> {current:.3} ({delta_pct:+.1}%) {verdict}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, f64)]) -> impl Fn(&str) -> Option<f64> + 'a {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    #[test]
    fn compare_flags_only_upward_moves_over_the_tolerance() {
        let previous = [("a", 10.0), ("b", 10.0), ("c", 10.0), ("z", 0.0)];
        let current = vec![
            ("a".to_string(), 12.0),
            ("b".to_string(), 7.0),
            ("c".to_string(), 10.5),
            ("d".to_string(), 99.0),
            ("z".to_string(), 1.0),
        ];
        let v = compare(lookup(&previous), "absent", &current, Some(10.0));
        // `d` has no previous value and `z` a non-positive one: neither counts.
        assert_eq!(v.compared, 3);
        assert_eq!(v.regressions.len(), 1);
        assert_eq!(v.regressions[0].0, "a");
        // The widest move is `b`'s -30%, a direction that never fails.
        let (key, pct) = v.widest.expect("a widest move");
        assert_eq!(key, "b");
        assert!((pct + 30.0).abs() < 1e-9);
    }

    #[test]
    fn compare_without_a_tolerance_judges_nothing() {
        let previous = [("a", 10.0)];
        let current = vec![("a".to_string(), 30.0)];
        let v = compare(lookup(&previous), "absent", &current, None);
        assert_eq!(v.compared, 1);
        assert!(v.regressions.is_empty());
        let (_, pct) = v.widest.expect("a widest move");
        assert!((pct - 200.0).abs() < 1e-9);
    }
}

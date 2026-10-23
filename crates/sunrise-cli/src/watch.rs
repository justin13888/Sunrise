//! `sync --watch`: keep a vault level with the relay until told to stop.
//!
//! `sync --once` is a bounded wait for cron and CI. `--watch` is the same wait
//! in a loop, for a terminal left open or a service manager: each round drains
//! the outbox and waits for the session to be live, a round that succeeds is
//! followed by a short pause, and a round that fails is retried after a delay
//! that doubles up to a cap and resets on the next success. The sync driver
//! reconnects on its own; this loop is what decides how often to look, and
//! how loudly to say the relay is gone.
//!
//! The loop is written against three injected things — the round, the sleep
//! and the stop — so the policy is unit-tested without a relay, a clock or a
//! signal. The binary passes the client layer's bounded wait, `tokio`'s
//! sleep, and Ctrl-C (and `SIGTERM` on Unix).

use std::future::Future;
use std::time::Duration;

/// How often to look, and how long to back off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The delay after the first failed round.
    pub initial: Duration,
    /// The longest delay between failed rounds.
    pub max: Duration,
    /// The pause after a round that came back live.
    pub live_pause: Duration,
}

impl Default for Backoff {
    /// One second, doubling to a minute; five seconds between live rounds.
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            live_pause: Duration::from_secs(5),
        }
    }
}

impl Backoff {
    /// The delay after `failures` consecutive failed rounds (`failures >= 1`).
    #[must_use]
    pub fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(30);
        self.initial
            .checked_mul(1u32 << doublings)
            .map_or(self.max, |d| d.min(self.max))
    }
}

/// How one round ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Round {
    /// The outbox drained and the session is live.
    Live,
    /// It did not, and why.
    Failed(String),
}

/// What the loop tells its caller as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A round came back live after one that did not (or first).
    Live,
    /// A round failed; the next is in `retry_in`.
    Failed {
        /// Why.
        reason: String,
        /// When the next round runs.
        retry_in: Duration,
    },
}

/// Why the loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The stop future resolved.
    Stopped,
    /// `max_live_rounds` live rounds ran.
    Rounds,
}

/// Run rounds until `stop` resolves or `max_live_rounds` rounds came back
/// live, reporting each change of state through `report`.
///
/// `stop` is polled during every round and every pause, so a Ctrl-C ends the
/// loop at once rather than after the round in flight; a round cut short
/// leaves nothing half applied, because the core writes each op in its own
/// transaction.
pub async fn run<R, RF, S, SF, X>(
    policy: Backoff,
    mut round: R,
    mut sleep: S,
    stop: X,
    max_live_rounds: Option<u32>,
    report: &mut dyn FnMut(&Event),
) -> End
where
    R: FnMut() -> RF,
    RF: Future<Output = Round>,
    S: FnMut(Duration) -> SF,
    SF: Future<Output = ()>,
    X: Future<Output = ()>,
{
    tokio::pin!(stop);
    let mut failures: u32 = 0;
    let mut live_rounds: u32 = 0;
    let mut was_live = false;
    loop {
        let outcome = tokio::select! {
            biased;
            () = &mut stop => return End::Stopped,
            r = round() => r,
        };
        let pause = match outcome {
            Round::Live => {
                failures = 0;
                live_rounds = live_rounds.saturating_add(1);
                if !was_live {
                    report(&Event::Live);
                }
                was_live = true;
                if max_live_rounds.is_some_and(|max| live_rounds >= max) {
                    return End::Rounds;
                }
                policy.live_pause
            }
            Round::Failed(reason) => {
                failures = failures.saturating_add(1);
                was_live = false;
                let retry_in = policy.delay(failures);
                report(&Event::Failed { reason, retry_in });
                retry_in
            }
        };
        tokio::select! {
            biased;
            () = &mut stop => return End::Stopped,
            () = sleep(pause) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn policy() -> Backoff {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(8),
            live_pause: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_delay_doubles_to_its_cap() {
        let p = policy();
        let secs: Vec<u64> = (1..=6).map(|n| p.delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 8, 8]);
        assert_eq!(p.delay(u32::MAX), p.max);
    }

    #[tokio::test]
    async fn failures_back_off_a_success_resets_and_the_rounds_bound_ends_it() {
        let outcomes = RefCell::new(VecDeque::from([
            Round::Failed("relay down".into()),
            Round::Failed("relay down".into()),
            Round::Live,
            Round::Failed("blip".into()),
            Round::Live,
            Round::Live,
        ]));
        let slept = RefCell::new(Vec::new());
        let mut events = Vec::new();
        let end = run(
            policy(),
            || {
                let r = outcomes.borrow_mut().pop_front().expect("a round");
                async move { r }
            },
            |d| {
                slept.borrow_mut().push(d.as_secs());
                async {}
            },
            std::future::pending(),
            Some(3),
            &mut |e| events.push(e.clone()),
        )
        .await;
        assert_eq!(end, End::Rounds);
        // 1 s, 2 s, then live; the next failure starts again at 1 s.
        assert_eq!(*slept.borrow(), [1, 2, 5, 1, 5]);
        assert_eq!(
            events.iter().filter(|e| matches!(e, Event::Live)).count(),
            2,
            "live is said once per recovery, not every round"
        );
    }

    #[tokio::test]
    async fn a_stop_ends_the_loop_before_another_round_runs() {
        let rounds = RefCell::new(0u32);
        let end = run(
            policy(),
            || async {
                // Counted when the round runs, not when it is built.
                *rounds.borrow_mut() += 1;
                Round::Live
            },
            |_| async {},
            async {},
            None,
            &mut |_| {},
        )
        .await;
        assert_eq!(end, End::Stopped);
        assert_eq!(*rounds.borrow(), 0);
    }

    #[tokio::test]
    async fn a_stop_during_a_pause_ends_the_loop() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let tx = RefCell::new(Some(tx));
        let end = run(
            policy(),
            || async { Round::Failed("down".into()) },
            |_| {
                // The stop arrives while the loop is pausing.
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<()>()
            },
            async {
                let _ = rx.await;
            },
            None,
            &mut |_| {},
        )
        .await;
        assert_eq!(end, End::Stopped);
    }
}

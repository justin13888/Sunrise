//! Hybrid logical clocks — the causal ordering primitive that rides in
//! `OpEnvelope` field 5.
//!
//! A bare wall clock cannot order two devices' writes. It is not monotonic, it
//! is not agreed on, and a device whose clock is a year fast wins every
//! conflict it ever participates in, permanently and invisibly. That was the
//! defect in issue #21: `lww_wins` trusted `envelope.ts_ms` with no bound and
//! no logical component.
//!
//! A hybrid logical clock (Kulkarni et al., 2014) fixes both halves:
//!
//! * The **physical** component tracks wall time, so the ordering stays
//!   human-meaningful and a value read off a row still means roughly "when".
//! * The **logical** component absorbs everything the physical component
//!   cannot express — ties within a millisecond, and a peer whose clock is
//!   ahead of ours — so the total order stays consistent with causality even
//!   when the wall clocks disagree.
//!
//! The two invariants that make it work:
//!
//! 1. **Send is strictly increasing.** Every op a device emits carries an `Hlc`
//!    strictly greater than the previous one, whatever the wall clock does.
//! 2. **Receive absorbs the peer.** After observing a remote `Hlc`, this
//!    device's clock is strictly greater than it, so anything it emits
//!    afterwards sorts after the op it just saw. Causality is preserved
//!    without a vector clock.
//!
//! The receiver **stores the sender's value**, not its own post-merge value.
//! That is what makes the order replica-independent: two replicas that receive
//! the same op in different orders still record the same stamp for it, so they
//! reach the same LWW winner. Recording the local post-merge value instead
//! would make the winner depend on delivery order, which is precisely the
//! divergence LWW exists to avoid.
//!
//! See `docs/05-sync/conflict-resolution.md` and ADR-0016.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How far into the future a received `Hlc` may sit before it is refused.
///
/// A peer legitimately reaches us late — a laptop opened after a week offline
/// emits ops with old timestamps, and those are accepted without comment.
/// A peer cannot legitimately reach us *early*: an op stamped an hour from now
/// is a broken or hostile clock, and accepting it would let that device win
/// every conflict for the next hour.
///
/// Five minutes is generous for NTP-synced devices and small enough that a
/// misconfigured one loses its advantage in minutes rather than months.
pub const MAX_DRIFT_MS: u64 = 5 * 60 * 1000;

/// A hybrid logical clock reading.
///
/// Ordered by `(physical_ms, logical)` — the derived `Ord` is the intended
/// comparison and field order is load-bearing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Hlc {
    /// Wall-clock milliseconds since the Unix epoch, as advanced by the HLC
    /// rules. Never moves backwards for a given device.
    pub physical_ms: u64,
    /// Tie-breaking counter within one `physical_ms`, reset whenever the
    /// physical component advances.
    pub logical: u32,
}

/// Why a received [`Hlc`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum HlcError {
    /// The reading is further into the future than [`MAX_DRIFT_MS`] allows.
    #[error("HLC is {ahead_ms} ms ahead of this device; the limit is {MAX_DRIFT_MS} ms")]
    DriftTooLarge {
        /// How far ahead of the local wall clock the reading sat.
        ahead_ms: u64,
    },
}

impl Hlc {
    /// A reading at `physical_ms` with no logical offset.
    #[must_use]
    pub const fn at(physical_ms: u64) -> Self {
        Self {
            physical_ms,
            logical: 0,
        }
    }

    /// The next reading this device should stamp on an op it is emitting.
    ///
    /// `now_ms` is the device wall clock. The result is strictly greater than
    /// `self` even when the wall clock has stalled or gone backwards, which is
    /// what makes a device's own ops totally ordered without consulting `seq`.
    #[must_use]
    pub const fn send(self, now_ms: u64) -> Self {
        if now_ms > self.physical_ms {
            Self {
                physical_ms: now_ms,
                logical: 0,
            }
        } else {
            Self {
                physical_ms: self.physical_ms,
                logical: self.logical.saturating_add(1),
            }
        }
    }

    /// Absorb a reading received from a peer: `max(local, received, now) + 1`.
    ///
    /// Returns this device's new clock state. The caller stamps the received op
    /// with `received` — NOT with the value returned here — so that every
    /// replica records the same stamp for the same op.
    ///
    /// # Errors
    /// [`HlcError::DriftTooLarge`] when `received` sits more than
    /// [`MAX_DRIFT_MS`] beyond `now_ms`. The op is refused rather than
    /// absorbed: absorbing it would drag this device's own clock forward with
    /// the bad one and spread the skew to every peer it talks to next.
    pub const fn receive(self, received: Self, now_ms: u64) -> Result<Self, HlcError> {
        if received.physical_ms > now_ms.saturating_add(MAX_DRIFT_MS) {
            return Err(HlcError::DriftTooLarge {
                ahead_ms: received.physical_ms - now_ms,
            });
        }
        let physical = max3(self.physical_ms, received.physical_ms, now_ms);
        // The logical counter continues from whichever inputs are already at
        // `physical`; it restarts only when the wall clock alone set the pace.
        let logical = if physical == self.physical_ms && physical == received.physical_ms {
            max_u32(self.logical, received.logical).saturating_add(1)
        } else if physical == self.physical_ms {
            self.logical.saturating_add(1)
        } else if physical == received.physical_ms {
            received.logical.saturating_add(1)
        } else {
            0
        };
        Ok(Self {
            physical_ms: physical,
            logical,
        })
    }
}

const fn max3(a: u64, b: u64, c: u64) -> u64 {
    let ab = if a > b { a } else { b };
    if ab > c {
        ab
    } else {
        c
    }
}

const fn max_u32(a: u32, b: u32) -> u32 {
    if a > b {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_is_strictly_increasing_even_when_the_clock_stalls() {
        let mut h = Hlc::default();
        let mut prev = h;
        for _ in 0..1000 {
            h = h.send(1_700_000_000_000);
            assert!(h > prev, "send must strictly increase");
            prev = h;
        }
        assert_eq!(h.physical_ms, 1_700_000_000_000);
        // The first send took the wall clock and reset logical to 0; the other
        // 999 had nowhere to go but the counter.
        assert_eq!(h.logical, 999);
    }

    #[test]
    fn send_is_strictly_increasing_even_when_the_clock_goes_backwards() {
        let h = Hlc::at(7_200_000).send(7_200_000);
        // The wall clock jumps back an hour; the HLC must not.
        let next = h.send(7_200_000 - 3_600_000);
        assert!(next > h);
        assert_eq!(next.physical_ms, h.physical_ms);
    }

    #[test]
    fn send_resets_the_logical_counter_when_the_wall_clock_advances() {
        let h = Hlc {
            physical_ms: 5,
            logical: 9,
        };
        assert_eq!(h.send(6), Hlc::at(6));
    }

    #[test]
    fn receive_dominates_both_inputs() {
        let local = Hlc {
            physical_ms: 100,
            logical: 3,
        };
        let remote = Hlc {
            physical_ms: 100,
            logical: 7,
        };
        let merged = local.receive(remote, 100).unwrap();
        assert!(merged > local);
        assert!(merged > remote);
        assert_eq!(
            merged,
            Hlc {
                physical_ms: 100,
                logical: 8
            }
        );
    }

    #[test]
    fn receive_from_a_slightly_faster_peer_adopts_its_physical_component() {
        let local = Hlc::at(100);
        let remote = Hlc::at(150);
        let merged = local.receive(remote, 100).unwrap();
        assert_eq!(
            merged,
            Hlc {
                physical_ms: 150,
                logical: 1
            }
        );
    }

    #[test]
    fn receive_from_a_lagging_peer_is_accepted_and_ignored() {
        // A device that was offline for a week is not a clock problem.
        let now = 1_700_000_000_000;
        let local = Hlc::at(now);
        let week_ago = Hlc::at(now - 7 * 24 * 3_600_000);
        let merged = local.receive(week_ago, now).unwrap();
        assert_eq!(merged, Hlc::at(now).send(now));
    }

    #[test]
    fn receive_beyond_the_drift_window_is_refused() {
        let local = Hlc::at(1_000_000);
        let far_future = Hlc::at(1_000_000 + MAX_DRIFT_MS + 1);
        let err = local.receive(far_future, 1_000_000).unwrap_err();
        assert_eq!(
            err,
            HlcError::DriftTooLarge {
                ahead_ms: MAX_DRIFT_MS + 1
            }
        );
    }

    #[test]
    fn receive_at_exactly_the_drift_window_is_accepted() {
        let local = Hlc::at(1_000_000);
        let edge = Hlc::at(1_000_000 + MAX_DRIFT_MS);
        assert!(local.receive(edge, 1_000_000).is_ok());
    }

    #[test]
    fn ordering_is_physical_then_logical() {
        assert!(
            Hlc {
                physical_ms: 1,
                logical: 999
            } < Hlc {
                physical_ms: 2,
                logical: 0
            }
        );
        assert!(
            Hlc {
                physical_ms: 1,
                logical: 1
            } > Hlc {
                physical_ms: 1,
                logical: 0
            }
        );
    }

    /// The property that makes the whole thing worth having: after A sees B's
    /// op, everything A emits sorts after it — regardless of whose wall clock
    /// is right.
    #[test]
    fn causality_survives_a_wrong_local_clock() {
        // A's clock is an hour BEHIND B's, but still inside the drift window
        // in the direction that matters (B is ahead of A by more than a
        // millisecond but B's op is not from A's future by more than the
        // window, because A checks against its own now).
        let a_now = 1_000_000;
        let b_op = Hlc::at(a_now + MAX_DRIFT_MS - 1);
        let a_clock = Hlc::at(a_now).send(a_now);
        let a_after = a_clock.receive(b_op, a_now).unwrap();
        assert!(a_after > b_op, "A's clock now dominates B's op");
        let a_next_op = a_after.send(a_now);
        assert!(
            a_next_op > b_op,
            "A's next op sorts after the op that caused it"
        );
    }
}

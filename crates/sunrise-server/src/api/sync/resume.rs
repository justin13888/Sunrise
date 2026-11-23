//! Where each stream of a session got to: the `id:` an `ops` event carries,
//! and the `Last-Event-ID` that presents it back.
//!
//! `relay_frames.id` is one sequence across every channel, so one number
//! cannot say how far each of several streams has been sent: a session
//! replayed stream A's frames 1, 5, 9 and then stream B's 2, 3, and dropped
//! after 9, would resume B past the two frames it never received. A
//! [`Positions`] keeps the highest id sent per stream instead, and writes it
//! into every event id (ADR-0023's third amendment, `docs/05-sync/
//! wire-protocol.md`):
//!
//! | Session subscribed to | `id:` |
//! |---|---|
//! | one stream | that frame's `relay_frames.id`, in decimal |
//! | two or more streams | `<set>:<i>=<id>[,<i>=<id>…]` |
//!
//! `<set>` is [`set_name`] in 16 lowercase hex characters; `<i>` is a
//! stream's zero-based index in the `Subscribe` list, and `<id>` the highest
//! `relay_frames.id` of that stream the client has been sent. A stream with
//! no pair has had nothing sent since the `Subscribe`.
//!
//! An id the session cannot apply resumes nothing, and every stream replays
//! from its cursors: over-delivery is one idempotent no-op at the op-log gate,
//! a skip is data loss.

use std::collections::BTreeMap;

/// The BLAKE3 key-derivation context [`set_name`] hashes under.
const SET_CONTEXT: &str = "sunrise sync resume stream set";

/// A `Last-Event-ID`, read but not yet held against a session.
///
/// Parsing and applying are separate because the resume order needs the
/// first without the second: an id of either form on the first stream after
/// a `Subscribe` is refused with `SYNC_RESUME_CONFLICT` whether or not it
/// would have applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResumeId {
    /// A bare, non-zero `relay_frames.id`: the form a one-stream session
    /// mints, and the only form a server built before per-stream positions
    /// minted.
    Single(u64),
    /// A position per stream, against the stream set `set` names.
    Set {
        /// [`set_name`] of the list the indices count into.
        set: [u8; 8],
        /// Index into that list, to the highest id of it sent.
        positions: BTreeMap<usize, u64>,
    },
}

impl ResumeId {
    /// Read a `Last-Event-ID`. `None` for an id that makes no claim — empty,
    /// `0`, or neither form — which is served as a first connection.
    ///
    /// A set position whose pairs are all zero makes no claim either: a zero
    /// is not a position, as it is not one in the bare form.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if let Ok(n) = raw.parse::<u64>() {
            return (n > 0).then_some(Self::Single(n));
        }
        let (set_hex, pairs) = raw.split_once(':')?;
        let mut set = [0u8; 8];
        if set_hex.len() != 16 || hex::decode_to_slice(set_hex, &mut set).is_err() {
            return None;
        }
        let mut positions = BTreeMap::new();
        for pair in pairs.split(',') {
            let (i, id) = pair.split_once('=')?;
            let (i, id) = (i.parse::<usize>().ok()?, id.parse::<u64>().ok()?);
            // A repeated index is two answers to one question: neither is
            // trusted, so the whole id is.
            if positions.insert(i, id).is_some() {
                return None;
            }
        }
        positions.retain(|_, id| *id > 0);
        (!positions.is_empty()).then_some(Self::Set { set, positions })
    }
}

/// The name of a stream set: the first 8 bytes of a BLAKE3 key derivation
/// over the stream ids in `Subscribe` order.
///
/// An index means something only against the list it was minted from, and a
/// `Subscribe` replaces that list. Naming the list is what lets a stale index
/// be told apart from a current one rather than resume a different stream.
pub(super) fn set_name(streams: &[[u8; 16]]) -> [u8; 8] {
    let mut h = blake3::Hasher::new_derive_key(SET_CONTEXT);
    for sid in streams {
        h.update(sid);
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&h.finalize().as_bytes()[..8]);
    out
}

/// The highest `relay_frames.id` sent on each stream of one session, in
/// `Subscribe` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Positions {
    /// [`set_name`] of the session's streams, written into every multi-stream
    /// event id.
    set: [u8; 8],
    /// One per stream; `None` where nothing has been sent since the
    /// `Subscribe`, and that stream replays from its cursors.
    sent: Vec<Option<u64>>,
}

impl Positions {
    /// The positions a stream opens with: `resume` applied to `streams`, or
    /// none at all where it does not apply.
    ///
    /// A bare id applies only to a one-stream session, where it cannot name
    /// the wrong stream. A set position applies only to the set it names, and
    /// only where every index it uses is inside that set: an id that is wrong
    /// anywhere is not trusted anywhere.
    pub(super) fn open(streams: &[[u8; 16]], resume: Option<&ResumeId>) -> Self {
        let set = set_name(streams);
        let mut sent = vec![None; streams.len()];
        match resume {
            Some(ResumeId::Single(id)) if streams.len() == 1 => sent[0] = Some(*id),
            Some(ResumeId::Set {
                set: named,
                positions,
            }) if *named == set
                && streams.len() > 1
                && positions.keys().all(|i| *i < streams.len()) =>
            {
                for (i, id) in positions {
                    sent[*i] = Some(*id);
                }
            }
            _ => {}
        }
        Self { set, sent }
    }

    /// Where stream `i`'s replay starts: after this id, or from the cursors
    /// where it is `None`.
    pub(super) fn after(&self, i: usize) -> Option<u64> {
        self.sent.get(i).copied().flatten()
    }

    /// Whether any stream resumes on a position rather than its cursors.
    ///
    /// Test-only: `srv.sync.stream_open`'s `resumed` records whether an id
    /// was presented, not whether it applied, so the server never asks this.
    #[cfg(test)]
    fn resumed(&self) -> bool {
        self.sent.iter().any(Option::is_some)
    }

    /// Record that frame `id` of stream `i` has been handed to the client.
    ///
    /// The maximum rather than the latest: a live frame can repeat one the
    /// replay already sent (the receiver opens before the durable read), and
    /// that repeat says nothing new about how far the stream has got. Within
    /// one channel the relay publishes in log order, so the highest id sent is
    /// a prefix of the stream.
    pub(super) fn sent(&mut self, i: usize, id: u64) {
        if id == 0 {
            return;
        }
        if let Some(slot) = self.sent.get_mut(i) {
            *slot = Some(slot.map_or(id, |was| was.max(id)));
        }
    }

    /// The event id that states every position, for the event just sent.
    pub(super) fn event_id(&self) -> String {
        if let [only] = self.sent.as_slice() {
            return only.unwrap_or(0).to_string();
        }
        let pairs: Vec<String> = self
            .sent
            .iter()
            .enumerate()
            .filter_map(|(i, id)| id.map(|id| format!("{i}={id}")))
            .collect();
        format!("{}:{}", hex::encode(self.set), pairs.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 16] = [0xa1; 16];
    const B: [u8; 16] = [0xb2; 16];

    #[test]
    fn a_one_stream_session_keeps_the_bare_id() {
        let mut p = Positions::open(&[A], None);
        p.sent(0, 7);
        assert_eq!(p.event_id(), "7");
        assert_eq!(ResumeId::parse("7"), Some(ResumeId::Single(7)));
        let back = Positions::open(&[A], ResumeId::parse(&p.event_id()).as_ref());
        assert_eq!(back.after(0), Some(7));
    }

    #[test]
    fn a_multi_stream_id_round_trips_each_position() {
        let mut p = Positions::open(&[A, B], None);
        p.sent(0, 9);
        let id = p.event_id();
        assert_eq!(id, format!("{}:0=9", hex::encode(set_name(&[A, B]))));
        let back = Positions::open(&[A, B], ResumeId::parse(&id).as_ref());
        assert_eq!(back.after(0), Some(9));
        assert_eq!(
            back.after(1),
            None,
            "B was sent nothing: its cursors decide"
        );

        let mut back = back;
        back.sent(1, 3);
        let again = Positions::open(&[A, B], ResumeId::parse(&back.event_id()).as_ref());
        assert_eq!((again.after(0), again.after(1)), (Some(9), Some(3)));
    }

    #[test]
    fn a_position_never_moves_backwards() {
        let mut p = Positions::open(&[A, B], None);
        p.sent(1, 5);
        p.sent(1, 4);
        p.sent(1, 0);
        assert_eq!(p.after(1), Some(5));
    }

    #[test]
    fn an_id_that_cannot_apply_resumes_nothing() {
        let mut minted = Positions::open(&[A, B], None);
        minted.sent(0, 9);
        let id = ResumeId::parse(&minted.event_id());

        // Another set: the same streams in another order are another list.
        assert!(!Positions::open(&[B, A], id.as_ref()).resumed());
        // A bare id cannot say which of two streams it is about.
        assert!(!Positions::open(&[A, B], Some(&ResumeId::Single(9))).resumed());
        // A set position cannot resume a one-stream session.
        assert!(!Positions::open(&[A], id.as_ref()).resumed());
        // An index past the end makes the whole id untrusted.
        let out_of_range = format!("{}:0=9,2=4", hex::encode(set_name(&[A, B])));
        assert!(!Positions::open(&[A, B], ResumeId::parse(&out_of_range).as_ref()).resumed());
    }

    #[test]
    fn an_id_with_no_claim_is_not_a_resume_id() {
        let set = hex::encode(set_name(&[A, B]));
        for raw in [
            "",
            "0",
            "nonsense",
            "abc:0=1",
            &format!("{set}:"),
            &format!("{set}:0=0"),
            &format!("{set}:0=1,0=2"),
            &format!("{set}:x=1"),
        ] {
            assert_eq!(ResumeId::parse(raw), None, "{raw:?}");
        }
    }
}

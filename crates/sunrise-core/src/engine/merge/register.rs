//! The register fold of ADR-0065 §3–§4: what a register holds is the set of
//! its concurrent writes, and the winner is picked from that set by the
//! field's conflict class.
//!
//! # The fold, as a function of the versions applied
//!
//! For one register `f` of one entity:
//!
//! 1. **Candidates.** Every version that wrote `f` and is not below the
//!    legacy floor `L` (clarification D4: the floor filters before any class
//!    rule applies).
//! 2. **Antichain.** The candidates no other candidate descends from, by the
//!    version order of [`super::versions`].
//! 3. **Winner.** The member of the antichain with the greatest rank, which
//!    is origin first, then the class rule, then the stamp (D7):
//!
//!    | Class | Rank |
//!    |---|---|
//!    | quiet, tracked, merge, or a field this build does not know | `(user, stamp)` |
//!    | state | `(user, state ∈ TERMINAL_STATES, stamp)` |
//!    | tombstone | `(user, value == true, stamp)` |
//!
//!    `user` is 1 for a user write and 0 for a generated one, so a generated
//!    write never beats a concurrent user write, and still beats a user write
//!    it descends from, which is not in the antichain to beat.
//!
//! `merge_registers` holds the winner and `register_losers` the rest of the
//! antichain. A register whose antichain is empty, because the floor rose
//! past every write to it, holds no row: one below the floor read as absent
//! anyway (the module docs of [`super`]).
//!
//! Every version has its own stamp, so the rank is a total order and the
//! winner is an argmax over a set: no pair of writes is ever merged, so no
//! order of evaluation is involved. Take `a ≺ c`, with `b` concurrent with
//! both: the antichain is `{b, c}` and `a` is never compared, which a
//! pairwise fold of state ranks would get wrong for some groupings.
//!
//! # Why the stored set is that function, whatever order versions arrive in
//!
//! [`Registers`] keeps the antichain incrementally: a new write joins it
//! unless a member descends from it, and evicts the members it descends
//! from. Being left out is permanent once it is right:
//!
//! - the floor only rises;
//! - if `x ≺ y` and `y` is later dropped by the floor, `x` is below the floor
//!   too, since every edge points to a smaller stamp;
//! - if `x ≺ y ≺ z`, then `x ≺ z`, since a frontier includes its ancestors'.
//!
//! The order only gains pairs as versions arrive, and never claims one that
//! the full set would not. A version that arrives late can add pairs between
//! writes already kept, so [`Registers::refilter`] runs the antichain rule
//! again over every register of the entity whenever its frontiers were
//! recomputed. After that, the stored set is the antichain of the versions
//! applied.

use super::patch::registered_field;
use super::state::{dec, enc, stamp_cols};
use super::versions::{frontier_of, Frontier};
use super::{OpRef, Stamp, GENERATED};
use crate::engine::ids::blob16;
use ciborium::value::Value;
use rusqlite::{params, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::registry::{Conflict, EntitySpec, TERMINAL_STATES};

/// One write a register keeps: a member of its antichain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::engine) struct Member {
    /// The writing version's stamp, which also names it.
    pub(in crate::engine) at: Stamp,
    /// The value's canonical CBOR, or `None` for a write of *absent*.
    pub(in crate::engine) value: Option<Vec<u8>>,
    /// `user` or `generated`.
    pub(in crate::engine) origin: String,
}

impl Member {
    fn decoded(&self) -> Option<Value> {
        self.value.as_deref().map(dec)
    }
}

/// The conflict class a register field folds by: the one the registry
/// declares, and [`Conflict::Quiet`] for a field this build does not know
/// (ADR-0065 §3, unknown fields).
pub(in crate::engine) fn class_of(spec: &'static EntitySpec, field: &str) -> Conflict {
    registered_field(spec, field).map_or(Conflict::Quiet, |(_, f)| f.conflict)
}

/// A member's rank among its antichain (D7): origin, then the class rule,
/// then the stamp.
fn rank(class: Conflict, m: &Member) -> (bool, bool, Stamp) {
    let user = m.origin != GENERATED;
    let key = match class {
        Conflict::State => matches!(
            m.decoded(),
            Some(Value::Text(s)) if TERMINAL_STATES.contains(&s.as_str())
        ),
        Conflict::Tombstone => m.decoded() == Some(Value::Bool(true)),
        Conflict::Merge | Conflict::Quiet | Conflict::Tracked => false,
    };
    (user, key, m.at)
}

/// The winner of an antichain.
pub(in crate::engine) fn winner(class: Conflict, members: &[Member]) -> Option<&Member> {
    members.iter().max_by_key(|m| rank(class, m))
}

/// The frontiers the fold has read, by version.
struct Frontiers {
    id: [u8; 16],
    known: BTreeMap<OpRef, Frontier>,
}

impl Frontiers {
    /// `F(v)` for the version stamped `v`. A register with no version row
    /// behind it, which only a joined snapshot can hold, reaches itself and
    /// nothing else.
    fn of(&mut self, tx: &Transaction<'_>, v: Stamp) -> rusqlite::Result<&Frontier> {
        let op = v.op_ref();
        if !self.known.contains_key(&op) {
            let mut f = frontier_of(tx, &self.id, op)?;
            f.entry(v.device).or_insert(v);
            self.known.insert(op, f);
        }
        Ok(&self.known[&op])
    }

    /// Whether the version stamped `a` precedes the one stamped `b`.
    fn precedes(&mut self, tx: &Transaction<'_>, a: Stamp, b: Stamp) -> rusqlite::Result<bool> {
        if a == b {
            return Ok(false);
        }
        Ok(self.of(tx, b)?.get(&a.device).is_some_and(|r| *r >= a))
    }
}

/// What a register held when it was loaded.
#[derive(Debug, Default)]
struct Held {
    winner: Option<Member>,
    losers: Vec<Member>,
}

/// Every register of one entity, as the fold of one op reads and writes
/// them: loaded once, kept in memory, and written back by [`Self::flush`].
pub(in crate::engine) struct Registers {
    spec: &'static EntitySpec,
    id: [u8; 16],
    /// The legacy floor: no member is stamped below it.
    floor: Option<Stamp>,
    held: BTreeMap<String, Held>,
    /// Each register's antichain, as the fold leaves it.
    fields: BTreeMap<String, Vec<Member>>,
    /// The registers whose antichain may differ from what was loaded.
    dirty: BTreeSet<String>,
    frontiers: Frontiers,
}

fn read_members(
    tx: &Transaction<'_>,
    sql: &str,
    id: &[u8; 16],
) -> rusqlite::Result<Vec<(String, Member)>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map(params![&id[..]], |r| {
        let at = stamp_cols(r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?);
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, at))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (field, value, origin, at) = row?;
        if let Some(at) = at {
            out.push((field, Member { at, value, origin }));
        }
    }
    Ok(out)
}

impl Registers {
    /// Load every register of `id`. `floor` is the entity's legacy floor.
    pub(in crate::engine) fn load(
        tx: &Transaction<'_>,
        spec: &'static EntitySpec,
        id: &[u8; 16],
        floor: Option<Stamp>,
    ) -> rusqlite::Result<Self> {
        let mut held: BTreeMap<String, Held> = BTreeMap::new();
        for (field, m) in read_members(
            tx,
            "SELECT field, value, origin, hlc_ms, hlc_logical, device, seq, stream
             FROM merge_registers WHERE entity_id = ?",
            id,
        )? {
            held.entry(field).or_default().winner = Some(m);
        }
        for (field, m) in read_members(
            tx,
            "SELECT field, value, origin, hlc_ms, hlc_logical, device, seq, stream
             FROM register_losers WHERE entity_id = ?",
            id,
        )? {
            held.entry(field).or_default().losers.push(m);
        }
        let fields = held
            .iter()
            .map(|(f, h)| {
                let members = h.winner.iter().chain(&h.losers).cloned().collect();
                (f.clone(), members)
            })
            .collect();
        Ok(Self {
            spec,
            id: *id,
            floor,
            held,
            fields,
            dirty: BTreeSet::new(),
            frontiers: Frontiers {
                id: *id,
                known: BTreeMap::new(),
            },
        })
    }

    /// Fold one write of `field`: `value` at `at`, by a version of `origin`.
    /// Idempotent: the same write folded again changes nothing.
    pub(in crate::engine) fn offer(
        &mut self,
        tx: &Transaction<'_>,
        field: &str,
        value: Option<&Value>,
        at: Stamp,
        origin: &str,
    ) -> rusqlite::Result<()> {
        if self.floor.is_some_and(|l| at < l) {
            return Ok(());
        }
        let members = self.fields.entry(field.to_owned()).or_default();
        if members.iter().any(|m| m.at == at) {
            return Ok(());
        }
        for m in members.iter() {
            if self.frontiers.precedes(tx, at, m.at)? {
                return Ok(());
            }
        }
        let mut kept = Vec::with_capacity(members.len() + 1);
        for m in members.drain(..) {
            if !self.frontiers.precedes(tx, m.at, at)? {
                kept.push(m);
            }
        }
        kept.push(Member {
            at,
            value: value.map(enc).transpose()?,
            origin: origin.to_owned(),
        });
        *members = kept;
        self.dirty.insert(field.to_owned());
        Ok(())
    }

    /// Raise the legacy floor to `floor`: every write below it leaves its
    /// antichain.
    pub(in crate::engine) fn raise_floor(&mut self, floor: Stamp) {
        self.floor = Some(self.floor.map_or(floor, |l| l.max(floor)));
        for (field, members) in &mut self.fields {
            let before = members.len();
            members.retain(|m| m.at >= floor);
            if members.len() != before {
                self.dirty.insert(field.clone());
            }
        }
    }

    /// Apply the antichain rule again to every register, after the entity's
    /// frontiers were recomputed for a version that arrived late.
    pub(in crate::engine) fn refilter(&mut self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        self.frontiers.known.clear();
        for (field, members) in &mut self.fields {
            let all = std::mem::take(members);
            for m in &all {
                let mut dominated = false;
                for n in &all {
                    if self.frontiers.precedes(tx, m.at, n.at)? {
                        dominated = true;
                        break;
                    }
                }
                if !dominated {
                    members.push(m.clone());
                }
            }
            if members.len() != all.len() {
                self.dirty.insert(field.clone());
            }
        }
        Ok(())
    }

    /// Write every register whose antichain changed back to
    /// `merge_registers` and `register_losers`.
    pub(in crate::engine) fn flush(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        let none = Held::default();
        for field in &self.dirty {
            let members = self.fields.get(field).map_or(&[][..], Vec::as_slice);
            let held = self.held.get(field).unwrap_or(&none);
            let won = winner(class_of(self.spec, field), members);
            match (won, &held.winner) {
                (Some(w), held) if held.as_ref() != Some(w) => self.write_winner(tx, field, w)?,
                (None, Some(_)) => {
                    tx.prepare("DELETE FROM merge_registers WHERE entity_id = ? AND field = ?")?
                        .execute(params![&self.id[..], field])?;
                }
                // The winner stands, or there was none and is none.
                _ => {}
            }
            let losers: Vec<&Member> = members.iter().filter(|m| Some(*m) != won).collect();
            for gone in held.losers.iter().filter(|h| !losers.contains(h)) {
                let op = gone.at.op_ref();
                tx.prepare(
                    "DELETE FROM register_losers
                     WHERE entity_id = ? AND field = ? AND stream = ? AND device = ? AND seq = ?",
                )?
                .execute(params![
                    &self.id[..],
                    field,
                    &op.stream[..],
                    &op.device[..],
                    op.seq
                ])?;
            }
            for new in losers.iter().filter(|m| !held.losers.contains(m)) {
                self.write_loser(tx, field, new)?;
            }
        }
        Ok(())
    }

    fn write_winner(&self, tx: &Transaction<'_>, field: &str, w: &Member) -> rusqlite::Result<()> {
        tx.prepare(
            "INSERT INTO merge_registers
             (entity_id, field, value, hlc_ms, hlc_logical, device, seq, stream, origin)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (entity_id, field) DO UPDATE SET
                value = excluded.value, hlc_ms = excluded.hlc_ms,
                hlc_logical = excluded.hlc_logical, device = excluded.device,
                seq = excluded.seq, stream = excluded.stream, origin = excluded.origin",
        )?
        .execute(params![
            &self.id[..],
            field,
            w.value,
            w.at.hlc.physical_ms,
            w.at.hlc.logical,
            &w.at.device[..],
            w.at.seq,
            &w.at.stream[..],
            w.origin,
        ])?;
        Ok(())
    }

    /// `legacy` is 1 for the write of a full-state op. Every member is at or
    /// above the floor, and the floor is the greatest legacy stamp, so the
    /// floor's own op is the only legacy member there can be.
    fn write_loser(&self, tx: &Transaction<'_>, field: &str, m: &Member) -> rusqlite::Result<()> {
        tx.prepare(
            "INSERT OR REPLACE INTO register_losers
             (entity_id, field, stream, device, seq, hlc_ms, hlc_logical, value, origin, legacy)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            &self.id[..],
            field,
            &m.at.stream[..],
            &m.at.device[..],
            m.at.seq,
            m.at.hlc.physical_ms,
            m.at.hlc.logical,
            m.value,
            m.origin,
            i64::from(self.floor == Some(m.at)),
        ])?;
        Ok(())
    }
}

// ---- reading the unresolved sets ----

/// One stored write, as [`kept_registers`] reads it.
#[derive(Debug, Clone)]
pub(in crate::engine) struct Kept {
    pub(in crate::engine) member: Member,
    /// The write of a full-state op (D4): kept, and never surfaced.
    pub(in crate::engine) legacy: bool,
}

/// One register that keeps losers: its winner and every loser.
#[derive(Debug, Clone)]
pub(in crate::engine) struct KeptRegister {
    pub(in crate::engine) entity: [u8; 16],
    /// The entity's registry tag, from `merge_entities`.
    pub(in crate::engine) tag: String,
    pub(in crate::engine) field: String,
    pub(in crate::engine) winner: Member,
    pub(in crate::engine) losers: Vec<Kept>,
}

/// Every register that keeps at least one loser, for one entity or for all,
/// in `(entity, field)` order. Nothing is filtered: which losers are
/// surfaced is the caller's (ADR-0065 §4).
pub(in crate::engine) fn kept_registers(
    tx: &Transaction<'_>,
    entity: Option<&[u8; 16]>,
) -> rusqlite::Result<Vec<KeptRegister>> {
    let mut stmt = tx.prepare(
        "SELECT l.entity_id, e.kind, l.field,
                l.value, l.origin, l.hlc_ms, l.hlc_logical, l.device, l.seq, l.stream, l.legacy,
                w.value, w.origin, w.hlc_ms, w.hlc_logical, w.device, w.seq, w.stream
         FROM register_losers l
         JOIN merge_registers w ON w.entity_id = l.entity_id AND w.field = l.field
         JOIN merge_entities e ON e.entity_id = l.entity_id
         WHERE ?1 IS NULL OR l.entity_id = ?1
         ORDER BY l.entity_id, l.field, l.hlc_ms, l.hlc_logical, l.device, l.seq, l.stream",
    )?;
    let rows = stmt.query_map(params![entity.map(|e| e.to_vec())], |r| {
        let loser = stamp_cols(r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?);
        let won = stamp_cols(r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?, r.get(17)?);
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            loser.map(|at| (at, r.get::<_, Option<Vec<u8>>>(3), r.get::<_, String>(4))),
            r.get::<_, i64>(10)? != 0,
            won.map(|at| (at, r.get::<_, Option<Vec<u8>>>(11), r.get::<_, String>(12))),
        ))
    })?;
    let mut out: Vec<KeptRegister> = Vec::new();
    for row in rows {
        let (id, tag, field, loser, legacy, won) = row?;
        let (Some((at, value, origin)), Some((w_at, w_value, w_origin))) = (loser, won) else {
            continue;
        };
        let member = Member {
            at,
            value: value?,
            origin: origin?,
        };
        let winner = Member {
            at: w_at,
            value: w_value?,
            origin: w_origin?,
        };
        let entity = blob16(&id);
        let kept = Kept { member, legacy };
        match out.last_mut() {
            Some(last) if last.entity == entity && last.field == field => last.losers.push(kept),
            _ => out.push(KeptRegister {
                entity,
                tag,
                field,
                winner,
                losers: vec![kept],
            }),
        }
    }
    Ok(out)
}

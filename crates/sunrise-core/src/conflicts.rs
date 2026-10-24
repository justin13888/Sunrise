//! What [`Query::Conflicts`](crate::Query::Conflicts) returns: the writes the
//! fold keeps beside each register's winner (ADR-0065 §4).
//!
//! A conflict is a fact about the versions applied, not about this replica:
//! every replica that has applied the same versions reports the same
//! conflicts, and a write that descends from every member of a field's set
//! clears it everywhere it is applied. See `docs/05-sync/conflict-resolution.md`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use sunrise_cbor::CborValue;
use sunrise_id::registry::Conflict;
use sunrise_id::EntityRef;

/// A version's id: the op-ref `(stream, device, seq)` of the op that wrote it
/// (ADR-0065 §1).
///
/// Its text form, which the CLI prints and `sunrise conflicts resolve --pick`
/// reads, is `<stream hex>.<device hex>.<seq>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VersionRef {
    /// The stream the op was sealed in.
    pub stream: [u8; 16],
    /// The device that wrote it.
    pub device: [u8; 16],
    /// Its position in `(stream, device)`.
    pub seq: u64,
}

fn hex(b: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::with_capacity(32), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn unhex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

impl fmt::Display for VersionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}",
            hex(&self.stream),
            hex(&self.device),
            self.seq
        )
    }
}

/// [`VersionRef`]'s text form did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadVersionRef;

impl fmt::Display for BadVersionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a version is <stream hex>.<device hex>.<seq>")
    }
}

impl std::error::Error for BadVersionRef {}

impl FromStr for VersionRef {
    type Err = BadVersionRef;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let (Some(stream), Some(device), Some(seq), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(BadVersionRef);
        };
        Ok(Self {
            stream: unhex(stream).ok_or(BadVersionRef)?,
            device: unhex(device).ok_or(BadVersionRef)?,
            seq: seq.parse().map_err(|_| BadVersionRef)?,
        })
    }
}

/// Who made a write (ADR-0044 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOrigin {
    /// A person, through a command.
    User,
    /// The routine engine: an occurrence it materialized.
    Generated,
}

/// A field's conflict class (ADR-0065 §3), as the registry declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictClass {
    /// Kept, and surfaced only when asked for.
    Quiet,
    /// Kept and surfaced.
    Tracked,
    /// `Task.state`: terminal over open, kept and surfaced.
    State,
    /// `deleted`: delete wins, kept and surfaced, with the edits made while
    /// deleted.
    Tombstone,
    /// A register of a field whose writes all survive (a counter's legacy
    /// base). Never surfaced unless asked for.
    Merge,
    /// A `Task.blocked_by` edge or a `Stream.parent_id` that the read-time
    /// cycle rule sets aside (ADR-0065 §5, #333): concurrent writes closed a
    /// loop, and the edge reads as absent. Always surfaced.
    DependencyCycle,
}

impl From<Conflict> for ConflictClass {
    fn from(c: Conflict) -> Self {
        match c {
            Conflict::Merge => Self::Merge,
            Conflict::Quiet => Self::Quiet,
            Conflict::Tracked => Self::Tracked,
            Conflict::State => Self::State,
            Conflict::Tombstone => Self::Tombstone,
        }
    }
}

/// One write to a field, winning or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictWrite {
    /// The version that made it.
    pub version: VersionRef,
    /// Its stamp's physical time, Unix ms.
    pub hlc_ms: u64,
    /// Its stamp's logical counter.
    pub hlc_logical: u32,
    /// The device that made it (the version's device).
    pub device: [u8; 16],
    /// Who made it.
    pub origin: WriteOrigin,
    /// The value written; `None` when the write left the field absent (a
    /// full-state op that did not carry it).
    pub value: Option<CborValue>,
}

/// A branch that wrote to an entity without having seen its delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeletedEdit {
    /// The branch's newest version: one of the entity's heads.
    pub version: VersionRef,
    /// Its device.
    pub device: [u8; 16],
    /// The fields that version wrote.
    pub fields: Vec<String>,
}

/// One field of one entity whose concurrent writes did not all agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityConflict {
    /// The entity.
    pub entity: EntityRef,
    /// The field, by its wire name (`template.title` for a nested one).
    pub field: String,
    /// The field's class.
    pub class: ConflictClass,
    /// The write the entity shows.
    pub winner: ConflictWrite,
    /// The concurrent writes that lost, oldest first. Only those whose value
    /// differs from the winner's and that a `Patch` made: a full-state op's
    /// write is kept and never surfaced (ADR-0065 D4, D5).
    pub losers: Vec<ConflictWrite>,
    /// For `deleted` when the delete won: the branches that edited the entity
    /// without having seen the delete, oldest first. Empty otherwise.
    pub edited_while_deleted: Vec<DeletedEdit>,
    /// For [`ConflictClass::DependencyCycle`]: the loop the set-aside edge
    /// would close, from `entity` back to `entity`. Empty otherwise.
    ///
    /// The `winner` is then the write of the set-aside edge itself (its
    /// value the blocker or parent it names), the one the rule chose not to
    /// show, and `losers` is empty. Removing any edge of the loop ends it.
    pub cycle: Vec<EntityRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_ref_round_trips_through_its_text() {
        let v = VersionRef {
            stream: [0xab; 16],
            device: [0x01; 16],
            seq: 42,
        };
        let s = v.to_string();
        assert_eq!(
            s,
            "abababababababababababababababab.01010101010101010101010101010101.42"
        );
        assert_eq!(s.parse::<VersionRef>(), Ok(v));
        for bad in ["", "ab.cd.1", &format!("{s}.9"), &s.replace(".42", ".x")] {
            assert_eq!(bad.parse::<VersionRef>(), Err(BadVersionRef), "{bad}");
        }
    }
}

//! The one local write path (ADR-0065 §5): a command builds its op, appends
//! it to the op log, and folds it through the step a received op takes.
//!
//! [`Engine::append_and_fold`] is that path. It seals the op into the log
//! ([`Engine::ops_insert_at`]) and then hands it to
//! [`materialize_remote`], the dispatcher [`Engine::apply_remote_all`] and
//! [`Engine::rebuild_projection`] fold every received and replayed entity op
//! through. No command writes an entity row: the row writers are private to
//! [`super::merge`], so a command that tried would not compile.
//!
//! What this buys is that the writer and every peer derive a row from one
//! function of the same ops. A command reads the projection to build its op,
//! but the row it leaves behind is the fold's, not its own copy of what it
//! meant to write, so there is no second writer for the two to disagree
//! through, and a rebuild of the log lands on the rows the commands left.
//!
//! # What is written: the write mode (ADR-0065 §7)
//!
//! A command describes its write as the whole entity it leaves behind, the
//! full-state op of ADR-0044 §7. What reaches the log depends on the vault's
//! [`WriteMode`], decided inside the command's transaction
//! ([`Engine::write_mode`]):
//!
//! - **[`WriteMode::Patch`]**, once the vault requires `core.field_ops`: the
//!   op is rewritten as the `Patch` of the fields it changed
//!   ([`as_patch`]), naming the entity's heads as its parents, and a
//!   multi-entity commit is a group of `Patch`es carrying `group`. No
//!   full-state op is written. The first write into a vault every paired
//!   device can read field ops in makes the vault require the feature, in
//!   the same transaction.
//! - **[`WriteMode::Legacy`]**, while a paired device lacks the feature
//!   (clarification D9): the full-state op itself, and a group's members
//!   carry the `op_group` mark of clarification D13.
//!
//! A command whose writes are only correct together appends them as one
//! group ([`Engine::append_and_fold_group`], §6), which every replica folds
//! whole or not at all (`engine::groups`).
//!
//! [`materialize_remote`]: super::lww::materialize_remote

use super::groups::{mark, record_local, Group};
use super::lww::{materialize_member, materialize_remote, LwwStamp};
use super::merge::{encode_value, OpRef};
use super::{Engine, EngineError};
use crate::feature::FIELD_OPS_FEATURE;
use crate::inner_op::{encode_inner_op, InnerOp, PatchPayload};
use ciborium::value::Value;
use rusqlite::{params, Transaction};
use std::borrow::Cow;
use std::collections::BTreeMap;
use sunrise_domain::{CborValue, Unknowns};
use sunrise_id::registry::{Crdt, Merge};
use sunrise_id::{EntityKind, EntityRef};

/// How a vault's entity writes reach its log (ADR-0065 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteMode {
    /// The vault requires `core.field_ops`: every entity write is a `Patch`
    /// of the fields it changed, naming its writer's heads.
    Patch,
    /// A paired device, or this build, lacks `core.field_ops`: entity writes
    /// are full-state ops (clarification D9).
    Legacy,
}

/// A `Patch` a command writes, built field op by field op (ADR-0044 §1).
///
/// Its parents are the entity's heads as this replica holds them inside the
/// command's transaction, sorted by op-ref, and the key is left out when there
/// are none (ADR-0065 §2, plan §2F). A remove names the add-tags this replica
/// observed for the element, the adds a later remove has not named.
#[derive(Debug, Clone)]
pub(super) struct PatchBuilder {
    target: EntityRef,
    create: bool,
    generated: bool,
    fields: BTreeMap<String, Field>,
}

/// One field op a [`PatchBuilder`] holds until it is built.
#[derive(Debug, Clone)]
enum Field {
    Set(Value),
    Edit { add: Vec<Value>, remove: Vec<Value> },
    Inc(i64),
}

impl PatchBuilder {
    /// A `Patch` of `target` that writes nothing yet.
    pub(super) const fn new(target: EntityRef) -> Self {
        Self {
            target,
            create: false,
            generated: false,
            fields: BTreeMap::new(),
        }
    }

    /// The op creates the entity.
    pub(super) const fn create(mut self) -> Self {
        self.create = true;
        self
    }

    /// The op is the system's, not a user's (`"origin": "generated"`).
    pub(super) const fn generated(mut self) -> Self {
        self.generated = true;
        self
    }

    /// Write register `name`, `{"set": v}`.
    pub(super) fn set(mut self, name: &str, v: Value) -> Self {
        self.fields.insert(name.to_owned(), Field::Set(v));
        self
    }

    /// Add `add` to OR-set `name` and remove `remove` from it, with the tags
    /// this replica observed for each removed element.
    pub(super) fn edit(mut self, name: &str, add: Vec<Value>, remove: Vec<Value>) -> Self {
        self.fields
            .insert(name.to_owned(), Field::Edit { add, remove });
        self
    }

    /// Add `n` to counter `name`, `{"inc": n}`.
    pub(super) fn inc(mut self, name: &str, n: i64) -> Self {
        self.fields.insert(name.to_owned(), Field::Inc(n));
        self
    }

    /// The `Patch`, its parents and observed tags read inside `tx`. `None`
    /// when it writes nothing: an OR-set edit whose removes observed no tag
    /// and that adds nothing, or no field at all.
    pub(super) fn build(self, tx: &Transaction<'_>) -> rusqlite::Result<Option<InnerOp>> {
        let id = self.target.bytes();
        let mut fields = Unknowns::new();
        for (name, field) in self.fields {
            let op = match field {
                Field::Set(v) => kv("set", v),
                Field::Inc(n) => kv("inc", Value::Integer(n.into())),
                Field::Edit { add, remove } => {
                    let mut removed = Vec::new();
                    for element in remove {
                        let tags = observed_tags(tx, id, &name, &element)?;
                        if !tags.is_empty() {
                            removed.push(Value::Array(vec![
                                element,
                                Value::Array(tags.into_iter().map(OpRef::to_value).collect()),
                            ]));
                        }
                    }
                    let mut entries = Vec::new();
                    if !add.is_empty() {
                        entries.push((Value::Text("add".into()), Value::Array(add)));
                    }
                    if !removed.is_empty() {
                        entries.push((Value::Text("remove".into()), Value::Array(removed)));
                    }
                    if entries.is_empty() {
                        continue;
                    }
                    Value::Map(entries)
                }
            };
            fields.insert(name, CborValue(op));
        }
        if fields.is_empty() {
            return Ok(None);
        }
        let parents = super::merge::parents_of(tx, id)?;
        Ok(Some(InnerOp::Patch(Box::new(PatchPayload {
            target: self.target,
            create: self.create,
            origin: self.generated.then(|| PatchPayload::GENERATED.to_owned()),
            parents: (!parents.is_empty()).then_some(parents),
            group: None,
            fields,
            unknown: Unknowns::new(),
        }))))
    }
}

/// `{key: v}`.
fn kv(key: &str, v: Value) -> Value {
    Value::Map(vec![(Value::Text(key.into()), v)])
}

/// The add-tags of `element` in OR-set `field` of entity `id` that no remove
/// names: what a remove written here has observed (ADR-0044 §3).
fn observed_tags(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    element: &Value,
) -> rusqlite::Result<Vec<OpRef>> {
    let bytes = encode_value(element)?;
    let mut stmt = tx.prepare(
        "SELECT a.tag_stream, a.tag_device, a.tag_seq FROM merge_orset_adds a
         WHERE a.entity_id = ?1 AND a.field = ?2 AND a.element = ?3
           AND NOT EXISTS (
               SELECT 1 FROM merge_orset_removes r
               WHERE r.entity_id = a.entity_id AND r.field = a.field
                 AND r.element = a.element AND r.tag_stream = a.tag_stream
                 AND r.tag_device = a.tag_device AND r.tag_seq = a.tag_seq)
         ORDER BY a.tag_stream, a.tag_device, a.tag_seq",
    )?;
    let rows = stmt.query_map(params![&id[..], field, bytes], |r| {
        Ok(OpRef {
            stream: super::ids::blob16(&r.get::<_, Vec<u8>>(0)?),
            device: super::ids::blob16(&r.get::<_, Vec<u8>>(1)?),
            seq: u64::try_from(r.get::<_, i64>(2)?.max(0)).unwrap_or(0),
        })
    })?;
    rows.collect()
}

/// The whole-entity value a full-state entity op carries, or `None` for an op
/// that is not one.
fn full_state(op: &InnerOp) -> Option<Result<Value, ciborium::value::Error>> {
    Some(match op {
        InnerOp::TaskCreate(t) | InnerOp::TaskUpdate(t) | InnerOp::TaskDelete(t) => {
            Value::serialized(t)
        }
        InnerOp::StreamCreate(s) | InnerOp::StreamUpdate(s) | InnerOp::StreamDelete(s) => {
            Value::serialized(s)
        }
        InnerOp::ContextCreate(c) | InnerOp::ContextUpdate(c) | InnerOp::ContextDelete(c) => {
            Value::serialized(c)
        }
        InnerOp::RoutineCreate(r) | InnerOp::RoutineUpdate(r) | InnerOp::RoutineDelete(r) => {
            Value::serialized(&**r)
        }
        InnerOp::BlockCreate(b) | InnerOp::BlockUpdate(b) | InnerOp::BlockDelete(b) => {
            Value::serialized(&**b)
        }
        InnerOp::AttachmentCreate(a) | InnerOp::AttachmentDelete(a) => Value::serialized(&**a),
        _ => return None,
    })
}

/// Whether `op` is an entity write `core.field_ops` covers: a full-state op,
/// or a `Patch` of an entity that has full-state ops.
fn is_versioned_write(op: &InnerOp) -> bool {
    match op {
        InnerOp::Patch(p) => !p.target.kind().spec().ops.is_empty(),
        other => full_state(other).is_some(),
    }
}

/// The fields a `Patch` never writes (ADR-0044 §4, §10).
const UNWRITABLE: [&str; 3] = ["id", "created_at", "updated_at"];

fn cbor_error(e: impl std::fmt::Display) -> EngineError {
    EngineError::Cbor(e.to_string())
}

/// `name`'s entry in a CBOR map.
fn entry<'a>(map: &'a [(Value, Value)], name: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .map(|(_, v)| v)
}

/// `name`'s entry in an entity's CBOR map: under its parent's map for a
/// nested field (`template.title`), at the top otherwise.
fn path_entry<'a>(map: &'a [(Value, Value)], name: &str) -> Option<&'a Value> {
    match name.split_once('.') {
        Some((parent, child)) => entry(entry(map, parent)?.as_map()?, child),
        None => entry(map, name),
    }
}

/// The value that writes "absent" to register `name` of `kind`, whose new
/// state `after` leaves the field out: the value the record's serializer
/// leaves out for it, which its decoder reads the absent key as. `null` for
/// an optional field, the empty list for a list skipped when empty, `true`
/// for a flag that defaults to it, and so on: whichever candidate the record
/// writes back without the key. `null` when none is.
fn absent_value(kind: EntityKind, name: &str, after: &[(Value, Value)]) -> Value {
    let candidates = [
        Value::Null,
        Value::Array(Vec::new()),
        Value::Bool(false),
        Value::Bool(true),
        Value::Integer(0.into()),
        Value::Text(String::new()),
        Value::Map(Vec::new()),
    ];
    candidates
        .into_iter()
        .find(|c| {
            let mut entries = after.to_vec();
            super::merge::set_field(kind, &mut entries, name, c.clone());
            super::merge::round_trip(kind, Value::Map(entries))
                .and_then(|v| v.as_map().map(|m| path_entry(m, name).is_none()))
                .unwrap_or(false)
        })
        .unwrap_or(Value::Null)
}

/// The value field `name` of `kind` reads as when no op has written it: the
/// default the record's decoder gives a key it leaves out, or the
/// projection's own default for a field the decoder requires. `None` when
/// the field has none, so a create must write it.
fn default_value(kind: EntityKind, after: &[(Value, Value)], name: &str) -> Option<Value> {
    if let Some(v) = super::merge::field_default(kind, name) {
        return Some(v);
    }
    let without: Vec<(Value, Value)> = after
        .iter()
        .filter(|(k, _)| k.as_text() != Some(name))
        .cloned()
        .collect();
    let round = super::merge::round_trip(kind, Value::Map(without))?;
    let map = round.as_map()?;
    Some(entry(map, name).cloned().unwrap_or(Value::Null))
}

/// `v`'s elements, as an OR-set's value carries them, keyed by their
/// canonical CBOR.
fn elements(v: Option<&Value>) -> rusqlite::Result<BTreeMap<Vec<u8>, Value>> {
    let mut out = BTreeMap::new();
    for e in v.and_then(Value::as_array).into_iter().flatten() {
        out.insert(encode_value(e)?, e.clone());
    }
    Ok(out)
}

/// The field ops that turn `before` into `after`, two whole-entity values of
/// `kind` as its full-state op carries one: each field whose value differs, by
/// its CRDT type (ADR-0044 §3). `before` is `None` for a create, which writes
/// every field `after` holds a value other than its default for.
fn diff(
    kind: EntityKind,
    before: Option<&[(Value, Value)]>,
    after: &[(Value, Value)],
    mut out: PatchBuilder,
) -> Result<PatchBuilder, EngineError> {
    let Some(record) = kind.spec().records.first() else {
        return Ok(out);
    };
    let create = before.is_none();
    let before = before.unwrap_or(&[]);
    let int = |v: Option<&Value>| -> i64 {
        v.and_then(Value::as_integer)
            .and_then(|n| i64::try_from(n).ok())
            .unwrap_or(0)
    };
    for field in record.fields {
        let name = field.name;
        if UNWRITABLE.contains(&name) {
            continue;
        }
        let (b, a) = (entry(before, name), entry(after, name));
        match field.crdt {
            Crdt::Derived => {}
            Crdt::Register => {
                if create {
                    if let Some(a) = a {
                        // A create always writes its stream, even no stream
                        // (the legacy Inbox reference): a build before
                        // ADR-0046 has no default for a block's, and would
                        // hold the block unprojected for want of it.
                        if name == "stream_id"
                            || default_value(kind, after, name).as_ref() != Some(a)
                        {
                            out = out.set(name, a.clone());
                        }
                    }
                } else if a != b {
                    let v = a
                        .cloned()
                        .unwrap_or_else(|| absent_value(kind, name, after));
                    out = out.set(name, v);
                }
            }
            Crdt::OrSet => {
                let (held, wanted) = (elements(b)?, elements(a)?);
                let add: Vec<Value> = wanted
                    .iter()
                    .filter(|(k, _)| !held.contains_key(*k))
                    .map(|(_, v)| v.clone())
                    .collect();
                let remove: Vec<Value> = held
                    .iter()
                    .filter(|(k, _)| !wanted.contains_key(*k))
                    .map(|(_, v)| v.clone())
                    .collect();
                if !add.is_empty() || !remove.is_empty() {
                    out = out.edit(name, add, remove);
                }
            }
            Crdt::Counter => {
                let delta = int(a).saturating_sub(int(b));
                if delta != 0 {
                    out = out.inc(name, delta);
                }
            }
            // No entity with full-state ops has a map field: the one map,
            // `Preferences.values`, is written only by its own `Patch`.
            Crdt::Map => debug_assert!(false, "`{name}` is a map of a full-state entity"),
            Crdt::Nested => {
                let (b, a) = (
                    b.and_then(Value::as_map).map_or(&[][..], Vec::as_slice),
                    a.and_then(Value::as_map).map_or(&[][..], Vec::as_slice),
                );
                let mut keys: Vec<&str> = a.iter().filter_map(|(k, _)| k.as_text()).collect();
                keys.extend(b.iter().filter_map(|(k, _)| k.as_text()));
                keys.sort_unstable();
                keys.dedup();
                for k in keys {
                    let (bk, ak) = (entry(b, k), entry(a, k));
                    let path = format!("{name}.{k}");
                    if create {
                        if let Some(ak) = ak {
                            out = out.set(&path, ak.clone());
                        }
                    } else if ak != bk {
                        let v = ak
                            .cloned()
                            .unwrap_or_else(|| absent_value(kind, &path, after));
                        out = out.set(&path, v);
                    }
                }
            }
        }
    }
    // A key the record does not declare is never a change: a command reads
    // the entity's unknown keys and carries them back as it read them
    // (ADR-0044 §8), so the `Patch` writes none of them, and a newer build's
    // value stands.
    Ok(out)
}

/// What a command appended: an op's id in `ops` and its `seq` in
/// `(stream, this device)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Appended {
    /// The op's id in `ops`.
    pub(super) op_id: [u8; 16],
    /// Its `seq` in `(stream, this device)`.
    pub(super) seq: u64,
}

impl Appended {
    /// Nothing was appended: in [`WriteMode::Patch`] a write that changes no
    /// field writes no op. Its op id is all zeros and its `seq` is 0, which
    /// no logged op has.
    pub(super) const NOTHING: Self = Self {
        op_id: [0u8; 16],
        seq: 0,
    };
}

impl Engine {
    /// The vault's [`WriteMode`] inside `tx`.
    ///
    /// [`WriteMode::Patch`] when the vault requires `core.field_ops`. When it
    /// does not yet, but this build has the feature and every other paired
    /// device has advertised it, the vault is made to require it here, in
    /// this transaction ([`Self::require_if_unanimous`]), and the mode is
    /// `Patch`: the feature is required before the first `Patch` that uses
    /// it is sealed (ADR-0045 §7), and a command that writes nothing emits
    /// nothing. Otherwise [`WriteMode::Legacy`]. A build without the feature
    /// writes nothing in a vault that requires it: the seal guard refuses.
    ///
    /// # Errors
    /// Storage.
    pub(crate) fn write_mode(
        &self,
        tx: &Transaction<'_>,
        now_ms: u64,
    ) -> rusqlite::Result<WriteMode> {
        Ok(
            if self.require_if_unanimous(tx, FIELD_OPS_FEATURE, now_ms)? {
                WriteMode::Patch
            } else {
                WriteMode::Legacy
            },
        )
    }

    /// The op `op` is written as in `mode`. `None` when it writes nothing.
    ///
    /// In [`WriteMode::Legacy`], and for an op `core.field_ops` does not cover
    /// (an append-only record), `op` itself. In [`WriteMode::Patch`], a
    /// full-state op becomes the `Patch` of the fields that differ from the
    /// entity as this replica projects it, read inside `tx` by the reader the
    /// command read it with ([`as_patch`]), and a `Patch` without `parents`
    /// gains the entity's heads.
    fn written_as<'a>(
        &self,
        tx: &Transaction<'_>,
        op: &'a InnerOp,
        mode: WriteMode,
    ) -> Result<Option<Cow<'a, InnerOp>>, EngineError> {
        if mode == WriteMode::Legacy {
            return Ok(Some(Cow::Borrowed(op)));
        }
        if let InnerOp::Patch(p) = op {
            if p.parents.is_some() {
                return Ok(Some(Cow::Borrowed(op)));
            }
            let parents = super::merge::parents_of(tx, p.target.bytes())?;
            let mut p = p.clone();
            p.parents = (!parents.is_empty()).then_some(parents);
            return Ok(Some(Cow::Owned(InnerOp::Patch(p))));
        }
        if full_state(op).is_none() {
            return Ok(Some(Cow::Borrowed(op)));
        }
        Ok(as_patch(self, tx, op, &[])?.map(Cow::Owned))
    }

    /// Append `op` to this device's log under `stream`, and fold it.
    ///
    /// Inside `tx`, in the order that is safe ([`Self::log_slot`]): the
    /// stream's epoch is resolved first, which can mint one and log the
    /// `key_envelope` ops that share it, then the `seq` is read, then the op
    /// is stamped once. The op-log `inner_kind` and `target_kind` are read
    /// off the op, as the receive path reads them, so the two logs of one op
    /// agree.
    ///
    /// The op is first rewritten for the vault's [`WriteMode`]. In
    /// [`WriteMode::Patch`] a full-state op that changes no field writes
    /// nothing, and [`Appended::NOTHING`] is returned.
    ///
    /// The fold is [`materialize_remote`] with the stamp the envelope
    /// carries, the call a peer makes when it receives the envelope.
    pub(super) fn append_and_fold(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        stream: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<Appended> {
        let mode = if is_versioned_write(op) {
            self.write_mode(tx, now_ms)?
        } else if matches!(op, InnerOp::Patch(_)) && self.vault_requires(tx, FIELD_OPS_FEATURE)? {
            // A `Patch` of an entity with no full-state op (preferences)
            // names its heads in a field-ops vault, and requires nothing.
            WriteMode::Patch
        } else {
            WriteMode::Legacy
        };
        let Some(op) = self.written_as(tx, op, mode).map_err(to_sql)? else {
            return Ok(Appended::NOTHING);
        };
        let (done, lww) = self.append(tx, &op, stream, now_ms)?;
        materialize_remote(tx, &op, &lww, stream)?;
        Ok(done)
    }

    /// Append `members` as one group (ADR-0065 §6) and fold every one of
    /// them, in this transaction: the writes of a command that are only
    /// correct together. Each member is `(op, stream)`, sealed in order.
    /// Returns what each member appended, in order: [`Appended::NOTHING`]
    /// for a member that changed no field.
    ///
    /// The group's id is drawn from the injected rng and its size is the
    /// number of members written. A receiver holds each member back until it
    /// holds them all (`engine::groups`), so no replica shows part of the
    /// commit. In [`WriteMode::Patch`] every member is a `Patch` carrying
    /// `group`; in [`WriteMode::Legacy`] a full-state member carries the
    /// `op_group` mark (clarification D13). Fewer than two members are no
    /// group, and are appended as [`Self::append_and_fold`] appends them.
    pub(super) fn append_and_fold_group(
        &self,
        tx: &Transaction<'_>,
        members: Vec<(InnerOp, [u8; 16])>,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<Appended>> {
        self.append_members(tx, members, now_ms, true)
    }

    /// [`Self::append_and_fold_group`] in [`WriteMode::Patch`], and each
    /// member alone, as a build before groups wrote them, in
    /// [`WriteMode::Legacy`]: the writes of a command that a field-ops vault
    /// commits whole (a completion and its streak advance, a skip and its
    /// tombstone, a re-key and its tombstones), and that a vault an older
    /// device shares keeps writing as it always has.
    pub(super) fn append_and_fold_together(
        &self,
        tx: &Transaction<'_>,
        members: Vec<(InnerOp, [u8; 16])>,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<Appended>> {
        self.append_members(tx, members, now_ms, false)
    }

    fn append_members(
        &self,
        tx: &Transaction<'_>,
        members: Vec<(InnerOp, [u8; 16])>,
        now_ms: u64,
        legacy_group: bool,
    ) -> rusqlite::Result<Vec<Appended>> {
        let mode = if members.iter().any(|(op, _)| is_versioned_write(op)) {
            self.write_mode(tx, now_ms)?
        } else {
            WriteMode::Legacy
        };
        // Each member as it is written, and where it sits in `members`. Every
        // member is rewritten before any is sealed: they are writes to
        // distinct entities, each over the heads it found.
        let mut written: Vec<(usize, InnerOp, [u8; 16])> = Vec::with_capacity(members.len());
        for (i, (op, stream)) in members.into_iter().enumerate() {
            if let Some(op) = self.written_as(tx, &op, mode).map_err(to_sql)? {
                written.push((i, op.into_owned(), stream));
            }
        }
        let mut out = vec![Appended::NOTHING; written.last().map_or(0, |(i, ..)| i + 1)];
        let grouped = written.len() >= 2 && (mode == WriteMode::Patch || legacy_group);
        if !grouped {
            for (i, op, stream) in &written {
                let (done, lww) = self.append(tx, op, stream, now_ms)?;
                materialize_remote(tx, op, &lww, stream)?;
                out[*i] = done;
            }
            return Ok(out);
        }
        let mut id = [0u8; 16];
        self.rng.fill_bytes(&mut id);
        let group = Group {
            id,
            size: written.len() as u64,
        };
        let mut logged = Vec::with_capacity(written.len());
        for (i, mut op, stream) in written {
            debug_assert!(!op.is_control(), "a control op has no fold");
            // The slot is taken first: the mark names the member's own
            // op-ref, which is not known until its `seq` is.
            let slot = self.log_slot(tx, &stream, now_ms)?;
            let at = OpRef {
                stream,
                device: slot.lww.device,
                seq: slot.seq,
            };
            mark(&mut op, group, at);
            let done = self.seal(tx, &op, &stream, now_ms, slot)?;
            record_local(tx, group, at, done.1.hlc, now_ms)?;
            logged.push((i, op, stream, done));
        }
        for (i, op, stream, (done, lww)) in &logged {
            materialize_member(tx, op, lww, stream, Some(group.pair()))?;
            out[*i] = *done;
        }
        Ok(out)
    }

    /// Take a slot in `stream` and seal `op` into it.
    fn append(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        stream: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<(Appended, LwwStamp)> {
        debug_assert!(!op.is_control(), "a control op has no fold");
        let slot = self.log_slot(tx, stream, now_ms)?;
        self.seal(tx, op, stream, now_ms, slot)
    }

    /// Seal `op` into the log at `slot`.
    fn seal(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        stream: &[u8; 16],
        now_ms: u64,
        slot: super::oplog::LogSlot,
    ) -> rusqlite::Result<(Appended, LwwStamp)> {
        let inner = encode_inner_op(op)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let (seq, lww) = (slot.seq, slot.lww);
        let op_id = self.fresh_op_id(now_ms);
        self.ops_insert_at(
            tx,
            &op_id,
            stream,
            seq,
            lww.hlc,
            &inner,
            op.inner_kind(),
            op.target_kind(),
            Some(op.target_ref().bytes()),
            Some(now_ms),
            None,
            now_ms,
            &[],
            slot.epoch,
            &slot.key,
        )?;
        Ok((Appended { op_id, seq }, lww))
    }
}

/// Carry an engine error out through a transaction closure.
fn to_sql(e: EngineError) -> rusqlite::Error {
    match e {
        EngineError::Sqlite(e) => e,
        other => rusqlite::Error::ToSqlConversionFailure(Box::new(other)),
    }
}

/// The entity `target` as this replica's readers return it, as the CBOR map
/// its full-state op would carry: the value a command read before it built
/// its op. `None` when there is no row.
fn current(
    engine: &Engine,
    tx: &Transaction<'_>,
    target: EntityRef,
) -> Result<Option<Value>, EngineError> {
    let id = target.bytes();
    let value = match target.kind() {
        EntityKind::Task => super::task::read_task(tx, id)?.map(|t| Value::serialized(&t)),
        EntityKind::Stream => super::stream::read_stream(tx, id)?.map(|s| Value::serialized(&s)),
        EntityKind::Context => super::context::read_context(tx, id)?.map(|c| Value::serialized(&c)),
        EntityKind::Routine => super::routine::read_routine(tx, id)?.map(|mut r| {
            // The form a routine op is written in, as the command chose it
            // (`Engine::routine_op_form`), so a time it did not change does
            // not read as changed.
            let civil = engine
                .vault_requires(tx, super::routine::ROUTINE_CIVIL_TIMES_FEATURE)
                .unwrap_or(false);
            if !civil {
                if let Some(legacy) = r.legacy_wire_form() {
                    r = legacy;
                }
            }
            Value::serialized(&r)
        }),
        EntityKind::Block => super::block::read_block(tx, id)?.map(|b| Value::serialized(&b)),
        EntityKind::Attachment => {
            super::attachment::read_attachment(tx, id)?.map(|a| Value::serialized(&a))
        }
        _ => None,
    };
    value.transpose().map_err(cbor_error)
}

/// The `Patch` a full-state op `op` is written as in [`WriteMode::Patch`]:
/// the fields that differ from the entity as this replica holds it inside
/// `tx`, or every field set to other than its default for a create. `None`
/// when it changes no field.
///
/// A `*Create` is a `Patch` with `create`. A routine's generated occurrence
/// (a `task.create` naming its routine) is `"origin": "generated"`, the
/// origin the fold gives the same full-state op (ADR-0044 §3).
///
/// # Errors
/// A value that does not encode; storage.
pub(super) fn as_patch(
    engine: &Engine,
    tx: &Transaction<'_>,
    op: &InnerOp,
    forced: &[&str],
) -> Result<Option<InnerOp>, EngineError> {
    let Some(after) = full_state(op) else {
        return Ok(Some(op.clone()));
    };
    let after = after.map_err(cbor_error)?;
    let after = after.as_map().map_or(&[][..], Vec::as_slice);
    let target = op.target_ref();
    let kind = target.kind();
    debug_assert!(matches!(kind.spec().merge, Merge::Lww));
    let create = op.inner_kind().ends_with(".create");
    let mut builder = PatchBuilder::new(target);
    if create {
        builder = builder.create();
    }
    let names_routine = entry(after, "routine_id").is_some_and(|v| !v.is_null());
    if op.inner_kind() == "task.create" && names_routine {
        builder = builder.generated();
    }
    let held = if create {
        None
    } else {
        current(engine, tx, target)?
    };
    let before = held.as_ref().and_then(Value::as_map).map(Vec::as_slice);
    let mut builder = diff(kind, before, after, builder)?;
    // A register the command means to write whatever it held: the plan a
    // defer sets is a write of the plan, a resolution of it included.
    for name in forced {
        if !builder.fields.contains_key(*name) {
            let v = entry(after, name)
                .cloned()
                .unwrap_or_else(|| absent_value(kind, name, after));
            builder = builder.set(name, v);
        }
    }
    if builder.fields.is_empty() {
        if !create {
            return Ok(None);
        }
        // A create whose every field is its default still writes one, since
        // a `Patch` writes at least one field: the tombstone, which it holds.
        let deleted = entry(after, "deleted")
            .cloned()
            .unwrap_or(Value::Bool(false));
        builder = builder.set("deleted", deleted);
    }
    let patch = builder.build(tx)?;
    // The op a peer will check before it folds it (`check_patch`): a write
    // this build cannot make well is refused here, not sealed.
    if let Some(InnerOp::Patch(p)) = &patch {
        super::merge::check_patch(p)
            .map_err(|why| EngineError::Invalid(format!("the field ops of this write: {why:?}")))?;
    }
    Ok(patch)
}

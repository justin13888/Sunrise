-- 0015_entity_extra_columns.sql — forward-compat unknowns for the other six
-- entities.
--
-- `docs/10-cross-cutting/protocol-versioning.md` §7 states the rule: "unknown
-- CBOR map keys round-trip unchanged. A v1 client receiving a v2 op preserves
-- the unknown keys verbatim in storage and re-serializes them on outbound
-- merges", so "v2-only fields persist on v1-only devices". Capability bit 34
-- `CLI_FORWARD_COMPAT` asserts it, and it is a REQUIRED v1 bit.
--
-- Only three tables implemented it. `tasks.extra`, `blocks.extra` and
-- `attachments.extra` are read and written; `streams`, `contexts`, `routines`,
-- `focus_sessions` and `focus_session_ends` had no column, and their readers
-- in `engine.rs` hardcoded `Unknowns::new()`. Every entity
-- already carries `#[serde(flatten)] unknown: Unknowns` on the Rust struct, so
-- the field survived decode and was then dropped at the projection.
--
-- On an entity-level LWW merge model (ADR-0014) that is not a cosmetic loss,
-- and `crates/sunrise-domain/src/unknown.rs` had already written down why:
-- "the v1 device wins one conflict and the v2 fields are gone from every
-- replica". A device merely *reading and re-saving* a Stream was enough — the
-- next full-state op it emitted carried the truncated entity, and that op won
-- on every peer. Five of the nine column-projected entities silently destroyed a
-- newer peer's fields, while advertising that they did not.
--
-- Two tables are deliberately absent, for two different reasons.
--
-- `focus_interruptions`: `Interruption` is the one entity with no `unknown`
-- map at all, because its whole value is its key — see the doc comment on
-- `sunrise_domain::Interruption`. There is nothing to preserve.
--
-- `review_snapshots`: it already round-trips. That table stores the entity as
-- a whole-record CBOR `body` blob and `query_review_history` reads it back
-- with `ciborium::de::from_reader::<ReviewSnapshot, _>`, so the flattened
-- `unknown` map survives serialization without a sidecar column. A column here
-- would be a second, competing home for the same data. The distinction
-- generalises: what loses unknowns is a *column-wise* projection, not the op
-- log and not a whole-entity blob round-trip.
--
-- Additive and backfill-free: NULL decodes to an empty map, which is what a
-- row written before this migration meant anyway. No vault changes shape.

ALTER TABLE streams             ADD COLUMN extra BLOB;
ALTER TABLE contexts            ADD COLUMN extra BLOB;
ALTER TABLE routines            ADD COLUMN extra BLOB;
ALTER TABLE focus_sessions      ADD COLUMN extra BLOB;
ALTER TABLE focus_session_ends  ADD COLUMN extra BLOB;

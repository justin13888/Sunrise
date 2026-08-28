# 0020 — Three capabilities leave the v1 MUST set: stream sharing, Google Calendar, the standalone Note

**Status:** accepted

**Amends:** [`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md)
(four rows; the amendment cites this ADR from each).

## Context

The parity matrix is an accepted spec, and it is the document that says what v1
means. An audit of every capability it marks **MUST** for the two shipping
clients — the macOS app and the `sunrise` CLI — graded each one by
*reachability from a running binary*: not "is there a struct", not "do the tests
pass", but "can a user get to it". macOS scored 14 met, 2 partial, 10 unmet out
of 26; the CLI, 8 of 10.

Most of those gaps are being closed in code. Three are not, and this ADR is the
record of why, because the alternative — editing the matrix until it agrees with
the tree — is how a spec stops being worth reading. Two of the three are
deferrals with a reason; the third is a scope clarification that has been
travelling under a misleading label.

An honest note about what this ADR is *not*: v1 has not shipped. The matrix's
hard rule — "a capability MUST not regress mid-version; a v1.0 → v1.1 release
cannot remove a MUST" — governs a released product, and there is not one yet.
This is a pre-1.0 v1 definition being set, once, before anyone has been promised
anything. Nothing is being taken back from a user. That distinction is the whole
licence for this ADR, and if it were a v1.0 → v1.1 change the answer would be
"build it", not "write it down".

## Decision

### (a) Stream sharing — deferred

Parity rows **Sharing — accept invite** (macOS MUST) and **Sharing — view shared
stream as editor** (macOS MUST, CLI MUST) become **deferred**.

**The true starting position is worse than the tree makes it look.** The design
is documented in two accepted specs — [`sharing-with-others.md`](../03-crypto/sharing-with-others.md)
for the cryptographic layer and [`people-and-sharing.md`](../02-domain/people-and-sharing.md)
for the domain — and both are complete and internally consistent. Underneath
them:

* `crates/sunrise-domain/src/person.rs` defines `struct Person`, and
  `0013_baseline.sql` creates a `persons` table. **Grep finds no reference to
  that table anywhere in the repository, tests included** — the four hits are the
  `CREATE TABLE` itself and three doc files describing its absence. There is no
  `Command`, no op kind, no `Query`, no UniFFI surface, and no writer. The one
  live use of a `prs_` reference is `Task.assignee`, which the core carries as an
  opaque label.
* The sharing **primitives** are real and frozen: Ed25519 identity signatures,
  X25519 keys, XChaCha20-Poly1305 AEAD, `wrap_stream_key`/`unwrap_stream_key`
  under the vault root, the `OpEnvelope` codec, `DeviceCert` — all in
  `sunrise-crypto`, all covered by frozen test vectors.
* The sharing-**specific** crypto is not. `share_grant`, `share_revoke` and
  `share_decline` appear in no Rust or Swift file. `ShareGrantPayload` field 6 is
  an HPKE single-shot seal to the recipient's `ID_D_pub`; the `hpke` crate is
  declared in `[workspace.dependencies]` and **no crate depends on it**.
  `sunrise-crypto`'s own header says so out loud: "HPKE share grants and Noise XX
  pairing have type-level scaffolding here". Noise XX did get built, in
  `sunrise-pairing`, for device pairing. HPKE share grants did not, and
  `sunrise-pairing` contains no reference to sharing at all.

So the accurate framing is: **a complete specification, sound primitives
underneath it, and nothing in between** — neither the domain entity the design
operates on nor the control ops that carry it. This matters because the
half-built appearance is what makes the row look nearly done. It is not nearly
done; it has not been started.

Closing it requires, at minimum:

1. A grant/role model as a real entity — `viewer`/`editor`, owner-only granting
   — with commands, op kinds, queries and a seam surface, on top of a `Person`
   that something finally writes.
2. Invite issuance and acceptance: the `share_grant` control op, its Ed25519
   signature verification against the granter's published `ID_S_pub`, the
   accept/decline round trip, and the OOB fingerprint-verification UI the spec
   requires before a non-trivial share.
3. Per-stream key distribution to a **second identity** — HPKE Base sealing of
   the stream key for the recipient's device key, which is the one primitive in
   the design that the workspace does not currently link.
4. Server-side routing for a stream with more than one subject. The relay's
   channel key is `(account_id_hash, stream_id)`, so today a stream belongs to
   exactly one account by construction.
5. Authorization of inbound ops: the relay dropping ops whose signing identity
   does not hold `editor` on the target stream, as the spec's own client-side
   drop is explicitly not the boundary.
6. Revocation, and the stream-key rotation that revocation implies — re-wrapping
   a new epoch for every sibling device and remaining peer while excluding the
   revoked one, plus the `RELAY_GRANT_REVOKED` path for a recipient who was
   offline at the cutoff.
7. A macOS UI for all of it, and a CLI surface for the editor half.

**Why defer rather than build:** this is a second epic, and a security-critical
one. Every item above is a place where a plausible-looking design is wrong in a
way that does not fail a test. In an end-to-end-encrypted product a sharing model
that is *almost* right is not a partial feature — it is a vulnerability, and it
is the kind that ships silently and is discovered by someone else. Shipping v1
without sharing costs users a capability they can see is missing. Shipping
sharing without the design review it needs costs them a guarantee they believe
they have. The second is worse, and it is worse in a way that cannot be walked
back after the fact.

### (b) Calendar integration (Google) — deferred

Parity row **Calendar integration (Google)** (macOS MUST) becomes **deferred**.

**This was already decided; the matrix simply had not caught up.** Issue #4
("Read-only external calendar sync") was deferred out of the v1-rewrite epic
(#3) on 2026-08-28, on stated grounds: `sunrise-integrations` has zero reverse
dependencies, so the prerequisite is wiring the existing provider into the macOS
client, and adding more providers on top of a crate no target consumes would only
compound the orphan. Two accepted specs cannot both be authoritative about
whether Google Calendar ships in v1. The recorded decision stands and **the
matrix yields to it**.

What is *not* the reason: an unimplemented protocol. `crates/sunrise-integrations/src/gcal.rs`
is 850 lines of implemented, tested Google Calendar v3 read-only import — OAuth
PKCE authorization, code exchange and refresh with the durable-refresh-token
rule, change detection that suppresses phantom deletes on both window slide and
page truncation, all-day events, series linkage, access-role filtering — with
transport injected, so every test runs without a network. Nothing has been run
against the live API, which needs an OAuth client ID.

What is missing is everything on the near side of that crate: no crate depends on
it, `IntegrationProvider` has no implementor, there is no sync-cursor storage, no
external-event table, no reconciliation of imported events with `blocks`, and no
OAuth UI on macOS. `.github/scripts/orphan-crate-gate.py` quarantines
`sunrise-integrations` explicitly against issue #4, which is the mechanism that
keeps this parked rather than rotting — the gate's staleness check fails if the
entry is left behind once the crate is wired.

This is a wiring-and-storage gap, not a protocol gap, and whoever picks up #4
starts from working code.

### (c) Notes (rich text) — stays a MUST, with the scope split written down

Parity row **Notes (rich text)** (macOS MUST) **remains a MUST**. This entry is a
scope clarification, not a deferral, and the precision is the point: the row has
been carrying two different features under one name.

**What ships, and meets the row.** `Task.body` is fully plumbed. It is
`Option<NoteBody>` on `Task`, on `TaskDraft`, and — as `Option<Option<NoteBody>>`,
so a clear is distinguishable from a no-op — on `TaskPatch`. It persists to
`tasks.body`, it is indexed by the `search_idx` FTS5 table's `body` column, and it
crosses the UniFFI seam as `set_body` / `clear_body` on the task-edit DTO
(`setBody` / `clearBody` in generated Swift), which
`crates/sunrise-core-bindings/src/dto.rs` folds back into a `TaskPatch` through
`patch_field`. A rich-text editor over that field is being built on macOS now.
Notes-attached-to-a-task is a real v1 capability.

**What is deferred, item one: the free-standing `Note` entity.**
`crates/sunrise-domain/src/note.rs` defines `struct Note` and `0013_baseline.sql`
creates a `notes` table. As with `persons`, there is no command path, no op kind,
no query, no seam surface and no writer anywhere; `Query::EntityById` refuses
`EntityKind::Note`. A note in v1 is a *field on an entity*, never an entity of
its own. This is exactly what `docs/02-domain/notes.md` has said since it was
written — "Notes are rich-text bodies attached to a parent entity. Notes do not
exist as standalone entities" — so nothing here is new except that the parity
matrix now says it too.

**What is deferred, item two: merge-capable rich text.** `NoteBody` is an opaque
byte string on the wire (`bstr`; `#[serde(with = "serde_bytes")] Vec<u8>`), and
the interior grammar in [`notes.md`](../02-domain/notes.md) is a contract between
editors and renderers, not something the codec enforces. The workspace ships **no
text CRDT** — [ADR-0003](./0003-crdt-loro-vs-automerge.md) was never realized and
the `loro` dependency is deleted — so a body merges as one last-writer-wins unit
along with the rest of its owning entity's row, under
[ADR-0014](./0014-entity-level-lww-merge.md).

The consequence has to be stated plainly, because it is the part a user notices:
**two devices editing one note body concurrently produce one survivor, not a
merge.** One edit wins whole; the other is gone. Not a slower merge, not a
conflict marker — gone. That is a property of the v1 merge model, it applies
identically to every `NoteBody` (task, stream and routine), and it is the reason
"rich text" in v1 means "formatted text that syncs" and not "text two people can
type into at once".

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Build sharing for v1** | A grant model, invite flow, HPKE key distribution to a second identity, multi-subject relay routing, inbound authorization, revocation with key rotation, plus UI on two clients. It is a second epic, and the security review it needs is not compressible. |
| **Ship a minimal "share" that is read-only and unrevocable** | The failure mode of a partial sharing model in an E2EE product is a wrong security guarantee, not a small feature. Revocation is not a v1.1 add-on to a shipped grant. |
| **Leave the sharing rows as MUST and ship v1 without them** | A MUST that visibly did not ship teaches readers the matrix is decorative. Precisely the outcome the hard rules exist to prevent. |
| **Delete `Person` / `Note` / `persons` / `notes` and start clean later** | ADR-0019 deleted the TUI on exactly this reasoning, and it was right there — a *client* nobody runs is a build tax. A struct and an unwritten table are not: they cost nothing to keep, they are the shape the specs describe, and `0013_baseline.sql` is a frozen pre-1.0 baseline ([ADR-0018](./0018-storage-baseline-reset.md)) that should not be churned to remove two unused tables. Kept, with the status banners already on both domain specs. |
| **Wire `sunrise-integrations` into macOS for v1 anyway** | Reverses a recorded decision on issue #4 to make a table entry true. The sequencing argument there has not changed, and the orphan gate already tracks the crate against that issue. |
| **Delete `sunrise-integrations` as the honest form of deferral** | Considered, by analogy to the TUI. Rejected: it is 850 lines of tested, correct provider code whose only defect is having no consumer yet, and the consumer is a named open issue. Quarantine with a staleness check is a stronger guarantee than deletion plus intent. |
| **Demote Notes to SHOULD along with the other two** | Wrong, and the reason matters: the task-body half is being met. Demoting it would understate v1 and hide the real distinction, which is between a note-as-field (ships) and a note-as-entity (does not). |
| **Amend the matrix without an ADR** (chosen against) | A spec edited to agree with the code, by the party that wrote the code, with no recorded reason. This is the move the ledger discipline exists to make impossible. |

## Consequences

* **v1 ships without stream sharing on either client.** The E2EE, multi-device,
  single-identity story is unaffected — device pairing is a different mechanism
  (Noise XX, `sunrise-pairing`) and is not covered by this ADR. What a user
  cannot do in v1 is give another person access to a stream.
* **The `persons` and `notes` tables stay in the baseline schema, still unwritten,
  and that is now recorded rather than merely observable.** Both domain specs
  already carry status banners; this ADR is what they defer *to* for the
  scheduling decision.
* **`sunrise-integrations` stays quarantined by the orphan-crate gate against
  issue #4.** The quarantine list is checked for staleness on every run, so the
  entry cannot outlive the fix.
* **The Notes MUST is now precise.** A reader who asks "does v1 have notes?" gets
  "yes, on tasks, formatted, searchable, and last-writer-wins across devices"
  rather than a yes that quietly includes a standalone entity nothing writes.
* **The matrix's hard rules are intact.** No shipped capability is withdrawn,
  because nothing has shipped. The amendment says so in the file itself, so a
  later reader does not read a pre-1.0 scoping decision as a broken v1.0 promise.
* **[ADR-0019](./0019-swiftui-macos-client.md)'s accounting is unchanged.** It
  noted that four MUSTs were dead ends for a terminal; none of those four are
  these three. This ADR subtracts from the macOS column, which 0019 did not.

## What would force revisiting this

1. **Sharing becoming the reason someone cannot adopt Sunrise.** It is the most
   likely of the three, and the answer is a scoped epic with a written threat
   model reviewed before implementation — not an increment onto v1.
2. **The macOS client consuming the Google provider end to end.** That is the
   stated precondition on issue #4, and it un-blocks the whole external-calendar
   scope (Exchange and CalDAV included) rather than just this row.
3. **A text CRDT entering the workspace.** ADR-0014 names the trigger; if it
   fires, merge-capable note bodies come with it, and the standalone `Note`
   entity should be re-costed at the same time since both turn on the same
   question of whether a body is a mergeable document or a field.

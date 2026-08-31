---
status: accepted
---

# People

A Person represents either a Sunrise identity (someone the user shares with) or a non-Sunrise contact (someone referenced in tasks, like "Mom" or "@carlos").

## Fields

> **Status: the `Person` entity is unreachable in v1.**
> `crates/sunrise-domain/src/person.rs` defines the struct and
> `0013_baseline.sql` creates a `persons` table; there are **zero ops, zero
> commands, zero queries, zero writers** — no `InnerOp` variant, no `Command`,
> no `Query`, no UniFFI surface, and nothing that writes the table.
> `Query::EntityById` refuses `EntityKind::Person`. The only live use of a
> `prs_` reference is `Task.assignee`, which the core carries as an opaque
> label. [ADR-0020](../11-adr/0020-v1-must-demotions.md) §(a) deferred stream
> sharing out of the v1 MUST set, which is why the rows below have no
> implementation to describe, and kept `Person` and `persons` rather than
> deleting them. Everything below the field list is the sharing model as
> designed, not as shipped — see also
> [`../implementation/overview.md`](../implementation/overview.md).

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

```cddl
Person = {
    id:            tstr .regexp "prs_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:    timestamp,
    updated_at:    timestamp,
    display_name:  tstr,                     ; UI label; treat as plaintext
    identity_id?:  entity-ref,               ; idn_… if this Person has a Sunrise identity
    deleted:       bool,
    unknown-fields,                          ; see overview.md
}
```

The wire key is **`identity_id`**, not `linked_identity`. Earlier revisions of
this spec used the latter; the struct has never spelled it that way, so a
reader implementing against the old name would have found the field absent on
every payload.

**`handle`, `avatar`, `notes` and `contact_methods` do not exist**, and neither
does a `ContactMethod` type. They were specified and never modelled. Absent
means absent — a client cannot round-trip a handle by writing one, because
nothing on the wire carries it. (`unknown-fields` would preserve a key a
*newer* build wrote, but no build has ever written these.)

`display_name` is not length-capped in the domain.

## Self

The local user is also a Person. Their `identity_id` is the user's own identity. UI treats self specially in some surfaces (e.g. assignment).

## Linking to a cryptographic identity

A Person becomes a Sunrise *peer* by linking to an `identity_id`. This happens via:

1. **Invite link.** User generates a sharing invite for a Stream; sends the link OOB (Signal, email).
2. **Recipient onboarding.** Recipient signs in / signs up; redeems invite. Their identity public key is exchanged with the sender.
3. **Bidirectional mirror.** Each side adds the other as a Person with `identity_id` set.

After linking:

- Sharing capabilities apply.
- Avatar and display name from the peer's own profile (an opt-in profile sync; defaults to user-set local values).

See [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md) for the cryptographic layer.

## Privacy of People entries

People records are **per-vault** and never sent to the server in plaintext. Linked-identity public keys are sent to the server (encrypted under the user's vault key) only as needed for sharing operations.

## Sharing model summary

| Operation | Granularity | Effect |
|---|---|---|
| Share Stream | Stream + all child entities | Recipient can read/edit per role |
| Unshare | Stream | Future ops not delivered to recipient; recipient retains last-seen state locally (we cannot exfiltrate from their device) |
| Transfer ownership | Stream | New owner becomes responsible; permissions reset; original owner's access becomes "shared with" |

Roles in v1: `viewer`, `editor`. No `commenter` (no comments). No `admin` (no team admin surface).

### Capability matrix

| Capability | viewer | editor |
|---|---|---|
| Read all Tasks/Notes/Blocks/Attachments in the Stream | ✓ | ✓ |
| Decrypt and download Attachments | ✓ | ✓ |
| Export the Stream's content to file | ✓ | ✓ |
| Create / update / delete Tasks, Notes, Blocks | — | ✓ |
| Add or remove Contexts on a Task | — | ✓ |
| Upload new Attachments | — | ✓ |
| Edit Stream metadata (name, color, description) | — | — (owner only) |
| Add or remove sharing peers | — | — (owner only) |
| Trigger Stream-key rotation | — | — (owner only) |
| Archive or delete the Stream | — | — (owner only) |

Ops emitted by a viewer are dropped client-side before transmission; if a viewer's compromised client emits ops anyway, the relay enforces the same boundary by dropping ops whose signing identity does not have `editor` role on the target Stream.

### Permission elevation (viewer → editor)

Elevation is **prospective**: ops the recipient already received as a viewer are not re-evaluated, and future ops are accepted under the new role.

- The recipient retains the (decrypted) viewer-period ops; nothing is replayed.
- If the viewer attempted edit-ops while a viewer (which were dropped client-side), those drops are not undone; the recipient must re-make the edits.

## What sharing is *not*

- Not realtime collaborative editing of a Note like Google Docs. It is a
  single-cursor model with merged saves — and under v1's entity-level LWW
  ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)) that is not a
  prioritisation call but a capability the merge model does not have. Two
  people typing in one body produce one survivor.
- Not a feed/social graph. There is no "who follows whom."

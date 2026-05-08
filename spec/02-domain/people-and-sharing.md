---
status: accepted
---

# People

A Person represents either a Sunrise identity (someone the user shares with) or a non-Sunrise contact (someone referenced in tasks, like "Mom" or "@carlos").

## Fields

```cddl
Person = {
    id:            tstr .regexp "prs_[A-Z0-9]{26}",
    created_at:    tdate,
    updated_at:    tdate,
    display_name:  text<128>,
    handle?:       text<64>,                 ; "@carlos"
    avatar?:       BlobRef,                  ; locally-stored, optional
    notes?:        NoteBody,
    linked_identity?: tstr,                  ; "idn_…" if this Person has a Sunrise identity
    contact_methods: [* ContactMethod],      ; email, phone — locally only
    deleted:       bool,
}

ContactMethod = {kind: "email" / "phone" / "other", value: text}
```

## Self

The local user is also a Person. Their `linked_identity` is the user's own identity. UI treats self specially in some surfaces (e.g. assignment).

## Linking to a cryptographic identity

A Person becomes a Sunrise *peer* by linking to a `linked_identity`. This happens via:

1. **Invite link.** User generates a sharing invite for a Stream; sends the link OOB (Signal, email).
2. **Recipient onboarding.** Recipient signs in / signs up; redeems invite. Their identity public key is exchanged with the sender.
3. **Bidirectional mirror.** Each side adds the other as a Person with `linked_identity` set.

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

## What sharing is *not*

- Not realtime collaborative editing of a Note like Google Docs. (CRDT supports it; UX surface is *not* prioritized for v1 — it's a single-cursor model with merged saves.)
- Not a feed/social graph. There is no "who follows whom."

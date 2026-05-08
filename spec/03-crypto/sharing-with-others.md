---
status: draft
---

# Sharing with Others

A user shares a **Stream** (and all its descendant entities) with one or more other identities. Sharing is selective and revocable.

## Roles

- `viewer` — read-only.
- `editor` — read + propose ops (which on accept become authoritative).

No `admin`, no `commenter`, no per-task ACLs in v1. The Stream is the unit.

## Granting access

1. **Owner finds recipient identity.** Either by handle (server lookup of `idn_…` by user-handle/email) or by an identity public key fingerprint shared OOB.
2. **Owner verifies fingerprint.** UI strongly encourages OOB verification (QR, voice, in-person) for non-trivial shares. Stored as a "verified at" annotation on the recipient Person.
3. **Owner generates a share envelope.**
   - Wraps the Stream's current `StreamKey` under the recipient's `ID_D_pub` via X25519 ECDH + HKDF + AEAD.
   - Includes role and an expiration (optional).
   - Signs with `ID_S_priv`.
4. **Owner publishes the share envelope** as a control op (visible to the server's relay; routed to the recipient).
5. **Recipient receives** the envelope on next sync. UI shows "Stream X shared with you by Y. Accept?"
6. **On accept:** recipient's device decrypts the StreamKey, stores it locally, fetches Stream ops from the relay, decrypts, renders.
7. **On decline:** recipient's device sends a control-op "decline"; owner sees in their UI.

## Revoking access

Owner triggers revoke. This is a Stream key rotation (see [`key-rotation.md`](./key-rotation.md)) that re-wraps the new key for everyone *except* the revoked recipient.

The revoked recipient retains historical decryption ability; this is honestly disclosed in UI.

## What is shared

When sharing a Stream, the recipient gets:

- The Stream entity.
- All Tasks, Routines, Blocks, Notes, Attachments under it.
- Person references *if and only if* those Persons are explicitly opted-in by the owner (default: pseudonymous handle).
- *Not* the owner's other Streams, even those mentioned in Notes — references are scrubbed at egress (see below).

## Egress scrubbing

A Note in a shared Stream may contain `{kind: "ref", ref: "tsk_…"}` pointing to a Task in a *different* Stream. On op egress to a recipient who lacks that Stream:

1. The op is intercepted in the local sync layer before encryption-for-recipient.
2. Cross-stream refs are replaced with `{kind: "redacted", title: "<scrubbed>"}` (the title is also scrubbed unless the owner opts in to share titles).
3. The scrubbed op is encrypted under the recipient's StreamKey.

This is a per-egress operation; the **owner's** own copies are unaffected.

## Cross-server sharing

Both parties may be on different relays (managed cloud + self-host). The sharing protocol is relay-agnostic: the owner publishes the share envelope to *its* relay; the relay forwards to the recipient's relay (federation handshake) or the recipient's device polls in.

> **Open:** Federation between relays is not part of v1. v1 sharing requires both parties to be reachable via the same relay (or via an OOB delivery channel for the share envelope itself).

## Edits by editors

When an editor modifies a shared Stream:

1. Their device produces ops signed by *their* device key, encrypted under the StreamKey they hold.
2. Ops are published to the relay; the owner and other recipients receive them.
3. Ops carry the editor's identity ID; each op's authorship is preserved.

There is no "merge request" model. The CRDT merges directly. A future "review mode" toggle is tracked as v2.

## Privacy implications

- Sharing reveals to the relay the *fact* that two identities share something, plus op counts in the shared Stream.
- The fingerprint-verification UX is the user's defense against a malicious relay substituting a peer's public keys.

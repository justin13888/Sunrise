---
status: draft
---

# Shared Documents (Cross-User)

When user A shares a Stream with user B, that Stream's CRDT doc becomes a *shared document*. Both users' devices (and any future devices they pair) participate in its sync.

## Identity → identity routing

Each Stream has an "owner identity" plus zero or more "share grants" to other identities. The relay routes ops:

- To all of the **owner**'s devices (paired under the owner identity).
- To all of each **grantee**'s devices (paired under that identity).

## Op authorship

Each op carries the *originating device's* identity and device ID. Receivers can attribute "Alice did this" vs "Bob did this." The CRDT doesn't care; the UI does (for activity views, weekly review).

## Per-grantee authorization

A grant carries a role (`viewer` / `editor`):

- Viewer: their device's outbox refuses to emit ops on this Stream.
- Editor: ops are produced freely.

This is enforced **client-side** (their core checks role before emitting). The server *also* checks: it rejects ops on shared Streams from devices whose identity isn't a grantee with editor role. Defense in depth.

## Read-only attestation

Because the server *can* see device IDs and op metadata, it enforces editor-role checks. But it cannot read content. So a malicious grantee viewer running a modified client could in principle emit ops; the server rejects them. Other devices also reject them on signature check (no valid editor cert).

## Egress scrubbing (recap)

When an owner's device prepares an op for sync delivery to a grantee:

- Cross-stream entity references in note content are scrubbed (see [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md)).
- Attachment metadata is included; chunks fetched on demand.

Scrubbing happens at the **encryption-for-grantee** boundary. The owner's own copy retains the unscrubbed data.

## Joining and leaving

- **Join.** Grantee accepts; their devices fetch the encrypted Stream history (or a snapshot) and apply.
- **Leave (revoke).** Owner revokes; rotates StreamKey; future ops are inaccessible to former grantee. Existing data on grantee's devices remains decryptable (we cannot remote-wipe).

## Multi-grantee

A Stream can be shared with many grantees. The grant ops list is a CRDT set; concurrent grants/revokes converge.

## Concurrent edits across users

CRDT handles it identically to multi-device. A merge across users works because each device signs its own ops; signatures verify; ops merge.

## Visibility / privacy edge cases

- Person `@carlos` mentioned in a shared task: included with display name only, not contact methods.
- `assignee` field set to an unshared Person: scrubbed to a placeholder.
- Comment-on-task is a non-feature in v1, so no comment-author exposure question.

## Failure modes

| Failure | Behavior |
|---|---|
| Owner deletes Stream | Tombstone propagated; grantees lose access; their local copies show "deleted by owner." |
| Grantee leaves voluntarily | Owner sees "left." Owner's copy remains intact. |
| Grantee's account is deleted | Server stops delivering ops to their devices; share grant is left dangling; owner UI shows "(deleted account)." |

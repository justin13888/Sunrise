---
status: accepted
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

## Grant state machine

```
            ┌───────┐  recipient receives    ┌─────────┐
   create → │pending│ ───────────────────► │ accepted │
            └───────┘    valid grant         └─────────┘
                │                                │
                │ recipient rejects /             │ owner revokes
                │ expires before accept           │  OR expires
                ▼                                ▼
            ┌───────┐                      ┌─────────┐
            │declined│                     │ revoked │
            └───────┘                      └─────────┘
```

- `pending`: created by owner, not yet seen by recipient. Owner can cancel (transitions to `revoked`).
- `accepted`: recipient has decrypted and applied at least one op under the grant.
- `declined`: recipient explicitly declines (offers a UI). Owner sees the decline.
- `revoked`: owner revoked OR expired. Terminal.

State is on the grant record (`grant.state` field, CRDT LWW Register). Concurrent transitions resolve by the standard LWW rule.

## StreamKey rotation on revoke

- Owner emits `stream_key_rotate` immediately on revoke; new ops use the new epoch.
- Existing recipients receive the new epoch's key in a `share_key_distribute` op (one per remaining recipient, encrypted to their identity).
- The revoked recipient does not receive the new key (server enforces).
- Latency target: rotation completes within 5 s on a healthy connection. Until distribute completes, the owner's device queues new ops locally and emits them once all remaining recipients have a fresh key.

## Read-only attestation

Because the server *can* see device IDs and op metadata, it enforces editor-role checks. But it cannot read content. So a malicious grantee viewer running a modified client could in principle emit ops; the server rejects them. Other devices also reject them on signature check (no valid editor cert).

## Egress scrubbing

When an owner's device prepares an op for sync delivery to a grantee, cross-stream entity references in note content are scrubbed. Attachment metadata is included; chunks are fetched on demand. Scrubbing happens at the **encryption-for-grantee** boundary. The owner's own copy retains the unscrubbed data.

### Scrubbing implementation

Scrubbing happens **per envelope, per cohort**, at op-emit time:

1. Author's device builds the canonical (unscrubbed) op.
2. For each recipient cohort that this op fans out to, compute the cohort's accessible-Stream set.
3. Walk the op's references; replace any reference to an entity in a non-accessible Stream with `{kind: "redacted", reason: "private_ref"}`.
4. CBOR-encode the per-cohort variant; encrypt under the cohort's Stream key.
5. Emit each per-cohort envelope as a separate sub-op in the same OpBatch.

Editors **cannot** create cross-stream references in v1 — the UI prevents it because editors hold no ids for entities outside the shared Stream. Owner scrubs at emit time; there is no editor→owner re-scrubbing path.

### Entity reference format

References inside Note bodies and free-text fields use the canonical form:

```
[<display>](sr://<entity_kind>/<entity_id>)
```

Where:

- `entity_kind` ∈ `{task, stream, context, person, block, attachment, routine}`.
- `entity_id` is the prefixed id from [`../02-domain/identifiers.md`](../02-domain/identifiers.md), URL-safe (no encoding needed; ids are alphanumeric + underscore).
- `display` is the user-typed display text, escaped per markdown rules.

The scrubber recognizes `sr://` URIs and rewrites the URI portion to `redacted:` while preserving display text (or replacing with `(redacted)`).

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

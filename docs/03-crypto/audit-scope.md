# External cryptographic review: scope

**Status:** scoped, not commissioned. Decided by
[ADR-0061](../11-adr/0061-crypto-audit-scope.md).

This is the brief an external reviewer works from, and the record of what came
back. It names what is in scope, what is not, the threat model the review is
read against, the commit it is pinned to, and the rule a finding is held to.
Commissioning the review and receiving its report are acts for a person; this
document is what makes both checkable afterwards.

## What is reviewed

A design review **and** an implementation review: the specification is read
for whether it achieves its goal, and the code for whether it implements the
specification.

| Item | Design of record | Implementation |
|---|---|---|
| Key hierarchy: random per-`(stream_id, epoch)` Stream keys, wrapped under the vault root at rest and distributed by HPKE `key_envelope` | [ADR-0024](../11-adr/0024-key-hierarchy.md), [`identity-and-device-keys.md`](./identity-and-device-keys.md), [`key-rotation.md`](./key-rotation.md) | `crates/sunrise-crypto/src/stream_key.rs`, `crates/sunrise-crypto/src/hpke_seal.rs`, `crates/sunrise-crypto/src/keys.rs` |
| The identity chain: the signed hand-over from one account identity to its successor, its roster and its shares | [ADR-0037](../11-adr/0037-identity-transition.md), [`key-rotation.md`](./key-rotation.md) §Identity rotation | `crates/sunrise-crypto/src/identity_transition.rs`, `crates/sunrise-crypto/src/device_cert.rs`, `crates/sunrise-core/src/engine/identity.rs` |
| The revocation fold: peer-side enforcement re-derived from the op log | [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md), [ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md), [ADR-0058](../11-adr/0058-the-account-identity-is-the-membership-authority.md) | `crates/sunrise-core/src/engine/revocation.rs` |
| `header_sig_v2`: device request signatures over canonical JSON | [ADR-0022](../11-adr/0022-device-signature-canonical-json.md) | `crates/sunrise-http-sig/src/lib.rs` |
| The recovery blob: BIP-39 code, Argon2id stretch, sealed identity payload | [`recovery.md`](./recovery.md) | `crates/sunrise-crypto/src/recovery.rs`, `crates/sunrise-crypto/src/bip39.rs` |
| The `sunrise-crypto` crate as a whole: the op envelope, blob chunks, the Merkle chain, the primitive wrappers | [`primitives.md`](./primitives.md), [`data-encryption-format.md`](./data-encryption-format.md), [`audit-and-tamper-evidence.md`](./audit-and-tamper-evidence.md) | `crates/sunrise-crypto/src/` |

The frozen vectors in `crates/sunrise-crypto-test-vectors/src/lib.rs` are the
byte-exact reference for every wire format above, and are in scope as evidence
rather than as code.

## What is not

- Pairing and onboarding (Noise XX, SAS) and the server's OAuth surface. Both
  are in the standing quarterly scope of
  [`testing.md`](../10-cross-cutting/testing.md) §Quarterly external pen test
  and are not repeated here.
- Platform key storage: the Apple Keychain and the clients' unlock paths.
- Dependencies' own correctness (`ed25519-dalek`, `x25519-dalek`, `hpke`,
  `chacha20poly1305`, `blake3`, `argon2`). Their *use* is in scope; their
  internals are not.

## Threat model

Read against [`../01-architecture/threat-model.md`](../01-architecture/threat-model.md):
in particular A2 (a compromised server), A3 (a compromised device, and what
revoking it bounds) and A5 (a malicious peer). A finding that depends on an
adversary the threat model puts out of scope is still worth reporting, marked
as such.

## Pinned commit

The scope was drawn against `master@83496a14`. The engagement pins its own
commit when it starts, and that commit is recorded here; a finding is read
against the pinned commit, not against whatever `master` has become.

| Engagement | Reviewer | Pinned commit | Report received |
|---|---|---|---|
| — | not yet commissioned | — | — |

## What is asked of the reviewer, specifically

Beyond the general review, four questions the maintainers cannot answer for
themselves:

1. Does the roster and shares commitment in `identity_transition` admit two
   different hand-overs with one signature pair? `roster_digest` was once
   ambiguous in exactly that way; see its documentation.
2. Does the revocation fold converge to one answer on every replica whatever
   order the ops arrive in, and is there an op a revoked device can still emit
   that moves it?
3. HPKE Base mode authenticates no sender. Is every `key_envelope` open
   reachable only after the enclosing envelope's signature has verified?
4. Constant-time comparison is enforced by a name heuristic, not by types
   (below). Is there a secret compared with `==` under a name the heuristic
   does not read?

## Constant-time gate

`.github/scripts/constant-time-gate.sh` rejects a variable-time `==`, `!=`,
`.eq(` or `.ne(` whose operand is named like a MAC, tag, digest, hash,
signature, secret, key, nonce, checksum, token or proof, in `sunrise-crypto`,
`sunrise-pairing`, `sunrise-http-sig` and `sunrise-server::auth`. Every
exception is a line in `.github/scripts/constant-time-allowlist.tsv` with its
reason. Its blind spots, which question 4 above asks the reviewer to search:

- a secret bound to a name without one of those words;
- a comparison split across lines;
- `matches!` and `assert_eq!`, deliberately, since neither is how a verifier
  decides.

## Findings

Each finding is filed as an issue in this repository, stating its severity as
the reviewer rated it, and linked here.

A finding rated **high** or **critical** blocks the next release until its
issue is closed, or until an ADR records why the finding does not apply to
Sunrise. That is the same rule `testing.md` §Quarterly external pen test holds
the standing quarterly test to.

| Finding | Severity | Issue | Status |
|---|---|---|---|
| — | — | — | no report received |

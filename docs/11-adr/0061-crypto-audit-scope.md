# 0061 — An external review audits the key hierarchy, the identity chain and the revocation fold; constant-time comparison is a CI gate; and a pull request that touches the crypto is fuzzed

**Status:** accepted

**Built by** [#365](https://github.com/justin13888/Sunrise/issues/365).

**Depends on** the threat model of record,
[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md), and
on the three designs it puts in scope:
[ADR-0024](./0024-key-hierarchy.md) (random, wrapped Stream keys),
[ADR-0037](./0037-identity-transition.md) (the identity chain) and
[ADR-0041](./0041-peer-side-revocation-is-a-fold.md) (the revocation fold).
**Amends**
[`../10-cross-cutting/testing.md`](../10-cross-cutting/testing.md) §CI shape,
which recorded a per-pull-request fuzz budget as deliberately unwired.

## Context

An account's security now rests on three designs no one outside this
repository has read: the key hierarchy of ADR-0024, the identity chain of
ADR-0037 and the revocation fold of ADR-0041. `testing.md` §Quarterly external
pen test describes a standing quarterly scope in general terms — pairing, the
wire protocol, the crypto suite, server auth — names none of the three, and
records no review as having run.

Two mechanical gaps sit beside that one.

- **Constant-time comparison was a convention.** `subtle::ConstantTimeEq` is
  how `sunrise-crypto` compares an identity id, a content hash or a key
  (`crates/sunrise-crypto/src/keys.rs`,
  `crates/sunrise-crypto/src/blob_chunk.rs`,
  `crates/sunrise-crypto/src/device_cert.rs`,
  `crates/sunrise-crypto/src/identity_transition.rs`), and nothing stopped the
  next `==` on a MAC or a tag. `clippy.toml` banned only clocks and RNGs.
- **Fuzzing never ran on a pull request.** The six `cargo-fuzz` targets ran
  nightly, so a change to a decoder reached `master` before any fuzzer read it,
  and the three newest surfaces — the identity chain, device certs and the
  Stream-key unwrap — had no target at all.

## Decision

### 1. An external cryptographic design and implementation review, scoped in-repo

The scope is [`../03-crypto/audit-scope.md`](../03-crypto/audit-scope.md):
ADR-0024, ADR-0037, ADR-0041, `header_sig_v2`
([ADR-0022](./0022-device-signature-canonical-json.md)), the recovery blob, and
the `sunrise-crypto` crate as a whole, against the threat model above, at a
pinned commit that the document records. The deliverable is a findings report,
and every finding is filed as an issue in this repository and linked from that
document.

### 2. A high or critical finding blocks the next release

A finding the reviewer rates high or critical blocks the next release until its
issue is closed, or until an ADR records why the finding does not apply. That
is the rule `testing.md` already states for the quarterly pen test, extended to
this review, and it is written into `audit-scope.md` so that the scope and the
consequence are read together.

### 3. Constant-time comparison is a grep gate with a justified allowlist

`.github/scripts/constant-time-gate.sh` rejects `==`, `!=`, `.eq(` and `.ne(`
where an operand's name contains `mac`, `tag`, `digest`, `hash`, `sig`,
`secret`, `key`, `nonce`, `checksum`, `token` or `proof`, in
`sunrise-crypto`, `sunrise-pairing`, `sunrise-http-sig` and
`sunrise-server::auth`. A hit is rewritten to `ct_eq` or entered in
`.github/scripts/constant-time-allowlist.tsv` with the exact line and the
reason it is not a timing oracle. An entry whose line has gone fails the gate,
so the allowlist cannot outlive the code it excuses. The `constant-time` CI job
runs it and `constant-time-gate-contract` asserts that a `==` over a
`[u8; 32]` tag in `sunrise-crypto` turns it red.

### 4. A pull request that touches the crypto is fuzzed, for two minutes a target

The `fuzz-pr` CI job runs on a pull request that changes
`crates/sunrise-crypto/`, `crates/sunrise-wire-protocol/`,
`crates/sunrise-http-sig/`, `crates/sunrise-server/src/auth/` or `fuzz/`, and
only on one. It runs the targets those paths reach, for 120 seconds each,
from the committed seeds in `fuzz/seeds/`, on the nightly toolchain pinned in
the job. A failure uploads the reproducer, and the reproducer joins the seed
corpus with its fix. A nightly toolchain that cannot be installed, or cannot
build the harnesses, turns the job's verdict into a warning annotation rather
than a red check, because that is a fact about the toolchain and not about the
change. Three new targets — `identity_transition`, `device_cert` and
`key_envelope` — cover the surfaces that had none.

## Alternatives considered

- **Newtype every secret and tag byte array so `PartialEq` exists only through
  `ct_eq`, and deny raw `[u8; N]` equality in `clippy.toml`.** The issue's
  first preference. The clippy half is not expressible: `disallowed-methods`
  and `disallowed-types` match calls and paths, `==` is an operator clippy
  never resolves to a call it checks, and banning `[u8; N]` as a type would
  reject every id and public key in four crates. The newtype half is sound but
  is a type change across every public signature that carries a key, tag or
  digest — `DeviceCert::sig`, the transition body, the envelope — and so a
  refactor of its own, not a gate. The grep gate is what can land now; newtypes
  remain the better end state and nothing here stands in their way.
- **Fuzz every pull request.** Rejected for the reason `testing.md` gave
  before: two minutes a target over nine targets is a runner-hour a run, spent
  mostly on pull requests that touch no decoder. The path filter keeps the
  budget where the risk is.
- **A floating `nightly`.** Rejected for the pull-request job: a nightly that
  regressed would turn an unrelated change red. The nightly job keeps the
  floating toolchain, which makes it the early warning that the pin needs
  moving.

## Consequences

- An audit is scoped and the scope is reviewable in a diff; commissioning it,
  and receiving its report, are acts for a person, and the scope document says
  so.
- A new variable-time comparison over a secret-named value cannot merge
  without a written reason. One named otherwise still can: the gate reads
  names, not types, and `audit-scope.md` lists that blind spot for the
  reviewer.
- A pull request touching a covered path waits on up to roughly twenty minutes
  of fuzzing, and one touching none waits on nothing.
- The pinned nightly ages. When it has to move, it moves in `ci.yml` in one
  place, and the nightly job's floating toolchain shows whether the move is
  safe.

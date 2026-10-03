# 0057 — Cross-version merges are tested by driving a pinned baseline build out of process

**Status:** accepted

**Built by** [#326](https://github.com/justin13888/Sunrise/issues/326).

**Tests** the invariant [ADR-0042](./0042-v0-forever.md) §2 states and
[ADR-0045](./0045-schema-identity-and-feature-gating.md) designs for: merging a
vault across client versions never breaks and never loses data. The rules it
checks are [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md)
§Compatibility rules.

## Context

Every two-replica test in `crates/sunrise-e2e/tests/` linked one build of
`sunrise-core` into both replicas. `crates/sunrise-storage/src/vault_fixtures.rs`
opens old storage layouts with the current build, but no op ever crossed from
one build to another. So nothing tested the invariant, and a fix for one way of
breaking it (#320, #321, #322) could not be shown to hold.

Testing it needs two builds of one workspace running at once, a way to make the
newer one write what only it can write, and a reference for what both replicas
should hold afterwards. Each of those is a decision, and so is which older
build counts.

## Decision

### 1. The baseline runs in its own process, built inside an extracted copy of itself

`crates/sunrise-e2e/baseline-driver/` is a small binary, not a member of this
workspace. `build-baseline.sh <ref>` extracts the baseline commit with
`git archive` into `target/sunrise-baseline/<ref>/src/`, copies the driver in
as `crates/sunrise-baseline-driver` (which the baseline's own
`members = ["crates/*"]` picks up), and builds it against the baseline's
`Cargo.lock`. The binary is that commit's `sunrise-core` with no `HEAD` code in
it. The test process talks to it one JSON object per line over stdin and
stdout (`crates/sunrise-e2e/src/cross_version/baseline.rs` is the other end).

Rejected: the baseline as a renamed dependency,
`sunrise-core-baseline = { package = "sunrise-core", git = ..., rev = ... }`.
It cannot link. The floor below carries `rusqlite` 0.31 over
`libsqlite3-sys` 0.28, `HEAD` carries 0.40 over 0.38, and both declare
`links = "sqlite3"`, which cargo refuses to put in one graph. It would also
compile the whole older workspace into every `cargo test`, and put a git source
in `Cargo.lock` that `deny.toml`'s `unknown-git = "deny"` refuses.

### 2. The account is created by the baseline, and one of its vaults is upgraded

The driver creates the account the way the baseline creates one: vault B is the
first device, vault A is paired to it with the baseline's own pairing, and B is
closed without ever syncing. The test then opens B with `HEAD`. That open is
the upgrade a user performs, and the first thing checked: a `HEAD` that will
not open the vault is a violation. A stays on the baseline and syncs with B
through the real `HEAD` relay in process. At the end A is closed and reopened by
`HEAD`, which runs ADR-0045 §4's parked-op replay, and both are compared.

The upgrade is the only way two builds share an account that both builds
support. Pairing across builds is not: the baseline's pairing is the baseline's
own protocol.

### 3. The newer build's vocabulary is sealed by the harness, as a real device

`HEAD` relative to any baseline adds little that the baseline cannot read, and
relative to itself adds nothing. So the harness writes what a newer build would:
an unknown top-level field, an unknown enum value, an unknown `SunriseTime` kind
inside `due_at`, and an unknown op kind (`cross_version/future.rs`). Each is a
`HEAD` `Task` with that one thing added, sealed with `seal_envelope` as device
C, which `HEAD` pairs to B and which therefore holds a real cert and the Inbox
key. C writes nothing on the Inbox itself, so the harness owns that sequence.

These ops do not go through the relay. The harness hands each one to A and to B
with `apply_remote`, at points the scenario chooses: to one side now and to the
other at the next settle. That is what puts a newer write on either side of a
concurrent older-build write. A refusal comes back directly, and so does
whether the baseline kept what the op set.

### 4. The reference is computed from the ops, and a control run keeps it honest

The property compares each final projection with what a `HEAD`-only run of the
same ops would hold. A second `HEAD` run does not produce that: which of two
concurrent writes wins depends on hybrid logical clocks, and a second run cannot
reproduce them. `cross_version/model.rs` computes it instead. A settle opens an
epoch. A field's expected value is the last value written in the last epoch that
wrote it, and when two writers wrote it in that epoch, either value is
acceptable. This is the property's own rule: every field a writer set is present
unless a later concurrent write to that field won.

The same property runs `HEAD` against `HEAD` in the ordinary suite. That run may
show only `HEAD`'s own known gap, so a model that disagreed with `HEAD` would
fail every pull request.

### 5. Known violations are expected failures, each naming its issue

`cross_version/gaps.rs` lists every known violation: its issue, the runs it
applies to (every run, or named baselines), and one line on what goes wrong.
The classifier attributes each violation to one issue. A violation the table
expects is reported and passes. Any other violation fails the property. Each
entry also has a fixed scenario that must still produce it, so an entry fails
the suite on the day its defect stops reproducing and has to be removed then.

A fix at `HEAD` does not remove an entry scoped to an older baseline. A build
that has shipped never changes. Its entry goes when the matrix moves past it.

### 6. The baselines are the builds the invariant binds: the ADR-0042 floor and later

ADR-0042 withdrew the pre-release licence to ship a change an older build cannot
read. The first build it binds is the merge of #371, `d9566ade`. The CI matrix
is pinned commits at or after it. Today the only entry is the floor itself, and
it predates the fixes for #320, #321 and #322.

`v0.1.0-rc.1`, the only release tag, is excluded. It predates three
`CRYPTO_SUITE_V` bumps that the withdrawn licence allowed (`version.rs`, 3 to
5). One of them splits the AAD of the wrapped identity, so `HEAD` refuses an
rc.1 vault at open: the harness reports
`UpgradeRefused { who: B, error: "Keychain(VaultRootMismatch)" }` before any op
is exchanged. A run against rc.1 can never get past setup. It would measure
breaks the record already licenses, not the invariant. The same holds for every
older tag.

A release cut at or after the floor joins the matrix. If the driver does not
compile against its API, the pull request that adds the release changes the
driver.

### 7. The baseline half runs off the pull-request path

The `Cross-version merge` job runs on every merge to master, nightly, and on
`gh workflow run ci.yml --ref <branch>`. Like the macOS jobs, it does not run on
pull requests: its cost is a cold build of a second workspace, and it measures
builds other than the one under review. The `HEAD` control runs on every pull
request, inside `Rust (ubuntu-latest)`. A shrunken counterexample is written to
`proptest-regressions/tests/cross_version_convergence.txt` under
`crates/sunrise-e2e/`, the path the proptest-persistence gate expects, and
uploaded when the job fails.

## Alternatives considered

- **A renamed git dependency.** Rejected in §1: two `links = "sqlite3"`
  packages, a second workspace in every test build, and a git source.
- **Two `HEAD` runs as the reference.** Rejected in §4: hybrid logical clocks
  are not reproducible across runs, so the two runs disagree about concurrent
  writes for reasons that have nothing to do with versions.
- **A fake relay at the transport layer**, so the harness could reorder every
  frame. Rejected. It would mean implementing the sync protocol a second time,
  for each baseline's version of it, and testing that copy instead of the relay
  both builds actually talk to. Newer-build ops already reach each side in any
  order through `apply_remote`.
- **`v0.1.0-rc.1` as the baseline.** Rejected in §6.

## Consequences

- The invariant is asserted on every merge to master, against the oldest build
  it binds, and its control on every pull request.
- Adding a field, an enum variant or an op kind now comes with a harness case
  (`schema-versioning.md` §Adding a field, step 6). The four shapes in
  `FutureWrite` are where such a case goes.
- An expected failure cannot outlive its defect, and an unexpected one cannot
  pass.
- The driver is written against the oldest API in the matrix. A baseline whose
  API it does not compile against costs a driver change.

## What would force revisiting

- A release cut at or after the floor. It joins the matrix (§6).
- The relay's own wire protocol changing in a way an older client cannot speak.
  The harness would report it as the baseline no longer syncing. ADR-0009's
  N/N−1 rule is what decides whether that is allowed.
- Per-field merge (#319) landing. The model's reference already is per-field,
  and #319's entry leaves the table.

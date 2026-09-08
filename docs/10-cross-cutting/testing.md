---
status: accepted
---

# Testing Strategy

Test pyramid plus a few specialized layers for what makes Sunrise distinctive.

## Layers

### 1. Unit (broad, fast)

- Pure-function tests in `sunrise-core` for parsers, RRULE expansion, LWW merge, crypto envelopes.
- Run on `cargo test`; <30 seconds in CI.
- Coverage target: 80% lines on the core; 90% on crypto and on the merge path (`sunrise-core::engine::lww_wins` and the materialization it guards). The 90% figure previously named "CRDT modules", which do not exist — `crates/sunrise-crdt` was deleted by [ADR-0014](../11-adr/0014-entity-level-lww-merge.md), so that half of the gate applied to nothing. No Rust coverage tool is wired (CI runs `bun run test:coverage` for the TS side only), so neither target is measured or enforced today.

### 2. Property tests (thinner, deep)

- Merge convergence: random op streams across simulated devices.
- Capture parser fuzz.
- RRULE expansion across DST boundaries.
- Op envelope round-trip (encode/decode/encrypt/decrypt/sign/verify).

#### Counterexample persistence

When a property test fails, proptest shrinks the case and writes the seed to a
persistence file, which every later run replays before generating anything new.
Those files are **committed**: a shrunken counterexample is a test input the
suite discovered by itself, and it is the one output of a property test that
cannot be regenerated on demand. `crates/sunrise-core/proptest-regressions/engine.txt`
is the standing example — two cases from the control-op ordering bug, still
replayed on every `cargo test`.

There is exactly one shape, `<crate>/proptest-regressions/<source path>.txt`.
proptest's default (`FileFailurePersistence::SourceParallel`) produces it only
for proptests under `src/`, because it walks up from the source file looking for
a directory holding `lib.rs` or `main.rs` and an integration test has none above
it; it then prints `failed to find lib.rs or main.rs` and falls back to a flat
`<name>.proptest-regressions` beside the test. Every proptest in `tests/`
therefore sets `failure_persistence` to
`FileFailurePersistence::Direct("proptest-regressions/tests/<name>.txt")`
explicitly — the path is relative to the crate root, which is the working
directory cargo gives a test binary.

`.gitignore` deliberately carries no rule for either shape. The directory shape
is tracked; the flat shape means a proptest is missing that setting, and it
should be visible in `git status` rather than hidden.

`.github/scripts/proptest-persistence-gate.py` enforces all of that
(`mise run proptest-persistence`): every `proptest!` block under a crate's
`tests/` carries a `#![proptest_config(…)]`, the file names the
`proptest-regressions/tests/<name>.txt` derived from its own path, git ignores
neither that path nor an existing counterexample file, and no flat
`<name>.proptest-regressions` sits beside a test. Noticing the untracked file in
`git status` was the previous enforcement, which is the kind this repository has
replaced with a gate everywhere else.

#### Convergence property-test determinism

- **Library**: `proptest` (Rust) — a real dependency used by the property tests. Wire-bytes coverage beyond what proptest reaches comes from the `cargo-fuzz` binaries in `fuzz/` — see [Continuous fuzz targets](#continuous-fuzz-targets) for the target set, the seed corpus and the CI shape, rather than restating them here. The division of labour is the point: a property test generates *valid* structures, a fuzzer generates arbitrary bytes, and the first crash the `rrule` target found was an `INTERVAL` value the property test's `1u32..=3` strategy could never draw.
- **Reproducing a failure**: proptest's own persistence file. When a property test finds a counterexample it writes the case to disk and replays it on every later run, and that is the only reproduction mechanism any property test in this workspace has. **`SUNRISE_FUZZ_SEED` is not one of them** — no proptest reads it and no suite logs a resolved seed, so exporting it changes nothing here. It is the chaos harness's convention and is documented under [Network / chaos tests](#5-network--chaos-tests), where it is implemented and tested. Where those files land is settled in [§2](#2-property-tests-thinner-deep): a `tests/` proptest names its own path under `proptest-regressions/tests/`, the files are tracked rather than ignored, and a flat `<name>.proptest-regressions` beside a test file is the visible symptom of a proptest that forgot to say so.
- **Specified, not built:** one seed convention across both harnesses. The earlier text of this bullet asked every property test to read `SUNRISE_FUZZ_SEED` (hex), to fall back to the first 8 bytes of the workspace `HEAD` hash, and to log the resolved seed in a suite header. Nothing does. Plumbing a resolved seed into `ProptestConfig`'s RNG and logging it would make the sentence true everywhere and leave one reproduction story instead of two; it does not remove the need for the persistence file, because the shrinker still replays a *specific* minimal case. Tracked in [#119](https://github.com/justin13888/Sunrise/issues/119).
- **Volume**: 1 000 random op sequences per CI run; release branches run 100 000 nightly.
- **Assertion**: for every permutation of the same op set across N simulated devices, the final state is byte-identical (canonical CBOR comparison).

### 3. Integration (medium)

- End-to-end inside one process: spin up an in-memory server + N simulated clients; orchestrate scenarios:
  - Multi-device offline / online dance.
  - Sharing accept / edit / revoke.
  - Migration from old schema fixture.
  - Recovery code restore.

### 4. Per-platform UI tests (narrow)

- Critical-path smoke tests on each platform:
  - Cold launch → Today renders.
  - Capture → save → see in Today.
  - Mark done → state persists across relaunch.
- Platform-specific tooling: XCTest on macOS; Espresso and Playwright when Android and Web are scheduled. The CLI needs none — `crates/sunrise-cli/tests/cli.rs` runs the real binary against a real vault, which covers the same three critical paths without a UI harness at all.

### 5. Network / chaos tests

- Toxic-proxy between client and server: drop packets, corrupt bytes, delay, partition.
- Verify clients converge once the partition heals.
- Verify integrity warnings fire on tampered envelopes.
- **Seed**: the harness's RNG seed comes from `SUNRISE_FUZZ_SEED` when set — hex, a leading `0x` forcing hex, and a plain decimal also accepted — and otherwise from the fixed `DEFAULT_FUZZ_SEED` (`0x5352_5f43_4841_4f53`, "SR_CHAOS"), so a run reproduces out of the box without reading git state. `seed_from_env` in `crates/sunrise-e2e/src/chaos/toxic.rs` is the reader, and its unit tests cover hex, `0x`, decimal and absence; `crates/sunrise-e2e/tests/chaos.rs` xors the resolved value with a per-scenario tag so two scenarios never draw the same stream. **This is the only consumer of the variable in the workspace** — it is a chaos-harness convention, not a property-test one.

### 6. Performance tests

- Per-platform benchmark suite (op apply rate, capture latency, search latency).
- Run on representative hardware in CI (a Mac mini, an Android reference device, a Pixel emulator, a Linux runner).
- Regression alerts.

#### Perf-bench CI integration

- Per-platform baselines live in `bench/baseline.json`, committed to the repo.
- The suite runs on `schedule` and `workflow_dispatch` only, never on a pull
  request, and the comparison it prints against that baseline is informational.
- **As specified:** any benchmark regressing >5% blocks merge via a required check, and a nightly job opens a bot PR updating `bench/baseline.json`.
- **As built:** the `bench-regression` job in `ci.yml` runs on `schedule` and
  `workflow_dispatch` only — never on a pull request — and carries
  `continue-on-error: true`. It reports; it does not gate. There is no PR
  comment step and no baseline-updating bot PR.
- **The tolerance it prints against is 60%, not 5%.** The comparison step is
  `cargo run -q -p sunrise-bench --bin baseline -- --check target/criterion bench/baseline.json 60`,
  so even the informational report only flags a move larger than the noise floor
  below. 5% is the figure
  [`performance-budgets.md`](./performance-budgets.md#regression-policy)
  specifies, and it is what a dedicated-hardware gate would tighten to.
- **One platform is measured.** The job runs on `ubuntu-latest`, so
  `bench/baseline.json` holds numbers for `linux-x86_64` and `null` for every
  `darwin-aarch64` metric. §6's representative-hardware list — a Mac mini, an
  Android reference device, a Pixel emulator, a Linux runner — is specification;
  the Linux runner is the part that exists. The file is edited by hand.
- The job's own comment records why: measured on the shared runner, the same
  binary against its own recorded baseline swings +270% (`ws_handshake`) and
  −39% (`submit_create_task`) from scheduling noise alone. A gate that
  red-lights on noise is ignored within a week, and then it protects nothing.
  Promoting it needs dedicated hardware and full-length measurement runs.
- Tracked in [#33](https://github.com/justin13888/Sunrise/issues/33).

### 7. Privacy / safety tests

- Static checks: any code path under `telemetry/` or `logging/` that calls `.expose()` on a `Plain<T>` fails the build.
- Server-side: no log statement contains task IDs from a fixture vault (regex check on test logs).

## Test data

- Synthetic vault generator that produces known-shape data (N tasks, N streams, recurring routines).
- Fixture vaults for migration tests.

## CI matrix

- Per PR: Rust core unit + property + integration; web + desktop UI smoke.
- Nightly: full mobile UI on real-device simulators; chaos suite; performance benchmarks.
- Per release: manual a11y; manual cross-platform pairing flow.

## Coverage and what we don't measure

- We don't measure UI coverage by lines (false-precision).
- We measure UI by:
  - Critical-path scenarios passing.
  - Performance budgets met.
  - A11y audit clean.

## Mutation testing

`cargo test` answers "do the tests pass". It cannot answer "would the tests
notice if the code were wrong", and the two come apart quietly. The first
`cargo-mutants` run against this workspace made the point in six mutants:

```
MISSED crates/sunrise-sync/src/backoff.rs:61:37:
       replace * with + in Backoff::next_delay
```

`Backoff::next_delay` could add where it multiplies and the whole suite stayed
green.

### Scope

Four crates: `sunrise-core`, `sunrise-crypto`, `sunrise-sync`, `sunrise-domain`.
The scope is enforced by the `-p <crate>` in `mise.toml`'s `mutants` task and in
`ci.yml`'s matrix, and nowhere else.

`.cargo/mutants.toml`'s `exclude_globs` names five crates it would be actively
wrong to score, and those exclusions are reasoned rather than convenient — the
UniFFI seam (`sunrise-core-bindings`) is half Swift and cannot be scored
honestly from one side; `sunrise-bench` is benchmarks, not correctness code;
`sunrise-e2e` is a harness, and mutating a harness measures the harness's own
tests; `sunrise-relay-client` is generated by `build.rs`;
`sunrise-crypto-test-vectors` is frozen literals, where flipping a constant
tests the test.

That is five of twenty-three, which leaves fourteen crates neither scoped nor
excluded — `sunrise-server`, `sunrise-storage`, `sunrise-auth` and the rest.
The config does not keep them out; the `-p` does. A bare `cargo mutants` with
no `-p` mutates all fourteen, takes far longer than the numbers below, and
produces a run the gate fails with "no floor recorded" for each of them. Run it
through `mise run mutants <crate>`. That task does not check the name against
the four either — the argument goes straight to `-p` — so
`mise run mutants sunrise-storage` mutates an unscoped crate and the gate fails
that run the same way.

### Cost, measured

| Crate | mutants | `cargo test -p` |
|---|---:|---:|
| `sunrise-domain` | 1 352 | 8.8 s |
| `sunrise-core` | 740 | 22.9 s |
| `sunrise-crypto` | 296 | 11.4 s |
| `sunrise-sync` | 94 | 9.1 s |

At `--jobs 1`: **604 MB peak RSS** — about one `cargo build` — and roughly
1.8 s per mutant on `sunrise-sync`. Each additional job is another copy of the
source tree on disk and another resident rustc, which is why `mise run mutants`
pins one and says so. A full pass over all four is hours, which is why it runs
nightly and sharded rather than on a pull request.

`sunrise-crypto` needs the generous `timeout_multiplier`: it runs Argon2id at
production parameters (64 MiB, t=3, p=1), deliberately not weakened for tests,
and every surviving mutant pays that cost again.

### The gate

`.github/scripts/mutants-gate.py` scores a run against `mutants/baseline.json`
and fails on a drop. Caught rate is `caught / (caught + missed + timeout)`:
timeouts sit in the denominator and not the numerator, so a mutant that hung is
not scored as one a test refuted. Unviable mutants — ones that do not compile —
are excluded from both sides, being an artifact of mutating typed code rather
than a statement about the tests.

Which exit code means "scored, and the answer is no" and which means "nothing
was scored, do not read a verdict into this" is the contract `ci.yml` and the
gate's own printed remedies are built on, so it is asserted rather than
described: `.github/scripts/test_mutants_gate.py` synthesises its own outcomes
files and checks the code for every route in about a second. `mise run
mutants-gate-test` locally, and the `Mutation gate contract` job in CI, which
carries no schedule condition and so runs on every push, pull request and
nightly alike — the only part of mutation testing that does not wait for 04:00.

**≥ 90 % caught is the release sign-off requirement, and the baseline is what
climbs toward it.** The two are deliberately separate. A gate that failed from
day one would be switched off within a week, and then it would protect nothing —
the same failure mode this document records for the bench gate below. So the
floor ratchets: a run may hold or improve it and may not fall below it, and
moving it up is an explicit `mise run mutants-baseline --expect-shards
sunrise-sync=1` — the counts are required, see "Running it" below — in a commit
that says what was added to earn it.

**When it runs, and what that costs.** The `mutants` and `mutants-gate` jobs in
`ci.yml` are `schedule` (04:00 UTC) and `workflow_dispatch` only — never on a
pull request, because a full pass is hours and no pull request waits that long.
So this is a nightly ratchet on the trunk, not a merge gate: a pull request that
deletes the test pinning `Backoff::next_delay` merges green, and the gate says
so the next morning, against a trunk that already contains it. That is a
deliberate trade — a check nobody can wait for is a check that gets bypassed —
but it means the floor is a detector with up to a day of lag, not a barrier. To
score a branch before merging it rather than after, run the job on demand:

```bash
gh workflow run ci.yml --ref <branch>
```

### Running it

```bash
mise run mutants sunrise-sync                 # one crate
mise run mutants sunrise-domain --shard 0/6   # one slice of a big one

# record a floor, naming what it is meant to cover
mise run mutants-baseline --expect-shards sunrise-sync=1
```

`--expect-shards` is required when recording, because the task scores every
run sitting under `out/mutants/` and what is sitting there is whatever you last
ran. One shard of `sunrise-domain` banked unchecked becomes that crate's floor:
a rate measured over a sixth of its mutants, recorded as though it covered all
of them, and thereafter too low to fail on anything. For a deliberately partial
floor, call `.github/scripts/mutants-gate.py … --update --allow-partial`, which
skips the check and says so.

Shards are zero-based: a crate split six ways is `0/6` through `5/6`, and
`cargo mutants` rejects `6/6`. Each invocation writes its own directory —
`out/mutants/sunrise-domain-0-6/` and so on — because `cargo mutants` always
puts `mutants.out` directly under the directory it is given, so a shared one
means every shard overwrites the last. Surviving mutants land in that run's
`mutants.out/missed.txt`; `mise run mutants-baseline --expect-shards
sunrise-domain=6` scores every run under `out/mutants/` together, so a crate
covered in six local shards records one floor rather than six — and a stale
directory from an earlier crate is scored with them, which is what the gate's
"undeclared crate" failure means when it names one you did not ask for.

It is deliberately **not** in `lefthook.yaml`: the pre-push hook already runs
the whole Rust suite, and adding hours to a push is how a hook gets bypassed.

## Release gates

- All CI green.
- No P1 a11y regression.
- No new unfamiliar crash (per crash reporting baseline).
- Performance regression ≤5% on every benchmark.
- Sync convergence test: green across the version pair (current vs N-1).

## Security testing

Security testing is a first-class layer alongside unit and property tests. It has three pillars:

### Continuous fuzz targets

`cargo-fuzz` harnesses live in `fuzz/`, one binary per target. **All six of the
v1 target set are built and run.** Each drives a workspace crate through its
ordinary public API — nothing was widened to `pub` for the fuzzer's benefit,
because a surface only a fuzzer can reach is one no attacker reaches either.

| Target | Scope | What it asserts beyond "does not panic" |
|---|---|---|
| `op_envelope` | `sunrise-crypto` envelope decode + signature verify path. | Re-signing a decoded envelope yields a signature this build verifies, and changes no field but `sig`. |
| `wire_frame` | `sunrise-wire-protocol` framing + magic-prefix parser, and the canonical-CBOR payload codec behind each `MsgKind`. | Header and payload survive an encode/decode round trip, and the payload is exactly `decompressed_len` bytes long. |
| `rrule` | RRULE parser and DST-aware expansion (`sunrise-domain`). | `to_rfc5545` round-trips and is idempotent; `expand` stays inside its window and honours `COUNT`, across four zones including a 30-minute DST shift and a 12:45 base offset. |
| `ical` | inbound iCalendar feed parser (`sunrise-integrations`). | `ical::write` is a fixed point over `ical::parse`, and re-parsing its own output raises no notices. |
| `oauth_state` | bearer-token verification in `sunrise-server::auth`: JWT header parse, JWKS resolution, algorithm pinning, claim checks. | Verification never *succeeds*. No seed carries a private key, so an `Ok` is an accepted forgery — including the `HS256`-signed-with-the-public-key and `alg: none` classics. |
| `recovery_blob` | recovery-blob decode + KDF input validation (`sunrise-crypto`). | A payload that comes back is bound to the identity the caller demanded. |

One scope has been corrected against the original specification. `oauth_state`
was written as "OAuth/PKCE state-machine transitions in `sunrise-server::auth`",
and those are two different things: the PKCE and `state` exchange is a *client*
concern, implemented in `crates/sunrise-auth/src/login.rs`, where
`parse_redirect` is private and reachable only through a real loopback
listener. What `sunrise-server::auth` owns is the other half of the same trust
decision — a bearer token from an unauthenticated caller plus a discovery
document and a JWKS from a remote issuer — and that is what the target drives.

#### Running them

`cargo-fuzz` needs a **nightly** toolchain for `-Zsanitizer=address`, and this
workspace pins 1.91.1 (ADR-0026) with CI asserting that four files agree about
it. `fuzz/` is therefore its own cargo workspace, excluded from the root one
for the same class of reason `tools/uniffi-bindgen` is; `fuzz/Cargo.toml`
carries the argument. Nothing in `fuzz/` enters the root `Cargo.lock` and
nothing there is built by an ordinary workspace command.

```sh
cargo install cargo-fuzz --locked   # once
mise run fuzz-build                 # compile all six
mise run fuzz-smoke                 # 10s each against the committed seeds
mise run fuzz op_envelope 3600      # one target, one hour
```

`SUNRISE_FUZZ_TOOLCHAIN` pins the nightly when a run has to be reproducible;
`SUNRISE_FUZZ_SECONDS` raises the smoke budget. Neither is
[`SUNRISE_FUZZ_SEED`](#5-network--chaos-tests), which remains the chaos
harness's variable and its only consumer in the workspace.

Being a separate workspace has a cost worth stating rather than leaving to be
discovered. `mise run rust-clippy` and `mise run rust-doc` are `--workspace`
commands, so **neither reaches `fuzz/`** — the six harnesses are not lint-gated
and not rustdoc-gated. `mise run rust-fmt-check` does reach them, because it
names the manifest rather than the workspace. What holds the rest is that each
file is short, that `mise run fuzz-build` fails on anything the compiler
rejects, and that `crates/sunrise-log/tests/event_catalog.rs` reads all six as
a `BUILD_TOOLS` entry and refuses one that names `tracing` or grows a `mod`.

#### Seed corpus

`fuzz/seeds/<target>/` is **tracked**; `fuzz/corpus/`, `fuzz/artifacts/` and
`fuzz/target/` are ignored. The split matters: an empty corpus means a fuzzer
spends its whole budget rediscovering a six-byte magic prefix, and a corpus
that libFuzzer writes into is not something a reviewer can read a diff of.
Every seed comes from something the tree already had:

| Target | Seeds | Provenance |
|---|---|---|
| `op_envelope` | `signed_only`, `sealed` | The two frozen envelope vectors in `crates/sunrise-crypto-test-vectors` — `signed_only_envelope::ENCODED` and `sealed_envelope::ENCODED`, byte for byte. The harness's fixed device key is that crate's `DEVICE_SIGNING_SECRET`, so the seeds verify rather than merely decode. |
| `wire_frame` | `ping`, `ack`, `subscribe`, `stream_update_caught_up`, `close`, `op_batch`, `op_batch_zstd` | `encode_frame` output for each `MsgKind` that has a canonical payload codec, over the same `STREAM_ID` / `DEVICE_ID` the crypto vectors use. `op_batch` carries both frozen envelopes as its `ops`; `op_batch_zstd` is the same batch with the compression bit set, and is the only seed that reaches the decompression path and its bomb caps at all. |
| `rrule` | nine rule bodies | Every distinct valid `RRULE` the workspace's own tests and `.ics` fixtures use, plus `regression_interval_overflow` (below). |
| `ical` | `apple.ics`, `google.ics`, `fastmail.ics`, `outlook.ics` | `crates/sunrise-integrations/testdata/<vendor>/basic.ics`, unmodified. Four vendors fold, escape and time-zone their output differently, so the fuzzer starts from four shapes of line folding rather than one. |
| `oauth_state` | `rs256_full`, `no_keys`, `symmetric_jwks` | Hand-built `<bearer>\0<discovery>\0<JWKS>` triples: a well-formed RS256 token against a 2048-bit RSA key set, the same token against an empty key set, and the same token against an `oct` key set — the algorithm-confusion branch. |
| `recovery_blob` | `sealed` | `seal_recovery_blob` output for a fixed seed, identity and CSPRNG state; the harness unseals against the same constants. |

Any new crash a target finds opens a P1 bug, and the minimized input joins the
seed corpus. That has already happened once, on the first run:
`fuzz/seeds/rrule/regression_interval_overflow` is
`FREQ=DAILY;INTERVAL=700017975`, which aborted the process inside
`routine_gen::expand` — jiff's `Span::new().days(n)` panics outside
±7,304,484 and the guard around it was a `checked_add` that never ran. Fixed in
`crates/sunrise-domain/src/routine_gen.rs` with a regression test beside the
golden DST cases. The property test in the same file could not have found it:
it samples `interval in 1u32..=3`.

#### CI shape

The earlier text of this section said the targets "run on every CI build (short
budget) and nightly (long budget)". Half of that is now wired and half is
withdrawn:

- **Nightly — wired.** `.github/workflows/ci.yml`'s `fuzz` job, one matrix leg
  per target, 30 minutes each, gated to `schedule` and `workflow_dispatch`
  exactly like the `mutants` job. A finding uploads its reproducer as an
  artifact. **No run of it has ever completed**: GitHub Actions on this
  repository is billing-blocked and every job finishes in ~6 seconds having
  executed zero steps, so its timeout is derived rather than measured and the
  job's comment says so.
- **Short budget on every CI build — deliberately not wired.** Two independent
  reasons. It needs a nightly rustc, which would put a pull request's verdict
  at the mercy of a toolchain this repository does not pin and cannot assert;
  and a budget short enough for a pull request explores nothing the committed
  seed corpus does not already contain, so it would spend three runner-hours
  a day to re-derive a file that is already in the diff. What a pull request
  needs from `fuzz/` is that the harnesses still compile against the crates
  they drive, and that is `mise run fuzz-build` — cheap, but still a nightly
  toolchain, so it is a local gate rather than a CI job until the billing
  block lifts and someone can watch one run.

### Quarterly external pen test

A scoped external penetration test runs once per quarter. The standing scope covers: pairing/onboarding (Noise-XX), sync wire protocol (auth, replay, downgrade), crypto suite (envelope tampering, recovery-blob misuse), server auth surface (`sunrise-server::auth`), and integration OAuth flows. Findings are tracked in the same issue tracker as internal bugs; high/critical findings block the next minor release.

### Security-review gate

Changes to any of these modules require a security-focused review (a reviewer from the security-reviewers group) in addition to the normal code review:

- `sunrise-crypto`
- `sunrise-sync`
- `sunrise-server::auth`
- `sunrise-storage::migrations`
- pairing/onboarding code paths in `sunrise-core`

CI enforces the gate via a `CODEOWNERS` rule on these directories.

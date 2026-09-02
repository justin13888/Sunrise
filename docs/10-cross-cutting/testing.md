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

#### Convergence property-test determinism

- **Library**: `proptest` (Rust). Fuzz targets that need wire-bytes coverage are `cargo-fuzz` binaries in `fuzz/`: `op_envelope`, `wire_frame`, `rrule`, `ical`, `oauth_state`, `recovery_blob`.
- **Seed**: read from `SUNRISE_FUZZ_SEED` (hex) when set; otherwise default to the first 8 bytes of the workspace `HEAD` commit hash. Every CI run logs the resolved seed in the suite header so a failing run is reproducible by re-export.
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
through `mise run mutants <crate>`.

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

**≥ 90 % caught is the release sign-off requirement, and the baseline is what
climbs toward it.** The two are deliberately separate. A gate that failed from
day one would be switched off within a week, and then it would protect nothing —
the same failure mode this document records for the bench gate below. So the
floor ratchets: a run may hold or improve it and may not fall below it, and
moving it up is an explicit `mise run mutants-baseline` in a commit that says
what was added to earn it.

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
`mutants.out/missed.txt`; `mise run mutants-baseline` scores every run under
`out/mutants/` together, so a crate covered in six local shards records one
floor rather than six.

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

> **Specified, not built.** There is no `fuzz/` directory in this repository,
> no `cargo-fuzz` dependency, and no fuzz job in CI. The table below is the
> target set, not an inventory — tracked in
> [#32](https://github.com/justin13888/Sunrise/issues/32).

`cargo-fuzz` binaries will live in `fuzz/` and run on every CI build (short budget) and nightly (long budget). The v1 target set:

| Target | Scope |
|---|---|
| `op_envelope` | `sunrise-crypto` envelope decode + signature verify path. |
| `wire_frame` | `sunrise-sync` framing + magic-prefix parser. |
| `rrule` | RRULE parser and DST-aware expansion. |
| `ical` | inbound iCalendar feed parser (`sunrise-integrations`). |
| `oauth_state` | OAuth/PKCE state-machine transitions in `sunrise-server::auth`. |
| `recovery_blob` | recovery-blob decode + KDF input validation. |

Any new crash discovered by a fuzz target opens a P1 bug and the corresponding minimized input is added to the seed corpus.

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

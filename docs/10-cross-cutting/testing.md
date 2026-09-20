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
cannot be regenerated on demand.
`crates/sunrise-core/proptest-regressions/engine/tests.txt` is the standing
example — two cases from the control-op ordering bug, still replayed on every
`cargo test`.

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
- **Reproducing a failure**: two mechanisms, and they answer different questions. proptest's own **persistence file** replays the shrunken counterexample — the specific minimal case a failing CI run hands you, which no seed can regenerate once a strategy changes. **`SUNRISE_FUZZ_SEED`** replays the *run*: the same cases, in the same order. Where the persistence files land is settled in [§2](#2-property-tests-thinner-deep): a `tests/` proptest names its own path under `proptest-regressions/tests/`, the files are tracked rather than ignored, and a flat `<name>.proptest-regressions` beside a test file is the visible symptom of a proptest that forgot to say so.
- **Seed**: `crates/sunrise-test-seed` is the workspace's single reader of `SUNRISE_FUZZ_SEED`, and every `proptest!` block plumbs its answer into `ProptestConfig::rng_seed`:

    ```rust
    rng_seed: sunrise_test_seed::proptest_rng_seed(),
    ```

  Precedence is proptest's own `PROPTEST_RNG_SEED` first (its dial must keep working), then `SUNRISE_FUZZ_SEED`, then a fresh random seed drawn once for the process. The last of those is proptest's own default behaviour and is kept deliberately: pinning a constant would mean every CI run forever explores the same cases out of an infinite space. What changes is that the seed is no longer secret — every configured block **announces** the resolved value on stderr, which `cargo test` shows you for exactly the tests that failed:

    ```text
    proptest: RNG seed 0x00000000deadbeef — reproduce with SUNRISE_FUZZ_SEED=0x00000000deadbeef
    ```

  The chaos harness announces through the same function, so one grep over a CI log finds the reproduction for either half of the suite. This does not remove the need for the persistence file: a seed reproduces the run, the file replays the minimal case. Closed [#119](https://github.com/justin13888/Sunrise/issues/119).
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
- **Seed**: the harness's RNG seed comes from `SUNRISE_FUZZ_SEED` when set — hex, a leading `0x` forcing hex, and a plain decimal also accepted — and otherwise from the fixed `DEFAULT_FUZZ_SEED` (`0x5352_5f43_4841_4f53`, "SR_CHAOS"), so a chaos run reproduces out of the box without reading git state. `sunrise_test_seed::seed_from_env` is the reader, re-exported on `sunrise_e2e::chaos` where callers already name it, and its unit tests cover hex, `0x`, decimal and absence; `crates/sunrise-e2e/tests/chaos.rs` xors the resolved value with a per-scenario tag so two scenarios never draw the same stream, and `Toxic::new` announces the base value. **The variable is workspace-wide** — the property tests read the same one (see [§2](#convergence-property-test-determinism)) — but the two harnesses fall back differently when it is unset, because a chaos run wants the same fault schedule twice and a property run wants a wider search.

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

### Features

**Every mutation run passes `--all-features`, and a run that does not is not a
coverage measurement.**

cargo-mutants mutates the *source file*. cargo decides whether that file is
compiled. A module behind a non-default feature is therefore mutated and then
not built: the mutant changes nothing, the suite passes, and the mutant is
recorded MISSED — which in `outcomes.json` is the same value a mutant gets when
a test genuinely failed to kill it. The two are indistinguishable downstream,
so the gate scores the feature flag as though it were a test gap.

This was not hypothetical. `sunrise-sync` puts its SSE + POST client transport
behind a non-default `sse` feature, and neither `mise run mutants` nor `ci.yml`
enabled it. At `1d4b484`, on one tree and one 135-mutant population, with the
flag the only difference:

| | caught | missed | timeout | unviable | caught_pct |
|---|---:|---:|---:|---:|---:|
| without `--all-features` | 26 | 104 | 1 | 4 | 19.85% |
| with `--all-features` | 45 | 76 | 1 | 13 | 36.89% |

All 94 of `src/sse.rs`'s mutants missed — 94 of 94 — while the crate's own ten
`sse` tests, including the one asserting that every transport operation carries
a device binding, never compiled. Seventeen points of the crate's score were a
build configuration, and #193 read them as untested security code.

Why it went unnoticed for so long is worth keeping, because it is the part that
generalises. `cargo test --workspace` compiles `src/sse.rs` and runs all ten of
those tests — `sunrise-cli`, `sunrise-e2e`, `sunrise-bench` and
`sunrise-core-bindings` each depend on `sunrise-sync` with `features = ["sse"]`,
and Cargo unifies features across a workspace build. cargo-mutants does not do a
workspace build: it builds `-p sunrise-sync` alone, where nothing asks for the
feature and the default is off. So the ordinary suite and the mutation run
disagreed about which files existed, and only the mutation run was wrong.

`sunrise-sync`'s `sse` is the only feature any of the four scoped crates has
today, so the flag is a no-op for the other three. It is passed unconditionally
anyway, because the failure it prevents is silent: the next feature-gated module
would otherwise start under-reporting with nothing to say so.

That leaves the rule written in three places — the `mutants` task in
`mise.toml`, the `mutants` matrix in `.github/workflows/ci.yml`, and the
sentence in bold at the top of this section — with nothing keeping them in step.
`.github/scripts/mutants-flags-gate.py` now enforces the executable copies: it
joins shell continuations, splits each joined line into the commands it actually
runs, drops comments, and fails unless every `cargo mutants` invocation carries
`--all-features`. It fails separately, with a different exit code, when no file
it read holds an invocation at all, because a gate reporting green on a matrix
that no longer runs cargo-mutants is reporting on nothing.

Four details in that are load-bearing, and every one of them was established by
defeating an earlier version of the gate against real copies of these files.

The unit is an **invocation, not a line**. Shell puts several commands on one
line, so asking whether `--all-features` appears anywhere in a line is satisfied
by any of them. `cargo mutants --list -p X --all-features > population.txt &&
cargo mutants -p X --jobs 1` is one line, two invocations, a legitimate flag on
the one that measures nothing and no flag on the one that produces the floor —
and it reported exit 0. So does a trailing `# dropped --all-features
temporarily`, and so does a preceding `echo "we run with --all-features" && …`.
The gate lexes each joined line **once**, splits it at the separators `&&`,
`||`, `&`, `;` and `|` that the lexer finds unquoted, applies one comment rule
(a `#` that starts a word), and reads every `cargo mutants` pair in each command
as an invocation of its own, ending where the next one begins.

A **command substitution is a command too**. `$( … )` and backticks open a
nested context whose words are its own, and the enclosing command resumes after
the close with the substitution standing in it as one opaque word. Until it did,
`cargo mutants -p $(cargo metadata --all-features --no-deps --format-version 1)
--jobs 1` was exit 0 on the strength of a flag belonging to `cargo metadata`,
and its control — the same line without that flag — was exit 1; an unclosed
`$(` was not noticed at all. Both halves of the shape matter. The substitution
has to stay a word, because deleting it vacated the argument position it held
and made `-p $(…) --all-features` red on a correct tree; and the words inside it
have to stay a command, because a `cargo mutants` written inside `$( )` really
runs and dropping it would lose it from the count.

Lexing once is the load-bearing half of that sentence. Splitting the raw line
and then lexing the pieces put two quote rules in one file, and a single
backslash-escaped double quote — `--output "out/\"$slug"`, which is ordinary,
and which `mise.toml` already writes in the `fuzz-build` task's `run = "…"` — was
enough to desynchronise them. After the desync the splitter believed it was
inside a quotation to end of line, so it stopped splitting on `&&` and stopped
honouring `#`, while the lexer read the same text as words of the invocation:
three shapes reported green with the measuring invocation unflagged, one of them
the trailing-comment hole above restored verbatim. The mirror direction, an
escaped quote inside a legitimate `--exclude-re`, was exit 2 on a correct tree.
Two lexical rules for one thing are wrong in both directions at once, which is
the same argument that had already collapsed the two comment rules into one.

What satisfies the test is the flag **as written, before any `--`**. Three more
shapes carried the gate without carrying the feature selection: a second
invocation inside a single command, because only the first pair was read;
`--exclude-re '--all-features'`, because lexing threw the quotes away and a
regex that mentions the flag lexed to the flag; and `-- --all-features`, which
is an argument to `cargo test` and says nothing about what cargo-mutants built.
The two halves of that rule point in opposite directions, so only one of them is
about source text. The flag counts when a word is written `--all-features`, or
is that text under one consistent pair of quotes — `-p X "--all-features"` is a
feature selection somebody quoted, and going red on it is how a gate gets
switched off in a week. It does not count when it sits **where an option's value
sits**, and that test is decided by a named set of the cargo-mutants options
that take a value, applied before either source-text test so that
`--exclude-re --all-features` and `--exclude-re '--all-features'` get the same
verdict. Deciding it from the predecessor's *shape* instead — "starts with
`-`" — enforced the rule in one spelling of two and could not tell a boolean
switch from an option with a value, so `--no-times "--all-features"` was red on
a tree whose feature selection is genuinely present. The set is a whitelist on
purpose: an option-shaped word that is not in it lets the flag count, so a set
that goes stale against a future cargo-mutants costs a false green on an oddly
written tree and never a false red on a correct one. The `--`
terminator is recognised by its **value**: `"--"` and `\--` are the separator as
far as the shell is concerned, and matching its source text meant quoting it
turned the passthrough guard off while cargo-mutants still received the `--`.
A line that mentions cargo-mutants and will not lex at all — an unbalanced
quote — is exit 2 rather than a lenient reading, because the lenient reading let
the words of a trailing comment stand in for the command's own.

The file set is **`mise.toml` plus every `*.yml`, `*.yaml` and `*.sh` under
`.github/`**, at any depth, together with an **expected invocation count**. A
third executable copy was invisible to a gate that knew about two files; after
the set became `.github/workflows/*.yml` it was still invisible in
`.github/actions/rust-checks/action.yml`, which has eight `run:` steps, and in a
script under `.github/scripts/`. The count is there because the set being
discovered is what makes "no invocation anywhere" judgeable over the union —
moving the matrix from one workflow to another leaves a tree that is entirely in
step, and calling that a broken gate is how a gate gets switched off — and a
union cannot see a count fall from two to one. Move the matrix out of everything
globbed and `mise.toml`'s invocation keeps the union non-empty, so only a number
notices. It lives at `EXPECTED_INVOCATIONS` in the gate, and it is an
**equality**, not a floor: a floor is silent in the direction a tree actually
moves in. Add a third invocation and a floor of two needs no edit, so the number
stops describing the tree while the gate stays green — and from three, deleting
the matrix invocation outright lands back on the floor and is still green. Both
were executed. Requiring the number to match is what makes "adding or removing
an invocation means editing it in the same change" true rather than merely
written here. What the count cannot tell you is whether an invocation is one
anything runs: two in a script nothing references satisfy it exactly as two in
the matrix do, and deciding otherwise would need the gate to know what CI
executes, which is a different tool.

The count is asserted against this repository in the contract test, not only by
the live job. `Mutation flag gate` is not a required check, so a number that had
stopped describing the tree would otherwise have had nothing blocking to say so.

`.github/scripts/test_mutants_flags_gate.py` asserts that contract against
synthesised files, so watching the gate go red never requires editing the two
real ones. Both run as `Mutation flag gate` and `Mutation flag gate contract`,
on every pull request rather than on the nightly — the divergence is introduced
in a pull request, and the `mutants` matrix that would eventually notice it does
not report until 04:00 the next morning, by which time a floor has already been
compared against a differently-measured population.

What it deliberately does not check is this paragraph and the one above it.
Asserting a sentence checks the wording, not the rule, so the prose copy stays a
copy; the gate names this file in its failure output instead, so whoever is
changing the flags is told the third copy exists.

### Cost, measured

| Crate | mutants | `cargo test -p`, rebuilt (2026-09-07) |
|---|---:|---:|
| `sunrise-domain` | 1 356 | 4.9 s |
| `sunrise-core` | 1 259 | 14.0 s |
| `sunrise-crypto` | 519 | 2.7 s |
| `sunrise-sync` | 135 | 1.2 s |

Both columns are reproducible, and the left one is cheap enough that there is no
excuse for it being wrong:

```
cargo mutants --list -p <crate> --all-features | wc -l
```

The left column was re-taken at `1d4b484`; it had drifted on every row, by 28%
on `sunrise-core`, which had just gained `src/blob_fetch.rs`. The right column
is the original measurement, taken 2026-09-07, and was **not** re-taken with it
— which is why its heading carries that date. The two columns are from
different commits, and the right one is a cost estimate rather than a number
anything checks.

`--all-features` changes nothing about this command's output. `--list` returns
an identical population with and without it for all four crates today: three
have no features at all, and it makes no difference to `sunrise-sync` either,
because discovery mutates the *source file* and ignores the gate — which is
§Features' whole mechanism, a gated module mutated and then not built. The
§Features table above is the measurement: both of its rows are the same
135-mutant population, and `.cargo/mutants.toml` records `sunrise-sync` at 135
either way. The flag rides in this command only so the listing invocation
matches the campaign invocation, which is where it is load-bearing — see
§Features above.

`--list` parses the crate and prints one line per mutant **without building
anything**, so all four counts take seconds. The right column is
`cargo clean -p <crate> && cargo test -p <crate>` — the crate and its test
binaries rebuilt against already-built dependencies, wall clock, which is the
shape of the work `cargo-mutants` repeats once per mutant. Two runs agreed to
within 7%. It is *not* a from-cold figure and it is not the per-mutant cost:
`cargo-mutants` copies the whole source tree per job and builds inside the copy.
An earlier version of this table gave the column no definition at all, which is
why its numbers could not be checked and drifted by up to a third before anyone
noticed.

At `--jobs 1`: **604 MB peak RSS** — about one `cargo build`. (That one is the
original measurement and was not re-taken.)

Per-mutant cost does **not** transfer between crates, and assuming it does was
how this job's CI timeout came to be derived from the wrong crate. It spans a
factor of eight. Full local passes at `--jobs 1`, 2026-09-16 at `1d4b484`:

| Crate | mutants | wall | per mutant |
|---|---:|---:|---:|
| `sunrise-crypto` | 519 | 16 m | ~1.9 s |
| `sunrise-sync` | 135 | 7 m | ~3.1 s |
| `sunrise-domain` | 1 356 | 2 h | ~5.3 s |
| `sunrise-core` | 1 259 | — | ~15.4 s |

The right-hand column is the `wall` column divided by the `mutants` column —
full-pass wall clock ÷ mutants. It is an average over a whole pass, not a
marginal cost per additional mutant, so everything `cargo mutants` spends
inside one invocation is already amortised into it, **the unmutated baseline
build it runs once before any mutant included**. The three completed rows
recompute from the table itself: 960 / 519 = 1.85, 420 / 135 = 3.11,
7 200 / 1 356 = 5.31. The `wall` column is rounded to whole minutes, which is
the whole of the gap between 1.85 and the 1.9 recorded beside it.

That definition is what the column means anywhere it is reused. Multiplying it
by a *shard's* mutant count charges that shard a baseline build already, and
splitting a crate into more shards adds baseline builds this column does not
price — which is why `.github/workflows/ci.yml`'s `mutants` timeout comment
treats more shards as sub-proportional relief rather than free.

`sunrise-core`'s row is a partial sample over its first 76 mutants — it is the
one crate no local pass has run to completion — and projects to roughly 5.4
hours whole. It is why `sunrise-core` is still the only scoped crate without a
recorded floor. It is also the one row that cannot be recomputed here, because
its `wall` cell is empty: nothing in this repository records whether 15.4 is
the same full-pass average, taken over those 76 mutants, or a marginal rate
read off `cargo mutants`' own output. The two differ by one baseline build's
cost spread across 76 mutants — the average carries a 76th of it, the marginal
rate carries none — so anything derived from 15.4 inherits that ambiguity
until a completed pass records its wall clock.

Each additional job is another copy of the source tree on
disk and another resident rustc, which is why `mise run mutants` pins one and
says so. A full pass over all four is hours, which is why it runs nightly and
sharded rather than on a pull request.

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
nightly alike. It is one of three such jobs — `Mutation flag gate` and `Mutation
flag gate contract`, above under §Features, are the others — and between them
they are the whole of mutation testing that does not wait for 04:00. What they
have in common is that none of them runs a mutant: they check the parts of the
campaign that are text, which is why they can report in seconds on a pull
request while the measurement itself cannot.

A floor also has to say what produced it. `malformed()` in
`.github/scripts/mutants-gate.py` requires every crate carrying a `caught_pct`
to carry a `provenance` object with a non-empty `sha`, `date` and `command`, and
`--update` writes all three from the run it is banking — one change rather than
two, because the update path replaces each crate entry wholesale, so a
`provenance` added by hand would not survive the next `mise run
mutants-baseline`.

Four fields, of which the first three are required of every floor carrying a
`caught_pct` and `dirty` is required of a *stamp* rather than of a floor — see
below. They have fixed meanings, and they are fixed because the object is only
comparable across floors if they are:

| Field | Means |
|---|---|
| `sha` | the full 40-hex revision the mutants were **measured** at |
| `date` | the day the **measurement** ran |
| `command` | the invocation a person would type to reproduce it |
| `dirty` | whether the measured tree had uncommitted changes |

**The measurement, not the recording.** `--update` does not ask git what HEAD
is. It reads the revision from a `revision.json` written beside each
`outcomes.json` while the measurement was running — `mise.toml`'s `mutants` task
and `ci.yml`'s `mutants` matrix both call `mutants-gate.py --record-revision`
around the cargo-mutants run — and refuses, exit 2, when an outcomes file
carries no stamp or when two stamps name different revisions. The gap between
the two is the reason: a `sunrise-core` pass is about five hours and a
`sunrise-domain` pass about two, the tests that motivated the run are committed
while it is going, and `mise run mutants-baseline` may not run until the next
day. A floor stamped with HEAD at recording time names a revision that does not
produce the number beside it. Refusing is recoverable, because the outcomes are
still on disk; a floor banked against the wrong tree is not.

`dirty` comes from `git status --porcelain` at the same moment, and a dirty
measurement is recorded rather than refused — it measured something real, it
just does not reproduce at the named revision on its own. The three floors this
file shipped before the stamp existed carry no `dirty` at all, and its absence
means **unknown**, not clean: nobody can now establish whether those trees were
modified, and writing `false` would be exactly the invention the requirement
exists to stop.

Which is why a `revision.json` stamp must carry it, and is refused with exit 2
when it does not — the same refusal `sha` and `date` get, and not the leniency
the baseline's own `dirty` gets. The two rules point the same way read forwards
and backwards. A floor from before the stamp existed cannot answer the question
and says so by omission; a stamp is written at the one moment the question is
answerable, so a stamp that omits it is not an old floor, it is somebody who did
not look. Defaulting that to `false` would bank a floor asserting a clean tree
on nobody's authority, which is the invention again, one file along. The refusal
above invites a hand-written stamp, so this is a thing to get wrong by following
the instructions: `sha`, `date` and `dirty`, all three.

`command` is the human-facing invocation because the field exists so a reader
can re-run the measurement, and nobody re-runs one by typing the gate's argv.
`mise run mutants-baseline` passes it with `--command`; a person handing the
gate a nightly's artifacts by hand gets the argv, which in that one case *is*
the recipe.

**Re-recording a baseline written before any of this.** Running the full check
before `--update` writes makes such a file impossible to repair: every crate the
run did not measure fails it, so the write never happens, and the only way out
is to hand-edit the one field the design says must never be hand-added — after a
measurement that costs hours. So `--update` checks the shape of the entries it
is not replacing, without the provenance requirement, records what it measured,
and then runs the full check on the merged file. A crate still carrying no
provenance is reported by name with exit 2, and the floor just measured is
already saved, so the file is repaired one crate at a time by re-recording
rather than by hand.

That check is structural, and the distinction matters more here than it looks.
It establishes that a floor says where it came from. It cannot establish that
what it says is true — whether the named revision carried the tests the floor
beside it is worth, and whether a percentage quoted in prose was computed by the
rule it names, are both decidable only by re-running the campaign at that
revision, which is the work a recorded floor exists to avoid. Both of those
defects have occurred in this repository's own baseline, and neither is
something any check here can catch.

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
[`SUNRISE_FUZZ_SEED`](#5-network--chaos-tests), which seeds the chaos harness
and the property tests. `cargo-fuzz` keeps its own corpus and its own `-seed=`
flag and reads none of the three.

Being a separate workspace means every gate has to name the manifest to reach
it, and three now do. `mise run rust-fmt-check`, `mise run rust-clippy` and
`mise run rust-doc` each run twice — once over the workspace, once over
`fuzz/Cargo.toml` — so the six harnesses are formatted, linted and
rustdoc-checked on the same terms as everything else. CI's `rust` job carries
the clippy and rustdoc halves as steps of their own, on the pinned stable: only
`cargo fuzz run` needs the nightly, for `-Zsanitizer=address`. The root
`clippy.toml` applies to `fuzz/` too, its lookup walking up out of that
directory. Formatting is the one of the three CI does not repeat for `fuzz/`;
the pre-commit hook runs `mise run rust-fmt-check`, which does.

Two details are worth knowing before editing any of it. Every `[[bin]]` in
`fuzz/Cargo.toml` sets `doc = true`; it was `false` with `test` and `bench`
until the rustdoc gate arrived, which would have made that gate document
nothing and pass. And `mise run rust-check` and `mise run rust-test` are still
workspace-only: the harnesses have no tests to run and `mise run fuzz-build` is
what proves they still compile against the crates they drive.

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
| `ical` | `regression_negative_year.ics` | The minimized reproducer for the second finding (below). Not a vendor shape — a `DTSTART` no client emits, kept because the round trip it broke is the property the target asserts. |
| `oauth_state` | `rs256_full`, `no_keys`, `symmetric_jwks` | Hand-built `<bearer>\0<discovery>\0<JWKS>` triples: a well-formed RS256 token against a 2048-bit RSA key set, the same token against an empty key set, and the same token against an `oct` key set — the algorithm-confusion branch. |
| `recovery_blob` | `sealed` | `seal_recovery_blob` output for a fixed seed, identity and CSPRNG state; the harness unseals against the same constants. |

Any new crash a target finds opens a P1 bug, and the minimized input joins the
seed corpus. That has happened twice.

`fuzz/seeds/ical/regression_negative_year.ics` carries `DTSTART:-202
0302T100000` (issue #190). `jiff`'s `%Y` implements a superset of RFC 5545's
`date-fullyear`: it takes a sign and as few as one digit, and the directives
after it skip whitespace, so that value read as the year −202. No year outside
`0000`–`9999` has an iCalendar spelling, and `strftime`'s `%Y` pads to four
*columns including the sign*, so `ical::write` emitted `-2020302T100000`, which
its own parser then read as year −2020 and month 30 and dropped — the
`write(parse(write(x))) == write(x)` the target asserts, failing on every
nightly for as long as the target had run. Fixed in
`crates/sunrise-integrations/src/ical.rs` by holding a `DTSTART`/`DTEND` value
to the grammar before handing it to `strptime`, which makes the parser's output
range exactly what the writer can spell.

The first finding, on the first run:
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

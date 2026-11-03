//! The event catalogue and the log-field vocabulary, enforced against the files
//! the compiler actually builds.
//!
//! `docs/10-cross-cutting/logging.md` §3 says a new `ev` value "requires a
//! one-line entry in log-events.md so analysts can grep for meaning", and §11
//! asked for a per-package snapshot test to hold that line. No such test
//! existed, and under `tracing` a snapshot of a hand-maintained constant list
//! would only prove the list matches itself.
//!
//! This checks the two things that actually matter:
//!
//! - every event name emitted from a shipped target is grammatical and
//!   catalogued;
//! - every *field name* those events carry is on the redaction allowlist, so
//!   the record the catalogue promises can reach a subscriber at all;
//! - every field name a span built there carries is on the same allowlist.
//!   `RedactionLayer` cannot veto a span, so for spans this is the only gate:
//!   a span field rides every record written inside the span, in every build.
//!
//! # Why the file set is asked for rather than worked out
//!
//! Earlier versions walked `crates/*/src`, then every manifest directory, then
//! pinned the result, then took the crate roots from `cargo metadata` and
//! followed the module graph from each. Every one of those was this file
//! deciding for itself which bytes rustc would compile, and every one was
//! wrong: a directory walk cannot see a `#[path]` module or an `include!`; the
//! pinned walk fired on `cargo vendor` and a `tests/fixtures` crate; and the
//! module-graph parser silently skipped `mod r#type;` and resolved `#[path]`
//! against the wrong directory, shipping violations with every gate green.
//!
//! So nothing here parses Rust's module grammar. The set of shipped targets
//! comes from `cargo metadata`, and the set of files each one compiled comes
//! from the **dep-info** rustc wrote beside the artefact — the compiler's own
//! record of every file it read. That covers `#[path]`, raw identifiers,
//! `include!` and generated code under `target/` at no cost, because rustc
//! already knew. `crates/sunrise-relay-client`'s eleven thousand generated
//! lines, which no walk could reach and which needed an argued exception under
//! the old design, are simply in the set.
//!
//! ## Every record, not the newest one
//!
//! A `.d` describes **one compilation unit**: one crate under one feature set,
//! one profile and one target platform. It is therefore never a description of
//! the crate — a module behind `#[cfg(feature = "…")]` or `#[cfg(test)]` ships
//! and is absent from the record of any build that did not enable it.
//!
//! Selecting the *newest* `.d` per crate root made the covered set a function
//! of which cargo command ran last. Measured on one tree with a `task_title`
//! violation planted in `crates/sunrise-server/src/api/testing.rs`:
//! `cargo build --workspace` gave 24 tests passed over 180 files with
//! `testing.rs` absent; `cargo test -p sunrise-server --lib --no-run` named the
//! violation; touching `lib.rs` and building again passed with the violation
//! still there. One tree, three answers, decided by build order (#84).
//!
//! So every `.d` naming this crate root is unioned instead. The union is a
//! superset of every configuration ever built here, which is the safe
//! direction: it can read a file some configuration does not compile, and it
//! cannot silently stop reading one that ships.
//!
//! Recency keeps a job, and it is the *other* one. The newest record decides
//! **currency** — whether the build is stale — while the union decides
//! **coverage**. Conflating those two questions in one `max_by_key` is what
//! #84 reported.
//!
//! Only the per-unit records rustc writes under a `deps/` directory are read.
//! Cargo also writes an uplifted `.d` beside each finished artefact whose
//! dependency line is the whole transitive source closure in topological order
//! — `target/debug/libsunrise_bench.d` names 95 files across 10 crates — so its
//! first path is not a crate root, and two such files began with a path that is
//! a real target's `src_path`. Being newest, they won the selection: coverage
//! of `crates/sunrise-sync/src/sse/mod.rs` rested on `sunrise-bench` enabling the
//! `sse` feature and on cargo's topological ordering, neither of which anyone
//! intended to depend on.
//!
//! ## The price, and who pays it
//!
//! The price is a dependency on build state, and it is paid openly:
//! [`file_set_gates::every_shipped_target_has_current_dep_info`] fails when
//! dep-info is missing, unreadable, or older than a source the newest record
//! names, and [`file_set_gates::the_scan_reaches_what_cargo_compiles`] fails naming any `.rs` file git
//! tracks under a workspace crate's `src/` that no record covers. A gate that
//! quietly covered less because `target/` was cleaned, or because the last
//! build enabled fewer features, would be the same defect in build-system
//! clothes — so it is a named failure that says which files went missing and
//! what to build to get them back.
//!
//! # What this gate does not see
//!
//! Enumerated rather than implied, because the failure mode of every earlier
//! version of this file was a totality claim nobody had written down and so
//! nobody could check:
//!
//! - **Anything that arrives by macro expansion.** The scan reads source, not
//!   expansions. A `macro_rules!` written here is covered, because its body is
//!   source like any other — but an *external* macro that expands to a
//!   `tracing::` call is invisible. Dep-info names the *file*; what the tokens
//!   in it become is not recorded there.
//! - **A configuration nothing has ever built here.** The union covers every
//!   feature set, profile and platform whose records `target/` still holds, and
//!   nothing beyond that. A module behind a feature no build in this checkout
//!   has enabled — or behind `#[cfg(target_os = "android")]`, which no build
//!   here compiles at all — is in no record, and its source lines are read by
//!   nothing.
//!
//!   This no longer *hides*, which is the whole of the change. Every `.rs` file
//!   git tracks under a workspace crate's `src/` is checked against the union
//!   by [`file_set_gates::the_scan_reaches_what_cargo_compiles`], and one the union misses is
//!   named with what to build. `crates/sunrise-sync/src/sse/mod.rs`, behind
//!   `#[cfg(feature = "sse")]` and off by default, is the standing instance:
//!   it is in the set because `sunrise-cli` and `sunrise-bench` turn the feature
//!   on and cargo recorded those units, and if that ever stops being true the
//!   gate says so instead of covering less. What survives is the file reached by
//!   `#[path]` from *outside* a crate's `src/`, and a platform this checkout
//!   never builds: both are covered when a record exists and neither is
//!   asserted.
//! - **Code that is not part of a shipped target.** Benches, examples and
//!   build scripts are outside the set on purpose: a `build.rs` runs at build
//!   time rather than in the artefact whose logs an operator reads. Note that
//!   [`manifests::is_shipped_kind`] admits any `lib`, so `sunrise-e2e`, `sunrise-bench`
//!   and `sunrise-crypto-test-vectors` *are* scanned despite existing only to
//!   support tests. That is over-coverage rather than a gap — nothing leaks
//!   through it — and it stays, because narrowing a gate to make its wording
//!   exact is the wrong trade. The failure messages say "compiled into a
//!   workspace library or binary", which is true of all three.
//!
//!   A unit test written inside a shipped file is scanned, because it is not
//!   separable from the file. A whole file behind `#[cfg(test)] mod x;` is a
//!   different thing and follows the rule above it: `sunrise-server`'s
//!   `api/testing.rs` is in the set because some `cargo test` compiled it, and
//!   the union keeps it there once it is.
//! - **Crates built outside the workspace.** `cargo test` never builds one, so
//!   there is no dep-info to derive its sources from. Each is an argued entry
//!   in [`file_set::BUILD_TOOLS`] with a guard, and a new one fails
//!   [`file_set_gates::every_out_of_workspace_target_is_a_guarded_build_tool`]
//!   until someone
//!   argues it. Their sources are not part of the scanned set — but the guard
//!   does read the entry's own target file, for `tracing` and for a `mod`
//!   declaration, because "it cannot reach `tracing`" is a claim about that
//!   source and reading the declared dependency list was a proxy that a
//!   transitive `tracing` would have satisfied.
//! - **Span fields the macro call does not name.** The span gate reads the
//!   `tracing::*_span!` and `tracing::span!` calls in the file set, and the
//!   fields each one declares;
//!   [`span_scanner_checks::the_scan_actually_finds_spans_and_their_fields`]
//!   pins it to the two `http.request` sites in `api/observe.rs`. A field added
//!   later with `Span::record` must have been declared — as
//!   `tracing::field::Empty` — at the macro, where the gate reads it. What it
//!   cannot read is `#[instrument]`, which takes its fields from the decorated
//!   function's arguments rather than from anything written in the attribute,
//!   so every `#[instrument]` in the file set is refused rather than read. The
//!   first site that needs one decides how this scanner should read it.
//! - **Build config this file does not know to read.** The build-config gates
//!   read `.github/workflows/`, `.github/actions/`, `Dockerfile*` and the root
//!   `mise.toml` — a hardcoded list, and so an assertion about where build
//!   config lives, which is the class this file renounced for source. A nested
//!   `mise.toml`, a `mise.local.toml`, a `.mise/config.toml` or a
//!   `docker/Dockerfile` is read by neither the derivation nor the gate that
//!   checks it.
//! - **A third-party action compiling a foreign manifest.** Such an invocation
//!   lives in that action's repository, not this one, and nothing here can
//!   reach it. What holds instead is that the `uses:` list is short and
//!   reviewed.
//! - **A build reached through a script.** `.github/scripts/*` are invoked
//!   bare today. Following them properly means following arbitrary shell and
//!   Python; following them partly would report coverage that does not exist,
//!   which is the exact defect this file keeps having to remove. So they are
//!   not followed at all, and this paragraph is the record of that choice.
//!
//! # Why the invocation scanner fails closed
//!
//! The file set is a lookup now, but what an invocation *says* is still read
//! here, and that scanner reports what it cannot analyse rather than skipping
//! it. A braced or bracketed invocation, a quoted or raw-identifier field name,
//! a macro reached under another name — each is a named failure telling the
//! author what to write instead, or to decide deliberately that this one is
//! fine.
//!
//! # Layout
//!
//! One test target, `event_catalog`, split along what each part reads:
//!
//! - [`manifests`] — the manifests and build-config files this repository
//!   builds from, and `cargo metadata` over them;
//! - [`source`] — the comment- and string-blanking normaliser and the
//!   whole-word search every scanner shares;
//! - [`file_set`] — the shipped targets and the files rustc compiled for them,
//!   read from dep-info;
//! - [`events`] and [`spans`] — the event/field and span invocation scanners;
//! - [`file_set_gates`], [`vocabulary_gates`] and [`build_config_gates`] — the
//!   gates over those readings;
//! - [`event_scanner_checks`] and [`span_scanner_checks`] — the scanners on the
//!   shapes they have to survive.

mod build_config_gates;
mod event_scanner_checks;
mod events;
mod file_set;
mod file_set_gates;
mod manifests;
mod source;
mod span_scanner_checks;
mod spans;
mod vocabulary_gates;

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

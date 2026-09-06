//! End-to-end tests for the command-line client.
//!
//! These run the **real binary** against a real vault via
//! `CARGO_BIN_EXE_sunrise` (a path cargo sets for integration tests), so they
//! cover the whole stack — arg parsing, `Core::open`, the vault lock,
//! `SQLCipher` persistence, the capture parser, and the query path — in a
//! separate process, with no terminal and no GUI.
//!
//! Every client goes through the same `Core` API, so a green run here is real
//! evidence the vault works end to end, not just that it compiles. This is
//! what keeps the core reachable and provable without a client.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sunrise")
}

/// A keystore of this test's own.
///
/// Every vault is now keyed by a root the first open mints and the keystore
/// keeps, so a test that did not set this would write into the developer's
/// real `~/.local/share/sunrise/keys` and leak state between runs. It is put
/// under the vault directory purely because that is the one temp path each
/// test already has; the property that the key does **not** belong beside the
/// data is asserted in `sunrise_cli::vault`'s own tests, against separate
/// directories.
fn keystore(vault: &Path) -> std::path::PathBuf {
    vault.join("keystore")
}

/// The root the binary minted for this vault, so an in-process `Core` can open
/// the same vault the binary just wrote — the way to build a fixture for
/// something the CLI reads but does not create.
fn root_of(vault: &Path) -> [u8; 32] {
    sunrise_cli::vault::open_or_create(vault, &keystore(vault), &sunrise_core::SystemRng)
        .expect("resolve this vault's root")
}

fn run(vault: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("SUNRISE_VAULT", vault)
        .env("SUNRISE_KEYSTORE", keystore(vault))
        // Keep the relay out of it: subcommands are one-shot and offline.
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_VAULT_ROOT")
        .env_remove("SUNRISE_EXPORT_PAIRING_FILE")
        .env_remove("SUNRISE_PAIRING_FILE")
        .output()
        .expect("run sunrise")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn capture_commits_and_is_readable_back() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["capture", "Renew passport !1 ~1h"]);
    assert!(out.status.success(), "capture failed: {out:?}");
    let line = stdout(&out);
    assert!(
        line.starts_with("tsk_"),
        "capture must print the new task id, got {line:?}"
    );
    assert!(line.contains("Renew passport"));

    // A separate process must see it — proves it was persisted and that the
    // vault lock was released on exit rather than stranding the vault.
    let inbox = stdout(&run(dir.path(), &["inbox"]));
    assert!(
        inbox.contains("Renew passport"),
        "a second process must see the committed task, got {inbox:?}"
    );
}

/// The capture parser's annotations must survive all the way to storage, not
/// just to the draft.
#[test]
fn capture_applies_annotations_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    // `^+6h` lands inside Today's rolling 24h window; `^+30h` does not. This
    // pins the window boundary through the real query path.
    run(dir.path(), &["capture", "Standup ^+6h !2"]);
    run(dir.path(), &["capture", "Far future ^+30h"]);
    run(dir.path(), &["capture", "Unscheduled thing"]);

    let today = stdout(&run(dir.path(), &["today"]));
    assert!(
        today.contains("Standup"),
        "in-window task missing: {today:?}"
    );
    assert!(
        !today.contains("Far future"),
        "a task 30h out must not appear in Today: {today:?}"
    );
    assert!(
        !today.contains("Unscheduled thing"),
        "an unscheduled task must not appear in Today: {today:?}"
    );
}

#[test]
fn capture_resolves_a_stream_by_name() {
    let dir = tempfile::tempdir().unwrap();
    // The synthetic Inbox row is resolvable by name with no setup.
    let out = run(dir.path(), &["capture", "Triage me #inbox"]);
    assert!(out.status.success());
    let streams = stdout(&run(dir.path(), &["streams"]));
    assert!(streams.contains("Inbox"), "got {streams:?}");
    assert!(
        streams.contains("1 open"),
        "the captured task must count against Inbox: {streams:?}"
    );
}

/// The parser's governing invariant, asserted through the binary: an
/// unresolvable annotation warns but never silently eats the user's text.
#[test]
fn unresolved_annotation_warns_but_still_commits_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["capture", "Thing #nosuchstream"]);
    assert!(out.status.success(), "an unknown stream must not fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no stream matches \"nosuchstream\""),
        "the warning must go to stderr so stdout stays parseable: {err:?}"
    );
    assert!(
        stdout(&out).contains("#nosuchstream"),
        "the typed text must survive into the title"
    );
}

#[test]
fn search_finds_a_captured_task() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["capture", "Renew passport"]);
    run(dir.path(), &["capture", "Email Sara"]);
    let hits = stdout(&run(dir.path(), &["search", "passport"]));
    assert!(hits.contains("Renew passport"), "got {hits:?}");
    assert!(!hits.contains("Email Sara"), "search must filter: {hits:?}");
}

#[test]
fn empty_capture_is_rejected_rather_than_creating_a_blank_task() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["capture", "   "]);
    assert!(!out.status.success(), "a blank capture must fail loudly");
    assert!(stdout(&run(dir.path(), &["inbox"])).trim().is_empty());
}

#[test]
fn unknown_subcommand_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["frobnicate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown subcommand"));
}

#[test]
fn help_and_version_succeed() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["help"], vec!["--help"], vec!["-h"]] {
        let out = run(dir.path(), &args);
        assert!(out.status.success(), "{args:?} should succeed");
        assert!(stdout(&out).contains("USAGE"));
    }
    let v = run(dir.path(), &["--version"]);
    assert!(v.status.success());
    assert!(stdout(&v).starts_with("sunrise "));
}

/// Regression for the vault-lock brick bug, at the level a user would hit it:
/// running the CLI repeatedly against one vault must keep working.
#[test]
fn repeated_runs_reacquire_the_vault_lock() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        let out = run(dir.path(), &["capture", &format!("task {i}")]);
        assert!(out.status.success(), "run {i} failed: {out:?}");
    }
    assert!(
        dir.path().join("core.lock").exists(),
        "lock target persists"
    );
    assert_eq!(stdout(&run(dir.path(), &["inbox"])).lines().count(), 5);
}

/// `docs/07-clients/tui.md` §Capture from anywhere names `sunrise focus next`
/// as a first-class surface. The picks must be the *core's* ranking, not a
/// re-sort in the CLI: "what should I do next" has to give the same answer in
/// the terminal and in the TUI, or one of them is lying.
#[test]
fn next_ranks_by_leverage_and_focus_next_opens_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = id_of(&run(dir.path(), &["capture", "wait for the survey"]));
    let leaf = id_of(&run(dir.path(), &["capture", "book the movers"]));
    assert!(!blocker.is_empty() && !leaf.is_empty());

    let picks = stdout(&run(dir.path(), &["next"]));
    assert!(picks.contains("wait for the survey"), "got {picks:?}");
    assert!(picks.contains("unblocks"), "each row says why: {picks:?}");

    // `focus next` opens a session on the top pick and says so.
    let out = run(dir.path(), &["focus", "next"]);
    assert!(out.status.success(), "focus next failed: {out:?}");
    assert!(stdout(&out).contains("focus started on"), "{out:?}");
}

#[test]
fn done_completes_by_id_and_the_review_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "file the tax return"]));
    let out = run(dir.path(), &["done", &id]);
    assert!(out.status.success(), "done failed: {out:?}");

    let review = stdout(&run(dir.path(), &["review"]));
    assert!(review.contains("completed 1"), "got {review:?}");
    assert!(review.contains("created 1"), "got {review:?}");
}

#[test]
fn done_refuses_something_that_is_not_a_task_id() {
    // A script that mistypes an id must fail loudly, not complete nothing and
    // exit 0.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["done", "not-an-id"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
}

// ---------------------------------------------------------------------------
// Writes beyond capture and done
//
// "Read/write tasks" is a CLI MUST that the v1 audit graded partial:
// `CreateTask` and `CompleteTask` were the only two mutations this binary
// could reach, so a task's fields could be set at capture time and never
// again. `edit`, `defer` and `drop` are the triage half.
// ---------------------------------------------------------------------------

/// The scheduling facet, observed through the query path rather than through
/// the write's own output — `^+6h` lands inside Today's rolling window and
/// `^-` takes it back out, which is the same boundary
/// `capture_applies_annotations_end_to_end` pins. Relative spans, not
/// `tomorrow`, so the assertion does not depend on the host's zone.
#[test]
fn edit_schedules_a_task_and_can_clear_the_schedule_again() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Renew passport"]));
    assert!(
        !stdout(&run(dir.path(), &["today"])).contains("Renew passport"),
        "an unscheduled task must not start in Today"
    );

    let out = run(dir.path(), &["edit", &id, "^+6h"]);
    assert!(out.status.success(), "edit failed: {out:?}");
    assert!(
        stdout(&out).starts_with(&id),
        "edit must print the task it touched: {:?}",
        stdout(&out)
    );
    assert!(
        stdout(&run(dir.path(), &["today"])).contains("Renew passport"),
        "the edit did not reach storage"
    );

    // `^-` is the clear form; without it a schedule set by accident would be
    // unreachable.
    assert!(run(dir.path(), &["edit", &id, "^-"]).status.success());
    assert!(
        !stdout(&run(dir.path(), &["today"])).contains("Renew passport"),
        "^- must clear the schedule"
    );
}

/// A `#stream` token is `Command::PromoteToStream`, not a patch field, and an
/// `@ctx` token is a union with what the task already carries. Both are
/// observed through the listings the previous commit added.
#[tokio::test]
async fn edit_moves_a_task_between_streams_and_tags_it() {
    use sunrise_cli::livesync::{open_with_plan, SyncPlan};
    use sunrise_core::Command as CoreCommand;
    use sunrise_domain::{ContextDraft, StreamDraft};

    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().to_path_buf();
    // A second Stream and a Context to aim at. The CLI reads and captures
    // against these but mints neither, so the fixture is built in-process.
    {
        let (core, _) = open_with_plan(
            vault.clone(),
            "0.1.0+test",
            root_of(&vault),
            &SyncPlan::default(),
        )
        .await
        .expect("open the vault");
        core.submit(CoreCommand::CreateStream(StreamDraft {
            name: "Work".into(),
            ..Default::default()
        }))
        .await
        .expect("create stream");
        core.submit(CoreCommand::CreateContext(ContextDraft {
            name: "errands".into(),
            description: None,
        }))
        .await
        .expect("create context");
        core.shutdown().await;
    }

    let id = id_of(&run(&vault, &["capture", "Renew passport"]));
    let out = run(&vault, &["edit", &id, "#work @errands !1"]);
    assert!(out.status.success(), "edit failed: {out:?}");

    assert!(
        stdout(&run(&vault, &["stream", "work"])).contains("Renew passport"),
        "the task did not move streams"
    );
    assert!(
        !stdout(&run(&vault, &["inbox"])).contains("Renew passport"),
        "a moved task must leave the Inbox"
    );
    assert!(
        stdout(&run(&vault, &["context", "errands"])).contains("Renew passport"),
        "the context was not added"
    );
}

/// Capture's rule is that unrecognised text survives in the title. An edit has
/// no title to fall back into, so the opposite rule applies: one bad token
/// rejects the whole line and **nothing** is written.
#[test]
fn edit_refuses_a_line_with_a_bad_token_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Renew passport"]));

    let out = run(dir.path(), &["edit", &id, "^+6h nonsense"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not an edit token"), "got {err:?}");
    assert!(
        !stdout(&run(dir.path(), &["today"])).contains("Renew passport"),
        "the good half of a rejected line must not be applied either"
    );

    // An id with no tokens, and tokens with no id, are both usage errors.
    assert!(!run(dir.path(), &["edit", &id]).status.success());
    assert!(!run(dir.path(), &["edit", "!1"]).status.success());
    assert!(!run(dir.path(), &["edit"]).status.success());
}

/// `defer` is `Command::DeferTask`, not `edit ^when`: it bumps the task's
/// `deferred_count`, and that counter is what the weekly review reports.
#[test]
fn defer_moves_the_task_and_the_review_counts_the_deferral() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Renew passport"]));

    let out = run(dir.path(), &["defer", &id, "+3d"]);
    assert!(out.status.success(), "defer failed: {out:?}");
    assert!(stdout(&out).contains("deferred to"), "{:?}", stdout(&out));

    let review = stdout(&run(dir.path(), &["review"]));
    assert!(review.contains("deferred 1"), "got {review:?}");

    // A date the parser will not guess at must fail rather than land the task
    // somewhere arbitrary.
    let bad = run(dir.path(), &["defer", &id, "whenever"]);
    assert!(!bad.status.success(), "expected a non-zero exit: {bad:?}");
    assert!(!run(dir.path(), &["defer", &id]).status.success());
    assert!(!run(dir.path(), &["defer"]).status.success());
}

/// A triage surface that can say yes (`done`) and not no is half a surface.
#[test]
fn drop_soft_deletes_a_task_and_the_review_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Renew passport"]));
    run(dir.path(), &["capture", "Keep this one"]);

    let out = run(dir.path(), &["drop", &id]);
    assert!(out.status.success(), "drop failed: {out:?}");

    let inbox = stdout(&run(dir.path(), &["inbox"]));
    assert!(!inbox.contains("Renew passport"), "got {inbox:?}");
    assert!(
        inbox.contains("Keep this one"),
        "drop must take one task, not the stream: {inbox:?}"
    );

    let review = stdout(&run(dir.path(), &["review"]));
    assert!(review.contains("dropped 1"), "got {review:?}");

    assert!(!run(dir.path(), &["drop", "not-an-id"]).status.success());
    assert!(!run(dir.path(), &["drop"]).status.success());
}

/// Bulk is the automation case: one process, several tasks, one line.
#[test]
fn edit_defer_and_drop_all_take_several_ids() {
    let dir = tempfile::tempdir().unwrap();
    let a = id_of(&run(dir.path(), &["capture", "First thing"]));
    let b = id_of(&run(dir.path(), &["capture", "Second thing"]));

    let out = run(dir.path(), &["edit", &a, &b, "^+6h"]);
    assert!(out.status.success(), "bulk edit failed: {out:?}");
    assert_eq!(stdout(&out).lines().count(), 2, "one line per task");
    let today = stdout(&run(dir.path(), &["today"]));
    assert!(today.contains("First thing") && today.contains("Second thing"));

    assert!(run(dir.path(), &["defer", &a, &b, "+3d"]).status.success());
    let out = run(dir.path(), &["drop", &a, &b]);
    assert!(out.status.success(), "bulk drop failed: {out:?}");
    assert!(stdout(&run(dir.path(), &["inbox"])).trim().is_empty());
}

// ---------------------------------------------------------------------------
// Retitling
//
// The last unwritable field. `edit` reaches six of `TaskPatch`'s fields and
// deliberately not this one: its grammar refuses bare words, so a task
// captured with a typo used to be fixable only from the macOS app. `retitle`
// is the other half, and the tests below are mostly about the seam *between*
// the two verbs — each has the rule the other must not have.
// ---------------------------------------------------------------------------

/// The typo case the parity matrix names, followed through the FTS index: a
/// rename nobody can search for has only half landed.
#[test]
fn retitle_fixes_a_typo_and_search_finds_the_new_title() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Renew pasport"]));

    let out = run(dir.path(), &["retitle", &id, "Renew", "passport"]);
    assert!(out.status.success(), "retitle failed: {out:?}");
    let line = stdout(&out);
    assert!(line.contains("Renew passport"), "got {line:?}");
    assert!(
        line.contains("Renew pasport"),
        "the old title is echoed so the change is visible: {line:?}"
    );

    // A separate process, through the query path, and through search — the
    // rename has to reach the FTS index and not just the row.
    let inbox = stdout(&run(dir.path(), &["inbox"]));
    assert!(inbox.contains("Renew passport"), "got {inbox:?}");
    assert!(
        !inbox.contains("pasport"),
        "the typo must be gone: {inbox:?}"
    );
    assert!(
        stdout(&run(dir.path(), &["search", "passport"])).contains(&id),
        "the new title must be searchable"
    );
    assert!(
        !stdout(&run(dir.path(), &["search", "pasport"])).contains(&id),
        "the old title must not still be indexed"
    );
}

/// **The reason retitle is its own verb.** The two tails are parsed by
/// opposite rules, and each rule is right for its own verb: `retitle`'s tail
/// is a title by construction, so `#work` in it is four characters of text;
/// `edit`'s is all grammar, so a bare word stays a rejected token. Neither
/// could hold both rules at once, which is what a `--title` flag would have
/// asked of `edit`.
#[test]
fn a_title_is_all_text_where_an_edit_line_is_all_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Placeholder"]));

    // Sigils in a title are text, not tokens: the task is not moved to a
    // stream, not given a priority, and not rejected for "tomorow".
    let out = run(dir.path(), &["retitle", &id, "#work !1 tomorow"]);
    assert!(out.status.success(), "retitle must absorb sigils: {out:?}");
    assert!(
        stdout(&run(dir.path(), &["inbox"])).contains("#work !1 tomorow"),
        "the sigils belong in the title verbatim"
    );

    // And the rule `edit` depends on is untouched: a bare word is still a
    // malformed token, never a new title, and it still rejects the whole line.
    let out = run(dir.path(), &["edit", &id, "^+6h tomorow"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not an edit token"),
        "got {out:?}"
    );
    assert!(
        !stdout(&run(dir.path(), &["today"])).contains("#work"),
        "the good half of a rejected edit line must still not be applied"
    );
}

/// A title is the one field where "empty" is a line to refuse rather than a
/// value to store: `!-` clears a priority and nothing clears a title.
#[test]
fn retitle_refuses_an_empty_title_rather_than_writing_one() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Keep this title"]));

    for args in [
        vec!["retitle", &id, "   "],
        vec!["retitle", &id],
        vec!["retitle", "not-an-id"],
        vec!["retitle"],
    ] {
        let out = run(dir.path(), &args);
        assert!(!out.status.success(), "{args:?} must fail: {out:?}");
    }
    assert!(
        stdout(&run(dir.path(), &["inbox"])).contains("Keep this title"),
        "no refusal above may have written anything"
    );
}

/// Every other mutating verb takes `<id>...`; this one does not, because a
/// title is what tells two tasks apart. Refused with an explanation rather
/// than silently retitling the first and dropping the rest.
#[test]
fn retitle_takes_one_task_where_the_other_verbs_take_a_list() {
    let dir = tempfile::tempdir().unwrap();
    let a = id_of(&run(dir.path(), &["capture", "First thing"]));
    let b = id_of(&run(dir.path(), &["capture", "Second thing"]));

    let out = run(dir.path(), &["retitle", &a, &b, "Same name"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("retitle takes one task"),
        "the refusal must say why: {out:?}"
    );
    let inbox = stdout(&run(dir.path(), &["inbox"]));
    assert!(
        inbox.contains("First thing") && inbox.contains("Second thing"),
        "neither task may have been renamed: {inbox:?}"
    );
}

/// `focus end` closes what `focus <id>` opened. Without it a session started
/// here could only be closed from macOS, which is not a CLI a cron job can
/// use — and `--done` is the focus screen's "complete" action, which
/// auto-completes the task.
#[test]
fn focus_end_closes_the_running_session_and_done_completes_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "Write the report"]));

    // Nothing running yet: a refusal, not a silent success.
    assert!(!run(dir.path(), &["focus", "end"]).status.success());

    assert!(run(dir.path(), &["focus", &id]).status.success());
    let out = run(dir.path(), &["focus", "end"]);
    assert!(out.status.success(), "focus end failed: {out:?}");
    assert!(stdout(&out).starts_with("fcs_"), "got {out:?}");
    // A session is immutable once closed, so the second end finds none.
    assert!(!run(dir.path(), &["focus", "end"]).status.success());
    assert!(
        stdout(&run(dir.path(), &["review"])).contains("completed 0"),
        "a plain end must not complete the task"
    );

    // `--done` is the user saying they finished it.
    assert!(run(dir.path(), &["focus", &id]).status.success());
    let out = run(dir.path(), &["focus", "end", "--done"]);
    assert!(out.status.success(), "focus end --done failed: {out:?}");
    assert!(
        stdout(&run(dir.path(), &["review"])).contains("completed 1"),
        "--done must complete the task the session was opened on"
    );

    assert!(!run(dir.path(), &["focus", "end", "--nope"])
        .status
        .success());
}

// ---------------------------------------------------------------------------
// Multi-account
//
// `SUNRISE_VAULT` has always given each vault its own directory. Until
// `sunrise_cli::vault` it gave every one of them the same 32-byte root, so the
// directories were separate and the *accounts* were not: one key opened both,
// and both derived identical Stream keys. These assert the difference from the
// binary, which is the only place a user meets it.
// ---------------------------------------------------------------------------

/// Two `SUNRISE_VAULT` values are two accounts: different keys, no shared
/// data, and neither openable with the other's key.
#[test]
fn two_vaults_are_two_accounts_with_two_keys() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    run(a.path(), &["capture", "Only in A"]);
    run(b.path(), &["capture", "Only in B"]);

    assert!(stdout(&run(a.path(), &["inbox"])).contains("Only in A"));
    assert!(
        !stdout(&run(a.path(), &["inbox"])).contains("Only in B"),
        "two vaults must not share data"
    );
    assert_ne!(
        root_of(a.path()),
        root_of(b.path()),
        "two vaults keyed by one root are one account wearing two hats"
    );
}

/// The key is not in the vault directory, so a vault directory copied without
/// its keystore is ciphertext — and says so, in its own words, instead of
/// surfacing as a `SQLCipher` failure from three layers down.
#[test]
fn a_vault_without_its_key_refuses_and_explains() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["capture", "Something private"]);
    std::fs::remove_dir_all(keystore(dir.path())).expect("drop the keystore");

    let out = run(dir.path(), &["inbox"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no key for vault"), "got {err:?}");
    assert!(
        err.contains("SUNRISE_VAULT_ROOT"),
        "the message must name the way back in: {err:?}"
    );
}

/// A vault made before per-vault keys — a database with no `vault-id` — is
/// refused with the old root quoted, and the quoted root really does open it.
///
/// ADR-0018's precedent (a vault this build will not open refuses in its own
/// words) with a way out that ADR did not have: the old root was a constant,
/// not a deleted migration, so nothing has to be lost. The fixture is a real
/// pre-multi-account vault, written the way the old binary wrote every
/// one — under that root, with no marker — not a marker deleted after the
/// fact, so the recovery below is the genuine article.
#[test]
fn a_pre_multi_account_vault_is_refused_but_the_quoted_root_opens_it() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("SUNRISE_VAULT", dir.path())
            .env(
                "SUNRISE_VAULT_ROOT",
                sunrise_cli::vault::LEGACY_DEV_ROOT_HEX,
            )
            .env("SUNRISE_KEYSTORE", keystore(dir.path()))
            .env_remove("SUNRISE_SYNC_URL")
            .env_remove("SUNRISE_EXPORT_PAIRING_FILE")
            .env_remove("SUNRISE_PAIRING_FILE")
            .output()
            .expect("run sunrise")
    };
    assert!(legacy(&["capture", "Written under the old root"])
        .status
        .success());

    // This build will not guess that root...
    let out = run(dir.path(), &["inbox"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("before vaults had their own keys"),
        "got {err:?}"
    );
    assert!(
        err.contains(sunrise_cli::vault::LEGACY_DEV_ROOT_HEX),
        "the old root must be quoted, or the data is unreachable: {err:?}"
    );

    // ...and the root it quotes is the one that gets the data back, so the
    // break costs a user a command, not their vault.
    assert!(
        stdout(&legacy(&["inbox"])).contains("Written under the old root"),
        "the documented remedy has to actually work"
    );
}

/// `SUNRISE_VAULT_ROOT` opens a vault with a root supplied outright and never
/// touches the keystore — the stand-in for pairing, and the escape hatch the
/// two refusals above point at.
#[test]
fn an_explicit_root_opens_a_vault_with_no_keystore_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let hex = "ab".repeat(32);
    let rooted = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("SUNRISE_VAULT", dir.path())
            .env("SUNRISE_VAULT_ROOT", &hex)
            .env("SUNRISE_KEYSTORE", keystore(dir.path()))
            .env_remove("SUNRISE_SYNC_URL")
            .env_remove("SUNRISE_EXPORT_PAIRING_FILE")
            .env_remove("SUNRISE_PAIRING_FILE")
            .output()
            .expect("run sunrise")
    };
    assert!(rooted(&["capture", "Keyed by hand"]).status.success());
    assert!(stdout(&rooted(&["inbox"])).contains("Keyed by hand"));
    assert!(
        !keystore(dir.path()).exists(),
        "an explicit root must not file anything in the keystore"
    );
    assert!(
        !dir.path().join("vault-id").exists(),
        "nor claim the vault for one"
    );

    // A malformed one fails before it can be mistaken for a key.
    let out = Command::new(bin())
        .args(["inbox"])
        .env("SUNRISE_VAULT", dir.path())
        .env("SUNRISE_VAULT_ROOT", "nonsense")
        .env_remove("SUNRISE_SYNC_URL")
        .output()
        .expect("run sunrise");
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("hex characters"));
}

/// Multi-account is only a capability if a user can find their accounts.
#[test]
fn vaults_lists_what_this_machine_is_keyed_for() {
    let shared = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let store = shared.path().join("keys");
    let in_vault = |vault: &Path, args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("SUNRISE_VAULT", vault)
            .env("SUNRISE_KEYSTORE", &store)
            .env_remove("SUNRISE_VAULT_ROOT")
            .env_remove("SUNRISE_SYNC_URL")
            .output()
            .expect("run sunrise")
    };
    in_vault(a.path(), &["capture", "In A"]);
    in_vault(b.path(), &["capture", "In B"]);

    let listed = stdout(&in_vault(a.path(), &["vaults"]));
    assert_eq!(listed.lines().count(), 2, "got {listed:?}");
    assert!(listed.contains(a.path().to_str().unwrap()), "{listed:?}");
    assert!(listed.contains(b.path().to_str().unwrap()), "{listed:?}");
    // The one currently open is marked, so `vaults` answers "where am I" too.
    let current: Vec<&str> = listed
        .lines()
        .filter(|l| l.ends_with("[current]"))
        .collect();
    assert_eq!(current.len(), 1, "{listed:?}");
    assert!(current[0].contains(a.path().to_str().unwrap()));

    // And it answers without opening a vault, because it is what a user
    // reaches for when a vault will not open.
    let out = Command::new(bin())
        .args(["vaults"])
        .env("SUNRISE_VAULT", "/nonexistent/nowhere")
        .env("SUNRISE_KEYSTORE", &store)
        .env_remove("SUNRISE_SYNC_URL")
        .output()
        .expect("run sunrise");
    assert!(
        out.status.success(),
        "vaults must not need a vault: {out:?}"
    );
    assert_eq!(stdout(&out).lines().count(), 2);
}

/// Discoverable, or it does not exist for a user.
#[test]
fn the_write_verbs_are_documented_in_the_usage_block() {
    let dir = tempfile::tempdir().unwrap();
    let help = stdout(&run(dir.path(), &["help"]));
    for line in [
        "sunrise edit",
        "sunrise retitle",
        "sunrise defer",
        "sunrise drop",
        "sunrise focus end",
        "EDIT SYNTAX",
    ] {
        assert!(help.contains(line), "{line} missing from help:\n{help}");
    }
}

/// A CLI's stdout is its contract: `export … | jq` has to work, which is the
/// opposite of the interactive `:export` (that one writes a file, because the
/// alternate screen is no place for a CSV).
#[test]
fn export_writes_a_parseable_document_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "something to finish"]));
    run(dir.path(), &["done", &id]);

    let json = stdout(&run(dir.path(), &["export", "trends", "json"]));
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(parsed["table"], "trends");
    assert!(parsed["rows"].as_array().is_some_and(|r| !r.is_empty()));

    let csv = stdout(&run(dir.path(), &["export", "trends", "csv"]));
    assert!(
        csv.lines().next().unwrap().contains("week_start_ms"),
        "{csv}"
    );

    // …and a path argument writes there instead.
    let path = dir.path().join("out.csv");
    let out = run(
        dir.path(),
        &["export", "trends", "csv", path.to_str().unwrap()],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("week_start_ms"));
}

#[test]
fn export_rejects_a_dataset_it_does_not_have() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["export", "vibes"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
}

#[test]
fn contexts_and_routines_are_listable_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing yet: an empty listing is a successful listing.
    assert!(run(dir.path(), &["contexts"]).status.success());
    assert!(run(dir.path(), &["routines"]).status.success());
}

// ---------------------------------------------------------------------------
// Stream and Context views
//
// `docs/07-clients/parity-matrix.md` marks "Today / Inbox / Stream views (list
// form)" a CLI MUST. `today` and `inbox` had it; the Stream half did not exist
// — `Query::StreamTasks` was never issued from this binary — and neither did
// its Context counterpart.
// ---------------------------------------------------------------------------

/// The listing itself, reached by name and by id, against the one Stream every
/// vault has.
#[test]
fn stream_lists_the_tasks_in_one_stream() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["capture", "Renew passport"]);
    run(dir.path(), &["capture", "Book the movers"]);

    // By name, resolved the same way `#inbox` is in a capture line.
    let by_name = stdout(&run(dir.path(), &["stream", "inbox"]));
    assert!(by_name.contains("Renew passport"), "got {by_name:?}");
    assert!(by_name.contains("Book the movers"), "got {by_name:?}");

    // ...and by the id `sunrise streams` printed, so a script can pipe one in.
    let id = stdout(&run(dir.path(), &["streams"]))
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(
        id.starts_with("str_"),
        "streams must print an id, got {id:?}"
    );
    assert_eq!(stdout(&run(dir.path(), &["stream", &id])), by_name);
}

/// A name that resolves to nothing must fail loudly and say what to try, not
/// print an empty listing that reads as "this stream is empty".
#[test]
fn stream_refuses_a_name_it_cannot_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["stream", "nosuchstream"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no stream matches"), "got {err:?}");

    // And with no argument at all it is a usage error, not an empty listing.
    assert!(!run(dir.path(), &["stream"]).status.success());
    assert!(!run(dir.path(), &["context"]).status.success());
    let out = run(dir.path(), &["context", "nosuchcontext"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no context matches"));
}

/// Contexts cut across Streams, so "everything tagged `@errands`" is its own
/// listing rather than a filter over one Stream (`Query::ContextTasks`).
///
/// The Context and the tagged Task are seeded **in-process**, because the CLI
/// is a read-and-capture surface for Contexts by design — `@name` in a capture
/// line resolves an existing Context and never mints one — so there is no way
/// to set this fixture up through the binary. The listing under test still
/// runs as a separate process against the real vault.
#[tokio::test]
async fn context_lists_the_tasks_carrying_it_across_streams() {
    use sunrise_cli::livesync::{open_with_plan, SyncPlan};
    use sunrise_core::Command as CoreCommand;
    use sunrise_domain::{ContextDraft, TaskDraft};

    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().to_path_buf();
    {
        let (core, _) = open_with_plan(
            vault.clone(),
            "0.1.0+test",
            root_of(&vault),
            &SyncPlan::default(),
        )
        .await
        .expect("open the vault");
        let ctx = core
            .submit(CoreCommand::CreateContext(ContextDraft {
                name: "errands".into(),
                description: None,
            }))
            .await
            .expect("create context")
            .entity;
        for title in ["Post the parcel", "Collect the keys"] {
            core.submit(CoreCommand::CreateTask(TaskDraft {
                title: title.into(),
                contexts: vec![ctx],
                ..Default::default()
            }))
            .await
            .expect("create task");
        }
        core.submit(CoreCommand::CreateTask(TaskDraft {
            title: "Untagged thing".into(),
            ..Default::default()
        }))
        .await
        .expect("create task");
        // Releases the vault lock, so the binary below can take it.
        core.shutdown().await;
    }

    let listed = stdout(&run(&vault, &["context", "errands"]));
    assert!(listed.contains("Post the parcel"), "got {listed:?}");
    assert!(listed.contains("Collect the keys"), "got {listed:?}");
    assert!(
        !listed.contains("Untagged thing"),
        "a Context listing must be the tagged tasks, not every task: {listed:?}"
    );

    // A `@`-prefixed argument is the same thing typed the way capture spells
    // it, so muscle memory from the capture line works here too.
    assert_eq!(stdout(&run(&vault, &["context", "@errands"])), listed);
}

/// Both must be in `help`, or they do not exist for a user.
#[test]
fn the_stream_and_context_views_are_documented_in_the_usage_block() {
    let dir = tempfile::tempdir().unwrap();
    let help = stdout(&run(dir.path(), &["help"]));
    assert!(help.contains("sunrise stream <id|name>"), "got {help}");
    assert!(help.contains("sunrise context <id|name>"), "got {help}");
}

/// `sync --once` is for cron. Without a relay configured it must fail fast and
/// loudly rather than blocking a scheduled job forever.
#[test]
fn sync_once_refuses_to_hang_when_no_relay_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["sync", "--once"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    let err = format!("{}{}", String::from_utf8_lossy(&out.stderr), stdout(&out));
    assert!(err.contains("SUNRISE_SYNC_URL"), "got {err:?}");
}

/// First whitespace-separated token of the first stdout line — the id every
/// mutating subcommand prints first.
fn id_of(o: &Output) -> String {
    stdout(o)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `SUNRISE_EXPORT_PAIRING_FILE` is documented as writing this device's pairing
/// payload "on startup", for every subcommand.
///
/// Regression: `run` used to open the vault with `SyncPlan::default()` and
/// never build one from the environment, so the binary ignored all three
/// pairing/relay variables. The unit tests for `plan_from_env` passed the whole
/// time — nothing called it.
#[test]
fn a_subcommand_exports_this_devices_pairing_payload_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("device.pairing");

    // Run under a permissive umask, so 0600 is a property of the write and not
    // of the environment the test happened to inherit. Under umask 077 this
    // assertion passes with the fix reverted, which is what it did before.
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg("umask 0; exec \"$0\" \"$@\"").arg(bin());
        c
    };
    #[cfg(not(unix))]
    let mut cmd = Command::new(bin());

    let out = cmd
        .args(["inbox"])
        .env("SUNRISE_VAULT", dir.path())
        .env("SUNRISE_KEYSTORE", keystore(dir.path()))
        .env_remove("SUNRISE_VAULT_ROOT")
        .env("SUNRISE_EXPORT_PAIRING_FILE", &payload)
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_PAIRING_FILE")
        .output()
        .expect("run sunrise");

    assert!(out.status.success(), "inbox failed: {out:?}");
    let bytes = std::fs::read(&payload).expect("the payload must have been written");
    assert!(!bytes.is_empty(), "an empty payload is not a payload");
    // The payload is the whole account in the clear — `ID_S_priv`,
    // `ID_D_priv`, the vault root and every Stream key — so it must not land at
    // the process umask, which under the `umask 0` above would leave it 0666.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&payload).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }
    // stdout stays the contract: the demo banner goes to stderr.
    assert!(
        !stdout(&out).contains("exported pairing payload"),
        "the startup banner must not pollute stdout"
    );
}

/// A pairing payload named in the environment is adopted at startup, so the
/// two-replica walkthrough works from the binary and not only from the
/// library.
///
/// It replaces the old cert-file exchange: since ADR-0024 two vaults sharing a
/// root are two separate accounts, and joining one means adopting its identity
/// and Stream keys when the vault is created.
#[test]
fn a_subcommand_adopts_a_pairing_payload_when_asked() {
    let peer = tempfile::tempdir().unwrap();
    let peer_payload = peer.path().join("peer.pairing");
    let out = Command::new(bin())
        .args(["inbox"])
        .env("SUNRISE_VAULT", peer.path())
        .env("SUNRISE_KEYSTORE", keystore(peer.path()))
        .env_remove("SUNRISE_VAULT_ROOT")
        .env("SUNRISE_EXPORT_PAIRING_FILE", &peer_payload)
        .env_remove("SUNRISE_SYNC_URL")
        .output()
        .expect("run sunrise");
    assert!(out.status.success(), "peer setup failed: {out:?}");

    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["inbox"])
        .env("SUNRISE_VAULT", dir.path())
        .env("SUNRISE_KEYSTORE", keystore(dir.path()))
        .env_remove("SUNRISE_VAULT_ROOT")
        .env("SUNRISE_PAIRING_FILE", &peer_payload)
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_EXPORT_PAIRING_FILE")
        .output()
        .expect("run sunrise");

    assert!(out.status.success(), "adoption failed: {out:?}");
    let banner = String::from_utf8_lossy(&out.stderr);
    assert!(
        banner.contains("adopted pairing payload"),
        "the pairing payload was not adopted, stderr was {banner:?}"
    );
}

// ---------------------------------------------------------------------------
// iCalendar interchange (`docs/09-integrations/icalendar.md`)
// ---------------------------------------------------------------------------

/// An `.ics` document with one timed event inside the current civil week, so
/// `ical export week` covers it whenever the suite happens to run.
///
/// The binary reads the real clock — every subcommand does — so the fixture is
/// built against the same clock rather than against a frozen date that would
/// leave this test passing only in one particular week of 2026.
fn ics_this_week(uid: &str, summary: &str) -> String {
    let start = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .start_of_day()
        .expect("start of day")
        .timestamp();
    let end = start + jiff::SignedDuration::from_hours(1);
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
DTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        start.strftime("%Y%m%dT%H%M%SZ"),
        end.strftime("%Y%m%dT%H%M%SZ"),
    )
}

fn write_ics(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fixture");
    path
}

/// The property the whole import path exists to hold: importing the same file
/// twice gives one calendar, not two.
#[test]
fn ical_import_is_idempotent_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(
        dir.path(),
        "cal.ics",
        &ics_this_week("ev1@example.com", "Quarterly planning"),
    );
    let path = ics.to_str().unwrap();

    let first = run(dir.path(), &["ical", "import", path]);
    assert!(first.status.success(), "import failed: {first:?}");
    let first_out = stdout(&first);
    assert!(
        first_out.starts_with("new  blk_"),
        "a first import must report a new block: {first_out:?}"
    );
    assert!(first_out.contains("Quarterly planning"));

    // A separate process, so the dedup cannot be coming from in-memory state.
    let second = run(dir.path(), &["ical", "import", path]);
    assert!(second.status.success(), "re-import failed: {second:?}");
    let second_out = stdout(&second);
    assert!(
        second_out.starts_with("upd  blk_"),
        "a re-import must update, not create: {second_out:?}"
    );
    assert_eq!(
        second_out.split_whitespace().nth(1),
        first_out.split_whitespace().nth(1),
        "the same UID must land on the same block id"
    );

    // And the calendar really does hold one event, not two.
    let exported = stdout(&run(dir.path(), &["ical", "export", "week"]));
    assert_eq!(
        exported.matches("BEGIN:VEVENT").count(),
        1,
        "one event, not two: {exported}"
    );
}

/// `-` reads stdin, so `curl … | sunrise ical import -` needs no temp file.
#[test]
fn ical_import_reads_stdin() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let body = ics_this_week("stdin@example.com", "From a pipe");

    let mut child = Command::new(bin())
        .args(["ical", "import", "-"])
        .env("SUNRISE_VAULT", dir.path())
        .env("SUNRISE_KEYSTORE", keystore(dir.path()))
        .env_remove("SUNRISE_VAULT_ROOT")
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_EXPORT_PAIRING_FILE")
        .env_remove("SUNRISE_PAIRING_FILE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sunrise");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");

    assert!(out.status.success(), "stdin import failed: {out:?}");
    assert!(
        stdout(&out).contains("From a pipe"),
        "got {:?}",
        stdout(&out)
    );
}

/// stdout is the contract. The document goes there; the notices do not.
#[test]
fn ical_export_writes_the_document_to_stdout_and_notices_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(
        dir.path(),
        "cal.ics",
        // DESCRIPTION and LOCATION are real content a Block cannot hold; the
        // import must say so on stderr and still commit the event.
        &ics_this_week("ev1@example.com", "Design review").replace(
            "END:VEVENT",
            "DESCRIPTION:Bring the roadmap\r\nLOCATION:Room 4B\r\nEND:VEVENT",
        ),
    );
    let imported = run(dir.path(), &["ical", "import", ics.to_str().unwrap()]);
    assert!(imported.status.success());
    let notes = String::from_utf8_lossy(&imported.stderr);
    assert!(notes.contains("DESCRIPTION"), "got {notes:?}");
    assert!(notes.contains("LOCATION"), "got {notes:?}");
    assert!(
        !stdout(&imported).contains("DESCRIPTION"),
        "notices must not pollute stdout"
    );

    let out = run(dir.path(), &["ical", "export", "week"]);
    assert!(out.status.success(), "export failed: {out:?}");
    let doc = stdout(&out);
    assert!(doc.starts_with("BEGIN:VCALENDAR\r\n"), "got {doc:?}");
    assert!(doc.trim_end().ends_with("END:VCALENDAR"));
    assert!(doc.contains("SUMMARY:Design review"));
}

/// With a path the document is written there and only the path is printed,
/// exactly as `sunrise export <dataset> … <path>` behaves.
#[test]
fn ical_export_writes_to_a_path_when_given_one() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(dir.path(), "cal.ics", &ics_this_week("ev1", "Standup"));
    run(dir.path(), &["ical", "import", ics.to_str().unwrap()]);

    let target = dir.path().join("out.ics");
    let out = run(
        dir.path(),
        &["ical", "export", "week", target.to_str().unwrap()],
    );
    assert!(out.status.success(), "export failed: {out:?}");
    assert_eq!(stdout(&out).trim(), target.to_str().unwrap());
    let written = std::fs::read_to_string(&target).expect("the file must exist");
    assert!(written.contains("SUMMARY:Standup"), "got {written:?}");
}

/// Exporting and re-importing must be the identity, or a user who round-trips
/// their own calendar doubles it.
#[test]
fn ical_export_then_import_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(dir.path(), "cal.ics", &ics_this_week("ev1", "Standup"));
    run(dir.path(), &["ical", "import", ics.to_str().unwrap()]);

    let target = dir.path().join("out.ics");
    run(
        dir.path(),
        &["ical", "export", "week", target.to_str().unwrap()],
    );
    let back = run(dir.path(), &["ical", "import", target.to_str().unwrap()]);
    assert!(back.status.success(), "re-import failed: {back:?}");
    assert!(
        stdout(&back).starts_with("upd  "),
        "an exported calendar must re-import onto itself: {:?}",
        stdout(&back)
    );

    let doc = stdout(&run(dir.path(), &["ical", "export", "week"]));
    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 1, "got {doc}");
}

/// `--source` is the escape hatch for two calendars that share a UID.
#[test]
fn ical_import_keys_on_the_source_as_well_as_the_uid() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(dir.path(), "cal.ics", &ics_this_week("shared", "Standup"));
    let path = ics.to_str().unwrap();
    run(dir.path(), &["ical", "import", path]);
    let other = run(dir.path(), &["ical", "import", path, "--source", "team"]);
    assert!(other.status.success(), "{other:?}");
    assert!(
        stdout(&other).starts_with("new  "),
        "a second source is a second block: {:?}",
        stdout(&other)
    );

    let doc = stdout(&run(dir.path(), &["ical", "export", "week"]));
    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 2, "got {doc}");
}

/// A malformed file must fail loudly rather than reporting an import of zero
/// events, and a bad argument must not be guessed at.
#[test]
fn ical_refuses_input_that_is_not_a_calendar() {
    let dir = tempfile::tempdir().unwrap();
    let junk = write_ics(dir.path(), "junk.ics", "this is not a calendar\n");
    let out = run(dir.path(), &["ical", "import", junk.to_str().unwrap()]);
    assert!(!out.status.success(), "expected failure: {out:?}");

    assert!(!run(dir.path(), &["ical"]).status.success());
    assert!(!run(dir.path(), &["ical", "wat"]).status.success());
    assert!(!run(dir.path(), &["ical", "import", "/no/such/file.ics"])
        .status
        .success());
}

/// The subcommand has to be discoverable, or it does not exist for a user.
#[test]
fn ical_is_documented_in_the_usage_block() {
    let dir = tempfile::tempdir().unwrap();
    let help = stdout(&run(dir.path(), &["help"]));
    assert!(help.contains("sunrise ical import"), "got {help}");
    assert!(help.contains("sunrise ical export"), "got {help}");
}

/// The stream names a `sunrise streams` listing printed, Inbox dropped — it is
/// pinned to the top and is not part of any order under test.
fn stream_order(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .filter(|n| n != "Inbox")
        .collect()
}

/// `streams move` reorders the list, and the new order is what the next
/// `streams` prints — from a different process, so it really went to the vault
/// rather than to a per-run cache.
///
/// The three Streams are seeded in-process for the same reason
/// [`edit_moves_a_task_between_streams_and_tags_it`] seeds its one: the CLI
/// reads and reorders Streams but mints none.
#[tokio::test]
async fn streams_move_reorders_the_list_and_persists_it() {
    use sunrise_cli::livesync::{open_with_plan, SyncPlan};
    use sunrise_core::Command as CoreCommand;
    use sunrise_domain::StreamDraft;

    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().to_path_buf();
    {
        let (core, _) = open_with_plan(
            vault.clone(),
            "0.1.0+test",
            root_of(&vault),
            &SyncPlan::default(),
        )
        .await
        .expect("open the vault");
        for name in ["Alpha", "Bravo", "Charlie"] {
            core.submit(CoreCommand::CreateStream(StreamDraft {
                name: name.into(),
                ..Default::default()
            }))
            .await
            .expect("create stream");
        }
        core.shutdown().await;
    }

    // A new stream is appended, so an untouched vault lists them as made.
    assert_eq!(
        stream_order(&stdout(&run(&vault, &["streams"]))),
        ["Alpha", "Bravo", "Charlie"]
    );

    let out = run(&vault, &["streams", "move", "charlie", "before", "alpha"]);
    assert!(out.status.success(), "move failed: {out:?}");
    assert_eq!(stream_order(&stdout(&out)), ["Charlie", "Alpha", "Bravo"]);
    // ...and a fresh process reads the same order back out of the vault.
    assert_eq!(
        stream_order(&stdout(&run(&vault, &["streams"]))),
        ["Charlie", "Alpha", "Bravo"]
    );

    let out = run(&vault, &["streams", "move", "charlie", "last"]);
    assert!(out.status.success(), "move to last failed: {out:?}");
    assert_eq!(
        stream_order(&stdout(&run(&vault, &["streams"]))),
        ["Alpha", "Bravo", "Charlie"]
    );

    // A name that resolves to nothing fails loudly rather than shuffling
    // something at random, and so does a half-typed command.
    assert!(
        !run(&vault, &["streams", "move", "nope", "before", "alpha"])
            .status
            .success()
    );
    assert!(!run(&vault, &["streams", "move", "alpha"]).status.success());
    assert!(!run(&vault, &["streams", "move", "before", "alpha"])
        .status
        .success());
    // The Inbox has no Stream entity behind it and no position to write.
    assert!(!run(&vault, &["streams", "move", "inbox", "last"])
        .status
        .success());
}

//! Search over every indexed kind (issue #345, ADR-0052): one index writer
//! per kind with tombstone removal, the operators, CJK, substrings and
//! diacritics, grouped and highlighted results, and the indexes rebuilt from
//! the rows and re-tokenized for a new content language.
//!
//! Every write here runs the rebuild-identity check (`engine()` turns it on),
//! so each test also shows a rebuild of the projection reproduces both
//! indexes.

use super::*;
use crate::engine::rebuild::check::dump;
use crate::queries::{SearchGroup, SearchResults};
use sunrise_domain::search::{NoticeKind, SearchKind};
use sunrise_domain::{
    encode_note_body, Inline, NoteBlock, PrefTarget, PrefValue, StreamDraft, StreamPatch,
};

fn grouped(e: &Engine, db: &Db, text: &str) -> SearchResults {
    grouped_with(e, db, text, false, None)
}

fn grouped_with(
    e: &Engine,
    db: &Db,
    text: &str,
    prefix: bool,
    per_group: Option<u32>,
) -> SearchResults {
    match e
        .query(
            db,
            Query::SearchGrouped {
                text: text.into(),
                now_ms: NOW as u64,
                prefix,
                per_group,
            },
        )
        .unwrap()
    {
        QueryResult::Search(r) => *r,
        other => panic!("expected Search, got {other:?}"),
    }
}

fn group(r: &SearchResults, kind: SearchKind) -> Option<&SearchGroup> {
    r.groups.iter().find(|g| g.kind == kind)
}

/// Every hit's title, `kind:title`, in result order.
fn hits(e: &Engine, db: &Db, text: &str) -> Vec<String> {
    grouped(e, db, text)
        .groups
        .iter()
        .flat_map(|g| {
            g.hits
                .iter()
                .map(move |h| format!("{}:{}", g.kind.as_str(), h.title.text))
        })
        .collect()
}

fn note(text: &str) -> NoteBody {
    encode_note_body(&[NoteBlock::Paragraph {
        inline: vec![Inline::text(text)],
    }])
}

fn task_with(e: &Engine, db: &mut Db, draft: TaskDraft) -> EntityRef {
    e.apply(db, Command::CreateTask(draft)).unwrap().entity
}

fn new_stream(e: &Engine, db: &mut Db, name: &str, parent: Option<EntityRef>) -> EntityRef {
    e.apply(
        db,
        Command::CreateStream(StreamDraft {
            name: name.into(),
            parent_id: parent,
            description: Some(note("where the launch work lives")),
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn docs(db: &Db) -> Vec<(String, Vec<u8>)> {
    let mut stmt = db
        .conn()
        .prepare("SELECT kind, id FROM search_docs ORDER BY kind, id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn indexed(db: &Db, r: EntityRef) -> bool {
    docs(db).iter().any(|(_, id)| id[..] == r.bytes()[..])
}

/// The rows of the three search tables, by entity, as the rebuild check
/// compares them.
fn search_dump(db: &Db) -> Vec<(&'static str, Vec<String>)> {
    dump(db.conn())
        .into_iter()
        .filter(|(t, _)| t.starts_with("search_"))
        .collect()
}

#[test]
fn every_kind_is_indexed_and_a_tombstone_removes_it() {
    let (e, mut db) = (engine(), db());
    let errands = new_context(&e, &mut db, "Errands");
    let launch = new_stream(&e, &mut db, "Launch", None);
    let task = task_with(
        &e,
        &mut db,
        TaskDraft {
            title: "Renew the passport".into(),
            body: Some(note("bring two photos")),
            contexts: vec![errands],
            stream_id: Some(launch),
            ..Default::default()
        },
    );
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                stream_id: Some(launch),
                ..block_draft(9, 1, Some("Deep work on the rocket"))
            }),
        )
        .unwrap()
        .entity;
    let mut draft = attachment_draft(task);
    draft.filename = "boarding-pass.pdf".into();
    let file = e.apply(&mut db, Command::AttachFile(draft)).unwrap().entity;

    for (text, kind, entity) in [
        ("passport", SearchKind::Task, task),
        ("photos", SearchKind::Task, task),
        ("launch", SearchKind::Stream, launch),
        ("errands", SearchKind::Context, errands),
        ("rocket", SearchKind::Block, block),
        ("boarding", SearchKind::Attachment, file),
    ] {
        let r = grouped(&e, &db, text);
        let g = group(&r, kind).unwrap_or_else(|| panic!("{text}: no {kind:?} group"));
        assert!(
            g.hits.iter().any(|h| h.entity == entity),
            "{text} finds the {kind:?}"
        );
    }
    // A task is found by the names of its contexts.
    let by_context = grouped(&e, &db, "errands");
    assert!(group(&by_context, SearchKind::Task).is_some_and(|g| g.hits[0].entity == task));
    // An attachment opens what it is attached to.
    let r = grouped(&e, &db, "boarding");
    let hit = &group(&r, SearchKind::Attachment).unwrap().hits[0];
    assert_eq!(hit.opens, task);

    e.apply(&mut db, Command::DetachFile(file)).unwrap();
    e.apply(&mut db, Command::DeleteBlock(block)).unwrap();
    e.apply(&mut db, Command::DeleteTask(task)).unwrap();
    e.apply(&mut db, Command::DeleteContext(errands)).unwrap();
    e.apply(
        &mut db,
        Command::DeleteStream {
            id: launch,
            disposition: sunrise_domain::Disposition::Detach,
        },
    )
    .unwrap();
    for r in [file, block, task, errands, launch] {
        assert!(!indexed(&db, r), "{r} is unindexed by its tombstone");
    }
    assert!(grouped(&e, &db, "passport").groups.is_empty());
    assert!(docs(&db).is_empty());
}

#[test]
fn renaming_a_context_rewrites_the_tasks_that_carry_it() {
    let (e, mut db) = (engine(), db());
    let ctx = new_context(&e, &mut db, "Errands");
    let task = task_with(
        &e,
        &mut db,
        TaskDraft {
            title: "Buy milk".into(),
            contexts: vec![ctx],
            ..Default::default()
        },
    );
    e.apply(
        &mut db,
        Command::UpdateContext {
            id: ctx,
            patch: ContextPatch {
                name: Some("Chores".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(hits(&e, &db, "chores"), ["task:Buy milk", "context:Chores"]);
    assert!(hits(&e, &db, "errands").is_empty());
    e.apply(&mut db, Command::DeleteContext(ctx)).unwrap();
    assert!(
        hits(&e, &db, "chores").is_empty(),
        "a deleted context stops matching"
    );
    assert_eq!(hits(&e, &db, "milk"), ["task:Buy milk"]);
    let _ = task;
}

/// A context whose task row cannot be read, because its
/// `scheduling_constraints` column is not CBOR. `read_task` fails with
/// [`EngineError::Cbor`].
fn a_context_on_an_unreadable_task(e: &Engine, db: &mut Db) -> EntityRef {
    let ctx = new_context(e, db, "Errands");
    let task = task_with(
        e,
        db,
        TaskDraft {
            title: "Buy milk".into(),
            contexts: vec![ctx],
            ..Default::default()
        },
    );
    db.conn()
        .execute(
            "UPDATE tasks SET scheduling_constraints = x'ff' WHERE id = ?",
            params![&task.bytes()[..]],
        )
        .unwrap();
    ctx
}

/// The context reindex speaks `rusqlite::Result`, so it carries an engine
/// error out boxed. `snapshot::in_tx` unboxes it, so the caller sees the
/// error `read_task` raised, not an `Invalid` holding its text.
#[test]
fn a_reindex_failure_keeps_its_engine_error_variant_through_in_tx() {
    let (e, mut db) = (engine(), db());
    let ctx = a_context_on_an_unreadable_task(&e, &mut db);
    let err = crate::engine::snapshot::in_tx(&mut db, |tx| {
        crate::engine::merge::reindex_tasks_of(tx, ctx.bytes())?;
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(err, EngineError::Cbor(_)), "{err:?}");
}

#[test]
fn cjk_text_is_found_whole_in_part_and_by_two_characters() {
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "東京出張の準備");
    new_task(&e, &mut db, "大阪の会議");
    assert_eq!(hits(&e, &db, "東京出張"), ["task:東京出張の準備"]);
    assert_eq!(hits(&e, &db, "出張の準"), ["task:東京出張の準備"]);
    // Two characters cannot use trigrams; the title scan finds them.
    assert_eq!(hits(&e, &db, "東京"), ["task:東京出張の準備"]);
    assert_eq!(hits(&e, &db, "会議"), ["task:大阪の会議"]);
    assert!(hits(&e, &db, "京都").is_empty());
    let r = grouped(&e, &db, "東京");
    assert_eq!(
        group(&r, SearchKind::Task).unwrap().hits[0].title.ranges,
        vec![(0, 2)]
    );
}

/// A one- or two-character CJK term scans titles, and the scan stops at
/// `SHORT_CJK_CAP` rows: the group's total is the cap, not every title that
/// holds the term.
#[test]
fn a_short_cjk_term_finds_at_most_the_cap() {
    let (e, mut db) = (engine(), db());
    // The rebuild check after each of these writes would rebuild the
    // projection five hundred times; one rebuild below does the same job.
    crate::engine::rebuild::check::disable();
    let cap = usize::try_from(crate::engine::search::SHORT_CJK_CAP).unwrap();
    // Inside a longer CJK run, so the word index, which keeps the run as
    // one token, does not find the term: only the scan does.
    for i in 0..cap + 5 {
        new_task(&e, &mut db, &format!("黒猫{i}"));
    }
    let r = grouped(&e, &db, "猫");
    let tasks = group(&r, SearchKind::Task).unwrap();
    assert_eq!(tasks.total, u32::try_from(cap).unwrap());
    // Below the cap, every title is found.
    new_task(&e, &mut db, "犬の散歩");
    assert_eq!(
        group(&grouped(&e, &db, "犬"), SearchKind::Task)
            .unwrap()
            .total,
        1
    );
    e.rebuild_projection(&mut db).unwrap();
    assert_eq!(grouped(&e, &db, "猫"), r, "a rebuild finds the same");
}

/// A query with no text lists the newest first, and two made in the same
/// millisecond by id, whatever their titles.
#[test]
fn a_query_with_no_text_lists_the_newest_first() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let mut made = Vec::new();
    for (i, title) in ["b oldest", "a middle", "c newest"].into_iter().enumerate() {
        set_clock(&clock, NOW as u64 + 1_000 * i as u64);
        made.push((new_task(&e, &mut db, title), title));
    }
    assert_eq!(
        hits(&e, &db, "state:open"),
        ["task:c newest", "task:a middle", "task:b oldest"]
    );
    // Two more in one millisecond, after the rest: by id between them.
    set_clock(&clock, NOW as u64 + 5_000);
    let mut same = [
        (new_task(&e, &mut db, "y same"), "y same"),
        (new_task(&e, &mut db, "x same"), "x same"),
    ];
    same.sort_by_key(|(id, _)| *id);
    let want: Vec<String> = same
        .iter()
        .chain(made.iter().rev())
        .map(|(_, t)| format!("task:{t}"))
        .collect();
    assert_eq!(hits(&e, &db, "state:open"), want);
}

#[test]
fn a_substring_finds_the_word_it_is_inside() {
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "Renew the passport");
    new_task(&e, &mut db, "Pay the bill");
    assert_eq!(hits(&e, &db, "pass"), ["task:Renew the passport"]);
    assert_eq!(hits(&e, &db, "sport"), ["task:Renew the passport"]);
    // `pasport` is not `passport`: no typo tolerance.
    assert!(hits(&e, &db, "pasport").is_empty());
    let r = grouped(&e, &db, "sport");
    assert_eq!(
        group(&r, SearchKind::Task).unwrap().hits[0].title.ranges,
        vec![(13, 18)]
    );
}

#[test]
fn diacritics_and_case_are_folded_both_ways() {
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "Café with Zoë");
    new_task(&e, &mut db, "Update the resume");
    assert_eq!(hits(&e, &db, "cafe"), ["task:Café with Zoë"]);
    assert_eq!(hits(&e, &db, "ZOE"), ["task:Café with Zoë"]);
    assert_eq!(hits(&e, &db, "résumé"), ["task:Update the resume"]);
    // A substring across a folded letter.
    assert_eq!(hits(&e, &db, "afe"), ["task:Café with Zoë"]);
}

#[test]
fn word_hits_rank_above_substring_hits_and_titles_above_bodies() {
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "passport photos");
    task_with(
        &e,
        &mut db,
        TaskDraft {
            title: "Paperwork".into(),
            body: Some(note("the passport form")),
            ..Default::default()
        },
    );
    new_task(&e, &mut db, "passports for everyone");
    let order = hits(&e, &db, "passport");
    // Each is one word hit, so the word index's `bm25` decides: a title
    // outweighs a body, and the shorter title the longer one.
    assert_eq!(
        order,
        [
            "task:passport photos",
            "task:passports for everyone",
            "task:Paperwork"
        ]
    );
    let r = grouped(&e, &db, "passport");
    let body_hit = group(&r, SearchKind::Task)
        .unwrap()
        .hits
        .iter()
        .find(|h| h.title.text == "Paperwork")
        .unwrap();
    let snip = body_hit.snippet.as_ref().unwrap();
    assert_eq!(snip.text, "the passport form");
    assert_eq!(snip.ranges, vec![(4, 12)]);
    assert!(body_hit.title.ranges.is_empty());
}

#[test]
fn structured_operators_narrow_the_hits() {
    let (e, mut db) = (engine(), db());
    let work = new_stream(&e, &mut db, "Work", None);
    let ops = new_stream(&e, &mut db, "Ops", Some(work));
    let home = new_stream(&e, &mut db, "Home", None);
    let errands = new_context(&e, &mut db, "errands");
    let mk = |e: &Engine, db: &mut Db, title: &str, stream, priority, contexts: Vec<EntityRef>| {
        task_with(
            e,
            db,
            TaskDraft {
                title: title.into(),
                stream_id: Some(stream),
                priority,
                contexts,
                ..Default::default()
            },
        )
    };
    mk(&e, &mut db, "deploy api", work, Some(1), vec![]);
    mk(&e, &mut db, "deploy db", ops, Some(3), vec![errands]);
    let done = mk(&e, &mut db, "deploy garden", home, None, vec![errands]);
    e.apply(&mut db, Command::CompleteTask(done)).unwrap();

    let titles = |q: &str| {
        let mut v: Vec<String> = grouped(&e, &db, q)
            .groups
            .iter()
            .filter(|g| g.kind == SearchKind::Task)
            .flat_map(|g| g.hits.iter().map(|h| h.title.text.clone()))
            .collect();
        v.sort();
        v
    };
    // A stream takes in the streams nested under it.
    assert_eq!(titles("#work deploy"), ["deploy api", "deploy db"]);
    assert_eq!(titles("stream:ops deploy"), ["deploy db"]);
    assert_eq!(titles("@errands"), ["deploy db", "deploy garden"]);
    assert_eq!(titles("deploy state:done"), ["deploy garden"]);
    assert_eq!(titles("deploy state:open"), ["deploy api", "deploy db"]);
    assert_eq!(titles("!1"), ["deploy api"]);
    assert_eq!(titles("priority>=2 deploy"), ["deploy db"]);
    assert_eq!(titles("deploy -@errands"), ["deploy api"]);
    assert_eq!(titles("api OR garden"), ["deploy api", "deploy garden"]);
    assert_eq!(
        titles("deploy (#home OR !1)"),
        ["deploy api", "deploy garden"]
    );
    assert_eq!(titles("deploy is:inbox"), Vec::<String>::new());
    // `kind:` picks the groups.
    let r = grouped(&e, &db, "kind:stream");
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].kind, SearchKind::Stream);
    assert_eq!(r.groups[0].total, 3);
    // A stream filter matches tasks and blocks, never the stream itself.
    assert!(group(&grouped(&e, &db, "#work"), SearchKind::Stream).is_none());
    // A block's stream is read for it too, and `stream:Inbox` is no stream.
    for (title, stream_id) in [("deploy slot", Some(work)), ("deploy rest", None)] {
        e.apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                stream_id,
                ..block_draft(9, 1, Some(title))
            }),
        )
        .unwrap();
    }
    let blocks = |q: &str| -> Vec<String> {
        group(&grouped(&e, &db, q), SearchKind::Block)
            .map(|g| g.hits.iter().map(|h| h.title.text.clone()).collect())
            .unwrap_or_default()
    };
    assert_eq!(blocks("#work deploy"), ["deploy slot"]);
    assert_eq!(blocks("stream:inbox deploy"), ["deploy rest"]);
}

#[test]
fn date_operators_resolve_in_the_readers_day() {
    let (e, mut db) = (engine(), db());
    // NOW is 2023-11-14T22:13:20Z, a Tuesday, and the tests' zone is UTC.
    let at = |d: jiff::civil::Date| SunriseTime::AllDay { date: d };
    let today = jiff::civil::date(2023, 11, 14);
    for (title, due) in [
        ("due today", today),
        ("due friday", jiff::civil::date(2023, 11, 17)),
        ("due last month", jiff::civil::date(2023, 10, 2)),
    ] {
        task_with(
            &e,
            &mut db,
            TaskDraft {
                title: title.into(),
                hard_due_at: Some(at(due)),
                planned_at: Some(at(due)),
                ..Default::default()
            },
        );
    }
    new_task(&e, &mut db, "due never");
    let titles = |q: &str| {
        let mut v: Vec<String> = hits(&e, &db, q);
        v.sort();
        v
    };
    assert_eq!(titles("due:today"), ["task:due today"]);
    assert_eq!(titles("hard:fri"), ["task:due friday"]);
    assert_eq!(titles("due<today"), ["task:due last month"]);
    assert_eq!(titles("due<=tomorrow due>=yesterday"), ["task:due today"]);
    assert_eq!(
        titles("planned:this-week"),
        ["task:due friday", "task:due today"]
    );
    assert_eq!(titles("due:last-month"), ["task:due last month"]);
    assert_eq!(
        titles("due:2023-11-01..2023-11-30"),
        ["task:due friday", "task:due today"]
    );
    assert_eq!(titles("due>+2d"), ["task:due friday"]);
    assert_eq!(titles("due>+3d"), Vec::<String>::new());
    assert_eq!(titles("due>=+3d"), ["task:due friday"]);
    assert_eq!(titles("created:today kind:task").len(), 4);
    assert_eq!(titles("is:late"), ["task:due last month"]);
    assert_eq!(titles("has:deadline").len(), 3);
}

/// The three times and lateness in search (ADR-0047): `target:` reads the
/// soft deadline, `due:` the earlier of the two, `hard:` the hard one;
/// `is:late` is either deadline missed, `is:late-hard` the hard one, and
/// `is:stale` an undated task untouched for `stale_after_days`.
#[test]
fn the_three_times_and_lateness_are_operators() {
    let (e, mut db) = (engine(), db());
    // NOW is 2023-11-14T22:13:20Z, a Tuesday, in UTC.
    let day = |d: u8| SunriseTime::AllDay {
        date: jiff::civil::date(2023, 11, i8::try_from(d).unwrap()),
    };
    for (title, target, hard) in [
        ("soft missed", Some(day(13)), Some(day(20))),
        ("hard missed", None, Some(day(10))),
        ("target friday", Some(day(17)), None),
        ("on track", Some(day(15)), Some(day(16))),
        ("missed nothing", None, None),
    ] {
        task_with(
            &e,
            &mut db,
            TaskDraft {
                title: title.into(),
                target_at: target,
                hard_due_at: hard,
                ..Default::default()
            },
        );
    }
    new_stream(&e, &mut db, "missed deadlines", None);
    let titles = |q: &str| {
        let mut v: Vec<String> = hits(&e, &db, q);
        v.sort();
        v
    };
    assert_eq!(titles("target:fri"), ["task:target friday"]);
    assert_eq!(titles("hard:fri"), Vec::<String>::new());
    assert_eq!(titles("due:yesterday"), ["task:soft missed"]);
    assert_eq!(titles("due:+1d"), ["task:on track"]);
    assert_eq!(titles("is:late"), ["task:hard missed", "task:soft missed"]);
    assert_eq!(titles("is:late-hard"), ["task:hard missed"]);
    assert_eq!(titles("has:deadline").len(), 4);
    assert_eq!(
        titles("missed has:deadline"),
        ["task:hard missed", "task:soft missed"],
        "a stream has no deadline"
    );
    assert!(grouped(&e, &db, "is:stale").notices.is_empty());
    assert_eq!(titles("is:stale"), Vec::<String>::new());
    assert_eq!(titles("-is:late kind:task").len(), 3);
}

/// `is:stale` reads the reader's clock and the `stale_after_days`
/// preference: an undated task is stale once that many planner days have
/// passed since its last touch, `0` turns staleness off, and a deadline of
/// either kind keeps a task off the stale clock.
#[test]
fn is_stale_reads_the_stale_after_days_preference() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
    let e = Engine::from_clock(clock.clone(), Arc::new(SystemRng), kc);
    let mut db = db();
    let id = new_task(&e, &mut db, "undated");
    task_with(
        &e,
        &mut db,
        TaskDraft {
            title: "targeted".into(),
            target_at: Some(SunriseTime::AllDay {
                date: jiff::civil::date(2030, 1, 1),
            }),
            ..Default::default()
        },
    );
    let stale = |e: &Engine, db: &Db, at: i64| -> Vec<String> {
        let r = match e
            .query(
                db,
                Query::SearchGrouped {
                    text: "is:stale".into(),
                    now_ms: u64::try_from(at).unwrap(),
                    prefix: false,
                    per_group: None,
                },
            )
            .unwrap()
        {
            QueryResult::Search(r) => *r,
            other => panic!("{other:?}"),
        };
        r.groups
            .iter()
            .flat_map(|g| g.hits.iter().map(|h| h.title.text.clone()))
            .collect()
    };
    let day = 86_400_000i64;
    // 2023-11-14 22:13 UTC; fourteen planner days later is 2023-11-28.
    let midnight_28 = 1_701_129_600_000i64;
    assert_eq!(stale(&e, &db, midnight_28 - 1), Vec::<String>::new());
    assert_eq!(stale(&e, &db, midnight_28), ["undated"]);
    let set = |e: &Engine, db: &mut Db, n: u64| {
        e.apply(
            db,
            Command::SetPreference {
                key: "stale_after_days".into(),
                value: PrefValue::Uint(n),
                target: PrefTarget::Vault,
            },
        )
        .unwrap();
    };
    set(&e, &mut db, 0);
    assert_eq!(
        stale(&e, &db, midnight_28 + 400 * day),
        Vec::<String>::new()
    );
    set(&e, &mut db, 1);
    assert_eq!(stale(&e, &db, NOW + day), ["undated"]);
    // A touch resets the clock.
    *clock.0.lock() = u64::try_from(NOW + 3 * day).unwrap();
    e.apply(
        &mut db,
        Command::UpdateTask {
            id,
            patch: TaskPatch {
                priority: Some(Some(3)),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(stale(&e, &db, NOW + 3 * day + 1), Vec::<String>::new());
    assert_eq!(stale(&e, &db, NOW + 5 * day), ["undated"]);
}

#[test]
fn a_name_that_names_nothing_matches_nothing_and_says_so() {
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "deploy");
    new_stream(&e, &mut db, "Work", None);
    new_stream(&e, &mut db, "Workshop", None);
    let r = grouped(&e, &db, "#wrok deploy");
    assert!(r.groups.is_empty());
    let n = &r.notices[0];
    assert_eq!(n.kind, NoticeKind::UnknownName);
    assert_eq!(n.detail.as_deref(), Some("wrok"));
    assert_eq!((n.start, n.end), (1, 5));
    let r = grouped(&e, &db, "#wor");
    assert_eq!(r.notices[0].kind, NoticeKind::AmbiguousName);
    // An exact name wins over a longer one it starts.
    assert!(grouped(&e, &db, "#work").notices.is_empty());
    // An operator this build cannot evaluate yet is said so too.
    let r = grouped(&e, &db, "deploy is:fixed");
    assert!(r.groups.is_empty());
    assert_eq!(r.notices[0].kind, NoticeKind::Unsupported);
    // And what the parser read as text.
    let r = grouped(&e, &db, "colour:red");
    assert_eq!(r.notices[0].kind, NoticeKind::UnknownKey);
    assert_eq!(r.canonical, "colour:red");
}

#[test]
fn groups_come_in_the_fixed_order_each_capped_with_its_total() {
    let (e, mut db) = (engine(), db());
    for i in 0..12 {
        new_stream(&e, &mut db, &format!("alpha stream {i}"), None);
    }
    for i in 0..3 {
        new_task(&e, &mut db, &format!("alpha task {i}"));
    }
    new_context(&e, &mut db, "alpha");
    let r = grouped(&e, &db, "alpha");
    let kinds: Vec<SearchKind> = r.groups.iter().map(|g| g.kind).collect();
    assert_eq!(
        kinds,
        [SearchKind::Task, SearchKind::Stream, SearchKind::Context]
    );
    let streams = group(&r, SearchKind::Stream).unwrap();
    assert_eq!((streams.hits.len(), streams.total), (10, 12));
    let r = grouped_with(&e, &db, "alpha kind:stream", false, Some(100));
    assert_eq!(r.groups[0].hits.len(), 12);
}

#[test]
fn the_word_being_typed_matches_as_a_prefix_only_when_asked() {
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "quarterly report");
    // `qu` is two letters: no trigram, and no prefix unless the field asks.
    assert!(grouped(&e, &db, "qu").groups.is_empty());
    let r = grouped_with(&e, &db, "qu", true, None);
    assert_eq!(
        group(&r, SearchKind::Task).unwrap().hits[0].title.text,
        "quarterly report"
    );
    assert_eq!(
        group(&r, SearchKind::Task).unwrap().hits[0].title.ranges,
        vec![(0, 2)]
    );
}

#[test]
fn the_task_query_answers_tasks_with_operators() {
    let (e, mut db) = (engine(), db());
    let work = new_stream(&e, &mut db, "Work", None);
    task_with(
        &e,
        &mut db,
        TaskDraft {
            title: "ship it".into(),
            stream_id: Some(work),
            ..Default::default()
        },
    );
    new_task(&e, &mut db, "ship the parcel");
    let QueryResult::Tasks(tasks) = e
        .query(
            &db,
            Query::Search {
                text: "#work ship".into(),
                limit: 10,
            },
        )
        .unwrap()
    else {
        panic!("tasks");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "ship it");
}

#[test]
fn the_indexes_written_from_the_rows_are_the_ones_the_fold_wrote() {
    let (e, mut db) = (engine(), db());
    let ctx = new_context(&e, &mut db, "Errands");
    let s = new_stream(&e, &mut db, "Launch", None);
    let t = task_with(
        &e,
        &mut db,
        TaskDraft {
            title: "Renew the passport".into(),
            body: Some(note("東京出張 photos")),
            contexts: vec![ctx],
            stream_id: Some(s),
            ..Default::default()
        },
    );
    e.apply(&mut db, Command::AttachFile(attachment_draft(t)))
        .unwrap();
    e.apply(
        &mut db,
        Command::CreateBlock(block_draft(9, 1, Some("focus"))),
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: s,
            patch: StreamPatch {
                name: Some("Launch v2".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let folded = search_dump(&db);
    assert!(
        folded.iter().all(|(_, rows)| rows.len() == 5),
        "{folded:#?}"
    );
    db.conn().execute("DELETE FROM search_meta", []).unwrap();
    assert_eq!(e.ensure_search_index(&mut db).unwrap(), Some(5));
    assert_eq!(search_dump(&db), folded);
    assert_eq!(e.ensure_search_index(&mut db).unwrap(), None, "current now");
}

fn tokenizer(db: &Db) -> String {
    db.conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'search_words'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn set_language(e: &Engine, db: &mut Db, tag: &str) {
    e.apply(
        db,
        Command::SetPreference {
            key: "search.content_language".into(),
            value: PrefValue::Text(tag.into()),
            target: PrefTarget::Vault,
        },
    )
    .unwrap();
}

/// The task titles a query finds, sorted.
fn task_hits(e: &Engine, db: &Db, q: &str) -> Vec<String> {
    let mut v: Vec<String> = hits(e, db, q);
    v.sort();
    v
}

#[test]
fn a_new_content_language_reindexes_in_batches_and_keeps_answering() {
    let (e, mut db) = (engine(), db());
    for title in [
        "running shoes",
        "she runs daily",
        "東京出張",
        "Café visit",
        "passport renewal",
        "quarterly report",
        "deploy the api",
    ] {
        new_task(&e, &mut db, title);
    }
    e.ensure_search_index(&mut db).unwrap();
    assert!(tokenizer(&db).contains("porter"), "English stems");
    let neutral = ["東京", "pass", "cafe", "report", "deploy api", "daily"];
    let before: Vec<Vec<String>> = neutral.iter().map(|q| task_hits(&e, &db, q)).collect();
    // Stemming: `run` finds both.
    assert_eq!(task_hits(&e, &db, "run").len(), 2);

    set_language(&e, &mut db, "fr-CA");
    let first = e.reindex_search_step_by(&mut db, 3).unwrap();
    assert!(!first.finished);
    assert_eq!((first.copied, first.total), (3, 7));
    // Part way, search answers from the old, stemmed index.
    assert_eq!(task_hits(&e, &db, "run").len(), 2);
    // A doc written past the cursor and removed again leaves nothing behind.
    let gone = new_task(&e, &mut db, "placeholder");
    e.apply(&mut db, Command::DeleteTask(gone)).unwrap();
    let mut steps = 1;
    loop {
        let step = e.reindex_search_step_by(&mut db, 3).unwrap();
        steps += 1;
        if step.finished {
            assert_eq!(step.copied, step.total);
            break;
        }
    }
    assert_eq!(steps, 3);
    assert!(!tokenizer(&db).contains("porter"), "French does not stem");
    let after: Vec<Vec<String>> = neutral.iter().map(|q| task_hits(&e, &db, q)).collect();
    assert_eq!(before, after, "locale-neutral queries find the same hits");
    // `run` is now a word, not a stem: only the substring hit is left.
    assert_eq!(
        task_hits(&e, &db, "run"),
        ["task:running shoes", "task:she runs daily"]
    );
    assert!(task_hits(&e, &db, "shoe").contains(&"task:running shoes".to_owned()));
    // And it is done: the next step does nothing.
    assert!(e.reindex_search_step(&mut db).unwrap().finished);
    // Back to English stems again.
    set_language(&e, &mut db, "en");
    while !e.reindex_search_step(&mut db).unwrap().finished {}
    assert!(tokenizer(&db).contains("porter"));
}

#[test]
fn a_write_during_a_reindex_reaches_the_index_it_is_building() {
    let (e, mut db) = (engine(), db());
    let a = new_task(&e, &mut db, "alpha one");
    for t in ["beta two", "gamma three", "delta four"] {
        new_task(&e, &mut db, t);
    }
    e.ensure_search_index(&mut db).unwrap();
    set_language(&e, &mut db, "de");
    e.reindex_search_step_by(&mut db, 2).unwrap();
    // `a` was copied; rename it, and add a task after the cursor.
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: a,
            patch: TaskPatch {
                title: Some("omega one".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    new_task(&e, &mut db, "epsilon five");
    while !e.reindex_search_step_by(&mut db, 2).unwrap().finished {}
    assert_eq!(task_hits(&e, &db, "omega"), ["task:omega one"]);
    assert!(task_hits(&e, &db, "alpha").is_empty());
    assert_eq!(task_hits(&e, &db, "epsilon"), ["task:epsilon five"]);
    let words: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM search_words", [], |r| r.get(0))
        .unwrap();
    assert_eq!(words, 5);
}

#[test]
fn a_projection_rebuild_abandons_a_reindex_and_the_next_step_restarts_it() {
    let (e, mut db) = (engine(), db());
    for t in ["one", "two", "three", "four"] {
        new_task(&e, &mut db, t);
    }
    e.ensure_search_index(&mut db).unwrap();
    set_language(&e, &mut db, "ja");
    e.reindex_search_step_by(&mut db, 2).unwrap();
    e.rebuild_projection(&mut db).unwrap();
    let next: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'search_words_next'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(next, 0);
    assert_eq!(task_hits(&e, &db, "three"), ["task:three"]);
    while !e.reindex_search_step_by(&mut db, 2).unwrap().finished {}
    assert!(!tokenizer(&db).contains("porter"));
    assert_eq!(task_hits(&e, &db, "three"), ["task:three"]);
}

/// Fragments the grammar gives meaning to, and FTS5 syntax, so hostile input
/// exercises both.
const HOSTILE: &[&str] = &[
    "a",
    "pass",
    "東京",
    "東",
    "é",
    "\u{301}",
    "OR",
    "AND",
    "NOT",
    "NEAR",
    "NEAR(",
    "*",
    "^",
    "-",
    "(",
    ")",
    "\"",
    "'",
    ":",
    "#",
    "@",
    "!1",
    "#work",
    "@home",
    "stream:",
    "state:done",
    "kind:task",
    "due<fri",
    "planned:",
    "..",
    ",",
    "+",
    "{",
    "}",
    "[",
    "]",
    "\\",
    "%",
    "_",
    "\u{0}",
    " ",
    "  ",
    "\t",
    "\n",
    "col:x",
    "title:x",
    "rowid",
    ";",
    "--",
    "/*",
];

proptest::proptest! {
    // docs/10-cross-cutting/testing.md §2: a shrunken counterexample is
    // committed, at a path named here.
    #![proptest_config(proptest::prelude::ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/search.txt",
            ),
        )),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// No query is an FTS5 syntax error, or any other error, whatever it
    /// holds and whether or not the last word is a prefix; each answers the
    /// same on a second read and after a rebuild of the indexes from the
    /// rows; and the seeded doc is still found.
    #[test]
    fn hostile_input_never_errors(
        parts in proptest::collection::vec(proptest::sample::select(HOSTILE), 0..24),
        prefix in proptest::bool::ANY,
        raw in ".{0,40}",
    ) {
        begin_case();
        let mut db = db();
        let e = engine();
        crate::engine::rebuild::check::disable();
        let seeded = new_task(&e, &mut db, "haystack passport 東京出張");
        new_task(&e, &mut db, "haystack");
        new_task(&e, &mut db, "passport haystack form");
        let read = |db: &Db, text: &str| e.query(
            db,
            Query::SearchGrouped { text: text.to_owned(), now_ms: NOW as u64, prefix, per_group: None },
        ).map(|r| match r {
            QueryResult::Search(r) => *r,
            other => panic!("expected Search, got {other:?}"),
        });
        let texts = [parts.concat(), parts.join(" "), raw];
        let mut first = Vec::new();
        for text in &texts {
            let res = read(&db, text);
            proptest::prop_assert!(res.is_ok(), "{text:?}: {res:?}");
            let res = res.unwrap();
            proptest::prop_assert_eq!(&res, &read(&db, text).unwrap(), "{:?}: a second read", text);
            first.push(res);
        }
        e.rebuild_projection(&mut db).unwrap();
        for (text, before) in texts.iter().zip(&first) {
            proptest::prop_assert_eq!(before, &read(&db, text).unwrap(), "{:?}: after a rebuild", text);
        }
        let found = read(&db, "haystack 東京").unwrap();
        let found: Vec<EntityRef> = found.groups.iter().flat_map(|g| g.hits.iter().map(|h| h.entity)).collect();
        proptest::prop_assert_eq!(found, vec![seeded]);
    }
}

#[test]
fn search_matches_title_and_body_excludes_deleted_and_respects_limit() {
    let mut db = db();
    let e = engine();

    // Title hit.
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "quarterly kangaroo report".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    // Body hit (unique term only in the body).
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "misc notes".into(),
            body: Some(NoteBody(b"remember the kangaroo".to_vec())),
            ..Default::default()
        }),
    )
    .unwrap();
    // Deleted task that would otherwise match.
    let del = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "kangaroo obsolete".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(&mut db, Command::DeleteTask(del)).unwrap();

    let hits = match e
        .query(
            &db,
            Query::Search {
                text: "kangaroo".into(),
                limit: 10,
            },
        )
        .unwrap()
    {
        QueryResult::Tasks(v) => v,
        _ => panic!("wrong variant"),
    };
    let titles: Vec<_> = hits.iter().map(|t| t.title.clone()).collect();
    assert_eq!(
        hits.len(),
        2,
        "title + body hits, deleted excluded: {titles:?}"
    );
    assert!(titles.contains(&"quarterly kangaroo report".to_string()));
    assert!(titles.contains(&"misc notes".to_string()));
    assert!(!titles.contains(&"kangaroo obsolete".to_string()));

    // Limit is honored.
    let limited = match e
        .query(
            &db,
            Query::Search {
                text: "kangaroo".into(),
                limit: 1,
            },
        )
        .unwrap()
    {
        QueryResult::Tasks(v) => v,
        _ => panic!(),
    };
    assert_eq!(limited.len(), 1);

    // Empty / whitespace-only input returns nothing without touching FTS.
    let empty = match e
        .query(
            &db,
            Query::Search {
                text: "   ".into(),
                limit: 10,
            },
        )
        .unwrap()
    {
        QueryResult::Tasks(v) => v,
        _ => panic!(),
    };
    assert!(empty.is_empty());
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/mod.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// `Query::Search` must never surface an FTS5 syntax error, no matter
    /// how hostile the input (quotes, colons, stars, unbalanced quotes,
    /// `NEAR`, hyphens, parentheses).
    #[test]
    fn search_never_errors_on_hostile_input(s in ".*") {
        begin_case();
        let mut db = db();
        let e = engine();
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "haystack".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        let res = e.query(
            &db,
            Query::Search {
                text: s,
                limit: 20,
            },
        );
        proptest::prop_assert!(res.is_ok());
    }
}

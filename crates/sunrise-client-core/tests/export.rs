//! The full export (`docs/04-storage/export-format.md`) of a real vault with
//! every stored kind in it: what the manifest says, that it says it the same
//! way twice, that every record reads back as itself, and that no key leaves.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use sunrise_client_core::export::{export_all, ExportError, ExportOptions};
use sunrise_core::export::{AttachmentState, Manifest, MissingReason, EXPORTED_KINDS};
use sunrise_core::{Clock, Command, Core, CoreConfig, FocusStartDraft, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{
    BlockDraft, ContextDraft, FocusKind, Inline, NoteBlock, PrefTarget, PrefValue,
    ReviewSnapshotDraft, RoutineCatchupPolicy, RoutineDraft, SessionLength, StreamDraft,
    SunriseTime, Task, TaskDraft, TaskTemplate, Unknowns, Weekday,
};

const ROOT: [u8; 32] = [9; 32];

/// 2026-01-01T00:00:00Z, read in New York, so nothing here reads the host.
#[derive(Debug)]
struct Fixed;

impl Clock for Fixed {
    fn now_ms(&self) -> u64 {
        1_767_225_600_000
    }
    fn timezone(&self) -> String {
        "America/New_York".to_owned()
    }
}

async fn open(dir: &Path) -> Core {
    let cfg = CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+test",
        Arc::new(Fixed),
        Arc::new(SystemRng),
    );
    Core::open(
        cfg,
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(ROOT),
            paired: None,
        },
    )
    .await
    .expect("open a scratch vault")
}

/// One of everything the export carries.
async fn seed(core: &Core) {
    let day = jiff::civil::date(2026, 1, 2);
    let stream = core
        .submit(Command::CreateStream(StreamDraft {
            name: "Home".into(),
            ..StreamDraft::default()
        }))
        .await
        .unwrap()
        .entity;
    core.submit(Command::CreateContext(ContextDraft {
        name: "errands".into(),
        ..ContextDraft::default()
    }))
    .await
    .unwrap();
    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Call the plumber".into(),
            stream_id: Some(stream),
            scheduled_at: Some(SunriseTime::floating(day.at(9, 0, 0, 0))),
            due_at: Some(SunriseTime::all_day(day)),
            body: Some(sunrise_domain::encode_note_body(&[NoteBlock::Paragraph {
                inline: vec![Inline::text("Ask about the boiler.")],
            }])),
            ..TaskDraft::default()
        }))
        .await
        .unwrap()
        .entity;
    core.submit(Command::CreateBlock(BlockDraft {
        stream_id: stream,
        starts_at: SunriseTime::zoned(day.at(14, 0, 0, 0), "Europe/Berlin"),
        ends_at: SunriseTime::zoned(day.at(15, 0, 0, 0), "Europe/Berlin"),
        title: Some("Dentist".into()),
        title_track_task: false,
        tasks: Vec::new(),
    }))
    .await
    .unwrap();
    core.submit(Command::CreateRoutine(RoutineDraft {
        template: TaskTemplate {
            title: "Water plants".into(),
            stream_id: stream,
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        },
        rrule: sunrise_domain::RRule::parse("FREQ=WEEKLY").unwrap(),
        timezone: "UTC".into(),
        starts_at: jiff::Timestamp::from_millisecond(1_767_225_600_000)
            .unwrap()
            .into(),
        ends_at: None,
        scheduling_constraints: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
    }))
    .await
    .unwrap();
    core.attach_file(task, "../quote.txt".into(), "text/plain".into(), b"42 EUR")
        .await
        .unwrap();
    core.submit(Command::StartFocus(FocusStartDraft {
        task_id: task,
        kind: FocusKind::Work,
        length: SessionLength::OnePomodoro,
        energy: None,
    }))
    .await
    .unwrap();
    core.submit(Command::SaveReviewSnapshot(ReviewSnapshotDraft {
        window_start_ms: 1_766_620_800_000,
        window_end_ms: 1_767_225_600_000,
        note: Some("A quiet week.".into()),
        ..ReviewSnapshotDraft::default()
    }))
    .await
    .unwrap();
    core.submit(Command::SetPreference {
        key: "week_start".into(),
        value: PrefValue::Weekday(Weekday::Mo),
        target: PrefTarget::Vault,
    })
    .await
    .unwrap();
    core.submit(Command::CreateSavedView(sunrise_domain::SavedViewDraft {
        name: "Errands".into(),
        view: sunrise_domain::ViewKind::Today,
        filter: sunrise_domain::ViewFilter::default(),
        sort: sunrise_domain::ViewSort::Default,
    }))
    .await
    .unwrap();
}

/// Every file under `dir`, by `/`-separated relative path.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(at).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap();
                let rel: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
                out.insert(rel.join("/"), std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn the_manifest_vouches_for_every_file_and_reads_back() {
    let vault = tempfile::tempdir().unwrap();
    let core = open(vault.path()).await;
    seed(&core).await;
    let out = tempfile::tempdir().unwrap();
    let written = export_all(&core, out.path(), ExportOptions::default())
        .await
        .unwrap();

    let files = tree(out.path());
    let manifest = Manifest::from_bytes(&files["manifest.json"]).unwrap();
    assert_eq!(manifest, {
        let mut w = written.clone();
        w.files.sort_by(|a, b| a.path.cmp(&b.path));
        w
    });
    assert_eq!(manifest.exported_at, "2026-01-01T00:00:00Z");
    assert_eq!(manifest.zone, "America/New_York");

    // Every file but the manifest is listed, and every listing verifies.
    let listed: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let on_disk: Vec<&str> = files
        .keys()
        .map(String::as_str)
        .filter(|p| *p != "manifest.json")
        .collect();
    assert_eq!(listed, on_disk);
    for f in &manifest.files {
        assert!(f.matches(&files[&f.path]), "{} does not verify", f.path);
    }

    for kind in EXPORTED_KINDS {
        let body = &files[&format!("entities/{kind}.jsonl")];
        assert!(!body.is_empty(), "the seed vault has a {kind}");
    }
    assert!(files.contains_key("calendar.ics"));
    assert!(String::from_utf8_lossy(&files["calendar.ics"]).contains("SUMMARY:Dentist"));

    // The attachment is here, under its id and a filename that stays put.
    let [att] = manifest.attachments.as_slice() else {
        panic!("one attachment: {:?}", manifest.attachments);
    };
    assert_eq!(att.state, AttachmentState::Present);
    let path = att.path.as_deref().unwrap();
    assert_eq!(path, format!("attachments/{}/_quote.txt", att.id));
    assert_eq!(files[path], b"42 EUR");

    // The note's Markdown projection sits beside its JSON.
    let task = plumber(&lines(&files, "task"));
    let note = &files[&format!("notes/{}.md", task.id.to_str())];
    assert_eq!(note, b"Ask about the boiler.\n");
}

/// The seeded task with the note and the floating plan. The routine
/// materializes another, and ids are random, so the file's order is not it.
fn plumber(tasks: &[Task]) -> Task {
    tasks
        .iter()
        .find(|t| t.title == "Call the plumber")
        .cloned()
        .unwrap_or_else(|| panic!("the seeded task is exported: {tasks:?}"))
}

/// The records of `entities/<kind>.jsonl`, one per line.
fn lines<T: serde::de::DeserializeOwned>(files: &BTreeMap<String, Vec<u8>>, kind: &str) -> Vec<T> {
    files[&format!("entities/{kind}.jsonl")]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

#[tokio::test]
async fn every_record_reads_back_as_itself_with_its_kind() {
    let vault = tempfile::tempdir().unwrap();
    let core = open(vault.path()).await;
    seed(&core).await;
    let out = tempfile::tempdir().unwrap();
    export_all(&core, out.path(), ExportOptions::default())
        .await
        .unwrap();
    let files = tree(out.path());
    let records = core.export_records().unwrap();

    let tasks: Vec<Task> = lines(&files, "task");
    assert_eq!(tasks, records.tasks);
    assert_eq!(
        plumber(&tasks).scheduled_at,
        Some(SunriseTime::floating(
            jiff::civil::date(2026, 1, 2).at(9, 0, 0, 0)
        )),
        "a floating plan is still floating"
    );
    assert_eq!(
        lines::<sunrise_domain::Stream>(&files, "stream"),
        records.streams
    );
    assert_eq!(
        lines::<sunrise_domain::Context>(&files, "context"),
        records.contexts
    );
    assert_eq!(
        lines::<sunrise_domain::Routine>(&files, "routine"),
        records.routines
    );
    let blocks: Vec<sunrise_domain::Block> = lines(&files, "block");
    assert_eq!(
        blocks,
        records
            .blocks
            .iter()
            .map(|b| b.block.clone())
            .collect::<Vec<_>>()
    );
    assert!(matches!(&blocks[0].starts_at, SunriseTime::Zoned { tz, .. } if tz == "Europe/Berlin"));
    assert_eq!(
        lines::<sunrise_domain::FocusSession>(&files, "focus_session"),
        records.focus_sessions
    );
    assert_eq!(
        lines::<sunrise_domain::ReviewSnapshot>(&files, "review_snapshot"),
        records.review_snapshots
    );
    assert_eq!(
        lines::<sunrise_domain::Preferences>(&files, "preferences"),
        records.preferences.into_iter().collect::<Vec<_>>()
    );

    // An attachment is its record without the keys.
    let atts: Vec<serde_json::Value> = lines(&files, "attachment");
    assert_eq!(atts.len(), 1);
    assert!(atts[0].get("blob_key").is_none());
    assert_eq!(
        atts[0]["filename"], "../quote.txt",
        "the stored name is kept"
    );
}

#[tokio::test]
async fn the_same_vault_exports_the_same_bytes() {
    let vault = tempfile::tempdir().unwrap();
    let core = open(vault.path()).await;
    seed(&core).await;
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    export_all(&core, a.path(), ExportOptions::default())
        .await
        .unwrap();
    export_all(&core, b.path(), ExportOptions::default())
        .await
        .unwrap();
    assert_eq!(tree(a.path()), tree(b.path()));
}

#[tokio::test]
async fn no_key_leaves_the_vault() {
    let vault = tempfile::tempdir().unwrap();
    let core = open(vault.path()).await;
    seed(&core).await;
    let out = tempfile::tempdir().unwrap();
    export_all(&core, out.path(), ExportOptions::default())
        .await
        .unwrap();
    let att = core.export_records().unwrap().attachments.remove(0);
    let key = att.blob_key;
    let hex = key.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    let json = serde_json::to_string(&key.to_vec()).unwrap();
    let json = &json[1..json.len() - 1];
    for (path, bytes) in tree(out.path()) {
        for secret in [&key[..], &ROOT[..]] {
            assert!(!contains(&bytes, secret), "{path} holds key bytes");
        }
        assert!(
            !contains(&bytes, hex.as_bytes()),
            "{path} holds the blob key in hex"
        );
        assert!(
            !contains(&bytes, json.as_bytes()),
            "{path} holds the blob key as JSON"
        );
        assert!(!contains(&bytes, b"blob_key"), "{path} names a key field");
    }
}

#[tokio::test]
async fn bytes_that_are_not_here_are_listed_as_missing() {
    let vault = tempfile::tempdir().unwrap();
    let core = open(vault.path()).await;
    seed(&core).await;
    // The metadata stays and the chunks go: a device that has the row and
    // not the bytes. With no sync running, asking for them cannot bring them;
    // not asking says so instead.
    std::fs::remove_dir_all(vault.path().join("blobs")).unwrap();

    for (fetch, reason) in [
        (true, MissingReason::FetchFailed),
        (false, MissingReason::NotFetched),
    ] {
        let out = tempfile::tempdir().unwrap();
        let m = export_all(&core, out.path(), ExportOptions { fetch })
            .await
            .unwrap();
        let [att] = m.attachments.as_slice() else {
            panic!("one attachment");
        };
        assert_eq!(att.state, AttachmentState::Missing);
        assert_eq!(att.missing_reason, Some(reason));
        assert!(att.path.is_none());
        assert!(!out.path().join("attachments").exists());
    }
}

#[tokio::test]
async fn a_destination_with_something_in_it_is_refused() {
    let vault = tempfile::tempdir().unwrap();
    let core = open(vault.path()).await;
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("keep.txt"), b"mine").unwrap();
    let err = export_all(&core, out.path(), ExportOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, ExportError::NotEmpty(_)));
    assert_eq!(tree(out.path()).len(), 1, "nothing was written");

    // A path that does not exist yet is made.
    let fresh = out.path().join("new/export");
    export_all(&core, &fresh, ExportOptions::default())
        .await
        .unwrap();
    assert!(fresh.join("manifest.json").exists());
}

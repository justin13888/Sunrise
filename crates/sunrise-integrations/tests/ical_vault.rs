//! The iCalendar importer and exporter against a **real vault**.
//!
//! The unit tests beside `ical.rs` and `ical_map.rs` pin the syntax and the
//! mapping with no storage in the way. This file is the other half: it opens a
//! `Core` on a temporary `SQLCipher` vault, drives the same fixtures a user's
//! calendar client would hand it, and reads the Blocks back out through the
//! ordinary query path.
//!
//! The fixtures live in `testdata/` and are described by the README beside
//! them; `docs/09-integrations/icalendar.md` §Test surface points here.

use std::path::PathBuf;
use std::sync::Arc;

use sunrise_core::{Clock, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{inbox_stream_ref, SunriseTime};
use sunrise_integrations::ical;
use sunrise_integrations::ical_vault::{export, import, ExportWindow, ICS_SOURCE};

/// A fixed clock. The vault stamps `created_at` / `updated_at` from it, so
/// pinning it keeps every assertion below a function of the fixture alone.
#[derive(Debug)]
struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

/// 2026-03-02T12:00:00Z — inside the week every fixture schedules into.
const NOW_MS: u64 = 1_772_452_800_000;

fn fixture(vendor: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(vendor)
        .join("basic.ics");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

async fn open(dir: &std::path::Path) -> Core {
    let cfg = CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+test",
        Arc::new(FixedClock(NOW_MS)),
        Arc::new(SystemRng),
    );
    Core::open(
        cfg,
        Unlock::DevicePaired(VaultRootKey::from_bytes([3u8; 32])),
    )
    .await
    .expect("open vault")
}

/// Every live Block in the week around [`NOW_MS`].
async fn week_blocks(core: &Core) -> Vec<sunrise_core::queries::BlockRow> {
    let QueryResult::Blocks(rows) = core
        .query(Query::WeekBlocks { week_ms: NOW_MS })
        .await
        .expect("week blocks")
    else {
        panic!("expected blocks");
    };
    rows
}

/// The governing property: a second import of the same bytes must not produce
/// a second calendar.
#[tokio::test]
async fn re_importing_the_same_file_updates_rather_than_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let text = fixture("google");

    let first = import(&core, &text, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("first import");
    assert_eq!(first.created(), 3, "three VEVENTs: {first:?}");
    assert_eq!(first.updated(), 0);
    assert_eq!(first.failed, 0);
    let after_first = week_blocks(&core).await.len();
    assert_eq!(after_first, 3);

    let second = import(&core, &text, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("second import");
    assert_eq!(second.created(), 0, "nothing is new the second time");
    assert_eq!(second.updated(), 3);
    assert_eq!(
        second.block_ids(),
        first.block_ids(),
        "the same UIDs must land on the same Blocks"
    );
    assert_eq!(
        week_blocks(&core).await.len(),
        after_first,
        "a re-import must not grow the calendar"
    );

    core.shutdown().await;
}

/// The other half of the dedup rule from `docs/09-integrations/icalendar.md`:
/// the same UID from a *different* source is a different Block.
#[tokio::test]
async fn the_same_file_under_two_sources_is_two_calendars() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let text = fixture("google");

    import(&core, &text, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    let other = import(&core, &text, inbox_stream_ref(), "team-calendar")
        .await
        .expect("import");
    assert_eq!(
        other.created(),
        3,
        "a second source is a second set of Blocks"
    );
    assert_eq!(week_blocks(&core).await.len(), 6);

    core.shutdown().await;
}

/// An edited event re-imported under the same UID moves the Block it already
/// made, instead of leaving the old range behind next to a new one.
#[tokio::test]
async fn an_edited_event_moves_the_block_it_already_made() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let text = fixture("google");
    let first = import(&core, &text, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");

    let edited = text
        .replace(
            "SUMMARY:Coffee with Alice",
            "SUMMARY:Coffee with Alice and Bo",
        )
        .replace("DTSTART:20260306T140000Z", "DTSTART:20260306T160000Z")
        .replace("DTEND:20260306T150000Z", "DTEND:20260306T170000Z");
    let again = import(&core, &edited, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("re-import");
    assert_eq!(again.created(), 0);
    assert_eq!(again.block_ids(), first.block_ids());

    let rows = week_blocks(&core).await;
    assert_eq!(rows.len(), 3, "still three blocks");
    let moved = rows
        .iter()
        .find(|r| r.title.as_deref() == Some("Coffee with Alice and Bo"))
        .expect("the retitled block");
    assert_eq!(
        moved.block.starts_at,
        SunriseTime::instant("2026-03-06T16:00:00Z".parse().unwrap())
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.title.as_deref() == Some("Coffee with Alice")),
        "the pre-edit block must be gone, not shadowed"
    );

    core.shutdown().await;
}

/// A zoned event has to stay zoned through storage, or "09:00 in New York"
/// silently becomes an instant that drifts at the next DST rule change.
#[tokio::test]
async fn the_time_kinds_survive_a_round_trip_through_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    import(&core, &fixture("google"), inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");

    let rows = week_blocks(&core).await;
    let zoned = rows
        .iter()
        .find(|r| r.title.as_deref() == Some("Quarterly planning"))
        .expect("zoned block");
    assert_eq!(
        zoned.block.starts_at,
        SunriseTime::zoned(
            jiff::civil::date(2026, 3, 2).at(9, 0, 0, 0),
            "America/New_York"
        )
    );

    let all_day = rows
        .iter()
        .find(|r| r.title.as_deref() == Some("Company holiday"))
        .expect("all-day block");
    assert_eq!(
        all_day.block.starts_at,
        SunriseTime::all_day(jiff::civil::date(2026, 3, 4))
    );
    assert_eq!(
        all_day.block.ends_at,
        SunriseTime::all_day(jiff::civil::date(2026, 3, 5)),
        "DTEND is exclusive for a DATE value and stays that way"
    );

    core.shutdown().await;
}

/// Export → import must be the identity. If it were not, a user who exported
/// their week and read it back would double their calendar.
#[tokio::test]
async fn exporting_and_re_importing_is_the_identity() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    import(&core, &fixture("google"), inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    let before = week_blocks(&core).await;

    let doc = export(&core, ExportWindow::Week, NOW_MS)
        .await
        .expect("export");
    assert!(doc.starts_with("BEGIN:VCALENDAR\r\n"));
    assert_eq!(ical::parse(&doc).unwrap().events.len(), before.len());

    let back = import(&core, &doc, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("re-import");
    assert_eq!(
        back.created(),
        0,
        "an exported calendar re-imports onto itself"
    );
    assert_eq!(back.updated(), u32::try_from(before.len()).unwrap());

    let after = week_blocks(&core).await;
    assert_eq!(after.len(), before.len());
    for (a, b) in before.iter().zip(&after) {
        assert_eq!(a.block.id, b.block.id);
        assert_eq!(a.block.starts_at, b.block.starts_at);
        assert_eq!(a.block.ends_at, b.block.ends_at);
        assert_eq!(a.title, b.title);
    }

    core.shutdown().await;
}

/// A day export is the same document narrowed to one civil day.
#[tokio::test]
async fn a_day_export_covers_only_that_day() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    import(&core, &fixture("google"), inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");

    let day = export(&core, ExportWindow::Day, NOW_MS).await.expect("day");
    let events = ical::parse(&day).unwrap().events;
    assert_eq!(events.len(), 1, "only 2026-03-02 has an event: {events:?}");
    assert_eq!(events[0].summary.as_deref(), Some("Quarterly planning"));

    core.shutdown().await;
}

/// A `VTIMEZONE`'s own `DTSTART` must not be read as an event's, and a
/// `VALARM` inside an event must not swallow the event.
#[tokio::test]
async fn an_apple_export_with_vtimezone_and_valarm_imports_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let report = import(&core, &fixture("apple"), inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    assert_eq!(report.created(), 2, "{report:?}");
    assert_eq!(report.failed, 0);

    let rows = week_blocks(&core).await;
    let review = rows
        .iter()
        .find(|r| r.title.as_deref() == Some("Design review"))
        .expect("design review");
    assert_eq!(
        review.block.starts_at,
        SunriseTime::zoned(
            jiff::civil::date(2026, 3, 2).at(9, 30, 0, 0),
            "Europe/Berlin"
        )
    );

    // The second event states DURATION rather than DTEND.
    let one_to_one = rows
        .iter()
        .find(|r| r.title.as_deref() == Some("1:1 with Sam"))
        .expect("1:1");
    assert_eq!(
        one_to_one.block.ends_at,
        SunriseTime::zoned(
            jiff::civil::date(2026, 3, 3).at(14, 45, 0, 0),
            "Europe/Berlin"
        )
    );

    // And both unmodelled components are named, not dropped in silence.
    let dropped: Vec<&str> = report.dropped().map(|n| n.detail.as_str()).collect();
    assert!(
        dropped.iter().any(|d| d.starts_with("VTIMEZONE")),
        "{dropped:?}"
    );
    assert!(
        dropped.iter().any(|d| d.starts_with("VALARM")),
        "{dropped:?}"
    );

    core.shutdown().await;
}

/// A `VTODO` is a Task, not a Block, and this build does not map it. It has to
/// be reported as skipped rather than quietly ignored.
#[tokio::test]
async fn a_vtodo_is_reported_as_unmodelled_rather_than_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let report = import(&core, &fixture("fastmail"), inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    assert_eq!(report.created(), 1, "one VEVENT: {report:?}");
    assert!(
        report.dropped().any(|n| n.detail.starts_with("VTODO")),
        "{:?}",
        report.notices
    );

    // The folded DESCRIPTION reassembled, and was then reported as unheld.
    assert!(report
        .notices
        .iter()
        .any(|n| n.detail.starts_with("DESCRIPTION")));

    // The floating event stays floating: it is "19:00 wherever I am".
    let rows = week_blocks(&core).await;
    assert!(
        rows.iter()
            .any(|r| matches!(r.block.starts_at, SunriseTime::Floating { .. })),
        "the floating event must not have been pinned to an instant"
    );

    core.shutdown().await;
}

/// Outlook writes Windows zone names, which are not IANA. The documented
/// fallback is UTC plus a notice — never a silent reinterpretation.
#[tokio::test]
async fn an_unknown_timezone_falls_back_to_utc_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let report = import(&core, &fixture("outlook"), inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    assert_eq!(report.created(), 2, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.detail.contains("W. Europe Standard Time")),
        "{:?}",
        report.notices
    );

    let rows = week_blocks(&core).await;
    let sprint = rows
        .iter()
        .find(|r| r.title.as_deref() == Some("Sprint review"))
        .expect("sprint review");
    assert!(matches!(
        sprint.block.starts_at,
        SunriseTime::Instant { .. }
    ));

    core.shutdown().await;
}

/// Import survives a restart: the ids are derived, not remembered in process.
#[tokio::test]
async fn dedup_survives_closing_and_reopening_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let text = fixture("google");

    let core = open(dir.path()).await;
    let first = import(&core, &text, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    core.shutdown().await;
    drop(core);

    let core = open(dir.path()).await;
    let second = import(&core, &text, inbox_stream_ref(), ICS_SOURCE)
        .await
        .expect("import");
    assert_eq!(second.created(), 0);
    assert_eq!(second.block_ids(), first.block_ids());
    assert_eq!(week_blocks(&core).await.len(), 3);
    core.shutdown().await;
}

/// Exporting an empty week is a valid, empty document — not an error and not
/// an empty string a caller would have to special-case.
#[tokio::test]
async fn exporting_an_empty_week_yields_an_empty_calendar() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let doc = export(&core, ExportWindow::Week, NOW_MS)
        .await
        .expect("export");
    let parsed = ical::parse(&doc).expect("a well-formed document");
    assert!(parsed.events.is_empty());
    core.shutdown().await;
}

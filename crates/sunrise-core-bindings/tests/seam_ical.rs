//! iCalendar interchange across the seam, against a real vault.
//!
//! `flavor = "multi_thread"` throughout for the reason `seam.rs` gives: the
//! runtime `UniFFI` supplies is multi-threaded, and the core captures it.

use sunrise_core_bindings::{BindingError, CoreCommand};
use sunrise_integrations::ical::NoticeCode;
use sunrise_integrations::ical_vault::ExportWindow;

mod support;
use support::{draft, open_core};

/// An `.ics` with one event starting **now**, so it is inside today and inside
/// this week in every timezone the suite might run in. The seam opens a vault
/// with the production clock, so the fixture is anchored to that same clock
/// rather than to a date that would only work in one week of one year.
fn ics_now(now_ms: u64, uid: &str, summary: &str) -> String {
    let start = jiff::Timestamp::from_millisecond(i64::try_from(now_ms).unwrap())
        .expect("now")
        .round(jiff::Unit::Second)
        .expect("round");
    let end = start + jiff::SignedDuration::from_hours(1);
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//seam//EN\r\n\
BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
DTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        start.strftime("%Y%m%dT%H%M%SZ"),
        end.strftime("%Y%m%dT%H%M%SZ"),
    )
}

/// The property the macOS menu item depends on: a user who imports the same
/// file twice gets one calendar, so the app never has to ask them first.
#[tokio::test(flavor = "multi_thread")]
async fn importing_an_ics_is_idempotent_across_the_seam() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "ev1@example.com", "Quarterly planning");

    let first = core
        .import_ical(text.clone(), None, None)
        .await
        .expect("import");
    assert_eq!(first.created, 1, "{first:?}");
    assert_eq!(first.updated, 0);
    assert_eq!(first.failed, 0);
    assert_eq!(first.blocks.len(), 1);
    assert!(first.blocks[0].block.to_str().starts_with("blk_"));
    assert_eq!(first.blocks[0].title, "Quarterly planning");
    assert!(first.blocks[0].created);

    let second = core.import_ical(text, None, None).await.expect("re-import");
    assert_eq!(second.created, 0, "nothing is new the second time");
    assert_eq!(second.updated, 1);
    assert!(!second.blocks[0].created);
    assert_eq!(
        second.blocks[0].block, first.blocks[0].block,
        "the same UID must land on the same block"
    );
}

/// The counts are the seam's, not the client's, so every client says the same
/// thing about the same file — and a distinct `source` really is a distinct
/// calendar.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_source_is_a_second_calendar() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "shared", "Standup");
    core.import_ical(text.clone(), None, None)
        .await
        .expect("import");
    let other = core
        .import_ical(text, None, Some("team-calendar".into()))
        .await
        .expect("import");
    assert_eq!(other.created, 1);
    assert_ne!(
        other.blocks[0].block,
        core.import_ical(ics_now(core.now_ms(), "shared", "Standup"), None, None)
            .await
            .expect("import")
            .blocks[0]
            .block
    );
}

/// A blank `source` is not a source. It falls back to the default rather than
/// silently keying every import under the empty string.
#[tokio::test(flavor = "multi_thread")]
async fn a_blank_source_falls_back_to_the_default() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "ev1", "Standup");
    let first = core
        .import_ical(text.clone(), None, None)
        .await
        .expect("import");
    let blank = core
        .import_ical(text, None, Some("   ".into()))
        .await
        .expect("import");
    assert_eq!(blank.created, 0);
    assert_eq!(blank.blocks[0].block, first.blocks[0].block);
}

/// The notices have to reach the client, or the app is silently dropping the
/// user's data on their behalf.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_block_cannot_hold_reaches_the_client() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "ev1", "Design review").replace(
        "END:VEVENT",
        "DESCRIPTION:Bring the roadmap\r\nRRULE:FREQ=WEEKLY\r\n\
BEGIN:VALARM\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT",
    );
    let report = core.import_ical(text, None, None).await.expect("import");
    assert_eq!(report.created, 1, "the event still lands");

    let details: Vec<&str> = report.notices.iter().map(|n| n.detail.as_str()).collect();
    assert!(
        details.iter().any(|d| d.starts_with("DESCRIPTION")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.starts_with("RRULE")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.starts_with("VALARM")),
        "{details:?}"
    );
    assert!(report
        .notices
        .iter()
        .any(|n| n.code == NoticeCode::UnsupportedComponent));
    assert!(report
        .notices
        .iter()
        .any(|n| n.code == NoticeCode::UnmappedProperty));
}

/// Export → import is the identity across the seam too, so an app that offers
/// both cannot double a user's calendar.
#[tokio::test(flavor = "multi_thread")]
async fn exporting_and_re_importing_across_the_seam_is_the_identity() {
    let (_dir, core) = open_core().await;
    let first = core
        .import_ical(ics_now(core.now_ms(), "ev1", "Standup"), None, None)
        .await
        .expect("import");

    let doc = core
        .export_ical(ExportWindow::Week, core.now_ms())
        .await
        .expect("export");
    assert!(doc.starts_with("BEGIN:VCALENDAR\r\n"), "got {doc:?}");
    assert!(doc.contains("SUMMARY:Standup"));

    let back = core.import_ical(doc, None, None).await.expect("re-import");
    assert_eq!(
        back.created, 0,
        "an exported calendar re-imports onto itself"
    );
    assert_eq!(back.blocks[0].block, first.blocks[0].block);

    let again = core
        .export_ical(ExportWindow::Week, core.now_ms())
        .await
        .expect("export");
    assert_eq!(again.matches("BEGIN:VEVENT").count(), 1);
}

/// A day export is a narrower window over the same vault, not a different
/// document format.
#[tokio::test(flavor = "multi_thread")]
async fn a_day_export_is_a_valid_document_even_when_empty() {
    let (_dir, core) = open_core().await;
    let empty = core
        .export_ical(ExportWindow::Day, core.now_ms())
        .await
        .expect("export");
    assert!(empty.starts_with("BEGIN:VCALENDAR\r\n"));
    assert_eq!(empty.matches("BEGIN:VEVENT").count(), 0);

    core.import_ical(ics_now(core.now_ms(), "ev1", "Standup"), None, None)
        .await
        .expect("import");
    let one = core
        .export_ical(ExportWindow::Day, core.now_ms())
        .await
        .expect("export");
    assert_eq!(one.matches("BEGIN:VEVENT").count(), 1);
}

/// A file that is not a calendar is a typed error, not an import of zero
/// events that a user would read as success.
#[tokio::test(flavor = "multi_thread")]
async fn text_that_is_not_a_calendar_is_a_typed_error() {
    let (_dir, core) = open_core().await;
    let err = core
        .import_ical("this is not a calendar\n".into(), None, None)
        .await
        .expect_err("refused");
    assert!(matches!(err, BindingError::Calendar(_)), "got {err:?}");
}

/// A calendar imports into a stream. Handing it a task id has to fail at the
/// seam, where the message can still name what was passed.
#[tokio::test(flavor = "multi_thread")]
async fn importing_into_something_that_is_not_a_stream_is_refused() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Renew passport"),
        })
        .await
        .expect("create")
        .entity;
    let err = core
        .import_ical(ics_now(core.now_ms(), "ev1", "Standup"), Some(task), None)
        .await
        .expect_err("refused");
    assert!(matches!(err, BindingError::BadId { .. }), "got {err:?}");
}

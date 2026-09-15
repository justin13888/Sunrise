//! `Core`'s own tests.
//!
//! Kept as one module across the `core/` split, on the same rule
//! [`crate::engine::tests`] follows: the suite shares `cfg`, `unlock` and
//! `vault_footprint`, so moving it whole is what keeps the test diff at zero.
//! `core.rs` sat exactly at its package's size threshold before identity
//! rotation was added to it, and this is the boundary that cost nothing to
//! draw.

use super::*;
use crate::config::SystemRng;
use crate::vault_lock::VaultLockError;
use parking_lot::Mutex as PLMutex;
use std::sync::Arc;
use sunrise_crypto::keys::VaultRootKey;

#[derive(Debug)]
struct FakeClock(PLMutex<u64>);
impl crate::config::Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock()
    }
}

fn cfg(dir: &std::path::Path) -> CoreConfig {
    CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+test",
        Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
        Arc::new(SystemRng),
    )
}

fn unlock() -> Unlock {
    Unlock::DevicePaired {
        root: VaultRootKey::from_bytes([1u8; 32]),
        paired: None,
    }
}

/// Every durable trace an `export_pairing_payload` could leave: the op
/// log, the outbox, and the key rows.
fn vault_footprint(core: &Core) -> (i64, i64, i64) {
    let db = core.db.lock();
    let conn = db.conn();
    let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    (
        one("SELECT count(*) FROM ops"),
        one("SELECT count(*) FROM outbox"),
        one("SELECT count(*) FROM stream_keys"),
    )
}

/// **Opening a pairing screen must not change the vault** (issue #106).
///
/// `export_pairing_payload` minted the account's base epochs for one
/// revision, which put `key_envelope` ops in the log and rows in the outbox
/// — fanned out to every other device — for a user who might look at a QR
/// code and close it. The epochs are now established at `Core::open`, so
/// this call reads.
///
/// The three counters are compared rather than one because the write took
/// three forms: a `stream_keys` row, an op, and an outbox entry.
#[tokio::test]
async fn export_pairing_payload_does_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();

    let before = vault_footprint(&core);
    let payload = core.export_pairing_payload().unwrap();
    let again = core.export_pairing_payload().unwrap();
    let after = vault_footprint(&core);

    assert_eq!(
        before, after,
        "assembling a pairing payload must leave the op log, the outbox and \
         the key rows exactly as it found them"
    );
    assert_eq!(payload.stream_keys, again.stream_keys);
    core.close().await.unwrap();
}

/// The other half: the payload is still *complete* without that write.
///
/// A vault that has never been written to holds no Stream keys unless
/// something mints them, and a device paired from an empty payload can read
/// no control op at all — so it cannot even learn what it is missing. That
/// is why the mint existed. It now happens at open, and this pins the
/// property the move must not lose.
#[tokio::test]
async fn a_freshly_opened_vault_already_carries_its_base_epochs() {
    use sunrise_domain::INBOX_STREAM_BYTES;

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let payload = core.export_pairing_payload().unwrap();

    assert!(
        payload.stream_keys.contains_key(&[0u8; 16]),
        "the vault-meta key must travel, or the paired device reads no \
         control op ever"
    );
    assert!(
        payload.stream_keys.contains_key(&INBOX_STREAM_BYTES),
        "the Inbox is the one stream every account has"
    );
    core.close().await.unwrap();
}

#[tokio::test]
async fn open_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    // Sync status query is wired and works without a real engine.
    let qr = core.query(Query::SyncStatus).await.unwrap();
    assert!(matches!(qr, QueryResult::SyncStatus(_)));
    core.close().await.unwrap();
}

/// `Core::submit` classifies every command into a `DomainEvent` by an
/// explicit match with a `_ => Updated` fallback, so a new command that
/// creates or deletes something is silently reported as an *update* unless
/// its arm is added. A client driving its view off `changes()` would then
/// never learn a Block or an Attachment appeared.
#[tokio::test]
async fn create_and_delete_commands_publish_the_right_change_event() {
    use sunrise_domain::inbox::inbox_stream_ref;
    use sunrise_domain::{AttachmentDraft, BlockDraft, SunriseTime, TaskDraft};

    async fn next(rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>) -> DomainEvent {
        rx.recv().await.expect("an event")
    }

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let mut events = core.changes();

    let now = jiff::Timestamp::from_millisecond(1_700_000_000_000).unwrap();
    let hour = jiff::SignedDuration::from_hours(1);

    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Write the report".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == task));

    let block = core
        .submit(Command::CreateBlock(BlockDraft {
            stream_id: inbox_stream_ref(),
            starts_at: SunriseTime::instant(now),
            ends_at: SunriseTime::instant(now + hour),
            title: Some("Deep work".into()),
            title_track_task: false,
            tasks: Vec::new(),
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == block));

    core.submit(Command::BindTask { block, task })
        .await
        .unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Updated(id) if id == block));

    let attachment = core
        .submit(Command::AttachFile(AttachmentDraft {
            parent: task,
            filename: "receipt.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 4096,
            blob_key: [7u8; 32],
            blob_id: [9u8; 16],
            chunk_count: 1,
            content_hash: [11u8; 32],
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == attachment));

    core.submit(Command::DetachFile(attachment)).await.unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == attachment));

    core.submit(Command::DeleteBlock(block)).await.unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == block));

    core.close().await.unwrap();
}

/// The other eight arms of the same match.
///
/// `create_and_delete_commands_publish_the_right_change_event` covers six
/// of the fourteen enumerated commands. The remaining eight were carried
/// entirely by the `_ => DomainEvent::Updated(res.entity)` fallback, which
/// is exactly the shape that absorbs a dropped arm without a compile error:
/// delete a `Command::DeleteStream(_)` from the `Deleted` list and the
/// stream's disappearance is published as an *update*, so a client driving
/// its view off `changes()` leaves the row on screen.
#[tokio::test]
async fn every_create_and_delete_command_is_classified_by_its_own_arm() {
    use sunrise_domain::inbox::inbox_stream_ref;
    use sunrise_domain::{
        ContextDraft, FocusKind, RRule, RoutineCatchupPolicy, RoutineDraft, SessionLength,
        StreamDraft, TaskDraft, TaskTemplate,
    };

    async fn next(rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>) -> DomainEvent {
        rx.recv().await.expect("an event")
    }

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let mut events = core.changes();
    let now_ms = core.now_ms();

    // ---- the four `Created` arms this test owns ----

    let stream = core
        .submit(Command::CreateStream(StreamDraft {
            name: "Ops".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == stream));

    let context = core
        .submit(Command::CreateContext(ContextDraft {
            name: "errand".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == context));

    let routine = core
        .submit(Command::CreateRoutine(RoutineDraft {
            template: TaskTemplate {
                title: "Water plants".into(),
                stream_id: inbox_stream_ref(),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: jiff::Timestamp::from_millisecond(i64::try_from(now_ms).unwrap()).unwrap(),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == routine));

    let focus_task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Focus on this".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == focus_task));

    let session = core
        .submit(Command::StartFocus(crate::commands::FocusStartDraft {
            task_id: focus_task,
            kind: FocusKind::Work,
            length: SessionLength::OnePomodoro,
            energy: None,
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == session));

    // ---- the four `Deleted` arms ----

    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Doomed".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == task));

    core.submit(Command::DeleteTask(task)).await.unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == task));

    core.submit(Command::DeleteRoutine(routine)).await.unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == routine));

    core.submit(Command::DeleteContext(context)).await.unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == context));

    core.submit(Command::DeleteStream(stream)).await.unwrap();
    assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == stream));

    core.close().await.unwrap();
}

/// The three notification reads answer through `Core`, not only through the
/// engine — which is the surface both clients actually call.
#[tokio::test]
async fn the_notification_reads_answer_through_the_core() {
    use sunrise_domain::ReminderSettings;

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let now_ms = core.now_ms();

    assert!(matches!(
        core.query(Query::MorningSummary { now_ms }).await.unwrap(),
        QueryResult::MorningSummary(_)
    ));
    assert!(matches!(
        core.query(Query::EndOfDayPlan { now_ms }).await.unwrap(),
        QueryResult::EndOfDayPlan(_)
    ));
    assert!(matches!(
        core.query(Query::ReminderIntents {
            now_ms,
            horizon_ms: now_ms + 86_400_000,
            settings: ReminderSettings::default(),
        })
        .await
        .unwrap(),
        QueryResult::Reminders(_)
    ));
    core.close().await.unwrap();
}

/// `docs/08-features/recurrence-engine.md` requires materialization on a
/// periodic timer, not only at launch. Without it a long-running client
/// stops generating occurrences once it passes the horizon computed at
/// startup — a TUI left open over a weekend simply goes quiet.
///
/// Drives it with tokio's paused clock so the test is instant and
/// deterministic: the injected `FakeClock` supplies domain time, and
/// `tokio::time::advance` fires the timer.
#[tokio::test(start_paused = true)]
async fn routine_timer_materializes_past_the_launch_horizon() {
    use sunrise_domain::inbox::inbox_stream_ref;
    use sunrise_domain::routine::TaskTemplate;
    use sunrise_domain::rrule::RRule;
    use sunrise_domain::{RoutineCatchupPolicy, RoutineDraft};

    let dir = tempfile::tempdir().unwrap();
    let start_ms = 1_700_000_000_000u64;
    let clock = Arc::new(FakeClock(PLMutex::new(start_ms)));
    let cfg = CoreConfig::with_clock(
        dir.path().to_path_buf(),
        "0.1.0+test",
        clock.clone(),
        Arc::new(SystemRng),
    );
    let core = Arc::new(Core::open(cfg, unlock()).await.unwrap());

    core.submit(Command::CreateRoutine(RoutineDraft {
        template: TaskTemplate {
            title: "Water plants".into(),
            stream_id: inbox_stream_ref(),
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
        },
        rrule: RRule::parse("FREQ=DAILY").unwrap(),
        timezone: "UTC".into(),
        starts_at: jiff::Timestamp::from_millisecond(i64::try_from(start_ms).unwrap()).unwrap(),
        ends_at: None,
        scheduling_constraints: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
    }))
    .await
    .unwrap();

    let count = |core: &Arc<Core>| {
        let db = core.db.lock();
        db.conn()
            .query_row("SELECT COUNT(*) FROM tasks WHERE deleted = 0", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
    };
    let at_launch = count(&core);
    assert!(
        at_launch > 0,
        "creating a routine must materialize a horizon"
    );

    core.start_routine_timer(Duration::from_secs(60)).unwrap();

    // Move domain time well past the launch horizon, then let the timer run.
    *clock.0.lock() = start_ms + 90 * 24 * 60 * 60 * 1000;
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;
    for _ in 0..50 {
        if count(&core) > at_launch {
            break;
        }
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
    }

    assert!(
        count(&core) > at_launch,
        "the periodic timer must extend the horizon; had {at_launch}, still {} after ticks",
        count(&core)
    );
    core.shutdown().await;
}

/// `Core::capture` must resolve `#stream` against the vault's real streams,
/// which is the whole reason it exists rather than callers invoking the
/// domain parser directly.
#[tokio::test]
async fn capture_resolves_streams_from_the_vault() {
    use sunrise_domain::capture::Unresolved;
    use sunrise_domain::StreamDraft;

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let created = core
        .submit(Command::CreateStream(StreamDraft {
            name: "travel".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let stream_id = created.entity;

    let tz = jiff::tz::TimeZone::UTC;
    let c = core
        .capture("Renew passport #travel !2", &tz)
        .await
        .unwrap();
    assert_eq!(c.draft.title, "Renew passport");
    assert_eq!(c.draft.stream_id, Some(stream_id));
    assert_eq!(c.draft.priority, Some(2));
    assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);

    // The parsed draft must be directly submittable — the point of the API.
    core.submit(Command::CreateTask(c.draft)).await.unwrap();

    // An unknown stream is reported, not silently dropped.
    let c2 = core.capture("Something #nope", &tz).await.unwrap();
    assert_eq!(c2.draft.stream_id, None);
    assert!(matches!(
        c2.unresolved.as_slice(),
        [Unresolved::UnknownStream(_)]
    ));

    // The synthetic Inbox row is resolvable by name too.
    let c3 = core.capture("Triage me #inbox", &tz).await.unwrap();
    assert_eq!(
        c3.draft.stream_id,
        Some(sunrise_domain::inbox::inbox_stream_ref())
    );

    core.close().await.unwrap();
}

/// `@context` must resolve end to end: create a Context, capture a line
/// mentioning it, submit the draft, and find the task carrying it.
#[tokio::test]
async fn capture_resolves_contexts_from_the_vault() {
    use sunrise_domain::capture::Unresolved;
    use sunrise_domain::{ContextDraft, ContextPatch};

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let tz = jiff::tz::TimeZone::UTC;

    // Before the Context exists, `@errands` is reported, not guessed at,
    // and its text survives in the title.
    let miss = core.capture("Buy milk @errands", &tz).await.unwrap();
    assert!(miss.draft.contexts.is_empty());
    assert_eq!(miss.draft.title, "Buy milk @errands");
    assert!(matches!(
        miss.unresolved.as_slice(),
        [Unresolved::UnknownContext(n)] if n == "errands"
    ));

    let ctx = core
        .submit(Command::CreateContext(ContextDraft {
            name: "errands".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;

    let c = core.capture("Buy milk @errands !2", &tz).await.unwrap();
    assert_eq!(c.draft.title, "Buy milk");
    assert_eq!(c.draft.contexts, vec![ctx]);
    assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);

    // The parsed draft is directly submittable, and the task keeps the tag.
    let task = core
        .submit(Command::CreateTask(c.draft))
        .await
        .unwrap()
        .entity;
    match core.query(Query::EntityById(task)).await.unwrap() {
        QueryResult::Task(t) => {
            assert!(t.contexts.contains(&ctx), "the task carries @errands");
        }
        other => panic!("expected Task, got {other:?}"),
    }

    // Archiving takes it back out of capture resolution.
    core.submit(Command::UpdateContext {
        id: ctx,
        patch: ContextPatch {
            archived: Some(true),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let after = core.capture("Buy bread @errands", &tz).await.unwrap();
    assert!(after.draft.contexts.is_empty());
    assert!(matches!(
        after.unresolved.as_slice(),
        [Unresolved::UnknownContext(_)]
    ));

    core.close().await.unwrap();
}

/// Starting the timer twice must not spawn two tasks.
#[tokio::test(start_paused = true)]
async fn routine_timer_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let core = Arc::new(Core::open(cfg(dir.path()), unlock()).await.unwrap());
    core.start_routine_timer(Duration::from_secs(60)).unwrap();
    core.start_routine_timer(Duration::from_secs(60)).unwrap();
    assert!(core.routine_handle.lock().is_some());
    core.shutdown().await;
    assert!(
        core.routine_handle.lock().is_none(),
        "shutdown must reap the timer task, not leak it"
    );
}

#[tokio::test]
async fn identity_persists_and_outbox_hydrates_across_reopen() {
    use sunrise_domain::TaskDraft;
    let dir = tempfile::tempdir().unwrap();

    let device_id_first;
    let pending_before_close;
    {
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        // Not zero: opening a vault mints the account's base epochs,
        // publishes this device's certificate, and queues the
        // identity-sealed copies of those first Stream keys like any other
        // op.
        let announced = core.sync_pending().unwrap();
        assert!(announced > 0, "the vault announces itself at open");
        core.submit(Command::CreateTask(TaskDraft {
            title: "persisted".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
        pending_before_close = core.sync_pending().unwrap();
        device_id_first = match core.query(Query::SyncStatus).await.unwrap() {
            QueryResult::SyncStatus(s) => {
                // One more, not two: the Inbox's key is minted at open
                // along with the vault-meta one, so the first task in it
                // finds a key already there and queues only itself. It was
                // two while the Inbox epoch was minted lazily by whatever
                // first wrote to that stream.
                assert_eq!(
                    u64::from(s.outbox_pending),
                    announced + 1,
                    "only the task itself is newly pending"
                );
                // Read the device id straight from the vault for comparison.
                let db = core.db.lock();
                db.conn()
                    .query_row(
                        "SELECT device_id FROM local_identity WHERE id = 1",
                        [],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .unwrap()
            }
            _ => panic!("expected sync status"),
        };
        core.close().await.unwrap();
    }

    // Reopen: the same device identity loads, and the unacked outbox row
    // hydrates from disk.
    let core2 = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let device_id_second = {
        let db = core2.db.lock();
        db.conn()
            .query_row(
                "SELECT device_id FROM local_identity WHERE id = 1",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .unwrap()
    };
    assert_eq!(
        device_id_first, device_id_second,
        "same device id on reopen"
    );
    match core2.query(Query::SyncStatus).await.unwrap() {
        // The outbox hydrates from the DB, so the reopened vault sees
        // exactly what the first one left pending — announcement, key
        // envelope and task alike. The reopen itself adds nothing: the
        // certificate is published once per vault, not once per open.
        QueryResult::SyncStatus(s) => {
            assert_eq!(u64::from(s.outbox_pending), pending_before_close);
        }
        _ => panic!("expected sync status"),
    }
    core2.close().await.unwrap();
}

#[tokio::test]
async fn second_open_blocked_by_vault_lock() {
    let dir = tempfile::tempdir().unwrap();
    let _core1 = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let res = Core::open(cfg(dir.path()), unlock()).await;
    assert!(matches!(
        res,
        Err(CoreError::VaultLock(VaultLockError::AlreadyHeld { .. }))
    ));
}

/// Every writing and reading entry point refuses once the core is closed.
///
/// `CoreError::Closed` had no assertion anywhere, for all three of its
/// guards. This test called nothing at all and asserted nothing: it opened
/// a core, closed it, opened a second one and dropped it, with a comment
/// explaining that `close(&mut self)` consumed `self` so there was no way
/// to call anything on a closed handle. `Core::shutdown(&self)` is that
/// way — it is the shutdown path `Arc<Core>` needs and sets the same mark
/// — so the branch is reachable and asserted rather than documented.
///
/// `apply_remote_all` is handed garbage bytes deliberately: the guard has
/// to run *before* the envelope is decoded, or a closed core would still
/// report a parse error for work it should have refused outright.
#[tokio::test]
async fn every_entry_point_refuses_after_shutdown() {
    use sunrise_domain::TaskDraft;

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();

    // The same calls succeed while the core is open, so the errors below
    // are the closed mark and not a broken fixture.
    core.submit(Command::CreateTask(TaskDraft {
        title: "before".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    core.query(Query::Inbox).await.unwrap();

    core.shutdown().await;

    assert!(matches!(
        core.submit(Command::CreateTask(TaskDraft {
            title: "after".into(),
            ..Default::default()
        }))
        .await,
        Err(CoreError::Closed)
    ));
    assert!(matches!(
        core.query(Query::Inbox).await,
        Err(CoreError::Closed)
    ));
    assert!(matches!(
        core.apply_remote_all(b"not an envelope").await,
        Err(CoreError::Closed)
    ));

    // Idempotent, per `shutdown`'s contract.
    core.shutdown().await;
    assert!(matches!(
        core.query(Query::Inbox).await,
        Err(CoreError::Closed)
    ));
}

/// Both observer channels actually deliver.
///
/// This subscribed to `changes()` and `sync_status()` and dropped both
/// receivers, so it held whether `broadcast::Sender::subscribe` returns
/// and nothing else: a `submit` that published no event, or a status
/// change that reached no subscriber, passed it. A client driving its view
/// off these two channels is the only consumer either one has.
#[tokio::test]
async fn both_observer_channels_deliver_to_their_subscribers() {
    use sunrise_domain::TaskDraft;
    use sunrise_sync::SyncState;

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let mut changes = core.changes();
    let mut status = core.sync_status();

    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "observed".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    // Bounded, so a channel that publishes nothing fails the test rather
    // than hanging the suite.
    let wait = std::time::Duration::from_secs(5);
    let event = tokio::time::timeout(wait, changes.recv())
        .await
        .expect("submit published a change event")
        .expect("a change event");
    assert!(matches!(event, DomainEvent::Created(id) if id == task));

    core.sync_shared.set_state(SyncState::CatchingUp);
    let snapshot = tokio::time::timeout(wait, status.recv())
        .await
        .expect("a state change published a status event")
        .expect("a status event");
    assert_eq!(snapshot.state, SyncState::CatchingUp);
    // Pinned as it behaves today, not as it reads: the depth in a
    // broadcast snapshot is whatever `SyncShared` was last told, and only
    // the sync driver ever calls `set_pending`. With no driver running it
    // is still the count taken at `Core::open`, so it does NOT include the
    // submit above — while `Query::SyncStatus` reads the outbox directly
    // and does. The two disagree by exactly the rows submitted since open.
    let from_query = match core.query(Query::SyncStatus).await.unwrap() {
        QueryResult::SyncStatus(s) => s,
        other => panic!("expected SyncStatus, got {other:?}"),
    };
    assert_eq!(
        u64::from(from_query.outbox_pending),
        core.sync_pending().unwrap(),
        "the query path reads outbox depth from the database"
    );
    assert_eq!(
        u64::from(snapshot.outbox_pending) + 1,
        u64::from(from_query.outbox_pending),
        "and the broadcast snapshot is behind it by the one submit, because \
         nothing off the driver path republishes the depth"
    );

    core.close().await.unwrap();
}

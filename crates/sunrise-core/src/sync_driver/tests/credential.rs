//! How a renewed bearer reaches the relay: in band on a live session, at the
//! connect that carries it, and what each route reports.

use super::*;

/// A renewal written while a session is live reaches the relay **in that
/// session**, as a `0x12 RefreshToken` frame.
///
/// The alternative — let the token expire, take the `AUTH_TOKEN_EXPIRED`
/// close, reconnect — costs a full handshake, a re-subscribe, and a
/// catch-up window in which the client is not live, all on a schedule the
/// client already knew in advance. That is the case this closes.
#[tokio::test(start_paused = true)]
async fn a_renewed_bearer_reaches_a_live_session_without_reconnecting() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    let mut status_rx = core.sync_status();
    let (factory, server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
    core.start_sync(factory).unwrap();
    let _ = collect_until_live(&mut status_rx).await;
    let connects_when_live = server.lock().connect_count;

    credential.set(Some("renewed-token".into()));

    tokio::time::timeout(Duration::from_secs(10), async {
        while refreshes.lock().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no RefreshToken frame arrived");
    assert_eq!(
        refreshes.lock().as_slice(),
        ["renewed-token".to_string()],
        "the relay is told the new bearer, exactly once"
    );
    assert_eq!(
        server.lock().connect_count,
        connects_when_live,
        "and without tearing the session down to do it"
    );
    core.shutdown().await;
}

/// A renewal that landed while the driver was disconnected is consumed by
/// the connect that carried it, and that consumption is reported once.
///
/// The driver's one handle only ever falls behind between sessions: while
/// a session is up the pump consumes every write. The connect that ends
/// that gap reads the credential itself, so the relay has already been
/// given the new bearer in the `Authorization` header by the time the pump
/// starts, and a lagging handle would spend a round trip re-presenting it.
///
/// This drives the two seams the driver composes — the mark in `run` and
/// the report in `session` — in the order and with the state it composes
/// them in. Where each is *called* is pinned by the driver-level tests
/// below: the mark after the factory, by
/// [`a_renewal_landing_during_the_dial_still_reaches_the_live_session`],
/// and the report after the handshake, by
/// [`the_report_waits_for_the_handshake_not_for_the_connect`].
#[tokio::test]
async fn a_connect_that_consumed_a_renewal_logs_it_once() {
    let credential = TokenSource::new(Some("first-token".into()));
    let mut renewals = credential.watch();
    // The disconnected window: two renewals, no session to observe them.
    credential.set(Some("second-token".into()));
    credential.set(Some("third-token".into()));

    let mut marked = None;
    let carried = credential.read().version();
    let events = captured_events(|| {
        marked = mark_renewals_current(&mut renewals, carried, marked);
        note_marked_at_connect(marked.take());
    });
    assert_eq!(
        events,
        vec![LoggedEvent {
            ev: Some("sync.credential.marked_at_connect".into()),
            to_v: Some(2),
            ..LoggedEvent::default()
        }],
        "the connect carried both writes, and the event says which version it reached"
    );
    // One poll, not a timed wait: `changed` resolves on its first poll for
    // any write this handle has not observed, so a pending first poll is
    // the handle being current, whatever the host's load.
    let mut next = std::pin::pin!(renewals.changed());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(next.as_mut(), &mut cx).is_pending(),
        "the pump must not re-announce a bearer this connect already presented"
    );
}

/// The event's silent branch, in the shape that made the version
/// comparison this replaced a false positive.
///
/// A renewal announced **in band** by a live session has already been
/// consumed by the pump, so the reconnect that follows brings nothing
/// forward and must say nothing. The guard this replaced compared two
/// *source* versions taken at two connects, and the source's counter does
/// not go back down when the pump consumes a write: it fired here, saying
/// "connect carried a renewal from the offline window" about a renewal the
/// previous session had already announced. With a 45-minute renewal
/// cadence against long-lived sessions that is the ordinary path, not an
/// edge.
#[tokio::test]
async fn a_renewal_announced_in_band_is_not_claimed_by_the_next_connect() {
    let credential = TokenSource::new(Some("first-token".into()));
    let mut renewals = credential.watch();
    let mut marked = mark_renewals_current(&mut renewals, credential.read().version(), None);
    note_marked_at_connect(marked.take());

    // The session is live, and its pump consumes the renewal in band.
    credential.set(Some("second-token".into()));
    assert_eq!(renewals.changed().await, 1, "the pump sees the write");

    // The session drops; the driver reconnects with nothing outstanding.
    let carried = credential.read().version();
    let events = captured_events(|| {
        marked = mark_renewals_current(&mut renewals, carried, marked);
        note_marked_at_connect(marked.take());
    });
    assert!(
        events.is_empty(),
        "this connect brought nothing forward and must claim nothing, saw {events:?}"
    );
}

/// A renewal consumed by an attempt that never connected is reported
/// against the attempt that did.
///
/// The mark runs before the dial, so a renewal landing during backoff is
/// consumed by an attempt that may then fail. That is sound for the handle
/// — the next attempt re-reads the credential and carries at least as much
/// — and it is the report that has to follow the bearer. Emitted before the
/// relay has answered, the event lands between `sync.session.closed` and
/// `sync.backoff` for an attempt whose header never reached it, and the
/// attempt that did get through finds the handle already current and stays
/// silent.
#[tokio::test]
async fn a_renewal_consumed_by_a_failed_attempt_is_reported_by_the_next_connect() {
    let credential = TokenSource::new(Some("first-token".into()));
    let mut renewals = credential.watch();
    // A renewal lands while the driver sits in backoff.
    credential.set(Some("second-token".into()));

    let mut marked = None;
    let carried = credential.read().version();
    let during_the_failure = captured_events(|| {
        marked = mark_renewals_current(&mut renewals, carried, marked);
    });
    assert!(
        during_the_failure.is_empty(),
        "an attempt that never dialled presented no bearer, saw {during_the_failure:?}"
    );

    // The next attempt re-reads the credential, so it carries the same
    // renewal — and finds the handle already current, with nothing of its
    // own to bring forward.
    let on_the_connect = captured_events(|| {
        marked = mark_renewals_current(&mut renewals, carried, marked);
        note_marked_at_connect(marked.take());
    });
    assert_eq!(
        on_the_connect,
        vec![LoggedEvent {
            ev: Some("sync.credential.marked_at_connect".into()),
            to_v: Some(1),
            ..LoggedEvent::default()
        }],
        "the connect that actually presented the bearer is the one that reports it"
    );
}

/// Driver-level: the report follows the **handshake**, not the connect.
///
/// `Ok` from a [`ConnectFuture`] is a transport object. No factory in this
/// workspace can resolve one to `Err` — the shipped two build an
/// `SseTransport`, which dials nothing — so a report made in `run`'s `Ok`
/// arm would be made by every attempt, including the ones the relay never
/// answers. Here attempt 2 consumes the renewal and then loses its
/// handshake against a peer that is already gone; attempt 3 is the one the
/// relay answers, and it is the one that speaks.
///
/// The renewal is written inside attempt 1's factory call — after `run`
/// read attempt 1's credential, so attempt 1 did not carry it and its mark
/// leaves it pending — and attempt 2's read is the first to carry it. That
/// is the offline window placed by ordering rather than by a clock.
///
/// Four mutations this fails that nothing else catches: moving
/// `note_marked_at_connect` back into `run`'s `Ok` arm (the event lands
/// *before* attempt 2's `sync.session.closed`), deleting the call (no
/// event at all), dropping the carry-forward — passing `None` to
/// `mark_renewals_current`, or `renewals.mark_carried(carried)` without
/// the `.or(unreported)` — after which attempt 3 finds the handle current
/// and nothing is ever reported, and **dropping the `.take()`**, after
/// which the mark stays outstanding and session 4 reports the same version
/// a second time.
///
/// The fourth session is what makes that last one reachable. Session 3 is
/// scripted to drop after its subscribe, because `run_fake_server` runs
/// until a `Close` or a dead peer and would otherwise hold the driver in
/// session 3 until shutdown, leaving nothing to re-report into.
///
/// The driver is built here rather than through `Core::start_sync` so a
/// subscriber can be attached to `run`'s own future. `captured_events`
/// cannot reach it: `tracing::subscriber::with_default` is thread-local
/// and `start_sync` spawns.
// Three scripted sessions and the connect-count assertion put this one
// six lines past the limit; `session` above carries the same allow.
#[allow(clippy::too_many_lines)]
#[tokio::test(start_paused = true)]
async fn the_report_waits_for_the_handshake_not_for_the_connect() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    // One script, and attempts 1 and 2 never reach `inner` — each returns
    // its own dead-peer transport — so the script is popped by attempt 3.
    // Attempt 4 gets `Script::default()` and stays up until shutdown.
    let (inner, server, _batch_rx, subs, _refreshes) = harness_full(
        vec![Script {
            close_after_subscribe: true,
            ..Script::default()
        }],
        true,
    );
    let attempts = Arc::new(AtomicU64::new(0));
    let renewing = credential.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        let n = attempts.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // After `run` read this attempt's credential, so attempt 1
            // does not carry it and attempt 2 is the first that does.
            renewing.set(Some("renewed-token".into()));
        }
        if n < 2 {
            // A transport whose peer is already gone. The connect future
            // still resolves `Ok` — that is the whole point — and the
            // `Hello` is what fails.
            let (client, server) = duplex();
            drop(server);
            return Box::pin(async move { Ok(Box::new(client) as BoxTransport) }) as ConnectFuture;
        }
        inner(read)
    });

    let log = EventLog::default();
    let (status_tx, _status_rx) = broadcast::channel(64);
    let shared = SyncShared::new(status_tx, 0);
    shared.mark_active();
    let rng: Arc<dyn Rng> = Arc::new(SystemRng);
    let driver = tokio::spawn(
        run(Arc::downgrade(&core), Arc::clone(&shared), factory, rng)
            .with_subscriber(tracing_subscriber::registry().with(log.clone())),
    );

    // Wait for the report, and then for session 4 to be past the point at
    // which it could repeat it. `session` reports immediately after the
    // handshake and subscribes immediately after that, so the second
    // Subscribe frame the fake relay sees is session 4's, and by then
    // session 4 has already had its turn to speak.
    timeout(Duration::from_secs(10), async {
        loop {
            let seen = log
                .0
                .lock()
                .iter()
                .any(|e| e.ev.as_deref() == Some(MARKED_AT_CONNECT));
            if seen && subs.load(Ordering::SeqCst) >= 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no attempt reported the renewal it consumed, or no fourth session opened");

    shared.request_shutdown();
    timeout(Duration::from_secs(10), driver)
        .await
        .expect("the driver never stopped")
        .unwrap();

    // `subs >= 2` above is a proxy for "session 4 opened", and it is sound
    // only via three properties nothing here states. Say it in the
    // harness's own counter instead. **Two, not four**: `connect_count`
    // is bumped inside `harness_full`'s closure, and attempts 1 and 2
    // return their own dead-peer duplex without ever calling `inner()`, so
    // the harness sees attempts 3 and 4 and nothing else.
    {
        let seen = server.lock();
        assert_eq!(
            seen.connect_count, 2,
            "sessions 3 and 4 must both have been built through the harness, or \
             `reports.len() == 1` below proves nothing about repetition"
        );
        assert_eq!(
            seen.presented,
            [Some("renewed-token".into()), Some("renewed-token".into())],
            "both answered sessions presented the renewal"
        );
    }

    let events = log.0.lock().clone();
    let reports: Vec<&LoggedEvent> = events
        .iter()
        .filter(|e| e.ev.as_deref() == Some(MARKED_AT_CONNECT))
        .collect();
    assert_eq!(
        reports.len(),
        1,
        "the carried mark is reported exactly once — session 4 handshook too \
         and must not repeat it, saw {events:?}"
    );
    assert_eq!(
        reports[0].to_v,
        Some(1),
        "and it names the version the failed attempt consumed"
    );
    let names: Vec<&str> = events.iter().filter_map(|e| e.ev.as_deref()).collect();
    let failed = names
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == "sync.session.closed")
        .nth(1)
        .map(|(i, _)| i)
        .expect("attempt 2's session must have closed");
    let reported = names
        .iter()
        .position(|n| *n == MARKED_AT_CONNECT)
        .expect("the report must be in the log");
    assert!(
        reported > failed,
        "the report belongs to the attempt the relay answered, not the one it did not: {names:?}"
    );
    core.shutdown().await;
}

/// Driver-level: an attempt the relay never answers climbs the reconnect
/// schedule, and one it does answer starts the next reconnect from its
/// first step (#283).
///
/// `Backoff` and `next_backoff_delay` are tested in isolation elsewhere,
/// and both were correct while `run` retried every ~100 ms forever: the
/// defect was in the composition, where a `reset` in the connect-`Ok` arm
/// — taken on every attempt, since no factory here resolves `Err` — zeroed
/// the counter before it could advance. So this drives `run` itself and
/// reads the `sync.backoff` events an operator would.
///
/// Attempts 1-3 and 5-10 get a transport whose peer is already gone, so
/// the connect resolves `Ok` and the `Hello` fails; attempt 4 handshakes
/// with the fake relay and is dropped after its subscribe. The expected
/// `attempt` sequence is therefore `1, 2, 3` (climbing), `1` (reset by
/// attempt 4's handshake), `2, 3, 4, 5` (climbing), `0` (exhausted: the
/// flat 30 s), `1` (the cycle again). Resetting on the connect reads
/// `1, 1, 1, ...`; never resetting on the handshake reads `1, 2, 3, 4, ...`.
///
/// Paused time, so the thirty-second exhaustion arm costs nothing. The
/// core is opened first; nothing past that point does blocking I/O.
#[tokio::test(start_paused = true)]
async fn an_unanswered_reconnect_climbs_the_backoff_schedule() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    const ANSWERED_ATTEMPT: u64 = 3; // zero-based: the fourth connect
    const WANT: [u64; 10] = [1, 2, 3, 1, 2, 3, 4, 5, 0, 1];

    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;

    let (inner, server, _batch_rx, subs, _refreshes) = harness_full(
        vec![Script {
            close_after_subscribe: true,
            ..Script::default()
        }],
        true,
    );
    let attempts = Arc::new(AtomicU64::new(0));
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        if attempts.fetch_add(1, Ordering::SeqCst) == ANSWERED_ATTEMPT {
            return inner(read);
        }
        // A relay that is unreachable, as the shipped factory presents
        // one: the connect still resolves `Ok`, and the `Hello` fails.
        let (client, server) = duplex();
        drop(server);
        Box::pin(async move { Ok(Box::new(client) as BoxTransport) }) as ConnectFuture
    });

    let log = EventLog::default();
    let (status_tx, _status_rx) = broadcast::channel(64);
    let shared = SyncShared::new(status_tx, 0);
    shared.mark_active();
    let rng: Arc<dyn Rng> = Arc::new(SystemRng);
    let driver = tokio::spawn(
        run(Arc::downgrade(&core), Arc::clone(&shared), factory, rng)
            .with_subscriber(tracing_subscriber::registry().with(log.clone())),
    );

    let backoffs = || -> Vec<LoggedEvent> {
        log.0
            .lock()
            .iter()
            .filter(|e| e.ev.as_deref() == Some("sync.backoff"))
            .cloned()
            .collect()
    };
    // Virtual time: the schedule to the tenth reconnect is ~35 s.
    timeout(Duration::from_secs(600), async {
        while backoffs().len() < WANT.len() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the driver stopped reconnecting");

    shared.request_shutdown();
    timeout(Duration::from_secs(60), driver)
        .await
        .expect("the driver never stopped")
        .unwrap();

    assert_eq!(
        server.lock().connect_count,
        1,
        "exactly one attempt must have reached the fake relay"
    );
    assert!(
        subs.load(Ordering::SeqCst) >= 1,
        "the answered attempt must have got past its handshake"
    );

    let seen = backoffs();
    let got: Vec<u64> = seen[..WANT.len()]
        .iter()
        .map(|e| e.attempt.expect("sync.backoff carries attempt"))
        .collect();
    assert_eq!(
        got, WANT,
        "sync.backoff attempts must climb across unanswered reconnects and \
         restart after a handshake"
    );
    for (e, attempt) in seen.iter().zip(WANT) {
        let delay = e.delay_ms.expect("sync.backoff carries delay_ms");
        if attempt == 0 {
            assert_eq!(
                delay, 30_000,
                "the exhausted arm is a flat, un-jittered 30 s"
            );
        } else {
            let base = 100u64 << (attempt - 1);
            assert!(
                (base * 8 / 10..=base * 12 / 10).contains(&delay),
                "attempt {attempt} waited {delay} ms, outside ±20% of {base} ms"
            );
        }
    }
    core.shutdown().await;
}

/// End to end: a renewal that lands while the driver is disconnected is
/// **presented** by the reconnect, and so produces no `0x12 RefreshToken`
/// in the session that follows.
///
/// Driven through `run` — a real connect, a scripted drop, real `Backoff`
/// sleeps, reconnects, a handshake, a subscribe and the pump — rather than
/// through the seam. The offline window is placed by ordering rather than
/// by `Backoff::canonical`'s jittered delays: attempt 2 is a transport
/// whose peer is already gone, and the renewal is written inside its
/// factory call — after `run` read attempt 2's credential and before it
/// reads attempt 3's. Attempt 2 therefore does not carry it, no session is
/// up to announce it, and attempt 3 is the reconnect that carries it.
///
/// The two clauses are independent, which is why both are here. The
/// harness records the bearer each attempt was handed, so the reconnect
/// presenting the renewal is asserted directly rather than inferred from a
/// missing frame. Deleting the `mark_renewals_current` call in `run`
/// leaves that assertion green and fails the frame assertion with
/// `saw ["renewed-token"]`, which is precisely the wasted round trip #244
/// reported.
#[tokio::test(start_paused = true)]
async fn a_renewal_from_the_offline_window_is_presented_and_not_re_announced() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    // Two scripts: session 1, dropped after its subscribe, and session 3,
    // which stays up. Attempt 2 never reaches the harness.
    let (inner, server, _batch_rx, _subs, refreshes) = harness_full(
        vec![
            Script {
                close_after_subscribe: true,
                ..Default::default()
            },
            Script::default(),
        ],
        true,
    );
    let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let renewing = credential.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            // The offline window: session 1 is gone, and this attempt's
            // credential has already been read.
            renewing.set(Some("renewed-token".into()));
            let (client, server) = duplex();
            drop(server);
            return Box::pin(async move { Ok(Box::new(client) as BoxTransport) }) as ConnectFuture;
        }
        inner(read)
    });
    core.start_sync(factory).unwrap();

    timeout(Duration::from_secs(10), async {
        while server.lock().connect_count < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the driver never reconnected");
    // Paused time: this returns only once the reconnect's session has run
    // to a park, so a frame it was going to send has been sent.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        server.lock().presented,
        [
            Some("first-token".to_string()),
            Some("renewed-token".to_string())
        ],
        "the reconnect must present the renewal, not the bearer it opened with"
    );
    let seen = refreshes.lock().clone();
    assert!(
        seen.is_empty(),
        "and having presented it, must not re-announce it, saw {seen:?}"
    );

    // And the handle was not marked *past* the source: a renewal arriving
    // now, with the session up, still reaches the relay in band. Without
    // this the test would also pass for a driver that swallowed every
    // renewal forever.
    credential.set(Some("later-token".into()));
    timeout(Duration::from_secs(10), async {
        while refreshes.lock().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a renewal during the live session must still be announced");
    assert_eq!(
        refreshes.lock().as_slice(),
        ["later-token".to_string()],
        "and it is the later bearer, not a replay of the one the connect carried"
    );
    core.shutdown().await;
}

/// The placement, stated as the thing that distinguishes it from the site
/// the dead guard occupied: a renewal landing **during the dial** is still
/// announced in band.
///
/// `run` marks the handle to the version it read for this attempt,
/// immediately after `factory(read)` returns. Everything after the read —
/// the dial, the handshake, the subscribe, the whole session — is still
/// ahead of the relay, so a write landing there must reach it as a `0x12`
/// rather than wait for the next reconnect.
///
/// Moving the call back inside `session`, where the dead guard stood,
/// leaves the test above green and fails this one: the mark would then
/// happen *after* the dial and consume this write, and no frame would ever
/// be sent. That is the difference between the two sites, and it is the
/// whole of what the placement decision bought.
///
/// The write is issued inside the returned `ConnectFuture` rather than on
/// a timer, because "after the factory returned, before the session
/// exists" is an ordering rather than a duration and there is no wall
/// clock that names it.
#[tokio::test(start_paused = true)]
async fn a_renewal_landing_during_the_dial_still_reaches_the_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    let mut status_rx = core.sync_status();
    let (inner, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
    // A factory whose connect future performs the renewal, putting the
    // write after this attempt's bearer was read and before the session
    // that will carry it exists.
    let dialing = credential.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        let dialing = dialing.clone();
        let fut = inner(read);
        Box::pin(async move {
            dialing.set(Some("dialed-token".into()));
            fut.await
        }) as ConnectFuture
    });
    core.start_sync(factory).unwrap();
    let _ = collect_until_live(&mut status_rx).await;

    timeout(Duration::from_secs(10), async {
        while refreshes.lock().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a renewal landing after the read must not be consumed by the mark");
    assert_eq!(
        refreshes.lock().as_slice(),
        ["dialed-token".to_string()],
        "the relay is told the bearer this connect did not carry, exactly once"
    );
    core.shutdown().await;
}

/// **The falsifier for #273.** A renewal landing between the driver's read
/// of an attempt's credential and its mark is announced in band, not
/// consumed.
///
/// When the factory read the credential itself, `run` could only mark the
/// handle to whatever the source held once the factory returned, so a write
/// landing inside the factory call — after its read, before the mark — was
/// consumed without having been carried, and the relay kept the old bearer
/// until the next reconnect. That window was disclosed as a residual and
/// could not be tested. The write here lands in exactly that window, on
/// the factory's own thread, so it is deterministic: the mark must leave it
/// pending, and the pump must announce it.
///
/// Marking to the source's current version instead of the carried one
/// (`renewals.mark_current()` in the old shape) fails this with no frame.
#[tokio::test(start_paused = true)]
async fn a_renewal_between_the_read_and_the_mark_still_reaches_the_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    let mut status_rx = core.sync_status();
    let (inner, server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
    let renewing = credential.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        // Inside the factory call: `run` has read this attempt's
        // credential and has not yet marked the handle.
        renewing.set(Some("raced-token".into()));
        inner(read)
    });
    core.start_sync(factory).unwrap();
    let _ = collect_until_live(&mut status_rx).await;

    timeout(Duration::from_secs(10), async {
        while refreshes.lock().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a renewal landing between the read and the mark was consumed");
    assert_eq!(
        server.lock().presented,
        [Some("first-token".to_string())],
        "the attempt presented the bearer the driver read, not the racing write"
    );
    assert_eq!(
        refreshes.lock().as_slice(),
        ["raced-token".to_string()],
        "so the relay is told the racing write in band, exactly once"
    );
    core.shutdown().await;
}

/// The credential a caller reaches through `Core` is the *same cell* the
/// driver watches, even when sync was never configured at open.
///
/// This is the FFI seam's path: `SunriseCore::open` builds a
/// `CoreConfig::production`, which has `sync: None`, and the URL and first
/// bearer only arrive later at `start_sync`. While `sync_credential()`
/// minted a fresh `TokenSource` per call, that path had two cells — the
/// caller wrote one, the driver watched the other — so in-band renewal was
/// silently dead for every Swift caller while looking wired up.
#[tokio::test(start_paused = true)]
async fn a_renewal_reaches_the_driver_when_sync_was_unconfigured_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = CoreConfig::with_clock(
        dir.path().to_path_buf(),
        "0.1.0+test",
        Arc::new(TestClock(T0)),
        Arc::new(SystemRng),
    );
    assert!(cfg.sync.is_none(), "the FFI seam opens with sync off");
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    core.sync_credential().set(Some("issued-after-open".into()));
    assert_eq!(
        core.sync_credential().get().as_deref(),
        Some("issued-after-open"),
        "a write through one handle is visible through the next"
    );

    let mut status_rx = core.sync_status();
    let (factory, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
    core.start_sync(factory).unwrap();
    let _ = collect_until_live(&mut status_rx).await;

    core.sync_credential()
        .set(Some("renewed-after-open".into()));
    tokio::time::timeout(Duration::from_secs(10), async {
        while refreshes.lock().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the renewal must reach the live session on this path too");
    assert_eq!(
        refreshes.lock().as_slice(),
        ["renewed-after-open".to_string()]
    );
    core.shutdown().await;
}

/// A relay that did not agree `SrvTokenRefresh` is sent **no** `0x12` at
/// all, and the renewal waits for the next connect.
///
/// This is the reason the capability bit exists. An old relay ignores an
/// unknown frame silently, and a relay that accepted the refresh used to
/// be silent too — so the client could not tell "your session is good for
/// another hour" from "nothing happened", which call for opposite
/// behaviour. Negotiating first means the client only sends the frame to
/// someone it knows will answer.
#[tokio::test(start_paused = true)]
async fn a_relay_that_does_not_negotiate_refresh_is_sent_no_frame() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    let mut status_rx = core.sync_status();
    let (factory, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], false);
    core.start_sync(factory).unwrap();
    let _ = collect_until_live(&mut status_rx).await;

    credential.set(Some("renewed-token".into()));
    // Paused time: the session has seen the write and parked again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        refreshes.lock().is_empty(),
        "a relay that did not advertise the capability must not be sent the frame"
    );
    // The credential is still updated; it simply rides the next connect.
    assert_eq!(credential.get().as_deref(), Some("renewed-token"));
    core.shutdown().await;
}

/// Clearing the credential is not a renewal. An empty `RefreshToken` would
/// be rejected by the relay as an unverifiable token, ending a session
/// that was working — so nothing is sent.
#[tokio::test(start_paused = true)]
async fn clearing_the_credential_sends_no_refresh_frame() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );

    let mut status_rx = core.sync_status();
    let (factory, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
    core.start_sync(factory).unwrap();
    let _ = collect_until_live(&mut status_rx).await;

    credential.set(None);
    // Paused time: the session has seen the write and parked again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        refreshes.lock().is_empty(),
        "a cleared credential must not be sent as a refresh"
    );
    core.shutdown().await;
}

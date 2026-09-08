//! A capture subscriber hears its own thread even when another thread reached
//! the callsite first.
//!
//! `tracing` caches an `Interest` per callsite, globally, and computes it once
//! — on whichever thread reaches the callsite first. While exactly one
//! `Dispatch` is registered process-wide, `tracing-core` computes that interest
//! from *the registering thread's* default subscriber rather than from the
//! registry (`Dispatchers::rebuilder` returning `Rebuilder::JustOne`, whose
//! `for_each` calls `dispatcher::get_default`). A thread with no subscriber
//! therefore caches `Interest::never()` for a callsite another thread's
//! `with_default` capture is about to emit into, and that capture comes back
//! **empty** — not wrong, empty, which is why the failure it caused said
//! nothing (`#116`).
//!
//! `sunrise_log::init` pins a second, inert dispatcher whenever it builds a
//! capture, which keeps the registry off that fast path. This test is the
//! shape that defect takes, run deterministically: the racing thread is joined
//! before the installing thread emits, so it always wins the registration.
//!
//! It lives in a file of its own because it is only meaningful in a process
//! where nothing else has built a subscriber first. Adding a second test here
//! would let that test's dispatcher arm the accurate path and this one would
//! pass without the fix.

use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};

/// One callsite, reached from two threads.
fn probe() {
    tracing::debug!(ev = "log.interest_probe", "a record");
}

#[test]
fn a_callsite_first_reached_off_thread_still_reaches_the_capture() {
    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: "debug".to_owned(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");

    tracing::dispatcher::with_default(&dispatch, || {
        // A thread carrying no subscriber registers the callsite first.
        std::thread::spawn(probe).join().expect("the racing thread");
        // The installing thread emits into the callsite that thread cached.
        probe();
    });

    let out = cap.contents();
    assert!(
        !out.is_empty(),
        "the capture heard nothing: the callsite's interest was cached against \
         a thread that had no subscriber"
    );
    assert!(out.contains("log.interest_probe"), "{out}");
}

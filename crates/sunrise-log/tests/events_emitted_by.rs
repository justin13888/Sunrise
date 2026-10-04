//! `test_util::events_emitted_by` hears its own thread even when another
//! thread reached the callsite first.
//!
//! The same defect `tests/interest_cache.rs` pins for `build_subscriber`, run
//! against the test-only capture other crates call: without the pin in
//! `init::pin_interest_cache`, the racing thread caches `Interest::never()` for
//! the callsite while the capture's `Dispatch` is the only one registered, and
//! the capture comes back empty.
//!
//! It lives in a file of its own for the reason that one does: a second test
//! in this binary would register the pin first, and this one would pass with
//! `events_emitted_by` no longer calling it.

use sunrise_log::test_util::events_emitted_by;

/// One callsite, reached from two threads.
fn probe() {
    tracing::debug!(ev = "log.interest_probe", "a record");
}

#[test]
fn a_callsite_first_reached_off_thread_still_reaches_the_capture() {
    let seen = events_emitted_by(|| {
        // A thread carrying no subscriber registers the callsite first.
        std::thread::spawn(probe).join().expect("the racing thread");
        // The capturing thread emits into the callsite that thread cached.
        probe();
    });
    assert_eq!(
        seen,
        ["log.interest_probe"],
        "the capture heard nothing: the callsite's interest was cached against \
         a thread that had no subscriber"
    );
}

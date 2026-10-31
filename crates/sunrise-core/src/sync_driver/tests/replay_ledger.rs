//! #459: the session's catch-up count, driven by hand.

use crate::sync_driver::SyncShared;
use std::sync::Arc;
use sunrise_sync::SyncState;
use tokio::sync::broadcast;

const A: [u8; 16] = [1; 16];
const B: [u8; 16] = [2; 16];

fn catching_up() -> Arc<SyncShared> {
    let (status_tx, _status_rx) = broadcast::channel(4);
    let shared = SyncShared::new(status_tx, 0);
    shared.set_state(SyncState::CatchingUp);
    shared
}

#[test]
fn a_stated_backlog_counts_down_to_zero_and_stays_known() {
    let shared = catching_up();
    assert_eq!(shared.replay_fields(), (0, None), "nothing stated yet");
    shared.note_backlog(A, 5);
    shared.note_backlog(B, 2);
    assert_eq!(shared.replay_fields(), (0, Some(7)));
    shared.note_replayed(A, 3);
    assert_eq!(shared.replay_fields(), (3, Some(4)));
    shared.note_stream_caught_up(A);
    shared.note_replayed(B, 2);
    shared.note_stream_caught_up(B);
    assert_eq!(shared.replay_fields(), (5, Some(0)));
    shared.set_state(SyncState::Live);
    // Live ops are not catch-up, and a total known stays known.
    shared.note_replayed(A, 4);
    assert_eq!(shared.replay_fields(), (5, Some(0)));
}

#[test]
fn a_restatement_replaces_and_a_silent_relay_states_nothing() {
    let shared = catching_up();
    shared.note_replayed(A, 2);
    assert_eq!(shared.replay_fields(), (2, None), "a count with no total");
    shared.note_backlog(A, 9);
    // A re-subscribe reopens the stream; the relay states from the new
    // cursors and the old statement is gone.
    shared.note_backlog(A, 4);
    assert_eq!(shared.replay_fields(), (2, Some(4)));
    shared.reset_replay();
    assert_eq!(
        shared.replay_fields(),
        (0, None),
        "a new session starts over"
    );
}

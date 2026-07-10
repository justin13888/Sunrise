//! Sync state machine per `docs/05-sync/multi-device.md`.
//!
//! ```text
//! Disconnected ──connect ok──> Catching up
//!      ▲                            │
//!      │                         caught up
//!      │                            ▼
//!      └───────────────────────── Live ──disconnect──┐
//!                                                    ▼
//!                                             Disconnected
//! ```

use serde::{Deserialize, Serialize};

/// Sync session states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// No active session.
    Disconnected,
    /// Session opened; replaying op log to catch up to live tail.
    CatchingUp,
    /// Live; ops flow in real time.
    Live,
}

/// Triggers that advance the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStateTransition {
    /// Successful WS / long-poll connection.
    Connected,
    /// Catch-up batch completed; tail reached.
    CaughtUp,
    /// Underlying transport closed (peer or local).
    Disconnected,
    /// Hard error closed the session; stay Disconnected.
    Error,
}

/// Pure state machine. Transport / I/O lives elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncStateMachine {
    state: SyncState,
}

impl Default for SyncStateMachine {
    fn default() -> Self {
        Self {
            state: SyncState::Disconnected,
        }
    }
}

impl SyncStateMachine {
    /// Construct fresh in `Disconnected`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: SyncState::Disconnected,
        }
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> SyncState {
        self.state
    }

    /// Apply a transition. Returns the new state. Invalid transitions are
    /// no-ops (the spec allows reordering across reconnect; we do not error).
    pub fn apply(&mut self, t: SyncStateTransition) -> SyncState {
        self.state = match (self.state, t) {
            (SyncState::Disconnected, SyncStateTransition::Connected) => SyncState::CatchingUp,
            (SyncState::CatchingUp, SyncStateTransition::CaughtUp) => SyncState::Live,
            (
                SyncState::CatchingUp | SyncState::Live,
                SyncStateTransition::Disconnected | SyncStateTransition::Error,
            ) => SyncState::Disconnected,
            (s, _) => s,
        };
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let mut m = SyncStateMachine::new();
        assert_eq!(m.state(), SyncState::Disconnected);
        m.apply(SyncStateTransition::Connected);
        assert_eq!(m.state(), SyncState::CatchingUp);
        m.apply(SyncStateTransition::CaughtUp);
        assert_eq!(m.state(), SyncState::Live);
        m.apply(SyncStateTransition::Disconnected);
        assert_eq!(m.state(), SyncState::Disconnected);
    }

    #[test]
    fn error_during_catchup_drops_session() {
        let mut m = SyncStateMachine::new();
        m.apply(SyncStateTransition::Connected);
        m.apply(SyncStateTransition::Error);
        assert_eq!(m.state(), SyncState::Disconnected);
    }

    #[test]
    fn invalid_transitions_are_noops() {
        let mut m = SyncStateMachine::new();
        // Connected directly from Disconnected not via Connected: nothing.
        m.apply(SyncStateTransition::CaughtUp);
        assert_eq!(m.state(), SyncState::Disconnected);
        m.apply(SyncStateTransition::Disconnected);
        assert_eq!(m.state(), SyncState::Disconnected);
    }
}

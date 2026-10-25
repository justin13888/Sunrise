//! What the notification catalog reads from a vault besides the planner's
//! intents (`docs/08-features/notifications.md` #2, #12–#15, #347).
//!
//! Each is a read of rows this replica already holds; none writes anything.
//! The client layer turns them into notifications
//! (`sunrise_client_core::reminders`), and decides which it has already
//! shown.

use serde::{Deserialize, Serialize};
use sunrise_domain::SunriseTime;
use sunrise_id::EntityRef;

/// An open task with a hard deadline: what a deadline reminder (#2) is
/// planned from. A target date never is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlineCandidate {
    /// The task.
    pub task: EntityRef,
    /// Its title, to show.
    pub title: String,
    /// Its `hard_due_at`.
    pub hard_due_at: SunriseTime,
}

/// Which security event a [`SecurityEvent`] is (§Security events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityEventKind {
    /// Another device's certificate was applied: it joined the account (#13).
    DeviceJoined,
    /// A device is revoked, this one included (#14).
    DeviceRevoked,
    /// An identity rotation retired the recovery code: it carried no share
    /// to the code's holders (#15, ADR-0070 §1).
    RecoveryCodeRetired,
}

/// One security event this replica knows of, from after this device joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityEvent {
    /// Which event.
    pub kind: SecurityEventKind,
    /// What it is about: the device that joined or was revoked, or the
    /// identity the rotation installed.
    pub subject: [u8; 16],
    /// When it happened by its own record, epoch ms: the certificate's
    /// creation, the revocation's cut, the rotation's HLC.
    pub at_ms: u64,
    /// The device's nickname, for a device event.
    pub nickname: Option<String>,
    /// Whether the event is about this device: its own revocation.
    pub this_device: bool,
}

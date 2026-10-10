//! What every seam test file opens a vault with.
//!
//! A directory module rather than a `support.rs` file at the top of `tests/`,
//! so Cargo does not build it as a test target of its own.

use std::sync::Arc;

use sunrise_core_bindings::dto::TaskDraftIn;
use sunrise_core_bindings::SunriseCore;

const ROOT: [u8; 32] = [42u8; 32];

pub(crate) async fn open_core() -> (tempfile::TempDir, Arc<SunriseCore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "test".into(),
        None,
    )
    .await
    .expect("vault opens");
    (dir, core)
}

pub(crate) fn draft(title: &str) -> TaskDraftIn {
    TaskDraftIn {
        title: title.into(),
        body: None,
        stream_id: None,
        contexts: Vec::new(),
        priority: None,
        energy: None,
        estimated_duration_s: None,
        scheduled_at: None,
        due_at: None,
        scheduling_constraints: Vec::new(),
        assignee: None,
        reminder_lead_s: None,
    }
}

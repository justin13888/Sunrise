//! Test-only fixtures shared by the blob modules' unit tests.

use std::sync::Arc;

use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::TaskDraft;
use sunrise_id::EntityRef;

use crate::commands::Command;
use crate::config::CoreConfig;
use crate::core::Core;
use crate::unlock::Unlock;

/// A vault at `dir` on the production clock and RNG, under a fixed root key.
pub(crate) async fn open_vault(dir: &std::path::Path) -> Arc<Core> {
    let cfg = CoreConfig::production(dir.to_path_buf(), "0.1.0+test");
    Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes([7u8; 32]),
                paired: None,
            },
        )
        .await
        .expect("open"),
    )
}

/// A freshly created task, for a test that needs something to attach to.
pub(crate) async fn a_task(core: &Core) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: "Renew the passport".into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

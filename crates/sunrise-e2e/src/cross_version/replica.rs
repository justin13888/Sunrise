//! The replica surface the driver speaks, and the implementations of it that
//! live in this process. The third, the baseline in its own process, is
//! [`BaselineReplica`](super::baseline::BaselineReplica).

use std::sync::Arc;

use sunrise_core::{Command, CommandResult, Core, CoreError, EngineError, Query, QueryResult};
use sunrise_domain::{Task, TaskDraft, TaskPatch};
use sunrise_id::EntityRef;

use super::events::LoggedEvent;

/// What an `apply_remote` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Applied, parked, or an idempotent repeat: nothing wrong.
    Accepted,
    /// Refused.
    Refused {
        /// Whether the refusal is the one the sync driver counts as corruption.
        corruption: bool,
        /// The refusal.
        error: String,
    },
}

/// One replica of the two the property compares.
#[async_trait::async_trait]
pub(crate) trait Replica: Send {
    async fn create_task(
        &mut self,
        title: &str,
        priority: Option<u8>,
        stream: EntityRef,
    ) -> Result<EntityRef, String>;
    async fn set_title(&mut self, id: EntityRef, title: &str) -> Result<(), String>;
    async fn set_priority(&mut self, id: EntityRef, priority: Option<u8>) -> Result<(), String>;
    async fn delete(&mut self, id: EntityRef) -> Result<(), String>;
    async fn apply_remote(&mut self, envelope: &[u8]) -> Applied;
    async fn task(&mut self, id: EntityRef) -> Option<Task>;
    async fn status(&mut self) -> Result<(String, u32), String>;
    async fn knows(&mut self, devices: &[[u8; 16]]) -> bool;
    /// Whether this replica holds a `DeviceFeatures` naming `core.field_ops`
    /// from each of `devices`. A baseline has no notion of one, and says yes.
    async fn holds_field_ops_of(&mut self, devices: &[[u8; 16]]) -> bool;
    async fn take_events(&mut self) -> Vec<LoggedEvent>;
    async fn close(self: Box<Self>);
}

/// A `HEAD` core in this process.
#[derive(Debug)]
pub(crate) struct HeadReplica(pub(super) Arc<Core>);

impl HeadReplica {
    async fn submit(&self, cmd: Command) -> Result<CommandResult, String> {
        self.0.submit(cmd).await.map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Replica for HeadReplica {
    async fn create_task(
        &mut self,
        title: &str,
        priority: Option<u8>,
        stream: EntityRef,
    ) -> Result<EntityRef, String> {
        let draft = TaskDraft {
            title: title.to_owned(),
            stream_id: Some(stream),
            priority,
            ..TaskDraft::default()
        };
        Ok(self.submit(Command::CreateTask(draft)).await?.entity)
    }

    async fn set_title(&mut self, id: EntityRef, title: &str) -> Result<(), String> {
        let patch = TaskPatch {
            title: Some(title.to_owned()),
            ..TaskPatch::default()
        };
        self.submit(Command::UpdateTask { id, patch })
            .await
            .map(drop)
    }

    async fn set_priority(&mut self, id: EntityRef, priority: Option<u8>) -> Result<(), String> {
        let patch = TaskPatch {
            priority: Some(priority),
            ..TaskPatch::default()
        };
        self.submit(Command::UpdateTask { id, patch })
            .await
            .map(drop)
    }

    async fn delete(&mut self, id: EntityRef) -> Result<(), String> {
        self.submit(Command::DeleteTask(id)).await.map(drop)
    }

    async fn apply_remote(&mut self, envelope: &[u8]) -> Applied {
        match self.0.apply_remote(envelope).await {
            Ok(_) => Applied::Accepted,
            Err(e) => Applied::Refused {
                corruption: matches!(e, CoreError::Engine(EngineError::RemoteOpInvalid(_))),
                error: e.to_string(),
            },
        }
    }

    async fn task(&mut self, id: EntityRef) -> Option<Task> {
        match self.0.query(Query::EntityById(id)).await {
            Ok(QueryResult::Task(t)) => Some(*t),
            _ => None,
        }
    }

    async fn status(&mut self) -> Result<(String, u32), String> {
        match self.0.query(Query::SyncStatus).await {
            Ok(QueryResult::SyncStatus(s)) => Ok((format!("{:?}", s.state), s.outbox_pending)),
            Ok(other) => Err(format!("expected SyncStatus, got {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn knows(&mut self, devices: &[[u8; 16]]) -> bool {
        let Ok(QueryResult::Devices(rows)) = self.0.query(Query::DeviceList).await else {
            return false;
        };
        devices
            .iter()
            .all(|d| rows.iter().any(|r| r.device_id == *d))
    }

    async fn holds_field_ops_of(&mut self, devices: &[[u8; 16]]) -> bool {
        self.0
            .devices_lacking_feature(sunrise_core::FIELD_OPS_FEATURE)
            .is_ok_and(|lacking| !devices.iter().any(|d| lacking.contains(d)))
    }

    async fn take_events(&mut self) -> Vec<LoggedEvent> {
        // `HEAD` cores share one process and one capture; the run reads it
        // once for all of them rather than per replica.
        Vec::new()
    }

    async fn close(self: Box<Self>) {
        self.0.shutdown().await;
    }
}

/// The placeholder A holds between closing the old build and opening `HEAD`.
pub(super) struct Closed;

#[async_trait::async_trait]
impl Replica for Closed {
    async fn create_task(
        &mut self,
        _: &str,
        _: Option<u8>,
        _: EntityRef,
    ) -> Result<EntityRef, String> {
        Err("closed".into())
    }
    async fn set_title(&mut self, _: EntityRef, _: &str) -> Result<(), String> {
        Err("closed".into())
    }
    async fn set_priority(&mut self, _: EntityRef, _: Option<u8>) -> Result<(), String> {
        Err("closed".into())
    }
    async fn delete(&mut self, _: EntityRef) -> Result<(), String> {
        Err("closed".into())
    }
    async fn apply_remote(&mut self, _: &[u8]) -> Applied {
        Applied::Refused {
            corruption: false,
            error: "closed".into(),
        }
    }
    async fn task(&mut self, _: EntityRef) -> Option<Task> {
        None
    }
    async fn status(&mut self) -> Result<(String, u32), String> {
        Err("closed".into())
    }
    async fn knows(&mut self, _: &[[u8; 16]]) -> bool {
        false
    }
    async fn holds_field_ops_of(&mut self, _: &[[u8; 16]]) -> bool {
        false
    }
    async fn take_events(&mut self) -> Vec<LoggedEvent> {
        Vec::new()
    }
    async fn close(self: Box<Self>) {}
}

//! The web client's calls, typed: what `apps/web` used to spell as JSON.
//!
//! The page asked for four things — create a task, complete one, Today, the
//! Inbox — by writing `{"CreateTask":{"title":…,"contexts":[]}}` and its
//! siblings by hand, and read the answer by picking `Tasks` or `StreamTasks`
//! out of a tagged result. Each was a second statement of the core's serde
//! shape that only a test pinning both sides kept honest. These build the
//! real [`Command`] and [`Query`] values instead, and answer with the rows the
//! page draws, so the web names a call and its argument and nothing else.

use serde::Serialize;
use sunrise_core::{Command, CommandResult, Query, QueryResult};
use sunrise_domain::{stream_key, TaskDraft};
use sunrise_id::EntityRef;

use crate::{WebCore, WebCoreError};

/// One task row, as the web client draws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebTask {
    /// `tsk_…`.
    pub id: String,
    /// The title.
    pub title: String,
    /// The state as the core writes it: a known one (`todo`, `in_progress`,
    /// `done`, `cancelled`) or a newer build's, verbatim (ADR-0045 §6).
    pub state: String,
    /// The owning stream, `str_…`, or the private domain's id for a task
    /// that belongs to no stream (ADR-0046 §5).
    pub stream_id: String,
}

fn rows(result: &QueryResult) -> Result<Vec<WebTask>, WebCoreError> {
    let (QueryResult::Tasks(tasks) | QueryResult::StreamTasks(tasks)) = result else {
        return Err(WebCoreError::Unexpected("a task list"));
    };
    Ok(tasks
        .iter()
        .map(|task| WebTask {
            id: task.id.to_str(),
            title: task.title.clone(),
            state: task.state.as_str().to_owned(),
            stream_id: stream_key(task.stream_id).to_str(),
        })
        .collect())
}

impl WebCore {
    /// Capture a task into the Inbox; returns its id.
    ///
    /// # Errors
    ///
    /// As [`Self::submit_json`].
    pub async fn create_task(&self, title: &str) -> Result<String, WebCoreError> {
        let draft = TaskDraft {
            title: title.to_owned(),
            ..TaskDraft::default()
        };
        let result: CommandResult = self.live()?.submit(Command::CreateTask(draft)).await?;
        Ok(result.entity.to_str())
    }

    /// Complete the task `id`.
    ///
    /// # Errors
    ///
    /// [`WebCoreError::BadId`] for an id that is not one, and as
    /// [`Self::submit_json`].
    pub async fn complete_task(&self, id: &str) -> Result<(), WebCoreError> {
        let id = EntityRef::parse_any(id).map_err(|e| WebCoreError::BadId(e.to_string()))?;
        self.live()?.submit(Command::CompleteTask(id)).await?;
        Ok(())
    }

    /// Today at `now_ms`, every context, as rows.
    ///
    /// # Errors
    ///
    /// As [`Self::query_json`].
    pub async fn today(&self, now_ms: u64) -> Result<Vec<WebTask>, WebCoreError> {
        let query = Query::Today {
            now_ms,
            contexts: Vec::new(),
        };
        rows(&self.live()?.query(query).await?)
    }

    /// The Inbox, as rows.
    ///
    /// # Errors
    ///
    /// As [`Self::query_json`].
    pub async fn inbox(&self) -> Result<Vec<WebTask>, WebCoreError> {
        rows(&self.live()?.query(Query::Inbox).await?)
    }
}

/// The browser exports, beside the JSON ones in `lib.rs`.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod web {
    use std::sync::Arc;

    use wasm_bindgen::prelude::*;

    use crate::web::{js_err, WasmCore};

    fn rows_json(rows: &[super::WebTask]) -> Result<JsValue, JsValue> {
        serde_json::to_string(rows)
            .map(JsValue::from)
            .map_err(|e| js_err(e).into())
    }

    #[wasm_bindgen]
    impl WasmCore {
        /// `core.createTask(title)`: resolves to the new task's id.
        #[wasm_bindgen(js_name = createTask)]
        pub fn create_task(&self, title: String) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            wasm_bindgen_futures::future_to_promise(async move {
                inner
                    .create_task(&title)
                    .await
                    .map(JsValue::from)
                    .map_err(|e| js_err(e).into())
            })
        }

        /// `core.completeTask(id)`.
        #[wasm_bindgen(js_name = completeTask)]
        pub fn complete_task(&self, id: String) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            wasm_bindgen_futures::future_to_promise(async move {
                inner
                    .complete_task(&id)
                    .await
                    .map(|()| JsValue::UNDEFINED)
                    .map_err(|e| js_err(e).into())
            })
        }

        /// `core.today(nowMs)`: resolves to the rows, as JSON.
        #[wasm_bindgen(js_name = today)]
        pub fn today(&self, now_ms: f64) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            let now_ms = crate::clamp_ms(now_ms);
            wasm_bindgen_futures::future_to_promise(async move {
                let rows = inner
                    .today(now_ms)
                    .await
                    .map_err(|e| JsValue::from(js_err(e)))?;
                rows_json(&rows)
            })
        }

        /// `core.inbox()`: resolves to the rows, as JSON.
        #[wasm_bindgen(js_name = inbox)]
        pub fn inbox(&self) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            wasm_bindgen_futures::future_to_promise(async move {
                let rows = inner.inbox().await.map_err(|e| JsValue::from(js_err(e)))?;
                rows_json(&rows)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fixed_now, open};
    use sunrise_domain::{inbox_stream_ref, Task, TaskState, UnknownVariant, INBOX_STREAM_ID};
    use sunrise_id::EntityKind;

    /// The same walk as the JSON round trip in `lib.rs`, through the typed
    /// calls: capture, read back from the Inbox and Today, complete.
    #[tokio::test]
    async fn the_typed_calls_walk_a_task_through_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;

        let id = core.create_task("Renew passport").await.unwrap();
        assert!(id.starts_with("tsk_"), "{id}");
        let inbox = core.inbox().await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(
            (
                inbox[0].id.as_str(),
                inbox[0].title.as_str(),
                inbox[0].state.as_str()
            ),
            (id.as_str(), "Renew passport", "todo")
        );
        assert!(inbox[0].stream_id.starts_with("str_"), "{:?}", inbox[0]);

        // `create_task` takes no plan, so the planned task goes through the
        // JSON path on the same core. The unplanned capture stays out of Today.
        let planned: serde_json::Value = serde_json::from_str(
            &core
                .submit_json(
                    r#"{"CreateTask":{"title":"Book flights","contexts":[],"scheduled_at":"2026-01-01T00:00:00Z"}}"#,
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let planned_id = planned["entity"].as_str().expect("entity id is a string");
        let today = core.today(fixed_now()).await.unwrap();
        assert_eq!(today.len(), 1, "{today:?}");
        assert_eq!(
            (
                today[0].id.as_str(),
                today[0].title.as_str(),
                today[0].state.as_str()
            ),
            (planned_id, "Book flights", "todo")
        );

        core.complete_task(&id).await.unwrap();
        assert!(
            core.inbox().await.unwrap().is_empty(),
            "a done task leaves the Inbox view"
        );
    }

    #[tokio::test]
    async fn a_malformed_id_is_refused_before_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;
        let err = core.complete_task("tsk_nope").await.unwrap_err();
        assert!(matches!(err, WebCoreError::BadId(_)), "{err}");
    }

    fn task(stream_id: Option<EntityRef>, state: TaskState) -> Task {
        let draft = TaskDraft {
            title: "Renew passport".to_owned(),
            stream_id,
            ..TaskDraft::default()
        };
        // The timestamp is the epoch: no row reads it, and naming its type
        // would take a dependency this crate does not otherwise have.
        #[allow(clippy::default_trait_access)]
        let mut task = Task::from_draft(
            EntityRef::new(EntityKind::Task, [1; 16]),
            Default::default(),
            draft,
        );
        task.state = state;
        task
    }

    /// A row carries the task's own fields, whichever list it came in. A
    /// state this build does not know stays verbatim, not its `todo` reading.
    #[test]
    fn a_row_is_built_from_the_tasks_fields() {
        let stream = EntityRef::new(EntityKind::Stream, [2; 16]);
        let tasks = vec![task(
            Some(stream),
            TaskState::Unknown(UnknownVariant::new("snoozed")),
        )];
        let expected = vec![WebTask {
            id: EntityRef::new(EntityKind::Task, [1; 16]).to_str(),
            title: "Renew passport".to_owned(),
            state: "snoozed".to_owned(),
            stream_id: stream.to_str(),
        }];
        assert_eq!(rows(&QueryResult::Tasks(tasks.clone())).unwrap(), expected);
        assert_eq!(rows(&QueryResult::StreamTasks(tasks)).unwrap(), expected);
    }

    /// A task in no stream answers the private domain's id, which is what the
    /// web has always been handed for one (ADR-0046 §5), never an empty string.
    #[test]
    fn a_task_in_no_stream_rows_under_the_inbox_id() {
        let tasks = vec![
            task(None, TaskState::InProgress),
            task(Some(inbox_stream_ref()), TaskState::Done),
        ];
        let rows = rows(&QueryResult::Tasks(tasks)).unwrap();
        assert_eq!(rows[0].stream_id, INBOX_STREAM_ID);
        assert_eq!(rows[0].state, "in_progress");
        assert_eq!(rows[1].stream_id, INBOX_STREAM_ID);
        assert_eq!(rows[1].state, "done");
    }

    /// The rows serialise as the four fields `apps/web/src/wasm.ts` reads.
    #[test]
    fn a_row_serialises_as_the_web_reads_it() {
        let row = WebTask {
            id: "tsk_a".into(),
            title: "A".into(),
            state: "todo".into(),
            stream_id: "str_inbox".into(),
        };
        assert_eq!(
            serde_json::to_string(&[row]).unwrap(),
            r#"[{"id":"tsk_a","title":"A","state":"todo","stream_id":"str_inbox"}]"#
        );
    }
}

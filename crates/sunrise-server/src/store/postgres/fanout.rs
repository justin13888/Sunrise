//! The fan-out bus: Postgres `LISTEN/NOTIFY` as a wake-up, the durable relay
//! log as the payload (ADR-0062 §2).
//!
//! Every node runs one [`PgFanout`]: a connection of its own, outside the
//! pool, listening on `sunrise_relay`. An append anywhere in the cluster
//! notifies on commit with `<account_h>:<stream_id>:<frame_id>`. A node with
//! live subscribers on that channel reads the channel's frames after its
//! **mark** — the last id it published for the channel — in id order, and
//! publishes each into its own [`RelayHub`]. A node with none drops the notice
//! and forgets the channel's mark.
//!
//! The bus is the only thing that publishes on a node it feeds; the accepting
//! node does not publish its own frames directly (`crate::relay`'s §Several
//! nodes). Because a channel's ids are assigned in commit order
//! (`relay_log.rs`), "after the mark" is everything new, and every node
//! publishes a channel in the order the log numbered it.
//!
//! # Where a mark starts
//!
//! A stream calls [`LiveFeed::prime`] after taking its live receiver and
//! before reading the durable log. A channel without a mark gets the newest
//! id the log holds as its mark then: every frame that commits afterwards has
//! a larger id and reaches the receiver through the bus, and every frame that
//! committed before is in the read that follows. A notice for a live channel
//! that has no mark yet — its subscriber is between the receiver and the
//! prime — starts it just below the notice's frame, which is safe for the
//! same reason: frames before it committed earlier and are in that
//! subscriber's read.
//!
//! # A lost connection
//!
//! A notice sent while the listening connection is down is lost, so a
//! reconnect reads every live channel after its mark before it waits for new
//! notices; a read that fails drops the connection to force that. A frame can
//! reach a subscriber twice — through its replay and the bus, or after a
//! reconnect — and never zero times: the client's op-log gate drops a
//! duplicate, and a missed frame is data loss.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::relay_log::parse_notice;
use super::PgStore;
use crate::relay::{LiveFeed, RelayHub, StreamKey};
use crate::store::MetadataError;

/// The first wait before reconnecting a lost listener, doubled per failure.
const RECONNECT_FIRST: Duration = Duration::from_millis(250);

/// The longest wait between reconnect attempts.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// One node's listener on the cluster's relay notices.
#[derive(Debug, Clone)]
pub struct PgFanout {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: PgStore,
    hub: RelayHub,
    /// The last id published into the hub, per channel with live receivers.
    marks: tokio::sync::Mutex<HashMap<StreamKey, u64>>,
    task: parking_lot::Mutex<Option<tokio::task::AbortHandle>>,
}

impl PgFanout {
    /// Start listening, publishing into `hub`. The hub to serve is
    /// `hub.with_feed(Arc::new(feed))`.
    #[must_use]
    pub fn spawn(store: PgStore, hub: RelayHub) -> Self {
        let inner = Arc::new(Inner {
            store,
            hub,
            marks: tokio::sync::Mutex::new(HashMap::new()),
            task: parking_lot::Mutex::new(None),
        });
        let task = tokio::spawn(listen(Arc::clone(&inner)));
        *inner.task.lock() = Some(task.abort_handle());
        Self { inner }
    }

    /// Stop listening. Nothing more is published into the hub; what is
    /// published stays. For a node that stops serving, and for tests that
    /// stop a node.
    pub fn shutdown(&self) {
        if let Some(task) = self.inner.task.lock().take() {
            task.abort();
        }
    }
}

#[async_trait::async_trait]
impl LiveFeed for PgFanout {
    async fn prime(&self, key: StreamKey) -> Result<(), MetadataError> {
        let mut marks = self.inner.marks.lock().await;
        if let std::collections::hash_map::Entry::Vacant(mark) = marks.entry(key) {
            mark.insert(self.inner.store.relay_last_id(key).await?);
        }
        Ok(())
    }
}

impl Inner {
    /// Publish `key`'s frames after its mark, moving the mark past them.
    async fn catch_up(
        &self,
        marks: &mut HashMap<StreamKey, u64>,
        key: StreamKey,
        mark: u64,
    ) -> Result<(), MetadataError> {
        let frames = self.store.relay_frames_after(key, mark).await?;
        let mut last = mark;
        for frame in frames {
            last = last.max(frame.id);
            self.hub.publish(key, frame);
        }
        marks.insert(key, last);
        Ok(())
    }

    /// A notice that frame `id` committed on `key`.
    async fn notice(&self, key: StreamKey, id: u64) -> Result<(), MetadataError> {
        let mut marks = self.marks.lock().await;
        if self.hub.live_receivers(key) == 0 {
            marks.remove(&key);
            return Ok(());
        }
        let mark = marks
            .get(&key)
            .copied()
            .unwrap_or_else(|| id.saturating_sub(1));
        if id <= mark {
            return Ok(());
        }
        self.catch_up(&mut marks, key, mark).await
    }

    /// Every live channel, after its mark: what a (re)connected listener does
    /// before it trusts notices again.
    async fn catch_up_all(&self) -> Result<(), MetadataError> {
        let mut marks = self.marks.lock().await;
        let channels: Vec<(StreamKey, u64)> = marks.iter().map(|(k, m)| (*k, *m)).collect();
        for (key, mark) in channels {
            if self.hub.live_receivers(key) == 0 {
                marks.remove(&key);
            } else {
                self.catch_up(&mut marks, key, mark).await?;
            }
        }
        Ok(())
    }
}

/// Listen until aborted, reconnecting whenever the connection is lost.
async fn listen(inner: Arc<Inner>) {
    let mut wait = RECONNECT_FIRST;
    loop {
        match listen_once(&inner, &mut wait).await {
            Ok(()) => lost("the relay notice connection closed"),
            Err(e) => lost(&e),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RECONNECT_MAX);
    }
}

/// One connection's life: listen, catch up, then follow notices until the
/// connection or a read fails.
async fn listen_once(inner: &Inner, wait: &mut Duration) -> Result<(), String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let connect = inner.store.connect();
    let channel = connect.notify();
    let client = connect.listen(tx).await.map_err(|e| e.to_string())?;
    // An identifier, not a value, so it cannot be a bound parameter; it is
    // this module's constant or a test's schema-derived name.
    client
        .batch_execute(&format!("LISTEN {channel}"))
        .await
        .map_err(|e| e.to_string())?;
    // Listening before reading, so a frame that commits during the read is
    // either in it or announced after it.
    inner.catch_up_all().await.map_err(|e| e.to_string())?;
    *wait = RECONNECT_FIRST;
    while let Some(n) = rx.recv().await {
        if n.channel() != channel {
            continue;
        }
        if let Some((key, id)) = parse_notice(n.payload()) {
            inner.notice(key, id).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn lost(cause: &str) {
    tracing::error!(
        ev = "srv.store.failed",
        err_code = %sunrise_error::ErrorCode::RelayStorageUnavailable,
        err_kind = "transient",
        retryable = true,
        cause = %cause,
        "the relay notice listener lost its connection; reconnecting and re-reading every \
         subscribed channel"
    );
}

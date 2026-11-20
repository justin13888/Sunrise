//! What keeps a channel's live frames in log order: the per-channel publish
//! order on one node, and the [`LiveFeed`] that replaces it across several.
//!
//! Its own module because it changes for its own reason: how a frame reaches
//! the hub, as opposed to what the hub does with it once it has.

use std::sync::Arc;

use super::{Channel, ConnId, RelayHub, StreamKey};
use crate::store::MetadataError;

/// Feeds a hub the frames every node of a cluster accepts, in log order per
/// channel (ADR-0062 §2). The Postgres `LISTEN/NOTIFY` bus is the one
/// implementation.
#[async_trait::async_trait]
pub trait LiveFeed: Send + Sync + std::fmt::Debug {
    /// Make sure the feed will publish every frame of `key` committed from
    /// now on.
    ///
    /// A stream calls it after taking its live receiver and before reading
    /// the durable log, so whatever commits after this returns reaches the
    /// receiver and whatever committed before it is in the read: a frame can
    /// arrive both ways, never neither.
    async fn prime(&self, key: StreamKey) -> Result<(), MetadataError>;
}

/// A fresh connection id: 64 bits of OS entropy.
///
/// See [`ConnId`] for why it is random. With no entropy at all the id falls
/// back to 0, which is what every session would then share: the cost is a
/// session hearing its own frames back, which the client's op-log gate drops
/// as duplicates, never a frame it misses.
#[must_use]
pub fn new_conn() -> ConnId {
    let mut raw = [0u8; 8];
    if getrandom::getrandom(&mut raw).is_err() {
        return 0;
    }
    u64::from_le_bytes(raw)
}

impl RelayHub {
    /// This hub, fed by `feed` instead of by the requests that store frames.
    #[must_use]
    pub fn with_feed(mut self, feed: Arc<dyn LiveFeed>) -> Self {
        self.feed = Some(feed);
        self
    }

    /// Whether a [`LiveFeed`] publishes into this hub. When one does, the
    /// request that stored a frame does not publish it.
    #[must_use]
    pub fn fed(&self) -> bool {
        self.feed.is_some()
    }

    /// [`LiveFeed::prime`] for `key`; nothing on a hub with no feed, where the
    /// storing request publishes.
    ///
    /// # Errors
    /// The feed could not establish where the channel's log stands.
    pub async fn prime(&self, key: StreamKey) -> Result<(), MetadataError> {
        match &self.feed {
            Some(feed) => feed.prime(key).await,
            None => Ok(()),
        }
    }

    /// Hold `key`'s publish order: the request that appends a frame takes this
    /// before the append and publishes before dropping it, so the hub
    /// publishes the channel's frames in the order the log numbered them.
    ///
    /// Without it two appends on one channel can commit as 10 then 11 and
    /// publish as 11 then 10, and a stream that dropped between the two would
    /// resume after 11 and never see 10. Held across one append, which on a
    /// single node is one local transaction.
    pub async fn ordered(&self, key: StreamKey) -> tokio::sync::OwnedMutexGuard<()> {
        let order = Arc::clone(
            &self
                .inner
                .lock()
                .channels
                .entry(key)
                .or_insert_with(Channel::new)
                .order,
        );
        order.lock_owned().await
    }

    /// How many live receivers `key` has on this node.
    #[must_use]
    pub fn live_receivers(&self, key: StreamKey) -> usize {
        self.inner
            .lock()
            .channels
            .get(&key)
            .map_or(0, |c| c.tx.receiver_count())
    }

    /// Every channel with a live receiver on this node.
    #[must_use]
    pub fn live_channels(&self) -> Vec<StreamKey> {
        self.inner
            .lock()
            .channels
            .iter()
            .filter(|(_, c)| c.tx.receiver_count() > 0)
            .map(|(k, _)| *k)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: StreamKey = ([1; 16], [2; 16]);

    /// Drawn, not counted: a thousand ids, no repeat, and not a sequence.
    #[test]
    fn conn_ids_are_random() {
        let ids: Vec<ConnId> = (0..1000).map(|_| new_conn()).collect();
        let distinct: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(distinct.len(), ids.len());
        assert!(ids.windows(2).any(|w| w[1] != w[0].wrapping_add(1)));
    }

    /// A second appender on one channel waits for the first to publish; another
    /// channel does not.
    #[tokio::test]
    async fn the_publish_order_is_per_channel() {
        let hub = RelayHub::new();
        let held = hub.ordered(KEY).await;
        let same = tokio::time::timeout(std::time::Duration::from_millis(50), hub.ordered(KEY));
        assert!(same.await.is_err(), "the same channel waits");
        let other = ([1; 16], [3; 16]);
        drop(hub.ordered(other).await);
        drop(held);
        drop(hub.ordered(KEY).await);
    }

    /// Live receivers are counted per channel, and a dropped one stops
    /// counting; a hub with no feed primes nothing and says so.
    #[tokio::test]
    async fn live_receivers_are_counted_and_an_unfed_hub_primes_nothing() {
        let hub = RelayHub::new();
        assert!(!hub.fed());
        hub.prime(KEY).await.unwrap();
        assert_eq!(hub.live_receivers(KEY), 0);
        let rx = hub.subscribe_live(KEY);
        assert_eq!(hub.live_receivers(KEY), 1);
        assert_eq!(hub.live_channels(), vec![KEY]);
        drop(rx);
        assert_eq!(hub.live_receivers(KEY), 0);
        assert!(hub.live_channels().is_empty());
    }
}

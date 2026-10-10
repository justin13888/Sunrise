//! The local copy of attachment bytes: what this device keeps, what it lets
//! go, and what it fetches unasked on which network (ADR-0053 §5–§6, issue
//! #346).
//!
//! # The cache
//!
//! The sealed chunks of every blob this device holds live in its
//! [`BlobStore`]. Until this module nothing bounded them: a fetched blob was
//! kept forever. `blob_cache` (migration 0038) is the index the bound is
//! enforced over: one row per blob, its sealed size, the time it was last
//! opened, whether it is a thumbnail, and whether it has been evicted.
//!
//! The limit is the device-local preference `attachments.cache_limit_bytes`
//! (1 GB on a desktop, 200 MB on a handheld, 100 MB to 50 GB). Eviction runs
//! after every fetch, after every attach and at launch, least recently opened
//! first, until what the index holds is under the limit. It never evicts:
//!
//! - a blob with a `blob_uploads` row, pending or exhausted: this device may be
//!   its only holder, and an evicted unuploaded blob is a lost file;
//! - a thumbnail, which is small and is what makes the list usable offline;
//! - a blob pinned by an open preview ([`Core::pin_attachment`]).
//!
//! The record of an eviction is kept (`evicted_at_ms`), and that is what stops
//! the automatic fetch drain fetching the blob straight back: an evicted blob
//! returns only when somebody opens it, through [`Core::fetch_attachment`],
//! which is the ordinary lazy-fetch path.
//!
//! The record and the eviction it triggers happen under one hold of the
//! vault's database lock, so two fetches finishing together cannot each see
//! the other's blob missing from the total: after any store returns, the
//! index is under the limit, or every blob still in it is one eviction never
//! touches or the one just stored. That one is spared by its own pass, so a
//! blob larger than the whole limit is still there when the Download that
//! asked for it returns.
//!
//! # The network gate
//!
//! The client reports the class of its current network
//! ([`Core::set_network_class`]); the fetch drain decides
//! ([`auto_fetch_policy`]). A thumbnail fetches unasked on anything but a
//! constrained network; an original under the auto-fetch threshold fetches
//! unasked on an unmetered one, and on cellular only when
//! `attachments.auto_fetch_on_cellular` is on. A fetch somebody asked for
//! ignores both, because it is not unasked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;
use sunrise_crypto::blob_chunk::{CHUNK_PLAINTEXT_LEN, SEALED_CHUNK_LEN};
use sunrise_domain::{pref_spec, Attachment, DeviceClass, PrefValue};
use sunrise_id::EntityRef;
use sunrise_storage::BlobStore;

use crate::attach::AttachError;
use crate::core::{Core, CoreError};
use crate::engine::read_attachment;

/// The preference that bounds the cache, in bytes.
pub(crate) const CACHE_LIMIT_KEY: &str = "attachments.cache_limit_bytes";

/// The preference that lets originals auto-fetch on cellular.
pub(crate) const AUTO_FETCH_ON_CELLULAR_KEY: &str = "attachments.auto_fetch_on_cellular";

/// The limit when the vault's preferences cannot be read: the key's default
/// for the class of the platform this build runs on, the same value the
/// resolver gives a device with no row in `devices`. A fault then evicts
/// down to what this device would hold anyway, rather than to a handheld's
/// 200 MB on a desktop whose default is 1 GB.
fn platform_default_limit_bytes() -> u64 {
    let class = DeviceClass::of_platform(std::env::consts::OS);
    match pref_spec(CACHE_LIMIT_KEY).and_then(|spec| spec.default.value(class)) {
        Some(PrefValue::Uint(n)) => n,
        // Unreachable while the key is registered with a class default
        // (`a_class_dependent_default_follows_the_device`); the smaller
        // default, so it errs toward keeping less.
        _ => 200_000_000,
    }
}

/// The class of network the client is on, as it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkClass {
    /// Wi-Fi or wired, not metered. The default, so a client that never
    /// reports one keeps the behaviour it had before the gate existed.
    #[default]
    Unmetered,
    /// A cellular or otherwise metered link.
    Cellular,
    /// A link the OS marks constrained: Low Data Mode, or metered with data
    /// saver on.
    Constrained,
}

impl NetworkClass {
    const fn to_u8(self) -> u8 {
        match self {
            Self::Unmetered => 0,
            Self::Cellular => 1,
            Self::Constrained => 2,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Cellular,
            2 => Self::Constrained,
            _ => Self::Unmetered,
        }
    }
}

/// What the automatic fetch drain may fetch on one network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoFetchPolicy {
    /// Thumbnails, whatever their attachment's size.
    pub thumbnails: bool,
    /// Originals under the auto-fetch threshold.
    pub originals: bool,
}

/// ADR-0053 §3 and §6, as a function of the network and the preference.
#[must_use]
pub const fn auto_fetch_policy(network: NetworkClass, on_cellular: bool) -> AutoFetchPolicy {
    match network {
        NetworkClass::Unmetered => AutoFetchPolicy {
            thumbnails: true,
            originals: true,
        },
        NetworkClass::Cellular => AutoFetchPolicy {
            thumbnails: true,
            originals: on_cellular,
        },
        NetworkClass::Constrained => AutoFetchPolicy {
            thumbnails: false,
            originals: false,
        },
    }
}

/// How much this device's attachment cache holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheUsage {
    /// Sealed bytes of every blob the cache holds.
    pub used_bytes: u64,
    /// The part of `used_bytes` eviction may reclaim: what Clear cache frees.
    pub evictable_bytes: u64,
    /// The limit eviction holds `used_bytes` under.
    pub limit_bytes: u64,
}

/// One blob this device can fetch and store: an attachment's original or its
/// thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobRef {
    /// The attachment it belongs to.
    pub attachment: EntityRef,
    /// Local blob id, the chunks' AAD.
    pub blob_id: [u8; 16],
    /// Key the chunks are sealed under.
    pub blob_key: [u8; 32],
    /// Chunk count.
    pub chunk_count: u32,
    /// Plaintext size.
    pub size_bytes: u64,
    /// BLAKE3 of the plaintext.
    pub content_hash: [u8; 32],
    /// The relay's id for it.
    pub relay_id: [u8; 16],
    /// Whether it is a thumbnail, which eviction never touches.
    pub is_thumbnail: bool,
}

impl BlobRef {
    /// The original's blob, when it has a name on the relay or not: the
    /// relay id is only read by a fetch, which checks
    /// [`Attachment::is_fetchable`] first.
    pub(crate) fn original(att: &Attachment) -> Self {
        Self {
            attachment: att.id,
            blob_id: att.blob_id,
            blob_key: att.blob_key,
            chunk_count: att.chunk_count,
            size_bytes: att.size_bytes,
            content_hash: att.content_hash,
            relay_id: att.relay_blob_id().unwrap_or([0u8; 16]),
            is_thumbnail: false,
        }
    }

    /// The thumbnail's blob, when the attachment has a usable one.
    pub(crate) fn thumbnail(att: &Attachment) -> Option<Self> {
        let t = att.thumbnail()?;
        Some(Self {
            attachment: att.id,
            blob_id: t.blob_id,
            blob_key: t.blob_key,
            chunk_count: 1,
            size_bytes: u64::from(t.size_bytes),
            content_hash: t.content_hash,
            relay_id: t.relay_blob_id(),
            is_thumbnail: true,
        })
    }

    /// Bytes the sealed chunks take on disk: the plaintext plus one AEAD tag
    /// per chunk.
    pub(crate) fn sealed_bytes(&self) -> u64 {
        let overhead = (SEALED_CHUNK_LEN - CHUNK_PLAINTEXT_LEN) as u64;
        self.size_bytes
            .saturating_add(overhead.saturating_mul(u64::from(self.chunk_count)))
    }
}

/// The live half of the cache: the reported network and the open previews.
///
/// On [`Core`] rather than in a table because neither outlives the process: a
/// network class is a fact about now, and a preview open in a process that
/// has gone is not open.
#[derive(Debug, Default)]
pub(crate) struct CacheSignals {
    network: AtomicU8,
    /// The attachments an open preview holds: each one's original blob id,
    /// and how many previews hold it. Keyed by the attachment, so a pin is
    /// released without reading the row, which a delete may already have
    /// tombstoned.
    pins: Mutex<HashMap<EntityRef, ([u8; 16], u32)>>,
    /// A limit below the preference's 100 MB floor, so a test can fill the
    /// cache with a few kilobytes.
    #[cfg(test)]
    limit_override: Mutex<Option<u64>>,
}

impl CacheSignals {
    fn is_pinned(&self, blob_id: &[u8; 16]) -> bool {
        self.pins
            .lock()
            .values()
            .any(|(pinned, _)| pinned == blob_id)
    }
}

impl Core {
    /// Report the class of network this device is on now.
    ///
    /// The client calls this whenever the OS says the path changed. Nothing
    /// is fetched or cancelled here: the next automatic drain reads it.
    pub fn set_network_class(&self, class: NetworkClass) {
        self.cache_signals()
            .network
            .store(class.to_u8(), Ordering::Relaxed);
    }

    /// The class the client last reported, [`NetworkClass::Unmetered`] if it
    /// never has.
    #[must_use]
    pub fn network_class(&self) -> NetworkClass {
        NetworkClass::from_u8(self.cache_signals().network.load(Ordering::Relaxed))
    }

    /// What the automatic fetch drain may fetch now.
    pub(crate) fn current_auto_fetch_policy(&self) -> AutoFetchPolicy {
        let on_cellular = matches!(
            self.resolved_preference(AUTO_FETCH_ON_CELLULAR_KEY),
            Some(PrefValue::Bool(true))
        );
        auto_fetch_policy(self.network_class(), on_cellular)
    }

    /// Hold `id`'s bytes in the cache while a preview of them is open.
    ///
    /// Counted: two previews of one attachment pin it twice and release it
    /// on the second [`Core::unpin_attachment`].
    ///
    /// # Errors
    /// [`AttachError::NotFound`] for an unknown or tombstoned id.
    pub async fn pin_attachment(&self, id: EntityRef) -> Result<(), AttachError> {
        let att = self.attachment_row(id).await?;
        self.cache_signals()
            .pins
            .lock()
            .entry(id)
            .or_insert((att.blob_id, 0))
            .1 += 1;
        Ok(())
    }

    /// Release one pin [`Core::pin_attachment`] took. A no-op for an id with
    /// none.
    ///
    /// Reads no row: the preview being closed may be closing because its
    /// attachment was deleted, here or on another device, and a release that
    /// needed the live row would leave that blob pinned for the life of the
    /// process.
    pub fn unpin_attachment(&self, id: EntityRef) {
        let mut pins = self.cache_signals().pins.lock();
        if let Some((_, n)) = pins.get_mut(&id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                pins.remove(&id);
            }
        }
    }

    /// The cache's limit: `attachments.cache_limit_bytes`, resolved, or this
    /// platform's default for it when the vault cannot be read.
    fn cache_limit_bytes(&self) -> u64 {
        #[cfg(test)]
        if let Some(n) = *self.cache_signals().limit_override.lock() {
            return n;
        }
        match self.resolved_preference(CACHE_LIMIT_KEY) {
            Some(PrefValue::Uint(n)) => n,
            _ => platform_default_limit_bytes(),
        }
    }

    /// How much the cache holds, how much of it is evictable, and the limit.
    ///
    /// # Errors
    /// Storage failures.
    pub fn attachment_cache_usage(&self) -> Result<CacheUsage, AttachError> {
        let limit_bytes = self.cache_limit_bytes();
        let db = self.db();
        let used: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(sealed_bytes), 0) FROM blob_cache
                 WHERE evicted_at_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(CoreError::from)?;
        let evictable: u64 = evictable(db.conn(), self.cache_signals())
            .map_err(CoreError::from)?
            .iter()
            .map(|(_, n)| *n)
            .sum();
        Ok(CacheUsage {
            used_bytes: u64::try_from(used).unwrap_or(0),
            evictable_bytes: evictable,
            limit_bytes,
        })
    }

    /// Evict every evictable blob: Settings' **Clear cache**. Returns the
    /// sealed bytes freed.
    ///
    /// # Errors
    /// Storage or blob store failures.
    pub fn clear_attachment_cache(&self) -> Result<u64, AttachError> {
        Ok(self.evict_down_to(0)?)
    }

    /// Evict, least recently opened first, until the cache is under its limit.
    /// Returns the sealed bytes freed.
    ///
    /// Run at launch, and by every path that adds to the cache. The first time
    /// it runs in a vault it indexes any blob on disk the index does not know,
    /// which is every blob of a vault from before the index existed.
    ///
    /// # Errors
    /// Storage or blob store failures.
    pub fn enforce_attachment_cache(&self) -> Result<u64, AttachError> {
        self.index_untracked_blobs()?;
        let limit = self.cache_limit_bytes();
        Ok(self.evict_down_to(limit)?)
    }

    /// Record that `blob`'s chunks are here, opened now, and bring the cache
    /// back under its limit, in one hold of the database lock.
    ///
    /// The blob just stored is spared by this pass. It is the most recently
    /// opened, so it would go last anyway, but a blob larger than the limit
    /// would otherwise be evicted the moment its download finished, and the
    /// Download that asked for it would return with nothing to open. It is
    /// evicted by a later pass, once something newer has been opened.
    ///
    /// # Errors
    /// Storage or blob store failures.
    pub(crate) fn note_blob_cached(&self, blob: &BlobRef) -> Result<(), CoreError> {
        let limit = self.cache_limit_bytes();
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        let db = self.db();
        upsert_cached(db.conn(), blob, now_ms)?;
        evict(db.conn(), self, limit, Some(&blob.blob_id))?;
        Ok(())
    }

    /// Stamp `blob_id` as opened now, so it is the last to be evicted.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn touch_blob(&self, blob_id: &[u8; 16]) -> Result<(), CoreError> {
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        self.db().conn().execute(
            "UPDATE blob_cache SET last_access_ms = ? WHERE blob_id = ?",
            rusqlite::params![now_ms, &blob_id[..]],
        )?;
        Ok(())
    }

    fn evict_down_to(&self, limit: u64) -> Result<u64, CoreError> {
        let db = self.db();
        evict(db.conn(), self, limit, None)
    }

    /// Index every blob on disk the index does not know, as opened at time
    /// zero: older than anything opened since, so a vault upgraded with a full
    /// store evicts what predates the index first.
    ///
    /// Once per vault, recorded in `blob_cache_backfill`. Only a vault from
    /// before the index holds blobs it does not know: every later attach and
    /// fetch indexes what it stores. Run at every launch it would probe the
    /// store for every remote original never fetched, a cost that grows with
    /// the vault, under the database lock.
    fn index_untracked_blobs(&self) -> Result<(), CoreError> {
        let done: bool = self.db().conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM blob_cache_backfill)",
            [],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let ids: Vec<Vec<u8>> = {
            let db = self.db();
            let mut stmt = db.conn().prepare(
                "SELECT a.id FROM attachments a
                 WHERE NOT EXISTS (SELECT 1 FROM blob_cache c WHERE c.blob_id = a.blob_id)
                    OR (a.thumbnail_blob_id IS NOT NULL AND NOT EXISTS
                        (SELECT 1 FROM blob_cache c WHERE c.blob_id = a.thumbnail_blob_id))",
            )?;
            let rows = stmt
                .query_map([], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let store = BlobStore::new(self.vault_dir())?;
        let db = self.db();
        for raw in ids {
            let mut id = [0u8; 16];
            let take = raw.len().min(16);
            id[..take].copy_from_slice(&raw[..take]);
            let Some(att) = read_attachment(db.conn(), &id)? else {
                continue;
            };
            let blobs = std::iter::once(BlobRef::original(&att)).chain(BlobRef::thumbnail(&att));
            for blob in blobs {
                if store.has_all(&blob.blob_id, blob.chunk_count)? {
                    db.conn().execute(
                        "INSERT OR IGNORE INTO blob_cache
                         (blob_id, sealed_bytes, last_access_ms, is_thumbnail, evicted_at_ms)
                         VALUES (?, ?, 0, ?, NULL)",
                        rusqlite::params![
                            &blob.blob_id[..],
                            i64::try_from(blob.sealed_bytes()).unwrap_or(i64::MAX),
                            i64::from(blob.is_thumbnail),
                        ],
                    )?;
                }
            }
        }
        db.conn().execute(
            "INSERT OR IGNORE INTO blob_cache_backfill (id, done_at_ms) VALUES (1, ?)",
            [i64::try_from(self.now_ms()).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
}

/// Insert or refresh `blob`'s row: present, opened at `now_ms`.
fn upsert_cached(conn: &rusqlite::Connection, blob: &BlobRef, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO blob_cache (blob_id, sealed_bytes, last_access_ms, is_thumbnail, evicted_at_ms)
         VALUES (?, ?, ?, ?, NULL)
         ON CONFLICT (blob_id) DO UPDATE SET
             sealed_bytes = excluded.sealed_bytes,
             last_access_ms = excluded.last_access_ms,
             is_thumbnail = excluded.is_thumbnail,
             evicted_at_ms = NULL",
        rusqlite::params![
            &blob.blob_id[..],
            i64::try_from(blob.sealed_bytes()).unwrap_or(i64::MAX),
            now_ms,
            i64::from(blob.is_thumbnail),
        ],
    )?;
    Ok(())
}

/// The evictable blobs, least recently opened first, with their sealed sizes.
fn evictable(
    conn: &rusqlite::Connection,
    signals: &CacheSignals,
) -> rusqlite::Result<Vec<([u8; 16], u64)>> {
    let mut stmt = conn.prepare(
        "SELECT c.blob_id, c.sealed_bytes FROM blob_cache c
         WHERE c.evicted_at_ms IS NULL
           AND c.is_thumbnail = 0
           AND NOT EXISTS (SELECT 1 FROM blob_uploads u WHERE u.blob_id = c.blob_id)
         ORDER BY c.last_access_ms ASC, c.blob_id ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let raw: Vec<u8> = r.get(0)?;
            let n: i64 = r.get(1)?;
            let mut id = [0u8; 16];
            let take = raw.len().min(16);
            id[..take].copy_from_slice(&raw[..take]);
            Ok((id, u64::try_from(n).unwrap_or(0)))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(id, _)| !signals.is_pinned(id))
        .collect())
}

/// Evict least recently opened first until the cache holds at most `limit`
/// sealed bytes, or nothing evictable is left, sparing `keep`. Returns the
/// bytes freed.
fn evict(
    conn: &rusqlite::Connection,
    core: &Core,
    limit: u64,
    keep: Option<&[u8; 16]>,
) -> Result<u64, CoreError> {
    let used: i64 = conn.query_row(
        "SELECT COALESCE(SUM(sealed_bytes), 0) FROM blob_cache WHERE evicted_at_ms IS NULL",
        [],
        |r| r.get(0),
    )?;
    let mut used = u64::try_from(used).unwrap_or(0);
    if used <= limit {
        return Ok(0);
    }
    let store = BlobStore::new(core.vault_dir())?;
    let now_ms = i64::try_from(core.now_ms()).unwrap_or(i64::MAX);
    let mut freed = 0u64;
    for (blob_id, n) in evictable(conn, core.cache_signals())? {
        if used <= limit {
            break;
        }
        if keep == Some(&blob_id) {
            continue;
        }
        store.delete_all(&blob_id)?;
        conn.execute(
            "UPDATE blob_cache SET evicted_at_ms = ? WHERE blob_id = ?",
            rusqlite::params![now_ms, &blob_id[..]],
        )?;
        used = used.saturating_sub(n);
        freed = freed.saturating_add(n);
    }
    if freed > 0 {
        tracing::info!(
            ev = "core.attachment.cache_evicted",
            n_bytes = freed,
            "attachment bytes were evicted from this device's cache to keep it under its limit"
        );
    }
    Ok(freed)
}

#[cfg(test)]
mod tests;

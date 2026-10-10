//! The attachment cache, the network gate and the thumbnail byte path
//! (ADR-0053 §1–§6, issue #346).

use super::*;
use crate::attach::{AttachPreview, ThumbnailImage};
use crate::commands::Command;
use crate::config::CoreConfig;
use crate::unlock::Unlock;
use parking_lot::Mutex as PLMutex;
use std::sync::Arc;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{PrefTarget, TaskDraft, MAX_THUMBNAIL_BYTES};

#[derive(Debug)]
struct FakeClock(PLMutex<u64>);
impl crate::config::Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock()
    }
}

/// A counting RNG, so every key and id the core mints is distinct and
/// repeatable.
#[derive(Debug, Default)]
struct CountingRng(PLMutex<u8>);
impl crate::config::Rng for CountingRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        let mut n = self.0.lock();
        for b in dest.iter_mut() {
            *b = *n;
            *n = n.wrapping_add(1);
        }
    }
}

struct Vault {
    _dir: tempfile::TempDir,
    core: Arc<Core>,
    clock: Arc<FakeClock>,
    task: EntityRef,
}

impl Vault {
    async fn core_at(dir: &std::path::Path, clock: Arc<FakeClock>) -> Result<Core, CoreError> {
        let cfg = CoreConfig::with_clock(
            dir.to_path_buf(),
            "0.1.0+test",
            clock,
            Arc::new(CountingRng::default()),
        );
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes([7u8; 32]),
                paired: None,
            },
        )
        .await
    }

    /// Close the vault and open it again: a relaunch.
    async fn reopen(self) -> Self {
        let Self {
            _dir: dir,
            core,
            clock,
            task,
        } = self;
        drop(Arc::into_inner(core).expect("the only handle"));
        let core = Arc::new(
            Self::core_at(dir.path(), clock.clone())
                .await
                .expect("reopen"),
        );
        Self {
            _dir: dir,
            core,
            clock,
            task,
        }
    }

    async fn open() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(FakeClock(PLMutex::new(1_700_000_000_000)));
        let core = Arc::new(
            Self::core_at(dir.path(), clock.clone())
                .await
                .expect("open"),
        );
        let task = core
            .submit(Command::CreateTask(TaskDraft {
                title: "Renew the passport".into(),
                ..Default::default()
            }))
            .await
            .expect("task")
            .entity;
        Self {
            _dir: dir,
            core,
            clock,
            task,
        }
    }

    fn at(&self, ms: u64) {
        *self.clock.0.lock() = 1_700_000_000_000 + ms;
    }

    fn limit(&self, n: u64) {
        *self.core.cache_signals().limit_override.lock() = Some(n);
    }

    /// Attach `n` bytes and mark the upload done, so the blob is evictable.
    async fn uploaded(&self, name: &str, n: usize) -> Attachment {
        let att = self.attached(name, n).await;
        self.core.clear_blob_upload(&att.blob_id).unwrap();
        att
    }

    async fn attached(&self, name: &str, n: usize) -> Attachment {
        let bytes: Vec<u8> = (0..n).map(|i| u8::try_from(i % 251).unwrap()).collect();
        self.core
            .attach_file(self.task, name.into(), "text/plain".into(), &bytes)
            .await
            .expect("attach")
    }

    fn local(&self, att: &Attachment) -> bool {
        self.core.attachment_is_local(att).unwrap()
    }

    /// How many blobs have a directory in the blob store.
    fn blob_dirs(&self) -> usize {
        std::fs::read_dir(self.core.vault_dir().join("blobs"))
            .unwrap()
            .map(|prefix| std::fs::read_dir(prefix.unwrap().path()).unwrap().count())
            .sum()
    }
}

fn jpeg(n: usize) -> ThumbnailImage {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.resize(n, 0x42);
    ThumbnailImage {
        mime_type: "image/jpeg".into(),
        bytes,
    }
}

fn sealed_size(n: u64) -> u64 {
    BlobRef {
        attachment: EntityRef::new(sunrise_id::EntityKind::Attachment, [0; 16]),
        blob_id: [0; 16],
        blob_key: [0; 32],
        chunk_count: 1,
        size_bytes: n,
        content_hash: [0; 32],
        relay_id: [0; 16],
        is_thumbnail: false,
    }
    .sealed_bytes()
}

// ---- the network gate ----

#[test]
fn the_policy_follows_the_network_and_the_cellular_preference() {
    let all = AutoFetchPolicy {
        thumbnails: true,
        originals: true,
    };
    let thumbs = AutoFetchPolicy {
        thumbnails: true,
        originals: false,
    };
    let none = AutoFetchPolicy {
        thumbnails: false,
        originals: false,
    };
    assert_eq!(auto_fetch_policy(NetworkClass::Unmetered, false), all);
    assert_eq!(auto_fetch_policy(NetworkClass::Cellular, false), thumbs);
    assert_eq!(auto_fetch_policy(NetworkClass::Cellular, true), all);
    assert_eq!(auto_fetch_policy(NetworkClass::Constrained, true), none);
}

/// The drain's queue under each network class the client can report, with
/// the preference read from the vault rather than passed in.
#[tokio::test]
async fn the_fetch_drain_decides_from_the_reported_network() {
    let v = Vault::open().await;
    let plain = v.attached("a.txt", 64).await;
    let pictured = v
        .core
        .attach_file_with(
            v.task,
            "b.jpg".into(),
            "image/jpeg".into(),
            b"original",
            AttachPreview {
                width: Some(2),
                height: Some(1),
                thumbnail: Some(jpeg(32)),
            },
        )
        .await
        .unwrap();
    let thumb_id = pictured.thumbnail().unwrap().blob_id;
    // A replica that has the metadata and none of the bytes.
    std::fs::remove_dir_all(v.core.vault_dir().join("blobs")).unwrap();

    let queue = |core: &Core| -> Vec<([u8; 16], bool)> {
        core.attachments_awaiting_bytes()
            .unwrap()
            .into_iter()
            .map(|b| (b.blob_id, b.is_thumbnail))
            .collect()
    };

    assert_eq!(v.core.network_class(), NetworkClass::Unmetered);
    let unmetered = queue(&v.core);
    assert_eq!(unmetered[0], (thumb_id, true), "thumbnails come first");
    assert!(unmetered.contains(&(plain.blob_id, false)));
    assert!(unmetered.contains(&(pictured.blob_id, false)));

    v.core.set_network_class(NetworkClass::Cellular);
    assert_eq!(queue(&v.core), vec![(thumb_id, true)]);

    v.core
        .submit(Command::SetPreference {
            key: AUTO_FETCH_ON_CELLULAR_KEY.into(),
            value: PrefValue::Bool(true),
            target: PrefTarget::Device,
        })
        .await
        .unwrap();
    assert_eq!(queue(&v.core).len(), 3);

    v.core.set_network_class(NetworkClass::Constrained);
    assert!(queue(&v.core).is_empty());
}

// ---- thumbnails ----

#[tokio::test]
async fn a_thumbnail_is_its_own_blob_under_its_own_key_and_is_uploaded() {
    let v = Vault::open().await;
    let att = v
        .core
        .attach_file_with(
            v.task,
            "photo.jpg".into(),
            "image/jpeg".into(),
            b"the whole photo",
            AttachPreview {
                width: Some(4032),
                height: Some(3024),
                thumbnail: Some(jpeg(100)),
            },
        )
        .await
        .unwrap();
    let thumb = att.thumbnail().expect("a thumbnail");
    assert_ne!(thumb.blob_key, att.blob_key);
    assert_ne!(thumb.blob_id, att.blob_id);
    assert_eq!((att.width, att.height), (Some(4032), Some(3024)));

    let queued: Vec<[u8; 16]> = v
        .core
        .pending_blob_uploads()
        .unwrap()
        .into_iter()
        .map(|u| u.blob_id)
        .collect();
    assert!(queued.contains(&att.blob_id) && queued.contains(&thumb.blob_id));

    let (mime, bytes) = v.core.thumbnail_bytes(att.id).await.unwrap().unwrap();
    assert_eq!(mime, "image/jpeg");
    assert_eq!(bytes, jpeg(100).bytes);
    assert_eq!(
        v.core
            .thumbnail_bytes(v.attached("x", 3).await.id)
            .await
            .unwrap(),
        None
    );
}

/// While a paired device has not advertised `attachment.thumbnail`, the
/// engine records the attachment without its thumbnail. The thumbnail's
/// sealed chunks are then referenced by nothing, so they are deleted, and no
/// upload is queued for them.
#[tokio::test]
async fn a_thumbnail_the_engine_dropped_leaves_no_chunks_and_no_upload() {
    let v = Vault::open().await;
    // A paired device that never sent `DeviceFeatures`.
    v.core
        .db()
        .conn()
        .execute(
            "INSERT INTO devices (device_id, cert_blob, nickname, platform, created_at_ms)
             VALUES (?, x'00', 'old phone', 'ios', 0)",
            [&[0xEEu8; 16][..]],
        )
        .unwrap();

    let att = v
        .core
        .attach_file_with(
            v.task,
            "photo.jpg".into(),
            "image/jpeg".into(),
            b"the whole photo",
            AttachPreview {
                width: Some(4032),
                height: Some(3024),
                thumbnail: Some(jpeg(100)),
            },
        )
        .await
        .unwrap();
    assert!(att.thumbnail().is_none(), "the engine dropped it: {att:?}");

    let queued: Vec<[u8; 16]> = v
        .core
        .pending_blob_uploads()
        .unwrap()
        .into_iter()
        .map(|u| u.blob_id)
        .collect();
    assert_eq!(queued, vec![att.blob_id], "only the original uploads");
    assert_eq!(v.blob_dirs(), 1, "only the original's chunks are on disk");
    assert!(v.local(&att));
    let indexed: i64 = v
        .core
        .db()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_cache", [], |r| r.get(0))
        .unwrap();
    assert_eq!(indexed, 1, "only the original is indexed");
}

#[tokio::test]
async fn a_thumbnail_a_receiver_might_not_decode_is_refused_before_anything_is_written() {
    let v = Vault::open().await;
    for bad in [
        ThumbnailImage {
            mime_type: "image/avif".into(),
            bytes: vec![0, 0, 0, 0x1c],
        },
        ThumbnailImage {
            mime_type: "image/png".into(),
            bytes: jpeg(16).bytes,
        },
        jpeg(MAX_THUMBNAIL_BYTES as usize + 1),
    ] {
        let r = v
            .core
            .attach_file_with(
                v.task,
                "p".into(),
                "image/jpeg".into(),
                b"bytes",
                AttachPreview {
                    thumbnail: Some(bad),
                    ..AttachPreview::default()
                },
            )
            .await;
        assert!(matches!(r, Err(AttachError::BadThumbnail(_))), "{r:?}");
    }
    assert!(v.core.pending_blob_uploads().unwrap().is_empty());
}

// ---- the cache ----

/// Least recently opened goes first, and an open counts as use.
#[tokio::test]
async fn eviction_takes_the_least_recently_opened_first() {
    let v = Vault::open().await;
    v.at(1);
    let a = v.uploaded("a", 1000).await;
    v.at(2);
    let b = v.uploaded("b", 1000).await;
    v.at(3);
    let c = v.uploaded("c", 1000).await;
    v.at(4);
    v.core.attachment_bytes(a.id).await.unwrap();

    v.limit(2 * sealed_size(1000));
    assert_eq!(
        v.core.enforce_attachment_cache().unwrap(),
        sealed_size(1000)
    );
    assert!(v.local(&a) && !v.local(&b) && v.local(&c));

    v.limit(sealed_size(1000));
    v.core.enforce_attachment_cache().unwrap();
    assert!(v.local(&a) && !v.local(&c));
    let usage = v.core.attachment_cache_usage().unwrap();
    assert_eq!(usage.used_bytes, sealed_size(1000));
    assert_eq!(usage.limit_bytes, sealed_size(1000));
}

/// A blob not yet on the relay may exist nowhere else, a thumbnail is what
/// the list draws offline, and a preview is reading the blob right now.
#[tokio::test]
async fn eviction_never_touches_a_pending_upload_a_thumbnail_or_a_pinned_blob() {
    let v = Vault::open().await;
    let pending = v.attached("pending", 500).await;
    let pictured = v
        .core
        .attach_file_with(
            v.task,
            "p.jpg".into(),
            "image/jpeg".into(),
            &[9u8; 500],
            AttachPreview {
                thumbnail: Some(jpeg(64)),
                ..AttachPreview::default()
            },
        )
        .await
        .unwrap();
    let thumb = BlobRef::thumbnail(&pictured).unwrap();
    v.core.clear_blob_upload(&pictured.blob_id).unwrap();
    v.core.clear_blob_upload(&thumb.blob_id).unwrap();

    v.core.pin_attachment(pictured.id).await.unwrap();
    v.core.pin_attachment(pictured.id).await.unwrap();
    assert_eq!(v.core.clear_attachment_cache().unwrap(), 0);
    v.core.unpin_attachment(pictured.id);
    assert_eq!(
        v.core.clear_attachment_cache().unwrap(),
        0,
        "still pinned once"
    );
    v.core.unpin_attachment(pictured.id);

    let usage = v.core.attachment_cache_usage().unwrap();
    assert_eq!(usage.evictable_bytes, sealed_size(500));
    assert_eq!(v.core.clear_attachment_cache().unwrap(), sealed_size(500));
    assert!(v.local(&pending), "an upload that has not happened is kept");
    assert!(!v.local(&pictured));
    assert!(v.core.thumbnail_bytes(pictured.id).await.unwrap().is_some());
}

/// A preview closed because its attachment was deleted still releases its
/// pin: the release reads no row, so a tombstone cannot strand the blob
/// pinned for the life of the process.
#[tokio::test]
async fn a_pin_is_released_after_its_attachment_is_deleted() {
    let v = Vault::open().await;
    let att = v.uploaded("doomed", 500).await;
    v.core.pin_attachment(att.id).await.unwrap();
    v.core.submit(Command::DetachFile(att.id)).await.unwrap();
    assert_eq!(v.core.clear_attachment_cache().unwrap(), 0, "pinned");
    v.core.unpin_attachment(att.id);
    assert_eq!(v.core.clear_attachment_cache().unwrap(), sealed_size(500));
}

/// When the vault's preferences cannot be read, the limit is this
/// platform's default for the key, not a fixed handheld value.
#[test]
fn the_fallback_limit_is_this_platforms_default() {
    let class = DeviceClass::of_platform(std::env::consts::OS);
    assert_eq!(
        sunrise_domain::pref_spec(CACHE_LIMIT_KEY).and_then(|s| s.default.value(class)),
        Some(PrefValue::Uint(platform_default_limit_bytes()))
    );
    if class == DeviceClass::Desktop {
        assert_eq!(platform_default_limit_bytes(), 1_000_000_000);
    }
}

/// An evicted blob is not fetched back unasked, and comes back through the
/// ordinary fetch path when somebody asks.
#[tokio::test]
async fn a_blob_fetched_again_after_eviction_opens_and_is_counted_again() {
    let v = Vault::open().await;
    let att = v.uploaded("a", 2000).await;
    let original = v.core.attachment_bytes(att.id).await.unwrap();
    let sealed = v
        .core
        .sealed_chunks(&att.blob_id, att.chunk_count)
        .unwrap()
        .unwrap()
        .concat();

    v.core.clear_attachment_cache().unwrap();
    assert!(matches!(
        v.core.attachment_bytes(att.id).await,
        Err(AttachError::BytesNotHere { .. })
    ));
    assert!(
        v.core.attachments_awaiting_bytes().unwrap().is_empty(),
        "an evicted original waits to be asked for"
    );

    assert!(v.core.store_fetched_blob(&att, &sealed).unwrap());
    assert_eq!(v.core.attachment_bytes(att.id).await.unwrap(), original);
    assert_eq!(
        v.core.attachment_cache_usage().unwrap().used_bytes,
        sealed_size(2000)
    );
}

/// A Download of a blob larger than the whole limit returns with the bytes
/// here: the pass its own store runs spares it, and the next store evicts it.
#[tokio::test]
async fn a_blob_larger_than_the_limit_survives_its_own_fetch() {
    let v = Vault::open().await;
    let big = v.uploaded("big", 4000).await;
    let body = v
        .core
        .sealed_chunks(&big.blob_id, big.chunk_count)
        .unwrap()
        .unwrap()
        .concat();
    v.core.clear_attachment_cache().unwrap();
    v.limit(sealed_size(1000));

    v.at(10);
    assert!(v.core.store_fetched_blob(&big, &body).unwrap());
    assert!(
        v.local(&big),
        "the Download that asked for it has something to open"
    );

    v.at(20);
    let small = v.uploaded("small", 500).await;
    assert!(!v.local(&big) && v.local(&small));
}

/// Fetches finishing together each record their blob and enforce the limit
/// under one hold of the database lock, so none of them can leave the cache
/// over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limit_holds_under_concurrent_fetches() {
    let v = Vault::open().await;
    let mut bodies = Vec::new();
    for i in 0..8 {
        let att = v.uploaded(&format!("f{i}"), 3000).await;
        let body = v
            .core
            .sealed_chunks(&att.blob_id, att.chunk_count)
            .unwrap()
            .unwrap()
            .concat();
        bodies.push((att, body));
    }
    v.core.clear_attachment_cache().unwrap();
    let limit = 3 * sealed_size(3000);
    v.limit(limit);

    std::thread::scope(|s| {
        for (att, body) in &bodies {
            let core = &v.core;
            s.spawn(move || assert!(core.store_fetched_blob(att, body).unwrap()));
        }
    });

    let usage = v.core.attachment_cache_usage().unwrap();
    assert!(usage.used_bytes <= limit, "{usage:?}");
    let on_disk = bodies.iter().filter(|(a, _)| v.local(a)).count() as u64;
    assert_eq!(on_disk * sealed_size(3000), usage.used_bytes);
}

/// A vault from before the index existed has blobs the index does not know.
/// The launch pass finds them, so they count and can be evicted.
#[tokio::test]
async fn blobs_from_before_the_index_are_indexed_at_launch() {
    let v = Vault::open().await;
    let att = v.uploaded("old", 1000).await;
    v.core
        .db()
        .conn()
        .execute_batch("DELETE FROM blob_cache; DELETE FROM blob_cache_backfill;")
        .unwrap();
    assert_eq!(v.core.attachment_cache_usage().unwrap().used_bytes, 0);
    v.core.enforce_attachment_cache().unwrap();
    assert_eq!(
        v.core.attachment_cache_usage().unwrap().used_bytes,
        sealed_size(1000)
    );
    assert!(v.local(&att));
}

/// The pass that indexes blobs from before the index runs once per vault.
/// After it, every blob is indexed by the attach or fetch that stored it, so
/// a later launch does not probe the store again for each attachment whose
/// bytes are not here.
#[tokio::test]
async fn the_launch_indexing_pass_runs_once_per_vault() {
    let v = Vault::open().await;
    let done: i64 = v
        .core
        .db()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_cache_backfill", [], |r| r.get(0))
        .unwrap();
    assert_eq!(done, 1, "Core::open ran the pass");

    v.uploaded("old", 1000).await;
    v.core
        .db()
        .conn()
        .execute("DELETE FROM blob_cache", [])
        .unwrap();
    v.core.enforce_attachment_cache().unwrap();
    assert_eq!(
        v.core.attachment_cache_usage().unwrap().used_bytes,
        0,
        "a second pass would have indexed the blob again"
    );
}

// ---- at launch ----

/// Every `ev` field the events emitted on this thread carried, while the
/// guard lives.
#[derive(Clone, Default)]
struct EvLog(Arc<PLMutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EvLog {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Ev<'a>(&'a mut Vec<String>);
        impl tracing::field::Visit for Ev<'_> {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "ev" {
                    self.0.push(value.to_owned());
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        event.record(&mut Ev(&mut self.0.lock()));
    }
}

/// A cache that went over its limit while the vault was closed is brought
/// under it by `Core::open` itself, before anything else adds to it.
#[tokio::test]
async fn opening_a_vault_evicts_a_cache_over_its_limit() {
    let v = Vault::open().await;
    v.at(1);
    let old = v.uploaded("old", 1000).await;
    v.at(2);
    let new = v.uploaded("new", 1000).await;
    v.core
        .submit(Command::SetPreference {
            key: CACHE_LIMIT_KEY.into(),
            value: PrefValue::Uint(100_000_000),
            target: PrefTarget::Device,
        })
        .await
        .unwrap();
    // The cheapest way to a cache over a 100 MB floor: the index says the
    // older blob is 150 MB.
    v.core
        .db()
        .conn()
        .execute(
            "UPDATE blob_cache SET sealed_bytes = 150000000 WHERE blob_id = ?",
            [&old.blob_id[..]],
        )
        .unwrap();

    let v = v.reopen().await;
    assert!(!v.local(&old), "evicted during open");
    assert!(v.local(&new));
    let usage = v.core.attachment_cache_usage().unwrap();
    assert_eq!(usage.used_bytes, sealed_size(1000));
    assert_eq!(usage.limit_bytes, 100_000_000);
}

/// Enforcement that fails at launch is logged, and the vault opens anyway:
/// an over-full cache is no reason to refuse it.
#[tokio::test]
async fn a_failed_launch_enforcement_is_logged_and_the_vault_opens() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let v = Vault::open().await;
    v.core
        .db()
        .conn()
        .execute("DROP TABLE blob_cache", [])
        .unwrap();
    let Vault {
        _dir: vault_dir,
        core,
        clock,
        ..
    } = v;
    drop(Arc::into_inner(core).expect("the only handle"));

    let log = EvLog::default();
    let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
    let reopened = Vault::core_at(vault_dir.path(), clock).await;
    drop(guard);
    assert!(reopened.is_ok(), "{:?}", reopened.err());
    assert!(
        log.0
            .lock()
            .iter()
            .any(|ev| ev == "core.attachment.cache_enforce_failed"),
        "logged: {:?}",
        log.0.lock()
    );
}

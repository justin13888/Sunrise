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
    async fn open() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(FakeClock(PLMutex::new(1_700_000_000_000)));
        let cfg = CoreConfig::with_clock(
            dir.path().to_path_buf(),
            "0.1.0+test",
            clock.clone(),
            Arc::new(CountingRng::default()),
        );
        let core = Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes([7u8; 32]),
                    paired: None,
                },
            )
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
    v.core.unpin_attachment(pictured.id).await.unwrap();
    assert_eq!(
        v.core.clear_attachment_cache().unwrap(),
        0,
        "still pinned once"
    );
    v.core.unpin_attachment(pictured.id).await.unwrap();

    let usage = v.core.attachment_cache_usage().unwrap();
    assert_eq!(usage.evictable_bytes, sealed_size(500));
    assert_eq!(v.core.clear_attachment_cache().unwrap(), sealed_size(500));
    assert!(v.local(&pending), "an upload that has not happened is kept");
    assert!(!v.local(&pictured));
    assert!(v.core.thumbnail_bytes(pictured.id).await.unwrap().is_some());
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
        .execute("DELETE FROM blob_cache", [])
        .unwrap();
    assert_eq!(v.core.attachment_cache_usage().unwrap().used_bytes, 0);
    v.core.enforce_attachment_cache().unwrap();
    assert_eq!(
        v.core.attachment_cache_usage().unwrap().used_bytes,
        sealed_size(1000)
    );
    assert!(v.local(&att));
}

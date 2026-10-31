//! The blob upload's error arms (issue #604).

use super::*;

/// A relay whose `PUT` of a chunk answers `refusal`, after an `init` that
/// hands out `up_fresh`. Counts the `init` calls.
struct RefusesChunks {
    refusal: fn() -> TransportError,
    inits: u32,
    puts: u32,
}

#[async_trait]
impl Transport for RefusesChunks {
    async fn send_frame(&mut self, _f: Vec<u8>) -> Result<(), TransportError> {
        Ok(())
    }
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(None)
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
    async fn blob_init(
        &mut self,
        _stream_id: &[u8; 16],
        _chunk_count: u32,
        _size_bytes: u64,
    ) -> Result<String, TransportError> {
        self.inits += 1;
        Ok("up_fresh".into())
    }
    async fn blob_put_chunk(
        &mut self,
        _upload_id: &str,
        _chunk_idx: u32,
        _bytes: &[u8],
    ) -> Result<(), TransportError> {
        self.puts += 1;
        Err((self.refusal)())
    }
}

/// A vault with one attached file whose upload is queued under the
/// reserved id `up_reserved`.
async fn a_queued_upload(dir: &Path) -> (Arc<Core>, [u8; 16]) {
    let core = open_arc(dir).await;
    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "File the return".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    let att = core
        .attach_file(
            task,
            "return.pdf".into(),
            "application/pdf".into(),
            b"hello",
        )
        .await
        .unwrap();
    core.record_blob_upload_id(&att.blob_id, "up_reserved")
        .unwrap();
    (core, att.blob_id)
}

/// The queued row's `(attempts, upload_id)`, or `None` once it is gone.
fn upload_row(core: &Core, blob_id: &[u8; 16]) -> Option<(i64, Option<String>)> {
    use rusqlite::OptionalExtension;
    core.db()
        .conn()
        .query_row(
            "SELECT attempts, upload_id FROM blob_uploads WHERE blob_id = ?",
            rusqlite::params![&blob_id[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

/// A refusal of the upload id itself counts an attempt and drops the id,
/// so the next attempt reserves a fresh one; the row stays queued.
#[tokio::test]
async fn an_upload_refused_for_its_id_counts_an_attempt_and_drops_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let (core, blob_id) = a_queued_upload(dir.path()).await;
    let mut relay = RefusesChunks {
        refusal: || TransportError::Server {
            code: ErrorCode::SyncOpInvalid.as_str(),
            message: "unknown upload".into(),
        },
        inits: 0,
        puts: 0,
    };

    assert_eq!(
        crate::blob_sync::upload_queued_blobs(&core, &mut relay).await,
        Ok(())
    );
    assert_eq!(relay.inits, 0, "the reserved id is tried first");
    assert_eq!(relay.puts, 1);
    assert_eq!(upload_row(&core, &blob_id), Some((1, None)));

    crate::blob_sync::upload_queued_blobs(&core, &mut relay)
        .await
        .unwrap();
    assert_eq!(relay.inits, 1, "a dropped id is replaced by a fresh init");
}

/// Any other failure counts an attempt and keeps the reserved id, so the
/// retry re-uses the relay's pending upload rather than opening another.
#[tokio::test]
async fn an_upload_that_fails_otherwise_counts_an_attempt_and_keeps_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let (core, blob_id) = a_queued_upload(dir.path()).await;
    let mut relay = RefusesChunks {
        refusal: || TransportError::Unavailable("connection reset".into()),
        inits: 0,
        puts: 0,
    };

    crate::blob_sync::upload_queued_blobs(&core, &mut relay)
        .await
        .unwrap();
    assert_eq!(
        upload_row(&core, &blob_id),
        Some((1, Some("up_reserved".into())))
    );
    crate::blob_sync::upload_queued_blobs(&core, &mut relay)
        .await
        .unwrap();
    assert_eq!(relay.inits, 0, "the kept id is re-used");
    assert_eq!(
        upload_row(&core, &blob_id),
        Some((2, Some("up_reserved".into())))
    );
}

/// A transport with no blob routes ends the drain with `Err(())` and
/// charges the row nothing.
#[tokio::test]
async fn an_upload_over_a_transport_without_blob_routes_is_not_charged() {
    let dir = tempfile::tempdir().unwrap();
    let (core, blob_id) = a_queued_upload(dir.path()).await;
    let mut relay = RefusesChunks {
        refusal: || TransportError::Unsupported,
        inits: 0,
        puts: 0,
    };

    assert_eq!(
        crate::blob_sync::upload_queued_blobs(&core, &mut relay).await,
        Err(())
    );
    assert_eq!(
        upload_row(&core, &blob_id),
        Some((0, Some("up_reserved".into())))
    );
}

/// A queued blob whose chunks are not on this disk is dropped from the
/// queue without asking the relay anything, since no retry can upload it.
#[tokio::test]
async fn an_upload_whose_chunks_are_gone_leaves_the_queue_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let (core, blob_id) = a_queued_upload(dir.path()).await;
    std::fs::remove_dir_all(dir.path().join("blobs")).unwrap();
    let mut relay = RefusesChunks {
        refusal: || TransportError::Unavailable("never reached".into()),
        inits: 0,
        puts: 0,
    };

    assert_eq!(
        crate::blob_sync::upload_queued_blobs(&core, &mut relay).await,
        Ok(())
    );
    assert_eq!((relay.inits, relay.puts), (0, 0));
    assert_eq!(upload_row(&core, &blob_id), None);
}

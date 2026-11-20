//! Attachment bytes across the seam: attaching a file, reading it back,
//! downloading it, and the cache that holds it.
//!
//! The metadata rows cross as [`crate::dto::AttachmentItem`] through the
//! ordinary query path. What is here is the byte path, which needs a vault
//! method per step because each step can wait on a blob store or a relay.

use sunrise_core::{AttachmentFetchState, NetworkClass};
use sunrise_id::EntityRef;

use crate::{dto, BindingError, SunriseCore};

/// See [`sunrise_core::AttachmentFetchState`].
///
/// Declared remotely rather than mirrored so that a new state upstream fails
/// this crate's build instead of becoming a value the app cannot name.
#[uniffi::remote(Enum)]
pub enum AttachmentFetchState {
    Idle,
    Requested,
    Partial,
}

/// See [`sunrise_core::NetworkClass`].
#[uniffi::remote(Enum)]
pub enum NetworkClass {
    Unmetered,
    Cellular,
    Constrained,
}

impl From<sunrise_core::AttachError> for BindingError {
    fn from(e: sunrise_core::AttachError) -> Self {
        match e {
            sunrise_core::AttachError::BytesNotHere { id } => {
                Self::AttachmentNotHere { id: id.to_str() }
            }
            sunrise_core::AttachError::FetchCancelled { id } => {
                Self::AttachmentDownloadCancelled { id: id.to_str() }
            }
            other => Self::Attachment(other.to_string()),
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Attach a file to a task: seal the bytes, store them, record the
    /// metadata, and return the row that describes them.
    ///
    /// The bytes cross the seam whole rather than as a path. The core would
    /// otherwise have to open a file the app chose, and on macOS an
    /// app-chosen file arrives with a security scope the Rust side cannot
    /// hold — so the app reads it and hands over what it read.
    ///
    /// `mime_type` is the caller's: the platform's own type database is what
    /// knows that `.heic` is `image/heic`, and reimplementing that in the core
    /// would be a worse answer that also had to be maintained.
    ///
    /// `preview` is what the platform could tell about the file: the
    /// original's dimensions and a JPEG or PNG thumbnail it rendered
    /// (ADR-0053 §1). The core seals the thumbnail as a blob of its own. Pass
    /// [`dto::AttachPreviewIn`] with every field `None` when there is nothing.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] for an empty or oversized file, a
    /// thumbnail that is not a JPEG or PNG of one chunk, an unknown parent
    /// task, or a blob store that could not be written.
    pub async fn attach_file(
        &self,
        task: EntityRef,
        filename: String,
        mime_type: String,
        bytes: Vec<u8>,
        preview: dto::AttachPreviewIn,
    ) -> Result<dto::AttachmentItem, BindingError> {
        let att = self
            .inner
            .attach_file_with(task, filename, mime_type, &bytes, preview.into())
            .await?;
        Ok(dto::AttachmentItem::from(&att))
    }

    /// One attachment's thumbnail, verified, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// [`BindingError::AttachmentNotHere`] when the thumbnail has not been
    /// fetched yet. Everything else is [`BindingError::Attachment`].
    pub async fn thumbnail_bytes(
        &self,
        id: EntityRef,
    ) -> Result<Option<dto::ThumbnailOut>, BindingError> {
        Ok(self
            .inner
            .thumbnail_bytes(id)
            .await?
            .map(|(mime_type, bytes)| dto::ThumbnailOut { mime_type, bytes }))
    }

    /// Report the class of network this device is on, whenever the OS says
    /// it changed. The fetch drain decides from it what to fetch unasked.
    pub fn set_network_class(&self, network: NetworkClass) {
        self.inner.set_network_class(network);
    }

    /// Hold an attachment's bytes in the cache while a preview of them is
    /// open. Release with [`SunriseCore::unpin_attachment`].
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] for an unknown or tombstoned id.
    pub async fn pin_attachment(&self, id: EntityRef) -> Result<(), BindingError> {
        Ok(self.inner.pin_attachment(id).await?)
    }

    /// Release one pin [`SunriseCore::pin_attachment`] took. Infallible, and
    /// valid for an attachment deleted since it was pinned.
    pub fn unpin_attachment(&self, id: EntityRef) {
        self.inner.unpin_attachment(id);
    }

    /// What this device's attachment cache holds and its limit, for
    /// Settings → Storage.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the vault could not be read.
    pub fn attachment_cache_usage(&self) -> Result<dto::CacheUsageItem, BindingError> {
        Ok(self.inner.attachment_cache_usage()?.into())
    }

    /// Evict every evictable blob: **Clear cache**. Returns the bytes freed.
    /// Blobs not yet uploaded, thumbnails and open previews stay.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the vault or the blob store could not
    /// be written.
    pub fn clear_attachment_cache(&self) -> Result<u64, BindingError> {
        Ok(self.inner.clear_attachment_cache()?)
    }

    /// Bring the cache under its limit now: after the limit was lowered.
    /// Returns the bytes freed.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the vault or the blob store could not
    /// be written.
    pub fn enforce_attachment_cache(&self) -> Result<u64, BindingError> {
        Ok(self.inner.enforce_attachment_cache()?)
    }

    /// One attachment's plaintext, reassembled and verified.
    ///
    /// # Errors
    ///
    /// [`BindingError::AttachmentNotHere`] when this device holds the metadata
    /// but not the chunks — which is a state to render, not a failure to
    /// report. Everything else is [`BindingError::Attachment`].
    pub async fn attachment_bytes(&self, id: EntityRef) -> Result<Vec<u8>, BindingError> {
        Ok(self.inner.attachment_bytes(id).await?)
    }

    /// Whether this device holds every chunk of `attachment`.
    ///
    /// A list of attachments needs this per row to choose between "open" and
    /// "not downloaded", and asking by reassembling each file would read every
    /// byte of every attachment to draw a list.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the blob store cannot be read.
    pub fn attachment_is_local(
        &self,
        attachment: dto::AttachmentItem,
    ) -> Result<bool, BindingError> {
        Ok(self.inner.attachment_is_local(&attachment.to_domain()?)?)
    }

    /// Download one attachment's bytes now, whatever its size, and return when
    /// they are here.
    ///
    /// What the "Download" button in `docs/02-domain/attachments.md` §Lazy
    /// fetch calls. Below the 10 MiB auto-fetch threshold a client never needs
    /// this — the sync driver fetches those unasked — and above it this is the
    /// only route to the bytes at all.
    ///
    /// `async` because it is `GET /blobs/{id}` over however many 256 KiB
    /// chunks, and because the whole point is that the caller can draw a
    /// spinner beside it. It does **not** time out: the caller has the better
    /// instrument, which is a Cancel button and a person looking at it. Call
    /// [`SunriseCore::cancel_attachment_fetch`] to end it — cancelling the
    /// foreign `Task` will not, since a UniFFI async call carries no
    /// cancellation across the seam.
    ///
    /// Safe to call for an attachment already here: that case does no I/O
    /// beyond a per-chunk `exists` and returns immediately.
    ///
    /// # Errors
    ///
    /// [`BindingError::AttachmentDownloadCancelled`] when the user cancelled,
    /// which is not a failure to report. Everything else is
    /// [`BindingError::Attachment`], carrying wording that distinguishes an
    /// attachment nothing ever uploaded, a vault with no sync session, and a
    /// relay that would not serve the blob.
    pub async fn fetch_attachment(&self, id: EntityRef) -> Result<(), BindingError> {
        Ok(self.inner.fetch_attachment(id).await?)
    }

    /// Stop a running download and mark the attachment `partial`.
    ///
    /// Synchronous and immediate: it releases the pending
    /// [`SunriseCore::fetch_attachment`] with
    /// [`BindingError::AttachmentDownloadCancelled`] whether or not a relay is
    /// answering. A no-op for an id with nothing outstanding.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the vault could not be written.
    pub fn cancel_attachment_fetch(&self, id: EntityRef) -> Result<(), BindingError> {
        Ok(self.inner.cancel_attachment_fetch(id)?)
    }

    /// This device's cache state for one attachment's bytes.
    ///
    /// Read beside [`SunriseCore::attachment_is_local`] to choose what to draw:
    /// `Idle` with no local bytes is the placeholder and its Download button,
    /// `Requested` is the transfer and its Cancel button, and `Partial` is the
    /// interrupted transfer that restarts from byte 0 when asked again.
    ///
    /// Durable, so a row still reads `Partial` after the app is relaunched —
    /// which is the point of it being a cache state rather than a view state.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the vault could not be read.
    pub fn attachment_fetch_state(
        &self,
        id: EntityRef,
    ) -> Result<AttachmentFetchState, BindingError> {
        Ok(self.inner.attachment_fetch_state(id)?)
    }
}

#[cfg(test)]
mod tests {
    use crate::BindingError;
    use sunrise_core::AttachError;
    use sunrise_id::{EntityKind, EntityRef};

    /// Bytes not fetched yet and a download the user cancelled are not
    /// failures, so each crosses as its own variant carrying the id; every
    /// other attachment error crosses as `Attachment` with its message.
    #[test]
    fn each_attach_error_crosses_as_what_the_client_does_next() {
        let id = EntityRef::new(EntityKind::Attachment, [7; 16]);

        let not_here = BindingError::from(AttachError::BytesNotHere { id });
        assert!(
            matches!(&not_here, BindingError::AttachmentNotHere { id: i } if *i == id.to_str()),
            "{not_here:?}"
        );

        let cancelled = BindingError::from(AttachError::FetchCancelled { id });
        assert!(
            matches!(
                &cancelled,
                BindingError::AttachmentDownloadCancelled { id: i } if *i == id.to_str()
            ),
            "{cancelled:?}"
        );
        assert_eq!(
            cancelled.to_string(),
            AttachError::FetchCancelled { id }.to_string(),
            "the message is unchanged"
        );

        for other in [
            AttachError::FetchUnavailable { id },
            AttachError::NotOnRelay { id },
            AttachError::SyncOffline { id },
        ] {
            let message = other.to_string();
            let crossed = BindingError::from(other);
            assert!(
                matches!(&crossed, BindingError::Attachment(m) if *m == message),
                "{crossed:?}"
            );
        }
    }
}

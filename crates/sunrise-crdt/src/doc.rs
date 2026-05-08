//! `StreamDoc` — thin wrapper over `loro::LoroDoc` for a single Stream.

use loro::{LoroDoc, LoroValue};
use thiserror::Error;

/// Errors produced by [`StreamDoc`] operations.
#[derive(Debug, Error)]
pub enum StreamDocError {
    /// Wrapped Loro error.
    #[error("loro error: {0}")]
    Loro(String),
}

/// Convert any Loro-side error into our wrapper.
fn loro_err<E: core::fmt::Display>(e: E) -> StreamDocError {
    StreamDocError::Loro(e.to_string())
}

/// One Loro document representing a single Stream's state.
///
/// The wrapper exposes the small surface needed by the rest of the
/// workspace: scalar reads/writes via a top-level map (`"meta"`), text via
/// `"body"`, and binary update import/export. Direct access to `LoroDoc`
/// is intentionally not provided so callers go through the typed API.
pub struct StreamDoc {
    inner: LoroDoc,
}

impl core::fmt::Debug for StreamDoc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamDoc").finish_non_exhaustive()
    }
}

impl StreamDoc {
    /// Construct a new empty doc with `peer_id` as the local peer.
    ///
    /// `peer_id` should derive from the device id so concurrent ops from the
    /// same physical device land under a stable peer.
    #[must_use]
    pub fn new(peer_id: u64) -> Self {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id).expect("set_peer_id");
        Self { inner: doc }
    }

    /// Set a scalar field on the top-level `"meta"` map.
    pub fn set_meta_scalar(
        &self,
        key: &str,
        value: impl Into<LoroValue>,
    ) -> Result<(), StreamDocError> {
        let map = self.inner.get_map("meta");
        map.insert(key, value).map_err(loro_err)?;
        Ok(())
    }

    /// Read a scalar field from the top-level `"meta"` map.
    #[must_use]
    pub fn get_meta_scalar(&self, key: &str) -> Option<LoroValue> {
        let map = self.inner.get_map("meta");
        map.get(key)
            .map(|v| v.into_value().unwrap_or(LoroValue::Null))
    }

    /// Append text to the `"body"` rich-text container.
    pub fn append_body_text(&self, s: &str) -> Result<(), StreamDocError> {
        let text = self.inner.get_text("body");
        let len = text.len_unicode();
        text.insert(len, s).map_err(loro_err)?;
        Ok(())
    }

    /// Get the full body text.
    #[must_use]
    pub fn body_text(&self) -> String {
        self.inner.get_text("body").to_string()
    }

    /// Set a numeric counter-like field on the meta map.
    ///
    /// Note: this is a plain LWW map entry, NOT a CRDT PN-counter — concurrent
    /// increments on different replicas overwrite rather than sum. True
    /// PN-counter semantics will be layered above this in the op-application
    /// engine (Phase 8 sync), where each increment is a sealed op contributing
    /// `+delta` and the engine sums them deterministically.
    pub fn meta_set_int(&self, key: &str, value: i64) -> Result<(), StreamDocError> {
        let map = self.inner.get_map("meta");
        map.insert(key, value).map_err(loro_err)?;
        Ok(())
    }

    /// Read an integer field from the meta map; returns 0 if absent.
    #[must_use]
    pub fn meta_get_int(&self, key: &str) -> i64 {
        let map = self.inner.get_map("meta");
        match map.get(key) {
            Some(v) => match v.into_value().unwrap_or(LoroValue::Null) {
                LoroValue::I64(n) => n,
                #[allow(clippy::cast_possible_truncation)]
                LoroValue::Double(n) => n as i64,
                _ => 0,
            },
            None => 0,
        }
    }

    /// Commit the current outstanding edits as a new transaction. Loro
    /// commits implicitly on export, but explicit commit lets callers
    /// control op grouping.
    pub fn commit(&self) {
        self.inner.commit();
    }

    /// Export all updates from the local doc since the empty state. Used to
    /// push the doc to a peer that has no prior knowledge.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, StreamDocError> {
        self.inner.commit();
        let snap = self
            .inner
            .export(loro::ExportMode::Snapshot)
            .map_err(loro_err)?;
        Ok(snap)
    }

    /// Export updates since the peer's known state. `peer_state` is the
    /// VersionVector serialized form returned by [`Self::version_vector`].
    pub fn export_updates_since(&self, peer_state: &[u8]) -> Result<Vec<u8>, StreamDocError> {
        self.inner.commit();
        let vv = loro::VersionVector::decode(peer_state).map_err(loro_err)?;
        let updates = self
            .inner
            .export(loro::ExportMode::updates(&vv))
            .map_err(loro_err)?;
        Ok(updates)
    }

    /// Apply remote updates / snapshot to this doc.
    pub fn import_updates(&self, bytes: &[u8]) -> Result<(), StreamDocError> {
        self.inner.import(bytes).map_err(loro_err)?;
        Ok(())
    }

    /// Return the local version vector encoded as bytes; suitable to send to
    /// a peer so they can compute "updates since me".
    #[must_use]
    pub fn version_vector(&self) -> Vec<u8> {
        self.inner.commit();
        self.inner.oplog_vv().encode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_round_trip() {
        let d = StreamDoc::new(1);
        d.set_meta_scalar("name", "Work: Acme").unwrap();
        let v = d.get_meta_scalar("name").unwrap();
        assert_eq!(v.into_string().unwrap().to_string(), "Work: Acme");
    }

    #[test]
    fn meta_int_round_trip() {
        let d = StreamDoc::new(1);
        d.meta_set_int("streak", 3).unwrap();
        assert_eq!(d.meta_get_int("streak"), 3);
        assert_eq!(d.meta_get_int("missing"), 0);
    }

    #[test]
    fn text_appends() {
        let d = StreamDoc::new(1);
        d.append_body_text("hello ").unwrap();
        d.append_body_text("world").unwrap();
        assert_eq!(d.body_text(), "hello world");
    }

    #[test]
    fn snapshot_round_trips_to_fresh_replica() {
        let a = StreamDoc::new(1);
        a.set_meta_scalar("name", "Work").unwrap();
        a.meta_set_int("streak", 5).unwrap();
        a.append_body_text("notes").unwrap();
        let snap = a.export_snapshot().unwrap();

        let b = StreamDoc::new(2);
        b.import_updates(&snap).unwrap();
        assert_eq!(
            b.get_meta_scalar("name")
                .unwrap()
                .into_string()
                .unwrap()
                .to_string(),
            "Work"
        );
        assert_eq!(b.meta_get_int("streak"), 5);
        assert_eq!(b.body_text(), "notes");
    }

    #[test]
    fn concurrent_text_edits_converge() {
        let a = StreamDoc::new(1);
        let b = StreamDoc::new(2);
        // Shared baseline: a's snapshot.
        a.append_body_text("hi").unwrap();
        let snap = a.export_snapshot().unwrap();
        b.import_updates(&snap).unwrap();
        assert_eq!(b.body_text(), "hi");
        // Concurrent appends.
        a.append_body_text(" from-a").unwrap();
        b.append_body_text(" from-b").unwrap();
        // Cross-import.
        let from_a = a.export_updates_since(&b.version_vector()).unwrap();
        b.import_updates(&from_a).unwrap();
        let from_b = b.export_updates_since(&a.version_vector()).unwrap();
        a.import_updates(&from_b).unwrap();
        // Both ends converge to byte-identical text (deterministic concurrent
        // insertion order).
        assert_eq!(a.body_text(), b.body_text());
        // Result contains both contributions.
        let merged = a.body_text();
        assert!(merged.contains("from-a"));
        assert!(merged.contains("from-b"));
    }
}

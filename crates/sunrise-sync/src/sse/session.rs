//! The [`Transport`] impl: the frame-to-operation table, the read loop, and the
//! one place a trait method reaches the blob and account surfaces.

use super::http::{field_str, field_u32, field_u64, protocol, MAX_EVENT_BYTES};
use super::SseTransport;
use crate::transport::{
    BlobCommit, IdentityChainPinned, KeyBundlePin, KeyBundlePinned, RevokeOutcome, SnapshotPin,
    Transport, TransportError,
};
use async_trait::async_trait;
use base64::Engine as _;
use http_body_util::BodyExt as _;
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, CursorEntry, FrameFlags, Hello, HelloAck, MsgKind,
    OpBatchPayload, RefreshTokenAckPayload, RefreshTokenPayload, SubscribePayload,
};

#[async_trait]
impl Transport for SseTransport {
    // One arm per frame kind, each of them short. The length is the size of the
    // protocol rather than complexity in any one branch, and splitting it would
    // scatter the mapping this module exists to state in one place.
    #[allow(clippy::too_many_lines)]
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Cancelled);
        }
        let (head, payload) = decode_frame(&frame).map_err(|e| protocol(&e))?;

        match head.msg_kind {
            MsgKind::Hello => {
                // `Hello` and `HelloAck` ride plain ciborium rather than the
                // canonical codec every other payload uses — one of the five
                // defects ADR-0023 catalogues, and fixed in place rather than
                // here, so this matches what the driver encodes today.
                let hello: Hello =
                    ciborium::de::from_reader(&payload[..]).map_err(|e| protocol(&e))?;
                let reply = self
                    .call(
                        "POST",
                        "/api/v1/sync/session",
                        Some(serde_json::json!({
                            "client_app_v": hello.client_app_v,
                            "client_platform": hello.client_platform,
                            "wire_proto_supported": hello.wire_proto_supported,
                            "doc_schema_min": hello.doc_schema_min,
                            "doc_schema_max": hello.doc_schema_max,
                            "crypto_suite_supported": hello.crypto_suite_supported,
                            "capabilities": hello.capabilities,
                            "trace": hello.trace,
                        })),
                    )
                    .await?;
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                let parsed: serde_json::Value =
                    serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
                // A reply naming no session is a failed handshake, not a
                // quiet one.
                // `crates/sunrise-server/src/api/sync/credential.rs:59#SessionResponse`
                // declares `session_id` as a `String` rather than an
                // `Option<String>`, so a `200` without it is malformed by the
                // server's own contract. Accepting it used to leave
                // `self.session` `None` while a `HelloAck` went to the driver
                // as though the handshake had succeeded: every later `call`
                // then went up with no `x-sunrise-session` for the relay to
                // refuse, and the next stream open reported "no sync session:
                // send Hello first" — naming the wrong cause to whoever read
                // the log, because `Hello` *was* sent and *was* acked.
                let Some(session) = parsed.get("session_id").and_then(|s| s.as_str()) else {
                    return Err(TransportError::Protocol(
                        "handshake reply carries no session_id".to_owned(),
                    ));
                };
                self.session = Some(session.to_owned());

                let ack = HelloAck {
                    server_app_v: field_str(&parsed, "server_app_v"),
                    wire_proto: field_u32(&parsed, "wire_proto")?,
                    crypto_suite: field_u32(&parsed, "crypto_suite")?,
                    doc_schema_floor: field_u32(&parsed, "doc_schema_floor")?,
                    capabilities: field_u64(&parsed, "capabilities"),
                    server_time_ms: field_u64(&parsed, "server_time_ms"),
                };
                let mut payload = Vec::new();
                ciborium::ser::into_writer(&ack, &mut payload).map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::Subscribe => {
                let sub = SubscribePayload::decode(&payload).map_err(|e| protocol(&e))?;
                let streams: Vec<serde_json::Value> = sub
                    .streams
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "stream_id": hex::encode(s.stream_id),
                            "cursors": s.cursors.iter().map(cursor_json).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                let reply = self
                    .call(
                        "POST",
                        "/api/v1/sync/subscribe",
                        Some(serde_json::json!({ "streams": streams })),
                    )
                    .await?;
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                // The stream set changed, so the open stream is stale. Dropping
                // it makes the next read reopen against the new set.
                self.events = None;
                self.buf.clear();
                // And the resume point goes with it, which is the load-bearing
                // half. `Last-Event-ID` says "I received everything up to here";
                // the cursors in a `Subscribe` say "I have *applied* everything
                // up to here". Those differ exactly when delivery succeeded and
                // application did not — a dropped frame, a client that restarted
                // mid-batch — and that is precisely when the driver re-sends
                // `Subscribe` to recover. Keeping the id would resume past the
                // ops the cursors are asking for, so the recovery path would
                // silently skip what it exists to fetch. The stricter of the two
                // statements wins, and a client restating its cursors is making
                // the stricter one.
                self.last_event_id = None;
            }

            MsgKind::OpBatch => {
                let batch = OpBatchPayload::decode(&payload).map_err(|e| protocol(&e))?;
                let ops: Vec<String> = batch
                    .ops
                    .iter()
                    .map(|o| base64::engine::general_purpose::STANDARD.encode(o))
                    .collect();
                let reply = self
                    .call(
                        "POST",
                        "/api/v1/sync/ops",
                        Some(serde_json::json!({
                            "stream_id": hex::encode(batch.stream_id),
                            "batch_id": batch.batch_id,
                            "ops": ops,
                        })),
                    )
                    .await?;
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                let parsed: serde_json::Value =
                    serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
                let ack = AckPayload {
                    batch_id: batch.batch_id,
                    stream_id: batch.stream_id,
                    server_first_seen_ms: field_u64(&parsed, "server_first_seen_ms"),
                };
                let payload = ack.encode().map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::RefreshToken => {
                let refresh = RefreshTokenPayload::decode(&payload).map_err(|e| protocol(&e))?;
                let reply = self
                    .call(
                        "POST",
                        "/api/v1/sync/session/refresh",
                        Some(serde_json::json!({ "token": refresh.token })),
                    )
                    .await?;
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                let parsed: serde_json::Value =
                    serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
                let ack = RefreshTokenAckPayload {
                    expires_at_ms: field_u64(&parsed, "expires_at_ms"),
                };
                let payload = ack.encode().map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::RefreshTokenAck, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            // The stream keeps itself alive with comments, so a liveness probe
            // needs no round trip: answering locally keeps the driver's
            // ping/pong bookkeeping working with nothing on the wire.
            MsgKind::Ping => {
                self.inbound.push_back(
                    encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[])
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::Close => {
                self.closed = true;
                self.events = None;
            }

            other => {
                return Err(TransportError::Protocol(format!(
                    "no operation carries {other:?} upstream"
                )));
            }
        }
        Ok(())
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            if let Some(frame) = self.inbound.pop_front() {
                return Ok(Some(frame));
            }
            if self.closed {
                return Ok(None);
            }
            if self.events.is_none() {
                self.open_events().await?;
            }

            // Drain what is already buffered before waiting on the socket.
            while let Some((id, data)) = self.take_event() {
                if let Some(id) = id {
                    self.last_event_id = Some(id);
                }
                if data.is_empty() {
                    continue;
                }
                let event: serde_json::Value =
                    serde_json::from_str(&data).map_err(|e| protocol(&e))?;
                if let Some(frame) = Self::frame_for(&event)? {
                    return Ok(Some(frame));
                }
            }

            // Unreachable, and kept because `as_mut` has to answer something:
            // the guard above opens the stream when `events` is `None`, and a
            // successful `open_events` assigns it, so control only arrives here
            // with `Some`. The drain loop touches `buf` alone. A mutant that
            // changes what this arm returns therefore survives by construction
            // rather than for want of a test.
            let Some(body) = self.events.as_mut() else {
                return Ok(None);
            };
            match body.frame().await {
                None => {
                    // The relay closed the stream. The driver's reconnect is
                    // what decides whether to come back, so this is a graceful
                    // end rather than an error.
                    self.events = None;
                    return Ok(None);
                }
                Some(Err(e)) => {
                    self.events = None;
                    return Err(TransportError::Unavailable(e.to_string()));
                }
                Some(Ok(chunk)) => {
                    if let Some(data) = chunk.data_ref() {
                        if self.buf.len() + data.len() > MAX_EVENT_BYTES {
                            self.events = None;
                            return Err(TransportError::Protocol(
                                "event exceeded the frame cap without terminating".to_owned(),
                            ));
                        }
                        self.buf.extend_from_slice(data);
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.closed = true;
        self.events = None;
        self.buf.clear();
        Ok(())
    }

    async fn revoke_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        self.revoke(device_id).await
    }

    async fn blob_init(
        &mut self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        self.init_blob(stream_id, chunk_count, size_bytes).await
    }

    async fn blob_put_chunk(
        &mut self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        self.put_blob_chunk(upload_id, chunk_idx, bytes).await
    }

    async fn blob_finalize(
        &mut self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        self.finalize_blob(upload_id, ciphertext_hash, chunk_hashes)
            .await
    }

    async fn blob_fetch(&mut self, blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        self.fetch_blob(blob_id).await
    }

    async fn snapshot_latest(
        &mut self,
        stream_id: &[u8; 16],
    ) -> Result<Option<SnapshotPin>, TransportError> {
        self.latest_snapshot(stream_id).await
    }

    async fn snapshot_pin(
        &mut self,
        stream_id: &[u8; 16],
        blob_id: &[u8; 16],
    ) -> Result<SnapshotPin, TransportError> {
        self.pin_snapshot(stream_id, blob_id).await
    }

    async fn key_bundle_latest(&mut self) -> Result<Option<KeyBundlePin>, TransportError> {
        self.latest_key_bundle().await
    }

    async fn key_bundle_pin(
        &mut self,
        blob_id: &[u8; 16],
        manifest_digest: &[u8; 32],
        replaces: Option<&[u8; 16]>,
    ) -> Result<KeyBundlePinned, TransportError> {
        self.pin_key_bundle(blob_id, manifest_digest, replaces)
            .await
    }

    async fn identity_chain_latest(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.latest_identity_chain().await
    }

    async fn identity_chain_pin(
        &mut self,
        chain: &[u8],
        replaces: Option<&[u8; 32]>,
    ) -> Result<IdentityChainPinned, TransportError> {
        self.pin_identity_chain(chain, replaces).await
    }

    async fn recovery_blob_replace(&mut self, blob: &[u8]) -> Result<(), TransportError> {
        self.replace_recovery_blob(blob).await
    }
}

pub(super) fn cursor_json(c: &CursorEntry) -> serde_json::Value {
    serde_json::json!({
        "device_id": hex::encode(c.device_id),
        "last_applied_seq": c.last_applied_seq,
    })
}

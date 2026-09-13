//! Declaring what a session receives, and where from.
//!
//! The cursor vocabulary and the one operation that writes it: [`subscribe`]
//! replaces a session's stream set, and a [`StreamSubscription`] carries the
//! [`DeviceCursor`]s that decide how much of the retained backlog
//! `GET /sync/events` replays. [`parse_id`] lives here because these are the
//! types that spell ids as hex on the wire; the publish path shares it.

use crate::api::error::ApiError;
use crate::api::signed::Signed;
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::params::header::Headers;
use serde::{Deserialize, Serialize};
use sunrise_wire_protocol::{CursorEntry, SubscribeEntry};

use super::credential::{resolve, SessionHeader};

/// One device's position in a stream.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeviceCursor {
    /// The originating device, 32 hex characters.
    pub device_id: String,
    /// The highest `seq` from that device this subscriber has applied.
    pub last_applied_seq: u64,
}

/// One stream to receive on, with the subscriber's cursors.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct StreamSubscription {
    /// The stream, 32 hex characters.
    pub stream_id: String,
    /// Per-device positions. Anything already covered is not replayed.
    #[serde(default)]
    pub cursors: Vec<DeviceCursor>,
}

/// `POST /api/v1/sync/subscribe` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeRequest {
    /// The streams this session wants. **Replaces** the current set rather than
    /// adding to it, which is what a re-sent `Subscribe` frame did.
    pub streams: Vec<StreamSubscription>,
}

/// Replace the set of streams this session receives on.
///
/// Takes effect on the next `GET /sync/events`. A stream already open keeps its
/// current set: re-subscribing mid-stream is a reconnect, which is what the
/// socket's re-`Subscribe` amounted to once the fan-out had been rebuilt.
///
/// It also **voids any resume point the client is holding**: the cursors this
/// carries are the client's newest statement of where it is, and a
/// `Last-Event-ID` minted before them is an older one. The next stream to
/// present both is refused with `400 SYNC_RESUME_CONFLICT` rather than served
/// from one of them; `GET /sync/events` carries the reasoning.

#[kynos::post("/api/v1/sync/subscribe", operation_id = "subscribeStreams")]
pub async fn subscribe(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    Signed {
        caller,
        value: body,
    }: Signed<SubscribeRequest>,
) -> Result<kynos::response::status::NoContent, ApiError> {
    let now_ms = state.clock.now_ms();
    let (id, session) = resolve(&state, &header, &caller, now_ms)?;

    let mut streams = Vec::with_capacity(body.streams.len());
    for s in &body.streams {
        streams.push(SubscribeEntry {
            stream_id: parse_id(&s.stream_id, "stream_id")?,
            cursors: s
                .cursors
                .iter()
                .map(|c| {
                    Ok(CursorEntry {
                        device_id: parse_id(&c.device_id, "device_id")?,
                        last_applied_seq: c.last_applied_seq,
                    })
                })
                .collect::<Result<Vec<_>, ApiError>>()?,
        });
    }
    let _ = session;
    tracing::debug!(
        ev = "srv.sync.subscribe",
        n_streams = streams.len() as u64,
        "stream set replaced"
    );
    state.sessions.update(&id, now_ms, |s| {
        s.streams = streams;
        s.subscribe_unserved = true;
    });

    Ok(kynos::response::status::NoContent)
}

/// Parse a 16-byte id from 32 hex characters.
///
/// Case-insensitive, because `hex::decode_to_slice` is. The doc and the
/// refusal below both used to say *lowercase*, which no input could ever
/// trigger: an uppercase id has always decoded to the same sixteen bytes. The
/// prose was corrected rather than the parse, since tightening it would start
/// refusing requests this server has always accepted.
pub(super) fn parse_id(s: &str, field: &'static str) -> Result<[u8; 16], ApiError> {
    let mut out = [0u8; 16];
    if s.len() != 32 {
        return Err(ApiError::validation(format!(
            "{field} must carry 32 hex characters"
        )));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation(format!("{field} must be hex")))?;
    Ok(out)
}

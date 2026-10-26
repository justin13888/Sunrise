//! Declaring what a session receives, and where from.
//!
//! The cursor vocabulary and the one operation that writes it: [`subscribe`]
//! replaces a session's stream set, and a [`StreamSubscription`] carries the
//! [`DeviceCursor`]s that decide how much of the retained backlog
//! `GET /sync/events` replays. The hex ids these types carry are parsed by
//! [`crate::api::ids::parse_hex16`], the parser every route shares.

use crate::api::error::ApiError;
use crate::api::ids::parse_hex16;
use crate::api::signed::Signed;
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::params::header::Headers;
use serde::{Deserialize, Serialize};
use sunrise_wire_protocol::{CursorEntry, SubscribeEntry};

use super::credential::{resolve, SessionHeader};

/// One device's position in a stream.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct DeviceCursor {
    /// The originating device, 32 lowercase hex characters.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub device_id: String,
    /// The highest `seq` from that device this subscriber has applied.
    pub last_applied_seq: u64,
}

/// One stream to receive on, with the subscriber's cursors.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct StreamSubscription {
    /// The stream, 32 lowercase hex characters.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub stream_id: String,
    /// Per-device positions. Anything already covered is not replayed.
    #[serde(default)]
    pub cursors: Vec<DeviceCursor>,
}

/// `POST /api/v1/sync/subscribe` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
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
    let (id, session) = resolve(&state, &header, &caller, now_ms).await?;

    let mut streams = Vec::with_capacity(body.streams.len());
    for s in &body.streams {
        streams.push(SubscribeEntry {
            stream_id: parse_hex16(&s.stream_id, "stream_id")?,
            cursors: s
                .cursors
                .iter()
                .map(|c| {
                    Ok(CursorEntry {
                        device_id: parse_hex16(&c.device_id, "device_id")?,
                        last_applied_seq: c.last_applied_seq,
                    })
                })
                .collect::<Result<Vec<_>, ApiError>>()?,
        });
    }
    let _ = session;
    // The cursors are also this device's acknowledgement of what it has
    // applied, which is what a blob tombstone's quorum is computed from
    // (`docs/02-domain/attachments.md` §Deletion). A caller that signed with no
    // device has nothing to attribute them to, and counts toward no quorum.
    if let Some(device) = &caller.device {
        let declared: Vec<crate::store::DeclaredCursor> = streams
            .iter()
            .flat_map(|s| {
                s.cursors.iter().map(|c| crate::store::DeclaredCursor {
                    stream_id: s.stream_id,
                    origin_device: c.device_id,
                    applied_seq: c.last_applied_seq,
                })
            })
            .collect();
        state
            .store
            .record_cursors(&device.device_id, &declared, now_ms)
            .await?;
    }
    tracing::debug!(
        ev = "srv.sync.subscribe",
        n_streams = streams.len() as u64,
        "stream set replaced"
    );
    state
        .sessions
        .update(&id, now_ms, move |s| {
            s.streams = streams;
            s.subscribe_unserved = true;
        })
        .await?;

    Ok(kynos::response::status::NoContent)
}

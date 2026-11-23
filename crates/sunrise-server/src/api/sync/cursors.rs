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
use crate::sync_session::SharedStream;

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
    /// The viewer grant to read this stream through, 32 lowercase hex
    /// characters: the stream is then another account's, and is read from
    /// that account's channel (ADR-0074). Absent for one of the caller's own
    /// streams.
    ///
    /// The grant must be claimed by the caller's account, name this stream,
    /// and not have expired, or the whole subscribe is refused with
    /// `403 RELAY_GRANT_REVOKED`. A set naming a granted stream's id twice
    /// is refused. The cursors of a granted stream are not recorded as this
    /// device's acknowledgement: they gate nothing of the owner's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub grant: Option<String>,
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
    let mut shared = Vec::new();
    for s in &body.streams {
        let stream_id = parse_hex16(&s.stream_id, "stream_id")?;
        if let Some(grant) = &s.grant {
            let grant_id = parse_hex16(grant, "grant")?;
            let g = super::grants::in_force(
                &state,
                &grant_id,
                &session.account_id,
                Some(&stream_id),
                now_ms,
            )
            .await?
            .ok_or_else(super::grants::not_in_force)?;
            shared.push(SharedStream {
                stream_id,
                grant_id,
                owner: crate::account_ns::account_key(&g.owner_id),
            });
        }
        streams.push(SubscribeEntry {
            stream_id,
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
    // One id in the event stream must mean one channel: a granted stream
    // whose id the set also names as the caller's own, or through a second
    // grant, would make its `ops` events ambiguous.
    if shared.iter().any(|g| {
        streams
            .iter()
            .filter(|s| s.stream_id == g.stream_id)
            .count()
            > 1
    }) {
        return Err(ApiError::validation(
            "a stream read through a grant may appear only once in the set",
        ));
    }
    // The cursors are also this device's acknowledgement of what it has
    // applied, which is what a blob tombstone's quorum is computed from
    // (`docs/02-domain/attachments.md` §Deletion). A caller that signed with no
    // device has nothing to attribute them to, and counts toward no quorum.
    // A granted stream's cursors are left out: the quorum is the owner's
    // devices', and a viewer's acknowledgement gates nothing of the owner's.
    if let Some(device) = &caller.device {
        let declared: Vec<crate::store::DeclaredCursor> = streams
            .iter()
            .filter(|s| !shared.iter().any(|g| g.stream_id == s.stream_id))
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
            s.shared = shared;
            s.subscribe_unserved = true;
        })
        .await?;

    Ok(kynos::response::status::NoContent)
}

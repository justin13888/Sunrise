//! The session's handshake and subscription frames, and the queue of frames
//! the pump owes the transport.

use std::collections::HashSet;

use crate::core::Core;
use crate::events::DomainEvent;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_id::EntityKind;
use sunrise_wire_protocol::{
    encode_frame, Capability, CapabilityBits, FrameFlags, Hello, HelloAck, MsgKind, SubscribeEntry,
    SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};

/// One frame the pump owes the transport, in the order it was queued.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outgoing {
    /// A frame sent as it is.
    Frame(Vec<u8>),
    /// An in-flight batch's frame, named by its `batch_id`. Sending it arms
    /// the batch's [`Due`] deadline from the moment the send returns.
    Batch(u64),
}

pub(super) fn build_hello(app: &str) -> Hello {
    let (client_app_v, client_platform) = match app.split_once('+') {
        Some((v, p)) => (v.to_string(), p.to_string()),
        None => (app.to_string(), std::env::consts::OS.to_string()),
    };
    Hello {
        client_app_v,
        client_platform,
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        // The floor is what this build can READ; DOC_SCHEMA_V is what it
        // WRITES. Advertising the same number for both would tell a peer we
        // cannot read our own predecessor, which is exactly the forward
        // compatibility the split exists to keep.
        doc_schema_min: u32::from(DOC_SCHEMA_FLOOR),
        doc_schema_max: u32::from(DOC_SCHEMA_V),
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        // The optional bit rides alongside the required ones. `HelloAck`
        // carries the AND of the two sides, so asking is how we find out
        // whether the relay can take an in-band refresh at all.
        capabilities: REQUIRED_CLIENT_BITS.0
            | REQUIRED_SERVER_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
        trace: String::new(),
    }
}

/// The relay's `HelloAck`, or `None` for one this build cannot read.
///
/// The relay chose these bytes, so they are read under the framing bound
/// (`sunrise_cbor::MAX_VALUE_DEPTH`): the ack is one map of scalars, and a
/// field a newer relay adds is held to the same eight levels, the map
/// included.
pub(super) fn decode_hello_ack(payload: &[u8]) -> Option<HelloAck> {
    sunrise_cbor::decode_bounded(&mut &payload[..], sunrise_cbor::MAX_VALUE_DEPTH).ok()
}

pub(super) fn encode_hello(h: &Hello) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(h, &mut buf).map_err(|_| ())?;
    Ok(buf)
}

/// A `Subscribe` covering every known stream with its current cursors — the
/// anti-entropy resync frame.
///
/// Identical in shape to the one the handshake sends, and safe to repeat: the
/// relay replaces a stream's subscription rather than adding a second one, and
/// replays only what the cursors do not already cover.
pub(crate) fn encode_subscribe_all(core: &Core) -> Result<Vec<u8>, ()> {
    let entries = core.sync_subscribe_entries().map_err(|_| ())?;
    let sub = SubscribePayload { streams: entries };
    let bytes = sub.encode().map_err(|_| ())?;
    encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &bytes).map_err(|_| ())
}

/// Queue a `Subscribe` for every Stream a delivery just taught this device
/// about, so that Stream's task ops start flowing.
///
/// Scanned across **every** event one delivery produced, not just the first: a
/// `key_envelope` op releases whatever was parked waiting for its key, and a
/// `stream.create` can be among them. A driver that read only the first event
/// would never subscribe, and the Stream's tasks would never arrive.
pub(super) fn subscribe_to_new_streams(
    events: &[DomainEvent],
    subscribed: &mut HashSet<[u8; 16]>,
    pending_sends: &mut Vec<Outgoing>,
) {
    for ev in events {
        let DomainEvent::Created(r) = ev else {
            continue;
        };
        if r.kind() != EntityKind::Stream {
            continue;
        }
        let sid = *r.bytes();
        if subscribed.insert(sid) {
            if let Ok(frame) = encode_subscribe_one(sid) {
                pending_sends.push(Outgoing::Frame(frame));
            }
        }
    }
}

pub(super) fn encode_subscribe_one(stream_id: [u8; 16]) -> Result<Vec<u8>, ()> {
    let sub = SubscribePayload {
        streams: vec![SubscribeEntry {
            cursors: Vec::new(),
            stream_id,
        }],
    };
    let bytes = sub.encode().map_err(|_| ())?;
    encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &bytes).map_err(|_| ())
}

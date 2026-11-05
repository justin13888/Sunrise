//! The handshake's wire shapes.

use crate::sync_driver::frames::decode_hello_ack;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
use sunrise_wire_protocol::HelloAck;

/// The relay's ack is read under the framing bound: a field it does not know
/// is skipped up to the bound, the ack's map counted, and one level more is
/// no ack at all.
#[test]
fn a_hello_ack_is_read_under_the_framing_bound() {
    use ciborium::value::Value;
    let ack = HelloAck {
        server_app_v: "relay".into(),
        wire_proto: u32::from(WIRE_PROTO_V),
        crypto_suite: u32::from(CRYPTO_SUITE_V),
        doc_schema_floor: u32::from(DOC_SCHEMA_FLOOR),
        capabilities: 0,
        server_time_ms: 1,
    };
    let with_unknown = |levels: usize| {
        let Value::Map(mut map) = Value::serialized(&ack).unwrap() else {
            panic!("an ack is a map");
        };
        let mut nest = Value::Integer(1.into());
        for _ in 0..levels {
            nest = Value::Array(vec![nest]);
        }
        map.push((Value::Text("zz_newer".into()), nest));
        let mut out = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut out).unwrap();
        out
    };
    let bound = sunrise_cbor::MAX_VALUE_DEPTH;
    assert_eq!(
        decode_hello_ack(&with_unknown(bound - 1)),
        Some(ack.clone())
    );
    assert_eq!(decode_hello_ack(&with_unknown(bound)), None);
}

/// A Stream a delivery names is subscribed with one frame naming the
/// session's whole set — the vault's streams and any the session holds that
/// the vault does not list — because the relay replaces the set it serves
/// with the frame's (#1005). A delivery naming nothing new sends nothing.
#[tokio::test]
async fn a_stream_learned_from_a_delivery_is_subscribed_with_the_rest() {
    use crate::sync_driver::frames::{subscribe_to_new_streams, Outgoing};
    use crate::{Command, DomainEvent};
    use std::collections::HashSet;
    use sunrise_wire_protocol::{decode_frame, MsgKind, SubscribePayload};

    let dir = tempfile::tempdir().unwrap();
    let core = super::open_arc(dir.path()).await;
    let garden = core
        .submit(Command::CreateStream(sunrise_domain::StreamDraft {
            name: "Garden".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    let unlisted = [0x33; 16];
    let mut subscribed: HashSet<[u8; 16]> = [
        crate::engine::META_STREAM,
        sunrise_domain::PRIVATE_DOMAIN_BYTES,
        unlisted,
    ]
    .into();
    let before = subscribed.clone();
    let delivery = [DomainEvent::Created(garden)];

    let mut out = Vec::new();
    subscribe_to_new_streams(&core, &delivery, &mut subscribed, &mut out);
    let [Outgoing::Frame(frame)] = out.as_slice() else {
        panic!("one Subscribe for the delivery: {out:?}");
    };
    let (head, payload) = decode_frame(frame).unwrap();
    assert_eq!(head.msg_kind, MsgKind::Subscribe);
    let named: HashSet<[u8; 16]> = SubscribePayload::decode(&payload)
        .unwrap()
        .streams
        .iter()
        .map(|e| e.stream_id)
        .collect();
    assert!(named.contains(garden.bytes()), "{named:?}");
    assert!(before.is_subset(&named), "{named:?}");

    subscribe_to_new_streams(&core, &delivery, &mut subscribed, &mut out);
    assert_eq!(out.len(), 1, "a stream already held is not re-subscribed");
}

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

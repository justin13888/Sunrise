//! Sync wire framing: the 11-byte header, the zstd bomb caps, and the
//! canonical-CBOR payload codecs behind each `MsgKind`.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "`sunrise-sync` framing + magic-prefix parser". The framing itself lives in
//! `sunrise-wire-protocol` (`sunrise-sync` is the transport that calls it), so
//! that is what this drives.
//!
//! # What arbitrary bytes reach here that a property test does not
//!
//! `decode_frame` reads a length prefix and a compression flag out of attacker
//! bytes and then hands the remainder to `zstd`. That is three separate
//! opportunities for a size to disagree with reality — `len_prefix` against
//! the actual body, the decompressed length against `MAX_DECOMPRESSED_BYTES`,
//! the whole frame against `MAX_FRAME_BYTES` — and a generator of *valid*
//! frames never makes any of them disagree.
//!
//! # The assertion
//!
//! A frame that decodes must re-encode to a frame that decodes to the same
//! header and the same payload. The re-encoded bytes need not equal the input
//! (a zstd frame re-compresses, and undefined flag bits are dropped on the way
//! in by design), so the round trip is stated over the decoded values rather
//! than the bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, CaughtUpPayload, ClosePayload, ErrorPayload, MsgKind,
    NackPayload, OpBatchPayload, RefreshTokenAckPayload, RefreshTokenPayload, SubscribePayload,
};

fuzz_target!(|data: &[u8]| {
    let Ok((header, payload)) = decode_frame(data) else {
        return;
    };

    // Round trip through the encoder.
    let Ok(reencoded) = encode_frame(header.msg_kind, header.flags, &payload) else {
        // The only way out is a size cap, and a frame that decoded is already
        // inside every one of them — but the encoder owns that judgement, not
        // this harness.
        return;
    };
    let (header2, payload2) =
        decode_frame(&reencoded).expect("a frame this build encoded must decode");
    assert_eq!(header, header2, "frame header did not survive a round trip");
    assert_eq!(
        payload, payload2,
        "frame payload did not survive a round trip"
    );

    // The header claims a decompressed length; the payload handed back must be
    // exactly that long, or a caller sizing a buffer from the header overruns.
    assert_eq!(
        payload.len() as u64,
        u64::from(header.decompressed_len),
        "decompressed_len disagrees with the payload the decoder returned"
    );

    // One layer deeper: the payload is canonical CBOR whose shape the msg_kind
    // names. `decode` rejects non-canonical encodings and trailing bytes, which
    // is a second parser over the same attacker-controlled bytes.
    match header.msg_kind {
        MsgKind::OpBatch => drop(OpBatchPayload::decode(&payload)),
        MsgKind::Ack => drop(AckPayload::decode(&payload)),
        MsgKind::Nack => drop(NackPayload::decode(&payload)),
        MsgKind::Subscribe => drop(SubscribePayload::decode(&payload)),
        MsgKind::StreamUpdate => drop(CaughtUpPayload::decode(&payload)),
        MsgKind::Error => drop(ErrorPayload::decode(&payload)),
        MsgKind::Close => drop(ClosePayload::decode(&payload)),
        MsgKind::RefreshToken => drop(RefreshTokenPayload::decode(&payload)),
        MsgKind::RefreshTokenAck => drop(RefreshTokenAckPayload::decode(&payload)),
        // Hello / HelloAck / snapshots / presence / ping / pong carry either no
        // payload or one with no canonical codec of its own in this crate.
        _ => {}
    }
});

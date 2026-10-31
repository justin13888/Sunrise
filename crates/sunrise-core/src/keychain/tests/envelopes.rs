//! Sealing and opening ops, and the `key_envelope` round trip.

use super::*;

#[test]
fn seal_open_round_trip() {
    let root = VaultRootKey::from_bytes([0x11; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [0u8; 16];
    let (epoch, key) = d
        .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let inner = b"inner op cbor bytes";
    let env = kc
        .seal_op_at(
            sid,
            1,
            sunrise_cbor::hlc::Hlc::at(42),
            inner,
            &SystemRng,
            epoch,
            &key,
        )
        .unwrap();
    assert_eq!(kc.open_op(&env).unwrap(), inner);
}

/// Two keys at one epoch: the AEAD tag picks, not a stored discriminator.
#[test]
fn an_op_opens_under_whichever_key_sealed_it() {
    let root = VaultRootKey::from_bytes([0x12; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [3u8; 16];
    let (epoch, mine) = d
        .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    // A second device's concurrently minted key for the same epoch.
    let theirs = StreamKey::from_bytes([0x99; 32]);
    d.with_tx(|tx| {
        kc.absorb_stream_key(
            tx,
            &sid,
            epoch,
            &theirs,
            KeySource::Envelope { sender: PEER },
            &SystemRng,
            0,
        )
    })
    .unwrap();
    assert_eq!(kc.stream_keys_at(&sid, epoch).len(), 2);

    for key in [&mine, &theirs] {
        let env = kc
            .seal_op_at(
                sid,
                1,
                sunrise_cbor::hlc::Hlc::at(1),
                b"payload",
                &SystemRng,
                epoch,
                key,
            )
            .unwrap();
        assert_eq!(kc.open_op(&env).unwrap(), b"payload");
    }
}

/// Which of two same-epoch keys a device seals under has to be a function
/// of the keys, not of the table.
///
/// Two devices can mint the same epoch concurrently — ADR-0024's whole
/// reason for `key_envelope` ops — and `current_stream_key_tx` documents
/// the tie-break: "returns the lowest `key_id`, so a device's choice of
/// which to seal under is deterministic rather than dependent on row
/// order". Nothing asserted it. `ORDER BY key_id` is one clause away from
/// `ORDER BY created_at_ms` or from no ordering at all, and with either of
/// those two devices holding the identical pair of keys can seal under
/// different ones depending on which envelope reached them first — so an
/// op's own replica opens it and its peer does not.
///
/// Both insertion orders are checked against the same pair of keys, with
/// different `created_at_ms` on each row, so an implementation that
/// ordered by arrival time or by rowid would make the two disagree.
#[test]
fn the_same_epoch_key_is_chosen_by_key_id_and_not_by_row_order() {
    let root = VaultRootKey::from_bytes([0x4d; 32]);
    let sid = [6u8; 16];

    // Two keys for one epoch, as two devices minting concurrently produce.
    let one = StreamKey::from_bytes([0x31; 32]);
    let two = StreamKey::from_bytes([0x77; 32]);
    let (id_one, id_two) = (stream_key_id(&one), stream_key_id(&two));
    assert_ne!(id_one, id_two, "two distinct keys have distinct ids");
    let lowest = if id_one <= id_two { &one } else { &two };

    let chosen_under = |first: &StreamKey, second: &StreamKey| -> StreamKey {
        let mut d = db(&root);
        let kc = open(&mut d, &root);
        d.with_tx(|tx| {
            kc.absorb_stream_key(
                tx,
                &sid,
                1,
                first,
                KeySource::Envelope { sender: PEER },
                &SystemRng,
                10,
            )
        })
        .unwrap();
        d.with_tx(|tx| {
            kc.absorb_stream_key(
                tx,
                &sid,
                1,
                second,
                KeySource::Envelope { sender: PEER },
                &SystemRng,
                20,
            )
        })
        .unwrap();
        assert_eq!(
            kc.stream_keys_at(&sid, 1).len(),
            2,
            "both keys really are stored at the epoch"
        );
        let (epoch, key) = d
            .with_tx(|tx| kc.current_stream_key_tx(tx, &sid))
            .unwrap()
            .expect("a live key for the epoch");
        assert_eq!(epoch, 1);
        key
    };

    let forward = chosen_under(&one, &two);
    let reverse = chosen_under(&two, &one);
    assert_eq!(
        stream_key_id(&forward),
        stream_key_id(lowest),
        "the lowest key_id is the one sealed under"
    );
    assert_eq!(
        stream_key_id(&reverse),
        stream_key_id(lowest),
        "and inserting them the other way round, with the other row carrying \
             the earlier created_at_ms, changes nothing"
    );
}

#[test]
fn key_envelopes_round_trip_to_both_recipient_classes() {
    let root = VaultRootKey::from_bytes([0x15; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [7u8; 16];
    let key = StreamKey::from_bytes([0x31; 32]);

    for (recipient, pubkey) in [
        (EnvelopeRecipient::Device, kc.device_dh_pub()),
        (EnvelopeRecipient::Identity, kc.identity_dh_pub()),
    ] {
        let sealed = kc
            .seal_key_envelope(&pubkey, &sid, 2, &key, &SystemRng)
            .unwrap();
        assert_eq!(
            kc.open_key_envelope(recipient, &sid, 2, &sealed).unwrap(),
            key
        );
        // Wrong epoch does not open.
        assert!(kc.open_key_envelope(recipient, &sid, 3, &sealed).is_err());
    }
}

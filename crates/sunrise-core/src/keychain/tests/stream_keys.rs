//! Minting, absorbing, and which held key is the one written under.

use super::*;

#[test]
fn minting_advances_the_epoch() {
    let root = VaultRootKey::from_bytes([0x13; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [4u8; 16];
    assert_eq!(d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap(), None);
    assert_eq!(
        d.with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap()
            .0,
        1
    );
    assert_eq!(
        d.with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap()
            .0,
        2
    );
    assert_eq!(
        d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap(),
        Some(2)
    );
}

#[test]
fn absorbing_the_same_key_twice_is_a_no_op() {
    let root = VaultRootKey::from_bytes([0x14; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [6u8; 16];
    let key = StreamKey::from_bytes([0x21; 32]);
    assert!(d
        .with_tx(|tx| kc.absorb_stream_key(
            tx,
            &sid,
            1,
            &key,
            KeySource::Envelope { sender: PEER },
            &SystemRng,
            0
        ))
        .unwrap());
    assert!(!d
        .with_tx(|tx| kc.absorb_stream_key(
            tx,
            &sid,
            1,
            &key,
            KeySource::Envelope { sender: PEER },
            &SystemRng,
            0
        ))
        .unwrap());
    assert_eq!(kc.stream_keys_at(&sid, 1).len(), 1);
}

/// Put `device` in the engine's read bound, as its revocation fold does.
fn read_bound(d: &Db, device: &[u8; 16]) {
    d.conn()
        .execute(
            "INSERT INTO device_read_bounds (device_id, first_bound_at_ms) VALUES (?, 0)",
            params![&device[..]],
        )
        .unwrap();
}

/// **A key a read-bounded device delivered is kept for reading and never
/// written under** (issue #280, ADR-0041 §Decision 4).
///
/// A revoked device running a modified client mints an epoch above the
/// live one and seals it to this device. The key must still be stored,
/// or ops sealed under it would be unreadable here and readable on a
/// replica that met the envelope first. It must not be live, or this
/// device's next write is sealed under a key the revoked device delivered.
/// Nor may the epoch below it, which the revoked device held while it was
/// a member: the stream reads as keyless and the next write mints.
///
/// The bound is applied *after* the key arrives, so this also pins that
/// the test runs when the live epoch is read and not at absorb time.
#[test]
fn a_key_a_read_bounded_device_delivered_is_read_but_never_written_under() {
    let root = VaultRootKey::from_bytes([0x15; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [7u8; 16];
    let cut = [0xc1; 16];
    let (_, mine) = d
        .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let theirs = StreamKey::from_bytes([0x5a; 32]);
    assert!(d
        .with_tx(|tx| kc.absorb_stream_key(
            tx,
            &sid,
            2,
            &theirs,
            KeySource::Envelope { sender: cut },
            &SystemRng,
            0
        ))
        .unwrap());

    // Before the bound, the sender is a member like any other.
    let live = |d: &mut Db| {
        d.with_tx(|tx| kc.current_stream_key_tx(tx, &sid))
            .unwrap()
            .expect("a live key")
    };
    let (epoch, key) = live(&mut d);
    assert_eq!((epoch, stream_key_id(&key)), (2, stream_key_id(&theirs)));

    read_bound(&d, &cut);

    assert!(
        d.with_tx(|tx| kc.current_stream_key_tx(tx, &sid))
            .unwrap()
            .is_none(),
        "no live key: not the bounded device's epoch 2, and not epoch 1 \
             below it, which the bounded device was a member for"
    );
    assert_eq!(d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap(), None);
    assert!(
        kc.stream_keys_at(&sid, 1)
            .iter()
            .any(|k| stream_key_id(k) == stream_key_id(&mine)),
        "the epoch minted here is still held"
    );
    assert_eq!(
        d.with_tx(|tx| kc.max_epoch_tx(tx, &sid)).unwrap(),
        Some(2),
        "and it still holds the bounded device's epoch"
    );
    assert!(
        kc.stream_keys_at(&sid, 2)
            .iter()
            .any(|k| stream_key_id(k) == stream_key_id(&theirs)),
        "which still opens every op sealed under it"
    );

    let (minted, _) = d
        .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    assert_eq!(
        minted, 3,
        "a rotation lands above every held key, not beside the bounded one"
    );
    assert_eq!(live(&mut d).0, 3);
}

/// A second delivery of the same key, by a device this replica has not
/// bounded, does not make it writable: the first sender still holds it.
#[test]
fn a_later_delivery_by_a_current_device_does_not_clear_a_bounded_senders_key() {
    let root = VaultRootKey::from_bytes([0x16; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [8u8; 16];
    let cut = [0xc2; 16];
    d.with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let theirs = StreamKey::from_bytes([0x5b; 32]);
    let absorb = |d: &mut Db, sender: [u8; 16]| {
        d.with_tx(|tx| {
            kc.absorb_stream_key(
                tx,
                &sid,
                2,
                &theirs,
                KeySource::Envelope { sender },
                &SystemRng,
                0,
            )
        })
        .unwrap()
    };
    assert!(absorb(&mut d, cut));
    assert!(
        !absorb(&mut d, PEER),
        "the key is not new, and the caller drains nothing"
    );
    read_bound(&d, &cut);
    assert_eq!(d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap(), None);

    // And the other order: the current device first, the bounded one
    // second. The bounded delivery still counts though the key was known.
    let sid2 = [9u8; 16];
    d.with_tx(|tx| kc.mint_epoch(tx, &sid2, &SystemRng, 0))
        .unwrap();
    for sender in [PEER, cut] {
        d.with_tx(|tx| {
            kc.absorb_stream_key(
                tx,
                &sid2,
                2,
                &theirs,
                KeySource::Envelope { sender },
                &SystemRng,
                0,
            )
        })
        .unwrap();
    }
    assert_eq!(
        d.with_tx(|tx| kc.current_epoch_tx(tx, &sid2)).unwrap(),
        None
    );
}

/// Two keys at the live epoch, the lower `key_id` delivered by a
/// read-bounded device: the tie-break passes over it rather than
/// choosing it for being lowest.
#[test]
fn the_same_epoch_tie_break_passes_over_a_bounded_senders_key() {
    let root = VaultRootKey::from_bytes([0x17; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [0x0a; 16];
    let cut = [0xc3; 16];
    let one = StreamKey::from_bytes([0x31; 32]);
    let two = StreamKey::from_bytes([0x77; 32]);
    let (low, high) = if stream_key_id(&one) <= stream_key_id(&two) {
        (&one, &two)
    } else {
        (&two, &one)
    };
    for (key, sender) in [(low, cut), (high, PEER)] {
        d.with_tx(|tx| {
            kc.absorb_stream_key(
                tx,
                &sid,
                1,
                key,
                KeySource::Envelope { sender },
                &SystemRng,
                0,
            )
        })
        .unwrap();
    }
    read_bound(&d, &cut);
    let (epoch, key) = d
        .with_tx(|tx| kc.current_stream_key_tx(tx, &sid))
        .unwrap()
        .expect("the current device's key is live");
    assert_eq!((epoch, stream_key_id(&key)), (1, stream_key_id(high)));
}

/// Every held key delivered by a read-bounded device, which is what a
/// revocation leaves on most streams until the rotation arrives.
///
/// An ordinary stream reads as having no live key, so the engine mints one.
/// The vault-meta stream reads as its highest held epoch instead, because
/// the revocation itself is sealed there under the epoch the cut device
/// still holds, and a meta mint would seal its own envelopes under itself.
#[test]
fn with_no_writable_key_only_the_vault_meta_stream_falls_back_to_the_held_epoch() {
    let root = VaultRootKey::from_bytes([0x18; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let cut = [0xc4; 16];
    let ordinary = [0x0b; 16];
    let meta = crate::engine::META_STREAM;
    for (sid, byte) in [(ordinary, 0x41), (meta, 0x42)] {
        for epoch in [3, 4] {
            d.with_tx(|tx| {
                kc.absorb_stream_key(
                    tx,
                    &sid,
                    epoch,
                    &StreamKey::from_bytes([byte ^ epoch.to_le_bytes()[0]; 32]),
                    KeySource::Envelope { sender: cut },
                    &SystemRng,
                    0,
                )
            })
            .unwrap();
        }
    }
    read_bound(&d, &cut);

    assert_eq!(
        d.with_tx(|tx| kc.current_epoch_tx(tx, &ordinary)).unwrap(),
        None
    );
    assert!(d
        .with_tx(|tx| kc.current_stream_key_tx(tx, &ordinary))
        .unwrap()
        .is_none());
    assert_eq!(
        d.with_tx(|tx| kc.current_epoch_tx(tx, &meta)).unwrap(),
        Some(4)
    );
    assert_eq!(
        d.with_tx(|tx| kc.current_stream_key_tx(tx, &meta))
            .unwrap()
            .map(|(e, _)| e),
        Some(4)
    );
}

/// **Two revocations: bounding a rotation's sole sender does not send
/// this device back to an epoch the first revoked device holds.**
///
/// Q is revoked while epoch 1, minted here, is live, so Q holds it. R
/// rotates the stream to epoch 2 and is its sole sender here. Then R is
/// revoked too. Epoch 2 is not writable, and epoch 1 below it is writable
/// by sender but held by Q, so an ordinary stream reads as keyless and the
/// next write mints above both. Before R is bounded, epoch 1 is the
/// pre-revocation key that stays live until a rotation arrives: nothing
/// here records that Q holds it.
#[test]
fn bounding_a_rotations_sole_sender_mints_rather_than_falling_back_below_it() {
    let root = VaultRootKey::from_bytes([0x19; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sid = [0x0c; 16];
    let (q, r) = ([0xc5; 16], [0xc6; 16]);
    d.with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let epoch = |d: &mut Db| d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap();

    read_bound(&d, &q);
    assert_eq!(
        epoch(&mut d),
        Some(1),
        "a key minted here stays live after Q's revocation, and Q holds it"
    );

    d.with_tx(|tx| {
        kc.absorb_stream_key(
            tx,
            &sid,
            2,
            &StreamKey::from_bytes([0x5c; 32]),
            KeySource::Envelope { sender: r },
            &SystemRng,
            0,
        )
    })
    .unwrap();
    assert_eq!(epoch(&mut d), Some(2), "R's rotation, which Q never held");

    read_bound(&d, &r);
    assert_eq!(
        epoch(&mut d),
        None,
        "not epoch 1, which Q was sealed before its cut"
    );
    let (minted, _) = d
        .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    assert_eq!(minted, 3);
    assert_eq!(epoch(&mut d), Some(3));
}

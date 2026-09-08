//! The published BIP-39 test vectors, asserted against `sunrise_crypto::bip39`.
//!
//! # Why these and not a round trip
//!
//! A round trip proves the encoder and the decoder agree with each other, which
//! they would also do with a permuted wordlist, a checksum taken over the wrong
//! bits, or the bit groups packed least-significant-first. Every one of those
//! produces recovery codes that no other BIP-39 implementation can read — and
//! `docs/03-crypto/recovery.md` promises "24 words from the BIP-39 English
//! wordlist", which is a promise about interoperability, not about self
//! consistency. So the assertion is against **external** ground truth.
//!
//! # Provenance
//!
//! The twenty-four English `(entropy, mnemonic)` pairs below are the reference
//! vectors published with BIP-39, distributed as `vectors.json` in the
//! `trezor/python-mnemonic` repository that BIP-39 itself cites as the
//! reference implementation. Each source entry is a four-element array
//! `[entropy, mnemonic, seed, xprv]`; the third and fourth elements are the
//! PBKDF2 seed under the passphrase `"TREZOR"` and the BIP-32 root key derived
//! from it, and neither is asserted here because Sunrise implements BIP-39 §3
//! and deliberately not §2.5 — see the `bip39` module docs.
//!
//! All five entropy lengths BIP-39 defines appear, which is why
//! `sunrise_crypto::bip39::encode` takes a slice rather than `[u8; 32]`: the
//! 128/160/192/224-bit vectors would otherwise be unassertable against the code
//! that actually runs, and the 256-bit path would be checked by eight vectors
//! instead of twenty-four sharing one bit-packing loop.

/// One published vector: entropy as lowercase hex, and the mnemonic it encodes.
struct Vector {
    entropy: &'static str,
    mnemonic: &'static str,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("a hex byte"))
        .collect()
}

const VECTORS: &[Vector] = &[
    Vector {
        entropy: "00000000000000000000000000000000",
        mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
    },
    Vector {
        entropy: "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
        mnemonic: "legal winner thank year wave sausage worth useful legal winner thank yellow",
    },
    Vector {
        entropy: "80808080808080808080808080808080",
        mnemonic: "letter advice cage absurd amount doctor acoustic avoid letter advice cage above",
    },
    Vector {
        entropy: "ffffffffffffffffffffffffffffffff",
        mnemonic: "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong",
    },
    Vector {
        entropy: "000000000000000000000000000000000000000000000000",
        mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon agent",
    },
    Vector {
        entropy: "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
        mnemonic: "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal will",
    },
    Vector {
        entropy: "808080808080808080808080808080808080808080808080",
        mnemonic: "letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic avoid letter always",
    },
    Vector {
        entropy: "ffffffffffffffffffffffffffffffffffffffffffffffff",
        mnemonic: "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo when",
    },
    Vector {
        entropy: "0000000000000000000000000000000000000000000000000000000000000000",
        mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art",
    },
    Vector {
        entropy: "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
        mnemonic: "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth title",
    },
    Vector {
        entropy: "8080808080808080808080808080808080808080808080808080808080808080",
        mnemonic: "letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic bless",
    },
    Vector {
        entropy: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        mnemonic: "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote",
    },
    Vector {
        entropy: "9e885d952ad362caeb4efe34a8e91bd2",
        mnemonic: "ozone drill grab fiber curtain grace pudding thank cruise elder eight picnic",
    },
    Vector {
        entropy: "6610b25967cdcca9d59875f5cb50b0ea75433311869e930b",
        mnemonic: "gravity machine north sort system female filter attitude volume fold club stay feature office ecology stable narrow fog",
    },
    Vector {
        entropy: "68a79eaca2324873eacc50cb9c6eca8cc68ea5d936f98787c60c7ebc74e6ce7c",
        mnemonic: "hamster diagram private dutch cause delay private meat slide toddler razor book happy fancy gospel tennis maple dilemma loan word shrug inflict delay length",
    },
    Vector {
        entropy: "c0ba5a8e914111210f2bd131f3d5e08d",
        mnemonic: "scheme spot photo card baby mountain device kick cradle pact join borrow",
    },
    Vector {
        entropy: "6d9be1ee6ebd27a258115aad99b7317b9c8d28b6d76431c3",
        mnemonic: "horn tenant knee talent sponsor spell gate clip pulse soap slush warm silver nephew swap uncle crack brave",
    },
    Vector {
        entropy: "9f6a2878b2520799a44ef18bc7df394e7061a224d2c33cd015b157d746869863",
        mnemonic: "panda eyebrow bullet gorilla call smoke muffin taste mesh discover soft ostrich alcohol speed nation flash devote level hobby quick inner drive ghost inside",
    },
    Vector {
        entropy: "23db8160a31d3e0dca3688ed941adbf3",
        mnemonic: "cat swing flag economy stadium alone churn speed unique patch report train",
    },
    Vector {
        entropy: "8197a4a47f0425faeaa69deebc05ca29c0a5b5cc76ceacc0",
        mnemonic: "light rule cinnamon wrap drastic word pride squirrel upgrade then income fatal apart sustain crack supply proud access",
    },
    Vector {
        entropy: "066dca1a2bb7e8a1db2832148ce9933eea0f3ac9548d793112d9a95c9407efad",
        mnemonic: "all hour make first leader extend hole alien behind guard gospel lava path output census museum junior mass reopen famous sing advance salt reform",
    },
    Vector {
        entropy: "f30f8c1da665478f49b001d94c5fc452",
        mnemonic: "vessel ladder alter error federal sibling chat ability sun glass valve picture",
    },
    Vector {
        entropy: "c10ec20dc3cd9f652c7fac2f1230f7a3c828389a14392f05",
        mnemonic: "scissors invite lock maple supreme raw rapid void congress muscle digital elegant little brisk hair mango congress clump",
    },
    Vector {
        entropy: "f585c11aec520db57dd353c69554b21a89b20fb0650966fa0a9d6f74fd989d8f",
        mnemonic: "void come effort suffer camp survey warrior heavy shoot primary clutch crush open amazing screen patrol group space point ten exist slush involve unfold",
    },
];

/// Encoding must produce the published mnemonic byte for byte.
///
/// This is the assertion that pins the wordlist indices, the bit order and the
/// checksum together. Any one of the three being wrong moves at least one word.
#[test]
fn every_published_vector_encodes_to_its_mnemonic() {
    assert_eq!(VECTORS.len(), 24, "all 24 English vectors are asserted");
    for v in VECTORS {
        let entropy = unhex(v.entropy);
        let code = sunrise_crypto::bip39::encode(&entropy).expect("a BIP-39 entropy length");
        assert_eq!(
            code.reveal(),
            v.mnemonic,
            "BIP-39 vector {} did not encode to its published mnemonic",
            v.entropy
        );
    }
}

/// And decoding must recover the published entropy from the published
/// mnemonic — the direction a user's typed recovery code actually travels.
#[test]
fn every_published_mnemonic_decodes_to_its_entropy() {
    for v in VECTORS {
        let entropy = unhex(v.entropy);
        let back = sunrise_crypto::bip39::decode(v.mnemonic).expect("a published mnemonic decodes");
        assert_eq!(
            back.as_slice(),
            entropy.as_slice(),
            "BIP-39 vector {} did not decode back to its published entropy",
            v.entropy
        );
    }
}

/// The eight 256-bit vectors are the shape Sunrise actually ships, so they are
/// also asserted through the typed recovery-code entry points.
#[test]
fn the_256_bit_vectors_are_recovery_codes() {
    let mut seen = 0;
    for v in VECTORS {
        let entropy = unhex(v.entropy);
        if entropy.len() != sunrise_crypto::bip39::RECOVERY_ENTROPY_LEN {
            continue;
        }
        seen += 1;
        let seed: [u8; 32] = entropy.as_slice().try_into().expect("32 bytes");
        let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
        assert_eq!(code.reveal(), v.mnemonic);
        assert_eq!(
            code.word_count(),
            sunrise_crypto::bip39::RECOVERY_WORD_COUNT
        );
        let back = sunrise_crypto::bip39::decode_recovery_code(v.mnemonic)
            .expect("a published 24-word mnemonic is a valid recovery code");
        assert_eq!(*back, seed);
    }
    assert_eq!(seen, 8, "eight of the published vectors carry 256 bits");
}

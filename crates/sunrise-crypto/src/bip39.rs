//! BIP-39 mnemonic codec — the recovery code a user writes down.
//!
//! Per `docs/03-crypto/recovery.md` §Recovery code: 24 words from the BIP-39
//! English wordlist, carrying 256 bits of raw entropy plus the 8-bit BIP-39
//! checksum, encoded as 24 × 11-bit indices.
//!
//! ```text
//! CS = ENT / 32                      // checksum bits
//! MS = (ENT + CS) / 11               // words
//! bits = entropy || SHA-256(entropy)[..CS bits]
//! word[i] = wordlist[bits[11i .. 11i+11]]
//! ```
//!
//! The 256-bit entropy — not the PBKDF2 "seed" BIP-39 §2.5 derives from a
//! mnemonic — is the working secret. `docs/03-crypto/recovery.md` §Server-side
//! material feeds those 32 bytes to Argon2id as the password, so this module
//! implements BIP-39 §3 (entropy ⇄ mnemonic) and deliberately not §2.5: a
//! second stretching step under a fixed empty passphrase would add nothing that
//! Argon2id at m = 64 MiB does not already do, and would put a second,
//! weaker KDF in front of the one the format is specified against.
//!
//! # Why this is implemented here rather than taken from a crate
//!
//! The algorithm is a bit-packing loop and one SHA-256, and the risk a
//! dependency would remove — a checksum that is subtly wrong — is removed
//! instead by testing against the **published** BIP-39 vectors rather than
//! against a round trip. A round trip passes with a permuted wordlist and with
//! a checksum computed over the wrong bits; `tests/bip39_vectors.rs` asserts
//! all twenty-four of the reference English vectors, and
//! `the_wordlist_is_the_published_one` pins the wordlist file to the SHA-256
//! digest BIP-39 publishes for it. Both halves are what make a code produced
//! here readable by any other BIP-39 implementation.
//!
//! # The code is a secret, and the types say so
//!
//! [`RecoveryCode`] has no `Display`, no `Serialize` and no `tracing::Value`,
//! and its `Debug` prints the word count and nothing else — the same shape
//! `sunrise_log::Plain` uses, so a recovery code cannot reach a log record by
//! being formatted into one. Reading it out is [`RecoveryCode::reveal`], named
//! at that length on purpose, and the buffer is zeroized on drop.

use core::fmt;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// The English wordlist, verbatim from BIP-39.
///
/// SHA-256 `2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda`,
/// the digest BIP-39 publishes for `english.txt`;
/// `the_wordlist_is_the_published_one` asserts it.
const WORDLIST_RAW: &str = include_str!("bip39-english.txt");

/// Entries in the wordlist — 2^11, which is what makes a word 11 bits.
pub const WORDLIST_LEN: usize = 2048;

/// Bits one word carries.
const BITS_PER_WORD: usize = 11;

/// Raw entropy behind a Sunrise recovery code, in bytes.
///
/// 256 bits, frozen by `docs/03-crypto/recovery.md` §Recovery code. It is also
/// the length [`crate::seal_recovery_blob`] takes as its Argon2id password.
pub const RECOVERY_ENTROPY_LEN: usize = 32;

/// Words in a Sunrise recovery code: 256 bits + 8 checksum bits over 11.
pub const RECOVERY_WORD_COUNT: usize = 24;

/// Why a mnemonic could not be encoded or decoded.
///
/// **No variant carries a word, a byte of entropy, or a position's contents.**
/// A decode failure is reported to a user who is retyping a secret, and an
/// error string that quotes the offending word is an error string that ends up
/// in a screenshot, a bug report, or a terminal scrollback.
#[derive(Debug, Error, PartialEq, Eq, Clone, Copy)]
pub enum Bip39Error {
    /// Entropy length is not one of the five BIP-39 sizes (16/20/24/28/32 B).
    #[error("entropy must be 16, 20, 24, 28 or 32 bytes, got {0}")]
    EntropyLen(usize),
    /// Word count is not one of 12, 15, 18, 21, 24.
    #[error("a mnemonic has 12, 15, 18, 21 or 24 words, got {0}")]
    WordCount(usize),
    /// A word is not in the English wordlist. Carries its 1-based position so
    /// a user can be pointed at it without the message repeating it.
    #[error("word {position} is not in the BIP-39 English wordlist")]
    UnknownWord {
        /// 1-based position of the offending word.
        position: usize,
    },
    /// The checksum does not match the entropy — a typo, a transposition, or a
    /// code from somewhere else.
    #[error("recovery code checksum does not match; check for a mistyped word")]
    Checksum,
}

/// A BIP-39 mnemonic, held so it cannot be logged and is wiped on drop.
///
/// Construct with [`encode`] or [`encode_recovery_code`]; read with
/// [`RecoveryCode::reveal`].
pub struct RecoveryCode {
    words: Zeroizing<String>,
}

impl RecoveryCode {
    /// The mnemonic itself: space-separated lowercase words.
    ///
    /// Named `reveal` rather than `as_str` because every call site is a place
    /// the user's one written-down secret escapes the type that protects it.
    /// The only sanctioned destinations are a screen and a user's clipboard —
    /// never a log record, a metric label, a file the process writes, or an
    /// error message.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.words
    }

    /// How many words the code has.
    #[must_use]
    pub fn word_count(&self) -> usize {
        self.words.split_whitespace().count()
    }
}

/// Prints the shape and never the words, so a `{:?}` anywhere — including a
/// `tracing` field — cannot leak the code.
impl fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RecoveryCode(<{} words redacted>)", self.word_count())
    }
}

/// The wordlist, indexed.
///
/// Parsed once. `include_str!` gives one `&'static str`, and both directions of
/// the codec want random access by index, so the line offsets are computed
/// rather than re-scanned per lookup.
fn wordlist() -> &'static [&'static str] {
    static CELL: OnceLock<Vec<&'static str>> = OnceLock::new();
    CELL.get_or_init(|| {
        WORDLIST_RAW
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect()
    })
}

/// Whether `bytes` is a BIP-39 entropy length.
fn checksum_bits(entropy_len: usize) -> Result<usize, Bip39Error> {
    match entropy_len {
        16 | 20 | 24 | 28 | 32 => Ok(entropy_len * 8 / 32),
        other => Err(Bip39Error::EntropyLen(other)),
    }
}

/// The first `bits` bits of `SHA-256(entropy)`, right-aligned in a `u8`.
fn checksum(entropy: &[u8], bits: usize) -> u8 {
    let digest = Sha256::digest(entropy);
    // `bits` is at most 8 (32 bytes of entropy), so the first digest byte holds
    // all of it and the shift is well defined.
    digest[0] >> (8 - bits)
}

/// Encode `entropy` as a BIP-39 English mnemonic.
///
/// Accepts every BIP-39 entropy length; Sunrise itself only ever uses 32 bytes
/// ([`encode_recovery_code`]). The wider domain is what lets the published
/// 128/160/192/224-bit vectors be asserted against this code rather than
/// against a second implementation written for the test.
///
/// # Errors
/// [`Bip39Error::EntropyLen`] for a length BIP-39 does not define.
pub fn encode(entropy: &[u8]) -> Result<RecoveryCode, Bip39Error> {
    let cs_bits = checksum_bits(entropy.len())?;
    let total_bits = entropy.len() * 8 + cs_bits;
    let cs = checksum(entropy, cs_bits);
    let list = wordlist();

    let mut words = Zeroizing::new(String::with_capacity(total_bits / BITS_PER_WORD * 9));
    for word_index in 0..total_bits / BITS_PER_WORD {
        let mut index = 0usize;
        for bit in 0..BITS_PER_WORD {
            let pos = word_index * BITS_PER_WORD + bit;
            let value = if pos < entropy.len() * 8 {
                (entropy[pos / 8] >> (7 - pos % 8)) & 1
            } else {
                // Checksum bits, most significant first.
                (cs >> (cs_bits - 1 - (pos - entropy.len() * 8))) & 1
            };
            index = (index << 1) | usize::from(value);
        }
        if word_index > 0 {
            words.push(' ');
        }
        // `index` is 11 bits, so it is in range for a 2048-entry list. The
        // list's length is asserted by `the_wordlist_has_2048_entries`.
        words.push_str(list[index]);
    }
    Ok(RecoveryCode { words })
}

/// Decode a BIP-39 English mnemonic back to its entropy, verifying the
/// checksum.
///
/// Whitespace runs and surrounding space are tolerated and words are matched
/// case-insensitively: a user retyping twenty-four words from paper should not
/// be refused for capitalising the first one.
///
/// # Errors
/// [`Bip39Error::WordCount`], [`Bip39Error::UnknownWord`] or
/// [`Bip39Error::Checksum`]. None of them quotes the input.
pub fn decode(mnemonic: &str) -> Result<Zeroizing<Vec<u8>>, Bip39Error> {
    let list = wordlist();
    let words: Vec<&str> = mnemonic.split_whitespace().collect();
    let entropy_len = match words.len() {
        12 => 16,
        15 => 20,
        18 => 24,
        21 => 28,
        24 => 32,
        other => return Err(Bip39Error::WordCount(other)),
    };
    let cs_bits = checksum_bits(entropy_len)?;

    let mut entropy = Zeroizing::new(vec![0u8; entropy_len]);
    let mut cs_seen = 0u8;
    let mut bit_pos = 0usize;
    for (position, word) in words.iter().enumerate() {
        let lower = word.to_ascii_lowercase();
        let index = list
            .binary_search(&lower.as_str())
            .map_err(|_| Bip39Error::UnknownWord {
                position: position + 1,
            })?;
        for bit in (0..BITS_PER_WORD).rev() {
            let value = u8::try_from((index >> bit) & 1).unwrap_or(0);
            if bit_pos < entropy_len * 8 {
                entropy[bit_pos / 8] |= value << (7 - bit_pos % 8);
            } else {
                cs_seen = (cs_seen << 1) | value;
            }
            bit_pos += 1;
        }
    }

    if cs_seen != checksum(&entropy, cs_bits) {
        // The entropy is wiped by `Zeroizing` on the early return; naming it
        // here is what documents that a failed decode leaves nothing behind.
        return Err(Bip39Error::Checksum);
    }
    Ok(entropy)
}

/// Encode a 32-byte recovery seed as the 24-word Sunrise recovery code.
///
/// This is the product path: `docs/03-crypto/recovery.md` fixes the length, so
/// this signature cannot express a shorter code.
#[must_use]
pub fn encode_recovery_code(seed: &[u8; RECOVERY_ENTROPY_LEN]) -> RecoveryCode {
    // The length is a compile-time constant and is one BIP-39 accepts, so the
    // only error `encode` can return here is unreachable.
    encode(seed).unwrap_or_else(|_| unreachable!("32 bytes is a BIP-39 entropy length"))
}

/// Decode a 24-word Sunrise recovery code back to its 32-byte seed.
///
/// # Errors
/// As [`decode`], plus [`Bip39Error::WordCount`] for a code that is a valid
/// BIP-39 mnemonic of some *other* length — 12 words is a real mnemonic and is
/// not a recovery code for this account, and a caller that accepted it would be
/// running Argon2id over 128 bits while telling the user it had 256.
pub fn decode_recovery_code(
    mnemonic: &str,
) -> Result<Zeroizing<[u8; RECOVERY_ENTROPY_LEN]>, Bip39Error> {
    let words = mnemonic.split_whitespace().count();
    if words != RECOVERY_WORD_COUNT {
        return Err(Bip39Error::WordCount(words));
    }
    let mut entropy = decode(mnemonic)?;
    let mut out = Zeroizing::new([0u8; RECOVERY_ENTROPY_LEN]);
    out.copy_from_slice(&entropy);
    entropy.zeroize();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wordlist is a fixed 2048 entries; every index in this module is
    /// eleven bits and indexes it unchecked.
    #[test]
    fn the_wordlist_has_2048_entries() {
        assert_eq!(wordlist().len(), WORDLIST_LEN);
    }

    /// Pinned to the digest BIP-39 publishes for `english.txt`.
    ///
    /// This is the assertion that a round trip cannot make: a permuted or
    /// mistranscribed list round-trips perfectly and produces codes no other
    /// implementation can read.
    #[test]
    fn the_wordlist_is_the_published_one() {
        let digest = Sha256::digest(WORDLIST_RAW.as_bytes());
        assert_eq!(
            hex::encode(digest),
            "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda",
            "the embedded wordlist is not BIP-39's english.txt"
        );
    }

    /// `decode` binary-searches, which is only correct on a sorted list.
    #[test]
    fn the_wordlist_is_sorted() {
        let list = wordlist();
        assert!(
            list.windows(2).all(|w| w[0] < w[1]),
            "binary search requires the wordlist to be sorted"
        );
    }

    #[test]
    fn a_recovery_code_is_twenty_four_words() {
        let code = encode_recovery_code(&[0x42; 32]);
        assert_eq!(code.word_count(), RECOVERY_WORD_COUNT);
        let back = decode_recovery_code(code.reveal()).expect("its own code decodes");
        assert_eq!(*back, [0x42; 32]);
    }

    /// A code of a different valid BIP-39 length is refused rather than
    /// silently stretched: 12 words is 128 bits and is not this account's key.
    #[test]
    fn a_twelve_word_mnemonic_is_not_a_recovery_code() {
        let short = encode(&[0u8; 16]).expect("16 bytes is a BIP-39 length");
        assert!(matches!(
            decode_recovery_code(short.reveal()),
            Err(Bip39Error::WordCount(12))
        ));
    }

    /// One swapped word breaks the checksum, which is the whole reason the
    /// checksum is in the format: the typo surfaces at code entry rather than
    /// as an Argon2id run that ends in an AEAD failure.
    #[test]
    fn a_single_wrong_word_fails_the_checksum() {
        let code = encode_recovery_code(&[7u8; 32]);
        let mut words: Vec<&str> = code.reveal().split(' ').collect();
        // Replace the first word with a different real word, so the failure is
        // the checksum rather than an unknown word.
        let replacement = if words[0] == "zoo" { "abandon" } else { "zoo" };
        words[0] = replacement;
        assert!(matches!(
            decode(&words.join(" ")),
            Err(Bip39Error::Checksum)
        ));
    }

    /// A word outside the list is named by position and never quoted.
    #[test]
    fn an_unknown_word_is_named_by_position_only() {
        let code = encode_recovery_code(&[3u8; 32]);
        let mut words: Vec<String> = code.reveal().split(' ').map(str::to_owned).collect();
        words[4] = "hunter2".to_owned();
        let err = match decode(&words.join(" ")) {
            Err(e) => e,
            Ok(_) => panic!("an invented word is not in the list"),
        };
        assert_eq!(err, Bip39Error::UnknownWord { position: 5 });
        assert!(
            !err.to_string().contains("hunter2"),
            "an error a user reads while retyping a secret must not quote it"
        );
    }

    /// Case and extra whitespace are the two things a person retyping from
    /// paper actually produces.
    #[test]
    fn case_and_spacing_are_tolerated() {
        let code = encode_recovery_code(&[0xAB; 32]);
        let noisy = format!("  {}  ", code.reveal().to_uppercase().replace(' ', "   "));
        let back = match decode_recovery_code(&noisy) {
            Ok(seed) => seed,
            Err(e) => panic!("a noisy but correct code must decode: {e}"),
        };
        assert_eq!(*back, [0xAB; 32]);
    }

    /// Nothing formats the words. `Debug` is the one trait a `tracing` field
    /// can reach on a type that implements neither `Display` nor `Value`.
    #[test]
    fn debug_does_not_print_the_code() {
        let code = encode_recovery_code(&[9u8; 32]);
        let rendered = format!("{code:?}");
        // Asserted whole rather than word by word: several BIP-39 words are
        // substrings of the redaction notice itself ("act" inside "redacted"),
        // so a per-word search fails on a `Debug` that is doing its job.
        assert_eq!(rendered, "RecoveryCode(<24 words redacted>)");
        assert!(
            !rendered.contains(code.reveal()),
            "the code must not survive a Debug format"
        );
    }

    #[test]
    fn an_undefined_entropy_length_is_refused() {
        assert!(matches!(
            encode(&[0u8; 17]),
            Err(Bip39Error::EntropyLen(17))
        ));
    }

    /// The 32-byte recovery seed round-trips for a spread of inputs, including
    /// the two the bit-packing loop is most likely to get wrong.
    #[test]
    fn every_bit_pattern_survives_the_round_trip() {
        for seed in [[0x00u8; 32], [0xFFu8; 32], [0x55u8; 32], [0xAAu8; 32]] {
            let code = encode_recovery_code(&seed);
            let back = match decode_recovery_code(code.reveal()) {
                Ok(back) => back,
                Err(e) => panic!("a code this module produced must decode: {e}"),
            };
            assert_eq!(*back, seed);
        }
    }

    /// A decode that fails leaves the caller with nothing, not with a
    /// half-filled buffer they might mistake for a seed.
    #[test]
    fn a_failed_decode_yields_no_entropy() {
        assert!(decode("abandon abandon abandon").is_err());
        assert!(decode("").is_err());
    }
}

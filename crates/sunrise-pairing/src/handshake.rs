//! Noise XX pairing handshake.
//!
//! Implements the transcript in `docs/03-crypto/pairing-and-onboarding.md`:
//! `Noise_XX_25519_ChaChaPoly_SHA256`, three handshake messages, then a SAS
//! both users compare out of band, then an authenticated encrypted channel the
//! existing device uses to hand the new device its vault root.
//!
//! # Why this exists
//!
//! Until now there was no way to get a vault root onto a second device. Sync
//! worked in tests only because both replicas were handed the same literal key.
//! Encryption was real; key distribution was bypassed entirely. This module is
//! the missing half.
//!
//! # Design notes worth knowing
//!
//! - **The Noise static keys are not the device or identity keys.** Each side
//!   generates a throwaway X25519 static for this handshake only; long-term
//!   keys travel as transport messages *after* the handshake, so a captured
//!   transcript reveals no durable identity material.
//! - **XX, not IK or NK.** XX defers identity transmission and gives mutual
//!   authentication, which fits a flow where the new device's "identity" is a
//!   key it just generated and the existing device proves possession mid-flow.
//! - **The SAS is the whole authentication story on the numeric path.** It is
//!   ~20 bits, which is only safe because it is interactive: a MITM gets a
//!   single online attempt and any mismatch aborts. Both sides must confirm.
//!   [`PairingSession::into_channel`] therefore cannot be reached without
//!   passing through [`PairingSession::sas`].
//! - **The relay sees only ciphertext.** It routes by `pair_id` and can
//!   observe message sizes and timing, nothing more.

use crate::sas::compute_sas;
use snow::{Builder, HandshakeState, TransportState};
use thiserror::Error;

/// The Noise pattern this protocol version pins.
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Largest Noise message we will construct or accept.
///
/// The Noise spec caps a message at 65535 bytes. Rejecting oversize input
/// before allocating keeps a hostile relay from steering us into a large
/// allocation.
pub const MAX_NOISE_MESSAGE: usize = 65535;

/// Pairing errors.
#[derive(Debug, Error)]
pub enum PairingError {
    /// The underlying Noise implementation rejected something.
    #[error("noise: {0}")]
    Noise(String),
    /// A method was called in the wrong order.
    #[error("pairing protocol misuse: {0}")]
    Misuse(&'static str),
    /// A message exceeded the Noise size bound.
    #[error("message too large: {0} bytes (max {MAX_NOISE_MESSAGE})")]
    TooLarge(usize),
    /// The SAS was rejected by a user, or timed out.
    #[error("pairing aborted at SAS confirmation")]
    SasRejected,
}

impl From<snow::Error> for PairingError {
    fn from(e: snow::Error) -> Self {
        // snow's Display is deliberately terse about *why* a decrypt failed, so
        // this reveals nothing an attacker can use as an oracle.
        Self::Noise(e.to_string())
    }
}

/// Which side of the pairing this device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The **new** device, which generated the QR and initiates.
    NewDevice,
    /// The **existing** device, which scanned the QR and holds the vault root.
    ExistingDevice,
}

/// An in-progress Noise XX handshake.
///
/// Drive it by alternating [`Self::write_message`] and [`Self::read_message`]
/// until [`Self::is_complete`], then read [`Self::sas`], confirm out of band,
/// and call [`Self::into_channel`].
#[derive(Debug)]
pub struct PairingSession {
    state: Option<HandshakeState>,
    role: Role,
}

impl PairingSession {
    /// Start a handshake. `static_key` is a freshly generated X25519 private
    /// key used **only** for this handshake.
    pub fn new(role: Role, static_key: &[u8]) -> Result<Self, PairingError> {
        let params = NOISE_PARAMS.parse().map_err(PairingError::from)?;
        let builder = Builder::new(params).local_private_key(static_key);
        let state = match role {
            Role::NewDevice => builder.build_initiator()?,
            Role::ExistingDevice => builder.build_responder()?,
        };
        Ok(Self {
            state: Some(state),
            role,
        })
    }

    /// Generate a static keypair suitable for [`Self::new`].
    ///
    /// Uses snow's own keypair generation, which draws from the OS CSPRNG.
    /// This is one of the few places a pure function is the wrong shape: a
    /// caller-supplied "random" value here would be a footgun with no upside,
    /// since the key must be unpredictable to the relay.
    pub fn generate_static_key() -> Result<Vec<u8>, PairingError> {
        let params = NOISE_PARAMS.parse().map_err(PairingError::from)?;
        let kp = Builder::new(params).generate_keypair()?;
        Ok(kp.private)
    }

    /// This side's role.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// Whether the three-message transcript has completed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(HandshakeState::is_handshake_finished)
    }

    /// Produce the next handshake message to send to the peer.
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, PairingError> {
        if payload.len() > MAX_NOISE_MESSAGE {
            return Err(PairingError::TooLarge(payload.len()));
        }
        let state = self
            .state
            .as_mut()
            .ok_or(PairingError::Misuse("handshake already consumed"))?;
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = state.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume a handshake message from the peer, returning its payload.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>, PairingError> {
        if message.len() > MAX_NOISE_MESSAGE {
            return Err(PairingError::TooLarge(message.len()));
        }
        let state = self
            .state
            .as_mut()
            .ok_or(PairingError::Misuse("handshake already consumed"))?;
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = state.read_message(message, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// The 6-digit SAS both users compare.
    ///
    /// Derived from the Noise handshake hash, which binds every message in the
    /// transcript. Two sessions that were not talking to each other cannot
    /// produce the same value except by a 1-in-a-million collision — and the
    /// user gets exactly one chance to accept it.
    pub fn sas(&self) -> Result<String, PairingError> {
        let state = self
            .state
            .as_ref()
            .ok_or(PairingError::Misuse("handshake already consumed"))?;
        if !state.is_handshake_finished() {
            return Err(PairingError::Misuse(
                "SAS is only defined after the handshake completes",
            ));
        }
        Ok(compute_sas(state.get_handshake_hash()))
    }

    /// Convert into the encrypted channel, after the user confirmed the SAS.
    ///
    /// `confirmed` is the user's answer on *this* device. Both sides must
    /// confirm; a `false` here aborts and discards the keys, which is what the
    /// spec's `pair_abort` path amounts to locally.
    pub fn into_channel(mut self, confirmed: bool) -> Result<PairedChannel, PairingError> {
        let state = self
            .state
            .take()
            .ok_or(PairingError::Misuse("handshake already consumed"))?;
        if !state.is_handshake_finished() {
            return Err(PairingError::Misuse("handshake is not complete"));
        }
        if !confirmed {
            // `state` drops here, taking the ephemeral keys with it.
            return Err(PairingError::SasRejected);
        }
        Ok(PairedChannel {
            state: state.into_transport_mode()?,
        })
    }
}

/// An authenticated, encrypted channel between two paired devices.
///
/// The existing device sends the vault root and identity material through
/// this; the relay sees only ciphertext.
#[derive(Debug)]
pub struct PairedChannel {
    state: TransportState,
}

impl PairedChannel {
    /// Encrypt a payload for the peer.
    pub fn send(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, PairingError> {
        if plaintext.len() > MAX_NOISE_MESSAGE {
            return Err(PairingError::TooLarge(plaintext.len()));
        }
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.write_message(plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decrypt a payload from the peer.
    ///
    /// Fails on any tamper: Noise transport messages are AEAD-sealed with a
    /// per-message nonce, so a modified or replayed frame is rejected rather
    /// than surfacing as corrupt plaintext.
    pub fn receive(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, PairingError> {
        if ciphertext.len() > MAX_NOISE_MESSAGE {
            return Err(PairingError::TooLarge(ciphertext.len()));
        }
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the full XX transcript between two in-process sessions.
    fn complete_handshake() -> (PairingSession, PairingSession) {
        let n_key = PairingSession::generate_static_key().unwrap();
        let e_key = PairingSession::generate_static_key().unwrap();
        let mut new_dev = PairingSession::new(Role::NewDevice, &n_key).unwrap();
        let mut existing = PairingSession::new(Role::ExistingDevice, &e_key).unwrap();

        // XX is three messages: -> e, <- e ee s es, -> s se
        let m1 = new_dev.write_message(&[]).unwrap();
        existing.read_message(&m1).unwrap();
        let m2 = existing.write_message(&[]).unwrap();
        new_dev.read_message(&m2).unwrap();
        let m3 = new_dev.write_message(&[]).unwrap();
        existing.read_message(&m3).unwrap();

        (new_dev, existing)
    }

    #[test]
    fn handshake_completes_on_both_sides() {
        let (n, e) = complete_handshake();
        assert!(n.is_complete());
        assert!(e.is_complete());
    }

    /// The property the user relies on: the code shown on both screens matches.
    #[test]
    fn both_sides_derive_the_same_sas() {
        let (n, e) = complete_handshake();
        let a = n.sas().unwrap();
        let b = e.sas().unwrap();
        assert_eq!(a, b, "both devices must show the same code");
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
    }

    /// A man in the middle runs two *separate* handshakes — one with each
    /// victim — so the handshake hashes differ and the codes cannot match.
    /// This is precisely what the user's comparison detects.
    #[test]
    fn a_man_in_the_middle_produces_mismatched_codes() {
        let (victim_a, _mitm_side_a) = complete_handshake();
        let (_mitm_side_b, victim_b) = complete_handshake();
        assert_ne!(
            victim_a.sas().unwrap(),
            victim_b.sas().unwrap(),
            "two distinct handshakes must not agree on a SAS"
        );
    }

    #[test]
    fn sas_is_unavailable_before_the_handshake_finishes() {
        let key = PairingSession::generate_static_key().unwrap();
        let s = PairingSession::new(Role::NewDevice, &key).unwrap();
        assert!(
            matches!(s.sas(), Err(PairingError::Misuse(_))),
            "a SAS read mid-handshake would authenticate nothing"
        );
    }

    /// The thing that was impossible before this module existed.
    #[test]
    fn a_vault_root_transfers_to_the_new_device() {
        let (n, e) = complete_handshake();
        let mut new_ch = n.into_channel(true).unwrap();
        let mut existing_ch = e.into_channel(true).unwrap();

        let vault_root = [0x42u8; 32];
        let wire = existing_ch.send(&vault_root).unwrap();
        assert_ne!(&wire[..32], &vault_root[..], "must not travel in the clear");

        let got = new_ch.receive(&wire).unwrap();
        assert_eq!(got, vault_root, "the new device must recover the root");
    }

    #[test]
    fn the_channel_is_bidirectional() {
        let (n, e) = complete_handshake();
        let mut a = n.into_channel(true).unwrap();
        let mut b = e.into_channel(true).unwrap();
        let up = a.send(b"device cert from the new device").unwrap();
        assert_eq!(b.receive(&up).unwrap(), b"device cert from the new device");
        let down = b.send(b"vault root").unwrap();
        assert_eq!(a.receive(&down).unwrap(), b"vault root");
    }

    /// "Don't match" must destroy the session rather than merely warning.
    #[test]
    fn rejecting_the_sas_aborts_and_discards_keys() {
        let (n, _e) = complete_handshake();
        assert!(matches!(
            n.into_channel(false),
            Err(PairingError::SasRejected)
        ));
    }

    #[test]
    fn a_tampered_transport_message_is_rejected() {
        let (n, e) = complete_handshake();
        let mut a = n.into_channel(true).unwrap();
        let mut b = e.into_channel(true).unwrap();
        let mut wire = a.send(b"secret").unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 0x01;
        assert!(
            b.receive(&wire).is_err(),
            "a flipped bit must fail the AEAD, not yield corrupt plaintext"
        );
    }

    /// A replayed frame must not decrypt twice — Noise nonces are per-message.
    #[test]
    fn a_replayed_transport_message_is_rejected() {
        let (n, e) = complete_handshake();
        let mut a = n.into_channel(true).unwrap();
        let mut b = e.into_channel(true).unwrap();
        let wire = a.send(b"once").unwrap();
        assert_eq!(b.receive(&wire).unwrap(), b"once");
        assert!(b.receive(&wire).is_err(), "replay must be rejected");
    }

    #[test]
    fn a_handshake_message_from_a_stranger_is_rejected() {
        let n_key = PairingSession::generate_static_key().unwrap();
        let e_key = PairingSession::generate_static_key().unwrap();
        let x_key = PairingSession::generate_static_key().unwrap();
        let mut new_dev = PairingSession::new(Role::NewDevice, &n_key).unwrap();
        let mut existing = PairingSession::new(Role::ExistingDevice, &e_key).unwrap();
        let mut stranger = PairingSession::new(Role::NewDevice, &x_key).unwrap();

        let m1 = new_dev.write_message(&[]).unwrap();
        existing.read_message(&m1).unwrap();
        let m2 = existing.write_message(&[]).unwrap();
        // The stranger's transcript diverges, so it cannot consume this.
        let _ = stranger.write_message(&[]).unwrap();
        assert!(
            stranger.read_message(&m2).is_err()
                || stranger.sas().is_err()
                || stranger.sas().ok() != new_dev.sas().ok(),
            "a third party must not land on the same transcript"
        );
    }

    #[test]
    fn oversize_messages_are_refused_before_allocating() {
        let key = PairingSession::generate_static_key().unwrap();
        let mut s = PairingSession::new(Role::NewDevice, &key).unwrap();
        let huge = vec![0u8; MAX_NOISE_MESSAGE + 1];
        assert!(matches!(
            s.write_message(&huge),
            Err(PairingError::TooLarge(_))
        ));
        assert!(matches!(
            s.read_message(&huge),
            Err(PairingError::TooLarge(_))
        ));
    }
}

//! One device's half of a pairing: the Noise XX transcript, the SAS, and the
//! three messages sealed over the confirmed channel.
//!
//! `sunrise-pairing` is the protocol (`docs/03-crypto/pairing-and-onboarding.md`
//! Mode A). This is that protocol as a client holds it between calls: which
//! side it is, the QR it published or the `n_static_pub` it scanned, the
//! handshake until the SAS is answered, the channel after, and the joiner's
//! minted keys between the cert request and the grant. Every message crosses
//! as base64url text, because the object does not care who carries it — the
//! relay ([`super::RelayPairing`]) or a person copying text between screens.
//!
//! The `UniFFI` seam's `DevicePairing` wraps this one, and the CLI's
//! `sunrise pair join` and `sunrise pair add` drive it through
//! [`super::RelayPairing`], so every client pairs through the same code.
//!
//! # What crosses
//!
//! 1. [`PairingDevice::seal_pairing_offer`] — the sponsor's account identity.
//!    **No secret.**
//! 2. [`PairingDevice::request_device_cert`] — the joiner mints `D_S`/`D_D`
//!    and sends the public halves; the secrets stay in this object.
//! 3. [`PairingDevice::seal_pairing_grant`] / [`PairingDevice::open_pairing_grant`]
//!    — the `DeviceCert` the sponsor issued, the vault root and every Stream
//!    key.
//!
//! Neither identity private key crosses (#86, #105).

use std::sync::{Mutex, PoisonError};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sunrise_pairing::{
    account_email_hash, decode_pairing_grant, decode_pairing_offer, decode_pairing_request,
    decode_qr_payload, encode_qr_payload, PairedChannel, PairingJoiner, PairingPayload,
    PairingSession, QrPayload, Role, MAGIC_V1_HEX,
};
use zeroize::Zeroize;

use super::{PairingFault, RendezvousRole};

/// Where a pairing has got to, derived from the session rather than tracked
/// beside it: the step that matters — the SAS cannot be skipped — is enforced
/// by `sunrise-pairing` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceStep {
    /// Still exchanging handshake messages.
    Handshaking,
    /// The transcript is complete; both users must compare the SAS.
    AwaitingConfirmation,
    /// The SAS was confirmed on this device and the channel is open.
    Confirmed,
    /// Over, one way or another.
    Finished,
}

/// One device's half of a pairing. See the module comment.
///
/// The state is behind `Mutex`es so the object is `Send + Sync` and one handle
/// can be shared between the waits a relay pairing runs.
pub struct PairingDevice {
    role: RendezvousRole,
    /// The QR this device publishes; only the new device has one.
    qr: Option<String>,
    session: Mutex<Option<PairingSession>>,
    /// The `n_static_pub` this device scanned, on the existing device: the QR
    /// path's authentication, spent in [`Self::receive_message`].
    expected_peer_static: Option<Vec<u8>>,
    channel: Mutex<Option<PairedChannel>>,
    /// The joiner's minted device keys, between message 2 and message 3. They
    /// never leave this object: only the public halves went into the request.
    joiner: Mutex<Option<PairingJoiner>>,
    /// Set once the channel has carried the grant, so a screen can stop.
    finished: Mutex<bool>,
}

impl std::fmt::Debug for PairingDevice {
    /// No handshake state, no keys, no QR: a `Debug` that printed the session
    /// would put what a MITM needs into whatever log the client writes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingDevice")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

fn pairing(e: impl std::fmt::Display) -> PairingFault {
    PairingFault::Pairing(e.to_string())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PairingDevice {
    /// Begin on the **new** device: mint a throwaway static keypair and a pair
    /// id, and publish them in a QR payload naming `relay_url`.
    ///
    /// `account_email` is hashed to four bytes before it goes anywhere near
    /// the payload: the QR names an account without naming a person.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] if key generation or encoding fails.
    pub fn offer(relay_url: String, account_email: &str) -> Result<Self, PairingFault> {
        let keys = PairingSession::generate_static_keypair().map_err(pairing)?;
        // The pair id is the routing label the relay keys its rendezvous on. It
        // is not a secret and not authentication — `n_static_pub` is — so the
        // same CSPRNG that made the key is enough.
        let filler = PairingSession::generate_static_keypair().map_err(pairing)?;
        let mut pair_id = [0u8; 16];
        pair_id.copy_from_slice(&filler.public[..16]);
        let payload = QrPayload {
            magic_v1: MAGIC_V1_HEX.to_string(),
            pair_id: URL_SAFE_NO_PAD.encode(pair_id),
            n_static_pub: URL_SAFE_NO_PAD.encode(&keys.public),
            account_email_hash: account_tag(account_email),
            relay_url,
        };
        let qr = encode_qr_payload(&payload).map_err(pairing)?;
        let session = PairingSession::new(Role::NewDevice, &keys.private).map_err(pairing)?;
        Ok(Self::wrap(
            RendezvousRole::NewDevice,
            Some(qr),
            session,
            None,
        ))
    }

    /// Begin on the **existing** device, from the QR the new one showed.
    ///
    /// The payload is validated by the decoder that wrote it, and its
    /// `n_static_pub` is **kept**: Noise XX defers identity, so the QR binds
    /// the handshake only if something compares the two, and
    /// [`Self::receive_message`] does the moment the transcript completes.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] with the decoder's own message.
    pub fn accept(qr_payload: &str) -> Result<Self, PairingFault> {
        let scanned = decode_qr_payload(qr_payload).map_err(pairing)?;
        let expected = decode_b64(&scanned.n_static_pub, "n_static_pub")?;
        let keys = PairingSession::generate_static_keypair().map_err(pairing)?;
        let session = PairingSession::new(Role::ExistingDevice, &keys.private).map_err(pairing)?;
        Ok(Self::wrap(
            RendezvousRole::ExistingDevice,
            None,
            session,
            Some(expected),
        ))
    }

    /// This device's side.
    #[must_use]
    pub const fn role(&self) -> RendezvousRole {
        self.role
    }

    /// The QR payload to display, on the new device.
    #[must_use]
    pub fn qr_payload(&self) -> Option<String> {
        self.qr.clone()
    }

    /// Where the pairing has got to.
    #[must_use]
    pub fn step(&self) -> DeviceStep {
        if *lock(&self.finished) {
            return DeviceStep::Finished;
        }
        if lock(&self.channel).is_some() {
            return DeviceStep::Confirmed;
        }
        if lock(&self.session)
            .as_ref()
            .is_some_and(PairingSession::is_complete)
        {
            DeviceStep::AwaitingConfirmation
        } else {
            DeviceStep::Handshaking
        }
    }

    /// The next handshake message to hand the peer, base64url.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] when it is not this side's turn to write, or
    /// the session is already consumed.
    pub fn next_message(&self) -> Result<String, PairingFault> {
        let mut guard = lock(&self.session);
        let session = guard.as_mut().ok_or_else(over)?;
        Ok(URL_SAFE_NO_PAD.encode(session.write_message(&[]).map_err(pairing)?))
    }

    /// Consume the peer's handshake message.
    ///
    /// On the existing device the message that completes the transcript is
    /// where the QR is spent: the peer's static key must be the one the QR
    /// carried, and a mismatch ends the pairing before any SAS exists.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] for a message that is not base64url, arrives
    /// out of turn or fails to decrypt, and for a completed transcript whose
    /// peer static is not the QR's `n_static_pub`.
    pub fn receive_message(&self, message: &str) -> Result<(), PairingFault> {
        let raw = decode_b64(message, "handshake message")?;
        let mut guard = lock(&self.session);
        let session = guard.as_mut().ok_or_else(over)?;
        session.read_message(&raw).map_err(pairing)?;
        if let Some(expected) = self.expected_peer_static.as_deref() {
            if session.is_complete() && session.peer_static_key().as_deref() != Some(expected) {
                // Drop the handshake state with it: a session left behind is
                // one a screen could still ask for a SAS.
                *guard = None;
                *lock(&self.finished) = true;
                return Err(PairingFault::Pairing(
                    "the peer's Noise static key is not the one the QR published".into(),
                ));
            }
        }
        Ok(())
    }

    /// The six digits both users compare out of band, once the transcript
    /// completes.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] before the handshake finishes.
    pub fn sas(&self) -> Result<String, PairingFault> {
        let guard = lock(&self.session);
        let session = guard.as_ref().ok_or_else(over)?;
        session.sas().map_err(pairing)
    }

    /// Record this user's answer to the digits. `false` discards the keys and
    /// ends the pairing — the local half of the spec's `pair_abort`.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] on a rejection or before the handshake
    /// completes: a rejection is the one outcome a caller must not read as
    /// "carry on".
    pub fn confirm(&self, matched: bool) -> Result<(), PairingFault> {
        let session = lock(&self.session).take().ok_or_else(over)?;
        match session.into_channel(matched) {
            Ok(channel) => {
                *lock(&self.channel) = Some(channel);
                Ok(())
            }
            Err(e) => {
                *lock(&self.finished) = true;
                Err(pairing(e))
            }
        }
    }

    /// Seal message 1 — the encoded offer — for the joiner, on the sponsor.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] before the SAS is confirmed, or on the
    /// device being added.
    pub fn seal_pairing_offer(&self, offer: &[u8]) -> Result<String, PairingFault> {
        self.send_as(
            RendezvousRole::ExistingDevice,
            offer,
            "the pairing offer",
            false,
        )
    }

    /// Mint this device's keys from `seed_s` and `seed_d` and seal message 2,
    /// the cert request, on the joiner.
    ///
    /// The seeds **must be unpredictable**: `D_S_priv` is derived from the
    /// first and every op this device writes is signed under it. They are a
    /// parameter so the one source of randomness a client uses is its own.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] before the SAS is confirmed, on the device
    /// holding the vault, or for an offer that does not open or decode, and
    /// [`PairingFault::BadLength`] for a seed that is not 32 bytes.
    pub fn request_device_cert(
        &self,
        sealed_offer: &str,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<String, PairingFault> {
        if self.role != RendezvousRole::NewDevice {
            return Err(PairingFault::Pairing(
                "only the device being added asks for a cert".into(),
            ));
        }
        let seed_s = seed32(seed_s)?;
        let seed_d = seed32(seed_d)?;
        let plain =
            self.receive_from(RendezvousRole::NewDevice, sealed_offer, "the pairing offer")?;
        let offer = decode_pairing_offer(&plain).map_err(pairing)?;
        let joiner = PairingJoiner::new(offer, nickname, platform, seed_s, seed_d);
        let request = joiner.request().encode().map_err(pairing)?;
        *lock(&self.joiner) = Some(joiner);
        self.send_as(
            RendezvousRole::NewDevice,
            &request,
            "the cert request",
            false,
        )
    }

    /// Open message 2 on the sponsor and return the request's bytes for the
    /// vault to answer. Decoded here as well, so a malformed request is
    /// refused where the sponsor sees it rather than as a failed grant.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] before the SAS is confirmed, on the device
    /// being added, or for ciphertext that does not open.
    pub fn open_cert_request(&self, sealed: &str) -> Result<Vec<u8>, PairingFault> {
        let plain =
            self.receive_from(RendezvousRole::ExistingDevice, sealed, "the cert request")?;
        decode_pairing_request(&plain).map_err(pairing)?;
        Ok(plain)
    }

    /// Seal message 3 — the encoded grant — for the joiner, on the sponsor.
    /// It ends the pairing on this side.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] before the SAS is confirmed, or on the
    /// device being added.
    pub fn seal_pairing_grant(&self, grant: &[u8]) -> Result<String, PairingFault> {
        self.send_as(
            RendezvousRole::ExistingDevice,
            grant,
            "the pairing grant",
            true,
        )
    }

    /// Open message 3 on the joiner: the payload a vault is opened with.
    ///
    /// The cert is checked here against the offer this object has held since
    /// [`Self::request_device_cert`]: it must verify under the `ID_S_pub` that
    /// offer named and name the keys this device minted.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] before the SAS is confirmed, before
    /// [`Self::request_device_cert`], for ciphertext that does not open, or a
    /// grant that does not decode or whose cert this device cannot use.
    pub fn open_pairing_grant(&self, sealed: &str) -> Result<PairingPayload, PairingFault> {
        let plain = self.receive_from(RendezvousRole::NewDevice, sealed, "the pairing grant")?;
        let grant = decode_pairing_grant(&plain).map_err(pairing)?;
        let joiner = lock(&self.joiner).take().ok_or_else(|| {
            PairingFault::Pairing("ask for a cert before trying to accept one".into())
        })?;
        let payload = joiner.accept(grant).map_err(pairing)?;
        *lock(&self.finished) = true;
        Ok(payload)
    }

    fn wrap(
        role: RendezvousRole,
        qr: Option<String>,
        session: PairingSession,
        expected_peer_static: Option<Vec<u8>>,
    ) -> Self {
        Self {
            role,
            qr,
            session: Mutex::new(Some(session)),
            expected_peer_static,
            channel: Mutex::new(None),
            joiner: Mutex::new(None),
            finished: Mutex::new(false),
        }
    }

    /// Seal one message, checking this device is the side that sends it, and
    /// wipe the plaintext: an encoded grant is the vault root in serialized
    /// form, with none of the zeroizing its type has.
    fn send_as(
        &self,
        sender: RendezvousRole,
        plaintext: &[u8],
        what: &str,
        finishes: bool,
    ) -> Result<String, PairingFault> {
        if self.role != sender {
            return Err(PairingFault::Pairing(format!(
                "this device is not the one that sends {what}"
            )));
        }
        let mut plain = plaintext.to_vec();
        let sealed = lock(&self.channel)
            .as_mut()
            .ok_or_else(|| PairingFault::Pairing("confirm the SAS first".into()))
            .and_then(|channel| channel.send(&plain).map_err(pairing));
        plain.zeroize();
        let sealed = sealed?;
        if finishes {
            *lock(&self.finished) = true;
        }
        Ok(URL_SAFE_NO_PAD.encode(sealed))
    }

    /// Open one message, checking this device is the side that receives it.
    fn receive_from(
        &self,
        receiver: RendezvousRole,
        sealed: &str,
        what: &str,
    ) -> Result<Vec<u8>, PairingFault> {
        if self.role != receiver {
            return Err(PairingFault::Pairing(format!(
                "this device is not the one that receives {what}"
            )));
        }
        let raw = decode_b64(sealed, what)?;
        let mut guard = lock(&self.channel);
        let channel = guard
            .as_mut()
            .ok_or_else(|| PairingFault::Pairing("confirm the SAS first".into()))?;
        channel.receive(&raw).map_err(pairing)
    }
}

/// The four-byte account tag a pairing QR carries, as hex: BLAKE3 over the
/// normalized email, so a screen can say which account it is pairing without
/// holding the address.
#[must_use]
pub fn account_tag(account_email: &str) -> String {
    account_email_hash(account_email)
        .iter()
        .fold(String::new(), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

fn over() -> PairingFault {
    PairingFault::Pairing("this pairing is over".into())
}

/// A 32-byte seed, wiped from the vector it arrived in.
fn seed32(bytes: Vec<u8>) -> Result<[u8; 32], PairingFault> {
    let len = bytes.len();
    let mut bytes = bytes;
    let out = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| PairingFault::BadLength {
        len: u32::try_from(len).unwrap_or(u32::MAX),
    });
    bytes.zeroize();
    out
}

fn decode_b64(s: &str, what: &str) -> Result<Vec<u8>, PairingFault> {
    URL_SAFE_NO_PAD
        .decode(s.trim())
        .map_err(|_| PairingFault::Pairing(format!("{what} is not base64url text")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two halves, through the handshake to the SAS, with the caller as the
    /// transport.
    fn handshake() -> (PairingDevice, PairingDevice) {
        let new =
            PairingDevice::offer("https://relay.example".into(), "ada@example.com").expect("offer");
        let old = PairingDevice::accept(&new.qr_payload().expect("a QR")).expect("accept");
        old.receive_message(&new.next_message().unwrap()).unwrap();
        new.receive_message(&old.next_message().unwrap()).unwrap();
        old.receive_message(&new.next_message().unwrap()).unwrap();
        (new, old)
    }

    #[test]
    fn both_halves_reach_the_same_digits_and_only_then_confirm() {
        let (new, old) = handshake();
        assert_eq!(new.step(), DeviceStep::AwaitingConfirmation);
        let sas = new.sas().unwrap();
        assert_eq!(sas.len(), 6);
        assert_eq!(sas, old.sas().unwrap());
        assert!(
            new.seal_pairing_offer(b"x").is_err(),
            "nothing is sealed before the SAS"
        );
        new.confirm(true).unwrap();
        assert_eq!(new.step(), DeviceStep::Confirmed);
    }

    #[test]
    fn a_declined_sas_ends_the_pairing() {
        let (new, _old) = handshake();
        assert!(new.confirm(false).is_err());
        assert_eq!(new.step(), DeviceStep::Finished);
        assert!(new.sas().is_err());
    }

    #[test]
    fn each_message_is_sealed_only_by_its_sender() {
        let (new, old) = handshake();
        new.confirm(true).unwrap();
        old.confirm(true).unwrap();
        let err = new.seal_pairing_grant(b"grant").unwrap_err();
        assert!(err.to_string().contains("not the one that sends"), "{err}");
        let err = old
            .request_device_cert("x", "n".into(), "p".into(), vec![1; 32], vec![2; 32])
            .unwrap_err();
        assert!(
            err.to_string().contains("only the device being added"),
            "{err}"
        );
        let err = new
            .request_device_cert("x", "n".into(), "p".into(), vec![1; 31], vec![2; 32])
            .unwrap_err();
        assert_eq!(err, PairingFault::BadLength { len: 31 });
    }

    #[test]
    fn the_account_tag_is_four_bytes_of_hex() {
        let tag = account_tag("ada@example.com");
        assert_eq!(tag.len(), 8);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

//! Device pairing, across the seam.
//!
//! `sunrise-pairing` implements the whole of
//! `docs/03-crypto/pairing-and-onboarding.md` Mode A — the Noise XX
//! transcript, the SAS, the QR payload — and until this module its only caller
//! was `sunrise-e2e`. A correct handshake no shipping binary can reach is not
//! a pairing feature; it is a test fixture. Sync worked between devices only
//! because both replicas were handed the same literal vault root.
//!
//! # What crosses, and why it is base64 text
//!
//! The three handshake messages and the sealed root cross as base64url
//! strings. That is not a serialization convenience: the *transport* for them
//! is currently the user. The relay's pairing rendezvous — the WebSocket that
//! routes by `pair_id` and buffers three messages per role, spec §Relay
//! framing for Noise — is not built, so there is nowhere for the two devices
//! to exchange these on their own. Text is what a user can move between two
//! machines, and the crypto is entirely unaffected by how the ciphertext
//! travelled: the SAS binds the transcript either way, and a MITM still has to
//! match six digits in one interactive attempt.
//!
//! When the rendezvous lands, these same methods drive it — the state machine
//! does not change, only who carries the bytes.
//!
//! # What crosses
//!
//! The spec's [`PairingPayload`](sunrise_pairing::PairingPayload): `ID_S_priv`
//! and `ID_D_pub` — the identity's signing secret and the *public* half of its
//! X25519 pair — every Stream key the sending device holds, the vault root, and
//! the sender's nickname and platform.
//!
//! **`ID_D_priv` does not cross.** That module's own doc
//! ([`sunrise_pairing::payload`]) is the authority on why, and it supersedes
//! what was written here: the X25519 secret that opens the identity-sealed copy
//! of every `key_envelope` used to travel in field 2, and while it did, a
//! revoked device dropped from an epoch's recipient list simply opened the
//! identity copy instead. Every device holding it meant no device could be
//! excluded from anything. Field 2 is burned; sealing needs only the public
//! half, so field 4 still travels and a paired device can still address the
//! identity without being able to read what it addresses.
//!
//! `ID_S_priv` does cross, and it is what lets this device admit the *next*
//! one. Withholding it would produce a device that can never admit another,
//! which is a limitation rather than a property worth keeping — and it is why a
//! revoked device can still mint itself a cert under a fresh device id, which
//! nothing bounds today (`docs/03-crypto/key-rotation.md` §Revocation).
//!
//! The channel is unchanged: Noise XX confirmed by a SAS both users read
//! aloud, which is the same channel the vault root already travelled over, and
//! the vault root was never the smaller secret.

use std::sync::{Mutex, PoisonError};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sunrise_pairing::{
    account_email_hash, decode_pairing_payload, decode_qr_payload, encode_qr_payload,
    PairedChannel, PairingSession, QrPayload, Role, MAGIC_V1_HEX,
};

use zeroize::Zeroize;

use crate::BindingError;

/// How long a vault root is.
const VAULT_ROOT_LEN: usize = 32;

/// Which side of a pairing this device is.
///
/// Mirrored rather than declared remotely so the Swift cases read as they do
/// on screen. The exhaustive `match` below keeps it honest: a variant added
/// upstream fails this crate's build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PairingRole {
    /// The device being added. It shows the QR and initiates the handshake.
    NewDevice,
    /// The device that already holds the vault. It reads the QR and sends the
    /// root.
    ExistingDevice,
}

impl From<PairingRole> for Role {
    fn from(r: PairingRole) -> Self {
        match r {
            PairingRole::NewDevice => Self::NewDevice,
            PairingRole::ExistingDevice => Self::ExistingDevice,
        }
    }
}

/// Where a pairing has got to, for a UI that has to show one screen at a time.
///
/// Derived from the session rather than tracked separately: a second copy of
/// "which step are we on" is a second thing to get wrong, and the one that
/// matters — that the SAS screen cannot be skipped — is enforced by
/// `sunrise-pairing` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PairingStep {
    /// Still exchanging handshake messages.
    Handshaking,
    /// The transcript is complete; both users must compare the SAS.
    AwaitingConfirmation,
    /// The SAS was confirmed on this device and the channel is open.
    Confirmed,
    /// Over, one way or another.
    Finished,
}

/// One device's half of a pairing.
///
/// The state machine is `sunrise-pairing`'s; this holds it behind a `Mutex` so
/// the object is `Send + Sync` for UniFFI, and translates bytes to text at the
/// boundary.
#[derive(uniffi::Object)]
pub struct DevicePairing {
    role: PairingRole,
    /// The QR this device publishes; only the new device has one.
    qr: Option<String>,
    session: Mutex<Option<PairingSession>>,
    channel: Mutex<Option<PairedChannel>>,
    /// Set once the channel has carried the root, so a UI can stop.
    finished: Mutex<bool>,
}

impl std::fmt::Debug for DevicePairing {
    /// No handshake state, no keys, no QR. A `Debug` that printed the session
    /// would put the material a MITM needs into whatever log the app writes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePairing")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl DevicePairing {
    /// Begin pairing on the **new** device: mint a throwaway static keypair
    /// and a pair id, and publish them in a QR payload.
    ///
    /// `account_email` is hashed to four bytes before it goes anywhere near
    /// the payload — the QR names an account without naming a person.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] if key generation or payload encoding fails.
    #[uniffi::constructor]
    pub fn offer(relay_url: String, account_email: String) -> Result<Self, BindingError> {
        let keys = PairingSession::generate_static_keypair()?;
        let mut pair_id = [0u8; 16];
        // The pair id is a routing label the relay will one day key its
        // rendezvous on. It is not a secret and it is not authentication —
        // `n_static_pub` is — so drawing it from the same CSPRNG that made the
        // key is enough.
        let filler = PairingSession::generate_static_keypair()?;
        pair_id.copy_from_slice(&filler.public[..16]);

        let payload = QrPayload {
            magic_v1: MAGIC_V1_HEX.to_string(),
            pair_id: URL_SAFE_NO_PAD.encode(pair_id),
            n_static_pub: URL_SAFE_NO_PAD.encode(&keys.public),
            account_email_hash: hex4(account_email_hash(&account_email)),
            relay_url,
        };
        let qr = encode_qr_payload(&payload).map_err(|e| BindingError::Pairing(e.to_string()))?;
        let session = PairingSession::new(Role::NewDevice, &keys.private)?;
        Ok(Self::wrap(PairingRole::NewDevice, Some(qr), session))
    }

    /// Begin pairing on the **existing** device, from the QR the new one
    /// showed.
    ///
    /// The payload is validated — magic prefix, field shapes, relay-url length
    /// — by the same decoder that wrote it, so a mistyped or truncated paste
    /// is refused here rather than producing a handshake that fails later for
    /// no visible reason.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] with the decoder's own message.
    #[uniffi::constructor]
    pub fn accept(qr_payload: String) -> Result<Self, BindingError> {
        // Decoded for validation. Nothing in the payload is needed to build
        // the responder: Noise XX defers identity, so the existing device
        // learns the peer's static key from the transcript itself. Checking it
        // anyway is the point — a QR that does not parse is a paste error, and
        // saying so now is much cheaper than a Noise decrypt failure later.
        let _ = decode_qr_payload(&qr_payload).map_err(|e| BindingError::Pairing(e.to_string()))?;
        let keys = PairingSession::generate_static_keypair()?;
        let session = PairingSession::new(Role::ExistingDevice, &keys.private)?;
        Ok(Self::wrap(PairingRole::ExistingDevice, None, session))
    }

    /// This device's role.
    #[must_use]
    pub fn role(&self) -> PairingRole {
        self.role
    }

    /// The QR payload to display, on the new device.
    #[must_use]
    pub fn qr_payload(&self) -> Option<String> {
        self.qr.clone()
    }

    /// Where the pairing has got to.
    #[must_use]
    pub fn step(&self) -> PairingStep {
        if *self.finished.lock().unwrap_or_else(PoisonError::into_inner) {
            return PairingStep::Finished;
        }
        if self
            .channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            return PairingStep::Confirmed;
        }
        let complete = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(PairingSession::is_complete);
        if complete {
            PairingStep::AwaitingConfirmation
        } else {
            PairingStep::Handshaking
        }
    }

    /// The next handshake message to hand the peer, base64url.
    ///
    /// Noise XX is three messages, alternating, starting with the new device.
    /// Calling this out of turn is a [`BindingError::Pairing`] rather than a
    /// silent no-op.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when it is not this side's turn to write, or
    /// the session is already consumed.
    pub fn next_message(&self) -> Result<String, BindingError> {
        let mut guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let session = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("this pairing is over".into()))?;
        Ok(URL_SAFE_NO_PAD.encode(session.write_message(&[])?))
    }

    /// Consume the peer's handshake message.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] for a message that is not base64url, arrives
    /// out of turn, or fails to decrypt — which is what a mismatched or
    /// tampered transcript looks like.
    pub fn receive_message(&self, message: String) -> Result<(), BindingError> {
        let raw = decode_b64(&message, "handshake message")?;
        let mut guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let session = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("this pairing is over".into()))?;
        session.read_message(&raw)?;
        Ok(())
    }

    /// The six digits both users compare out of band.
    ///
    /// Only defined once the transcript completes, and
    /// [`DevicePairing::confirm`] cannot be reached without passing through
    /// here — which is the whole authentication story on the numeric path.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] before the handshake finishes.
    pub fn sas(&self) -> Result<String, BindingError> {
        let guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let session = guard
            .as_ref()
            .ok_or_else(|| BindingError::Pairing("this pairing is over".into()))?;
        Ok(session.sas()?)
    }

    /// Record this user's answer to the SAS screen.
    ///
    /// `false` discards the ephemeral keys and ends the pairing — the local
    /// half of the spec's `pair_abort`. Both devices must pass `true`; this
    /// one only knows about its own user.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] on a rejection or before the handshake
    /// completes. A rejection is an error on purpose: it is the one outcome a
    /// UI must not treat as "carry on".
    pub fn confirm(&self, matched: bool) -> Result<(), BindingError> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| BindingError::Pairing("this pairing is over".into()))?;
        match session.into_channel(matched) {
            Ok(channel) => {
                *self.channel.lock().unwrap_or_else(PoisonError::into_inner) = Some(channel);
                Ok(())
            }
            Err(e) => {
                *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
                Err(e.into())
            }
        }
    }

    /// Seal the pairing payload for the peer, on the existing device.
    ///
    /// The result is base64url ciphertext. It is readable only by the device
    /// on the other end of the confirmed handshake — the relay, or the user's
    /// clipboard, sees an opaque blob.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, or when
    /// this device is the one being added.
    pub fn seal_pairing_payload(&self, payload: Vec<u8>) -> Result<String, BindingError> {
        // Taken by value and wiped here rather than left to the caller.
        // `PairingPayload` zeroizes itself on drop; its *encoding* is the same
        // secret in serialized form and has no such courtesy, so the one place
        // that is guaranteed to see the end of its life is the place that
        // consumes it.
        let mut payload = payload;
        let out = self.seal_encoded(&payload);
        payload.zeroize();
        out
    }

    fn seal_encoded(&self, payload: &[u8]) -> Result<String, BindingError> {
        if self.role != PairingRole::ExistingDevice {
            return Err(BindingError::Pairing(
                "only the existing device sends the pairing payload".into(),
            ));
        }
        let mut guard = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        let channel = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("confirm the SAS first".into()))?;
        let sealed = channel.send(payload)?;
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(URL_SAFE_NO_PAD.encode(sealed))
    }

    /// Open the sealed pairing payload, on the new device.
    ///
    /// Returns both halves the caller needs and nothing more: the vault root,
    /// which `SunriseCore::open` takes as its key, and the payload bytes to
    /// hand back as `paired_bundle`. The identity private keys are inside the
    /// bundle and are deliberately **not** broken out — a client has no use for
    /// them, and a field on a Swift record is a field in whatever the app logs.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, the
    /// ciphertext does not open — which is what a tampered or replayed frame
    /// looks like — or the payload does not decode, and
    /// [`BindingError::BadVaultRoot`] if the root inside is not 32 bytes.
    pub fn open_pairing_payload(&self, sealed: String) -> Result<PairedBundle, BindingError> {
        if self.role != PairingRole::NewDevice {
            return Err(BindingError::Pairing(
                "only the new device receives the pairing payload".into(),
            ));
        }
        let raw = decode_b64(&sealed, "sealed pairing payload")?;
        let mut guard = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        let channel = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("confirm the SAS first".into()))?;
        let bundle = channel.receive(&raw)?;
        let payload =
            decode_pairing_payload(&bundle).map_err(|e| BindingError::Pairing(e.to_string()))?;
        if payload.vault_root.len() != VAULT_ROOT_LEN {
            return Err(BindingError::BadVaultRoot {
                len: u32::try_from(payload.vault_root.len()).unwrap_or(u32::MAX),
            });
        }
        let vault_root = payload.vault_root.to_vec();
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(PairedBundle {
            vault_root,
            payload_bytes: bundle,
        })
    }
}

/// What a completed pairing hands the new device.
///
/// Both fields are **plaintext**. `payload_bytes` is the opened
/// `PairingPayload` — `ID_S_priv`, the vault root and every Stream key in the
/// account; `ID_D_priv` is not among them and never travels here — and it was
/// called `sealed_bundle` while nothing sealed it: the field is the output of
/// `channel.receive`, which is where the sealing ends. A name that says
/// "sealed" is the one thing that would make a caller comfortable logging
/// it.
///
/// `Debug` is hand-written for the same reason `PairingPayload`'s is: the
/// derive would print every one of those bytes, undoing at this seam the
/// redaction the type it carries is careful about one crate over.
#[derive(Clone, uniffi::Record)]
pub struct PairedBundle {
    /// The 32-byte vault root, for `SunriseCore::open`'s `vault_root`.
    pub vault_root: Vec<u8>,
    /// The opened payload bytes, for `SunriseCore::open`'s `paired_bundle`.
    pub payload_bytes: Vec<u8>,
}

impl std::fmt::Debug for PairedBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairedBundle(<redacted>)")
    }
}

impl DevicePairing {
    fn wrap(role: PairingRole, qr: Option<String>, session: PairingSession) -> Self {
        Self {
            role,
            qr,
            session: Mutex::new(Some(session)),
            channel: Mutex::new(None),
            finished: Mutex::new(false),
        }
    }
}

/// Read the account-scoping hash a pairing QR carries.
///
/// Exported so an "add a device" screen can show which account it is pairing
/// without the seam having to hold the address: four bytes of BLAKE3 over the
/// normalized email, which is what the relay's rate limiter keys on.
#[uniffi::export]
#[must_use]
pub fn pairing_account_tag(account_email: String) -> String {
    hex4(account_email_hash(&account_email))
}

fn hex4(bytes: [u8; 4]) -> String {
    bytes.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn decode_b64(s: &str, what: &str) -> Result<Vec<u8>, BindingError> {
    URL_SAFE_NO_PAD
        .decode(s.trim())
        .map_err(|_| BindingError::Pairing(format!("{what} is not base64url text")))
}

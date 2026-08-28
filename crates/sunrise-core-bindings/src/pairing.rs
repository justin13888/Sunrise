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
//! # What is not carried yet
//!
//! The spec's `PairingPayload` is a CBOR map of the identity keys, the
//! per-stream key tables, a nickname and a platform. What moves here is the
//! **32-byte vault root**, which is what `Core::open` needs and what
//! `Core::export_vault_root_for_pairing` produces: every per-stream key is
//! derived from it, so the receiving device reconstructs the same key schedule.
//! The identity private keys are not sent, so the new device signs with its own
//! device key and each side still has to accept the other's certificate via
//! `Command::TrustDevice`. That is a real limitation and it is deliberate —
//! sending a private identity key is not something to do speculatively.

use std::sync::{Mutex, PoisonError};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sunrise_pairing::{
    account_email_hash, decode_qr_payload, encode_qr_payload, PairedChannel, PairingSession,
    QrPayload, Role, MAGIC_V1_HEX,
};

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

    /// Seal the vault root for the peer, on the existing device.
    ///
    /// The result is base64url ciphertext. It is readable only by the device
    /// on the other end of the confirmed handshake — the relay, or the user's
    /// clipboard, sees an opaque blob.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the root is not 32 bytes, when the SAS
    /// has not been confirmed, or when this device is the one being added.
    pub fn seal_vault_root(&self, vault_root: Vec<u8>) -> Result<String, BindingError> {
        if vault_root.len() != VAULT_ROOT_LEN {
            return Err(BindingError::BadVaultRoot {
                len: u32::try_from(vault_root.len()).unwrap_or(u32::MAX),
            });
        }
        if self.role != PairingRole::ExistingDevice {
            return Err(BindingError::Pairing(
                "only the existing device sends the vault root".into(),
            ));
        }
        let mut guard = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        let channel = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("confirm the SAS first".into()))?;
        let sealed = channel.send(&vault_root)?;
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(URL_SAFE_NO_PAD.encode(sealed))
    }

    /// Open the sealed vault root, on the new device.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed or the
    /// ciphertext does not open — which is what a tampered or replayed frame
    /// looks like — and [`BindingError::BadVaultRoot`] if what came out is not
    /// 32 bytes.
    pub fn open_vault_root(&self, sealed: String) -> Result<Vec<u8>, BindingError> {
        if self.role != PairingRole::NewDevice {
            return Err(BindingError::Pairing(
                "only the new device receives the vault root".into(),
            ));
        }
        let raw = decode_b64(&sealed, "sealed vault root")?;
        let mut guard = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        let channel = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("confirm the SAS first".into()))?;
        let root = channel.receive(&raw)?;
        if root.len() != VAULT_ROOT_LEN {
            return Err(BindingError::BadVaultRoot {
                len: u32::try_from(root.len()).unwrap_or(u32::MAX),
            });
        }
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(root)
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

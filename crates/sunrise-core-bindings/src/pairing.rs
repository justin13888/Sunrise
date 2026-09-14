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
//! The three handshake messages and the three pairing messages cross as
//! base64url strings. That is not a serialization convenience: the *transport*
//! for them is currently the user. The relay's pairing rendezvous — the
//! WebSocket that routes by `pair_id` and buffers three messages per role, spec
//! §Relay framing for Noise — is not built, so there is nowhere for the two
//! devices to exchange these on their own. Text is what a user can move between
//! two machines, and the crypto is entirely unaffected by how the ciphertext
//! travelled: the SAS binds the transcript either way, and a MITM still has to
//! match six digits in one interactive attempt.
//!
//! When the rendezvous lands, these same methods drive it — the state machine
//! does not change, only who carries the bytes.
//!
//! # What crosses
//!
//! Three messages after the SAS, not one, and the direction alternates:
//!
//! 1. [`DevicePairing::seal_pairing_offer`] — the sponsor's account identity,
//!    `ID_S_pub` and `ID_D_pub` and the genesis anchor. **No secret.**
//! 2. [`DevicePairing::request_device_cert`] — the joiner mints `D_S`/`D_D`
//!    here and sends the public halves. The secrets stay in this object until
//!    step 3.
//! 3. [`DevicePairing::seal_pairing_grant`] / [`DevicePairing::open_pairing_grant`]
//!    — the `DeviceCert` the sponsor issued, the vault root, and every Stream
//!    key.
//!
//! **Neither identity private key crosses.** `ID_D_priv` stopped travelling in
//! #86, which closed #76: the X25519 secret that opens the identity-sealed copy
//! of every `key_envelope` used to travel, and while it did, a revoked device
//! dropped from an epoch's recipient list simply opened the identity copy
//! instead. `ID_S_priv` stopped travelling in #105: a `DeviceCert` names only
//! its subject and carries only the identity's signature, so a device holding
//! the key minted a valid cert for any device id it invented and rejoined after
//! being revoked.
//!
//! Withholding `ID_S_priv` is what costs a round trip — the sponsor cannot sign
//! a cert for keys the joiner has not minted yet — and it is why the seam grew a
//! step rather than shrinking the payload. Its consequence is real and is the
//! point: a device added this way can never sponsor another one, and
//! [`SunriseCore::can_sponsor_pairing`](crate::SunriseCore::can_sponsor_pairing)
//! is how a client asks before offering the button. `sunrise_pairing::protocol`
//! is the authority on all of it.
//!
//! The channel is unchanged: Noise XX confirmed by a SAS both users read
//! aloud, which is the same channel the vault root already travelled over, and
//! the vault root was never the smaller secret.

use std::sync::{Mutex, PoisonError};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sunrise_pairing::{
    account_email_hash, decode_pairing_grant, decode_pairing_offer, decode_pairing_request,
    decode_qr_payload, encode_pairing_payload, encode_qr_payload, PairedChannel, PairingJoiner,
    PairingSession, QrPayload, Role, MAGIC_V1_HEX,
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
    /// The `n_static_pub` this device scanned, on the existing device.
    ///
    /// `None` on the new device, which published the QR rather than reading
    /// one and so has nothing to compare the peer against. On the existing
    /// device it is the QR path's authentication, and
    /// [`DevicePairing::receive_message`] is where it is spent.
    expected_peer_static: Option<Vec<u8>>,
    channel: Mutex<Option<PairedChannel>>,
    /// The joiner's minted device keys, between message 2 and message 3.
    ///
    /// The state a one-shot pairing did not need. `D_S_priv` and `D_D_priv` are
    /// minted in [`DevicePairing::request_device_cert`] and consumed in
    /// [`DevicePairing::open_pairing_grant`], and they never leave this object:
    /// only the public halves went into the request, and a sponsor that minted
    /// them instead would hold permanent impersonation of this device.
    ///
    /// `None` on the sponsoring device, which mints nothing.
    joiner: Mutex<Option<PairingJoiner>>,
    /// Set once the channel has carried the grant, so a UI can stop.
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
        Ok(Self::wrap(PairingRole::NewDevice, Some(qr), session, None))
    }

    /// Begin pairing on the **existing** device, from the QR the new one
    /// showed.
    ///
    /// The payload is validated — magic prefix, field shapes, relay-url length
    /// — by the same decoder that wrote it, so a mistyped or truncated paste
    /// is refused here rather than producing a handshake that fails later for
    /// no visible reason.
    ///
    /// `n_static_pub` is **kept**, not merely validated. Noise XX defers
    /// identity: the responder learns the initiator's static key from the third
    /// message and has no opinion about which key that should have been, so the
    /// QR value binds the handshake only if something compares the two.
    /// [`DevicePairing::receive_message`] is what does, the moment the
    /// transcript completes.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] with the decoder's own message.
    #[uniffi::constructor]
    pub fn accept(qr_payload: String) -> Result<Self, BindingError> {
        let scanned =
            decode_qr_payload(&qr_payload).map_err(|e| BindingError::Pairing(e.to_string()))?;
        let expected = decode_b64(&scanned.n_static_pub, "n_static_pub")?;
        let keys = PairingSession::generate_static_keypair()?;
        let session = PairingSession::new(Role::ExistingDevice, &keys.private)?;
        Ok(Self::wrap(
            PairingRole::ExistingDevice,
            None,
            session,
            Some(expected),
        ))
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
    /// On the existing device, the message that completes the transcript is
    /// also where the QR is spent: the peer's Noise static key has arrived by
    /// then, and it must be the one the QR carried. A mismatch ends the pairing
    /// here rather than handing the user a SAS screen — an attacker who has
    /// substituted its own static has only six digits left to guess, and there
    /// is no reason to make it play for them when the out-of-band value already
    /// says it is the wrong device.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] for a message that is not base64url, arrives
    /// out of turn, or fails to decrypt — which is what a mismatched or
    /// tampered transcript looks like — and for a completed transcript whose
    /// peer static is not the QR's `n_static_pub`.
    pub fn receive_message(&self, message: String) -> Result<(), BindingError> {
        let raw = decode_b64(&message, "handshake message")?;
        let mut guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let session = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("this pairing is over".into()))?;
        session.read_message(&raw)?;
        if let Some(expected) = self.expected_peer_static.as_deref() {
            if session.is_complete() && session.peer_static_key().as_deref() != Some(expected) {
                // Drop the handshake state with it: there is no recovering
                // from this, and a session left in the map is one a UI could
                // still call `sas()` on.
                *guard = None;
                *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
                return Err(BindingError::Pairing(
                    "the peer's Noise static key is not the one the QR published".into(),
                ));
            }
        }
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

    /// Seal message 1 — the offer — for the joiner, on the sponsoring device.
    ///
    /// The result is base64url ciphertext, readable only by the device on the
    /// other end of the confirmed handshake. It carries no secret at all, and
    /// is still sealed: what the channel protects here is the *binding*, not
    /// the contents. A joiner that accepted an offer from somebody else would
    /// mint keys for a stranger's account and wait forever for a grant.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, or when
    /// this device is the one being added.
    pub fn seal_pairing_offer(&self, offer: Vec<u8>) -> Result<String, BindingError> {
        self.send_as(PairingRole::ExistingDevice, &offer, "the pairing offer", false)
    }

    /// Mint this device's keys and seal message 2 — the cert request — on the
    /// joiner.
    ///
    /// `sealed_offer` is what the sponsor's [`Self::seal_pairing_offer`]
    /// produced. `nickname` and `platform` are what this device wants to be
    /// called; the sponsor writes them into the cert, so they are what every
    /// peer will show for it.
    ///
    /// `seed_s` and `seed_d` are 32 bytes each, and **must be unpredictable**:
    /// `D_S_priv` is derived from the first and every op this device ever
    /// writes is signed under it. A Swift caller draws them from
    /// `SecRandomCopyBytes`. They are a parameter rather than drawn here so
    /// that the one source of randomness a client uses is the platform's and
    /// not a second one buried in a seam.
    ///
    /// The secrets stay inside this object until [`Self::open_pairing_grant`].
    /// They do not cross back to Swift, and the request does not carry them:
    /// only the public halves are sent, which is what separates this from a
    /// sponsor-mints-the-keys design where the sponsor would keep the ability
    /// to impersonate this device forever.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, when this
    /// device is the one holding the vault, when the offer does not open or
    /// does not decode, and [`BindingError::BadVaultRoot`] — reused for its
    /// length field — if either seed is not 32 bytes.
    pub fn request_device_cert(
        &self,
        sealed_offer: String,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<String, BindingError> {
        if self.role != PairingRole::NewDevice {
            return Err(BindingError::Pairing(
                "only the device being added asks for a cert".into(),
            ));
        }
        let seed_s = seed32(seed_s)?;
        let seed_d = seed32(seed_d)?;
        let plain = self.receive_from(PairingRole::NewDevice, &sealed_offer, "the pairing offer")?;
        let offer =
            decode_pairing_offer(&plain).map_err(|e| BindingError::Pairing(e.to_string()))?;
        let joiner = PairingJoiner::new(offer, nickname, platform, seed_s, seed_d);
        let request = joiner
            .request()
            .encode()
            .map_err(|e| BindingError::Pairing(e.to_string()))?;
        *self.joiner.lock().unwrap_or_else(PoisonError::into_inner) = Some(joiner);
        self.send_as(PairingRole::NewDevice, &request, "the cert request", false)
    }

    /// Open message 2 on the sponsor, returning the request for the core to
    /// answer.
    ///
    /// The bytes come back rather than the parsed struct, and go straight into
    /// [`SunriseCore::issue_pairing_grant`](crate::SunriseCore::issue_pairing_grant).
    /// The seam does not hold an open vault and the vault does not hold a Noise
    /// channel, so one of them has to hand the other an opaque blob; making it
    /// the *request* — which carries no secret — rather than the grant is the
    /// side of that trade with nothing to spill.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, when this
    /// device is the one being added, or when the ciphertext does not open —
    /// which is what a tampered or replayed frame looks like.
    pub fn open_cert_request(&self, sealed: String) -> Result<Vec<u8>, BindingError> {
        let plain =
            self.receive_from(PairingRole::ExistingDevice, &sealed, "the cert request")?;
        // Decoded and thrown away: the point is to refuse a malformed or
        // self-named request here, where the error reaches the sponsor's screen,
        // rather than inside the core where it would surface as a failed grant.
        decode_pairing_request(&plain).map_err(|e| BindingError::Pairing(e.to_string()))?;
        Ok(plain)
    }

    /// Seal message 3 — the grant — for the joiner, on the sponsor.
    ///
    /// This is the one message in the exchange that carries the account: the
    /// issued `DeviceCert`, the vault root and every Stream key. It ends the
    /// pairing on this side.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, or when
    /// this device is the one being added.
    pub fn seal_pairing_grant(&self, grant: Vec<u8>) -> Result<String, BindingError> {
        self.send_as(PairingRole::ExistingDevice, &grant, "the pairing grant", true)
    }

    /// Open message 3 on the joiner and assemble what `SunriseCore::open`
    /// takes.
    ///
    /// Returns both halves the caller needs and nothing more: the vault root,
    /// which `SunriseCore::open` takes as its key, and the payload bytes to
    /// hand back as `paired_bundle`. This device's own `D_S_priv`/`D_D_priv`
    /// are inside the bundle and are deliberately **not** broken out — a client
    /// has no use for them, and a field on a Swift record is a field in
    /// whatever the app logs.
    ///
    /// The cert is checked here, against the offer this object has been holding
    /// since [`Self::request_device_cert`]: it must verify under the `ID_S_pub`
    /// that offer named, and must name the keys this device minted. A grant
    /// that fails either is refused now rather than becoming a vault whose
    /// every op its peers reject.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, when
    /// [`Self::request_device_cert`] has not run, when the ciphertext does not
    /// open, or when the grant does not decode or its cert is not one this
    /// device can use, and [`BindingError::BadVaultRoot`] if the root inside is
    /// not 32 bytes.
    pub fn open_pairing_grant(&self, sealed: String) -> Result<PairedBundle, BindingError> {
        let plain = self.receive_from(PairingRole::NewDevice, &sealed, "the pairing grant")?;
        let grant = decode_pairing_grant(&plain).map_err(|e| BindingError::Pairing(e.to_string()))?;
        let joiner = self
            .joiner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                BindingError::Pairing("ask for a cert before trying to accept one".into())
            })?;
        let payload = joiner
            .accept(grant)
            .map_err(|e| BindingError::Pairing(e.to_string()))?;
        if payload.vault_root.len() != VAULT_ROOT_LEN {
            return Err(BindingError::BadVaultRoot {
                len: u32::try_from(payload.vault_root.len()).unwrap_or(u32::MAX),
            });
        }
        let vault_root = payload.vault_root.to_vec();
        let payload_bytes =
            encode_pairing_payload(&payload).map_err(|e| BindingError::Pairing(e.to_string()))?;
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(PairedBundle {
            vault_root,
            payload_bytes,
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
    fn wrap(
        role: PairingRole,
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

    /// Seal one pairing message, checking this device is the side that sends
    /// it.
    ///
    /// Six public methods would otherwise each repeat the role check, the lock,
    /// the "confirm the SAS first" error and the base64 — and the role check is
    /// the one that must not be forgotten: a UI that called a sponsor method on
    /// a joiner would produce a message the peer cannot make sense of, several
    /// legs after the mistake.
    ///
    /// `plaintext` is wiped after sealing. `PairingPayload` and `PairingGrant`
    /// zeroize themselves on drop; their *encodings* are the same secrets in
    /// serialized form and have no such courtesy, so the place that consumes
    /// one is the place that has to end its life.
    fn send_as(
        &self,
        sender: PairingRole,
        plaintext: &[u8],
        what: &str,
        finishes: bool,
    ) -> Result<String, BindingError> {
        if self.role != sender {
            return Err(BindingError::Pairing(format!(
                "this device is not the one that sends {what}"
            )));
        }
        let mut plain = plaintext.to_vec();
        let mut guard = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        let sealed = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("confirm the SAS first".into()))
            .and_then(|channel| Ok(channel.send(&plain)?));
        plain.zeroize();
        let sealed = sealed?;
        if finishes {
            *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        }
        Ok(URL_SAFE_NO_PAD.encode(sealed))
    }

    /// Open one pairing message, checking this device is the side that receives
    /// it.
    fn receive_from(
        &self,
        receiver: PairingRole,
        sealed: &str,
        what: &str,
    ) -> Result<Vec<u8>, BindingError> {
        if self.role != receiver {
            return Err(BindingError::Pairing(format!(
                "this device is not the one that receives {what}"
            )));
        }
        let raw = decode_b64(sealed, what)?;
        let mut guard = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        let channel = guard
            .as_mut()
            .ok_or_else(|| BindingError::Pairing("confirm the SAS first".into()))?;
        Ok(channel.receive(&raw)?)
    }
}

/// Read a 32-byte seed off the seam.
///
/// [`BindingError::BadVaultRoot`] is reused rather than a new variant: it is
/// already "a 32-byte key came across at the wrong length", it carries the
/// length, and a second error case meaning the same thing is a second string
/// for a Swift `switch` to miss.
fn seed32(bytes: Vec<u8>) -> Result<[u8; 32], BindingError> {
    let len = bytes.len();
    let mut bytes = bytes;
    let out = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| BindingError::BadVaultRoot {
        len: u32::try_from(len).unwrap_or(u32::MAX),
    });
    bytes.zeroize();
    out
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

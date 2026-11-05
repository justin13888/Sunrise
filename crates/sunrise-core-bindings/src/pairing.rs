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
//! base64url strings, because this object does not care who carries them.
//! Normally that is the relay: [`crate::pairing_relay::RelayPairing`] wraps one
//! of these and moves each string through the relay's pairing rendezvous
//! (`docs/06-server/api.md` §Pairing rendezvous), in the order below. Where the
//! relay cannot be reached, a client can still drive these methods by hand and
//! let the user copy the text between two screens. The crypto is unaffected by
//! how the ciphertext travelled: the SAS binds the transcript either way, and a
//! MITM still has to match six digits in one interactive attempt.
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
//!
//! # Where the protocol object lives
//!
//! In the client layer, as [`sunrise_client_core::pairing::PairingDevice`],
//! which the CLI's `sunrise pair join` and `sunrise pair add` drive too. This
//! type is that one behind UniFFI: the same calls, lowered onto
//! [`BindingError`].

use sunrise_client_core::pairing::{DeviceStep, PairingDevice, PairingFault, RendezvousRole};
use sunrise_pairing::{encode_pairing_payload, PairingPayload, Role};

use crate::BindingError;

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

impl From<RendezvousRole> for PairingRole {
    fn from(r: RendezvousRole) -> Self {
        match r {
            RendezvousRole::NewDevice => Self::NewDevice,
            RendezvousRole::ExistingDevice => Self::ExistingDevice,
        }
    }
}

impl From<DeviceStep> for PairingStep {
    fn from(s: DeviceStep) -> Self {
        match s {
            DeviceStep::Handshaking => Self::Handshaking,
            DeviceStep::AwaitingConfirmation => Self::AwaitingConfirmation,
            DeviceStep::Confirmed => Self::Confirmed,
            DeviceStep::Finished => Self::Finished,
        }
    }
}

/// The client layer's pairing failures, as the variants a Swift `switch`
/// already branches on.
impl From<PairingFault> for BindingError {
    fn from(e: PairingFault) -> Self {
        match e {
            PairingFault::Pairing(why) => Self::Pairing(why),
            // Reused for its length field: "a 32-byte key came across at the
            // wrong length", which a second variant would only restate.
            PairingFault::BadLength { len } => Self::BadVaultRoot { len },
            PairingFault::Vault(why) => Self::Core(why),
            PairingFault::Rendezvous(e) => e.into(),
        }
    }
}

/// One device's half of a pairing.
///
/// The state machine is `sunrise-pairing`'s, held between calls by the client
/// layer's [`PairingDevice`] — `Send + Sync`, so it can sit behind UniFFI —
/// which translates bytes to text at the boundary.
#[derive(uniffi::Object)]
pub struct DevicePairing {
    inner: PairingDevice,
}

impl std::fmt::Debug for DevicePairing {
    /// No handshake state, no keys, no QR. A `Debug` that printed the session
    /// would put the material a MITM needs into whatever log the app writes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePairing")
            .field("role", &self.role())
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
        Ok(Self {
            inner: PairingDevice::offer(relay_url, &account_email)?,
        })
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
        Ok(Self {
            inner: PairingDevice::accept(&qr_payload)?,
        })
    }

    /// This device's role.
    #[must_use]
    pub fn role(&self) -> PairingRole {
        self.inner.role().into()
    }

    /// The QR payload to display, on the new device.
    #[must_use]
    pub fn qr_payload(&self) -> Option<String> {
        self.inner.qr_payload()
    }

    /// Where the pairing has got to.
    #[must_use]
    pub fn step(&self) -> PairingStep {
        self.inner.step().into()
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
        Ok(self.inner.next_message()?)
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
        Ok(self.inner.receive_message(&message)?)
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
        Ok(self.inner.sas()?)
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
        Ok(self.inner.confirm(matched)?)
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
        Ok(self.inner.seal_pairing_offer(&offer)?)
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
        Ok(self
            .inner
            .request_device_cert(&sealed_offer, nickname, platform, seed_s, seed_d)?)
    }

    /// Open message 2 on the sponsor, returning the request for the core to
    /// answer.
    ///
    /// The bytes come back rather than the parsed struct, and go straight into
    /// [`SunriseCore::send_pairing_grant`](crate::SunriseCore::send_pairing_grant),
    /// which is the seam's name for the core's `issue_pairing_grant`.
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
        // Decoded there and thrown away: the point is to refuse a malformed or
        // self-named request here, where the error reaches the sponsor's
        // screen, rather than inside the core as a failed grant.
        Ok(self.inner.open_cert_request(&sealed)?)
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
        // The encoding is the vault root serialized, with none of the
        // zeroizing its type has; the seal wipes its own copy, and this one
        // is the caller's.
        let mut grant = grant;
        let sealed = self.inner.seal_pairing_grant(&grant);
        zeroize::Zeroize::zeroize(&mut grant);
        Ok(sealed?)
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
    /// device can use.
    pub fn open_pairing_grant(&self, sealed: String) -> Result<PairedBundle, BindingError> {
        PairedBundle::try_from(&self.inner.open_pairing_grant(&sealed)?)
    }
}

impl DevicePairing {
    /// The client layer's half this object wraps, for the seam calls that
    /// take one ([`crate::SunriseCore::send_pairing_offer`] and
    /// [`crate::SunriseCore::send_pairing_grant`]).
    pub(crate) const fn inner(&self) -> &PairingDevice {
        &self.inner
    }
}

impl TryFrom<&PairingPayload> for PairedBundle {
    type Error = BindingError;

    /// The two halves `SunriseCore::open` takes: the root, and the payload
    /// encoded for `paired_bundle`.
    fn try_from(payload: &PairingPayload) -> Result<Self, BindingError> {
        let payload_bytes =
            encode_pairing_payload(payload).map_err(|e| BindingError::Pairing(e.to_string()))?;
        Ok(Self {
            vault_root: payload.vault_root.to_vec(),
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

/// Read the account-scoping hash a pairing QR carries.
///
/// Exported so an "add a device" screen can show which account it is pairing
/// without the seam having to hold the address: four bytes of BLAKE3 over the
/// normalized email, which is what the relay's rate limiter keys on.
#[uniffi::export]
#[must_use]
pub fn pairing_account_tag(account_email: String) -> String {
    sunrise_client_core::pairing::account_tag(&account_email)
}

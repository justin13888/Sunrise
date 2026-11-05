//! A pairing whose messages travel through the relay: one [`PairingDevice`]
//! and the [`Rendezvous`] that carries its six messages, in the order the
//! protocol fixes.
//!
//! Nothing about the crypto changes: the relay sees Noise ciphertext, the SAS
//! binds the transcript, and the existing device still refuses a transcript
//! whose peer static is not the scanned QR's `n_static_pub` — before any SAS,
//! inside [`PairingDevice::receive_message`]. The `UniFFI` seam's
//! `RelayPairing` is this type lowered onto `BindingError`; the CLI's
//! `sunrise pair join` and `sunrise pair add` hold it directly.
//!
//! Three phases, as a client sees them:
//!
//! 1. **Code.** The new device calls [`RelayPairing::offer`] and shows
//!    [`RelayPairing::qr_payload`]; the existing device calls
//!    [`RelayPairing::accept`] with the text exactly as scanned.
//! 2. **Compare.** Both call [`RelayPairing::handshake`], which returns the
//!    six digits once the transcript completes.
//! 3. **Done.** On "match", the existing device calls [`RelayPairing::sponsor`]
//!    and the new one [`RelayPairing::join`]; on "don't match", either calls
//!    [`RelayPairing::reject`], which tells the other side through the relay.

use std::sync::Arc;

use sunrise_core::Core;
use sunrise_pairing::{decode_qr_payload, PairingPayload};

use super::device::PairingDevice;
use super::{scanned_pair_id, PairingFault, Rendezvous, RendezvousRole};

/// One device's half of a pairing over the relay. See the module comment.
pub struct RelayPairing {
    device: Arc<PairingDevice>,
    rendezvous: Rendezvous,
}

impl std::fmt::Debug for RelayPairing {
    /// The role only: see [`PairingDevice`]'s `Debug` for why.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayPairing")
            .field("role", &self.device.role())
            .finish_non_exhaustive()
    }
}

impl RelayPairing {
    /// Begin on the **new** device: mint the QR and open the relay session.
    ///
    /// The first handshake message is sent here, before the QR is shown:
    /// Noise XX starts with the new device, and sending it is what opens the
    /// session, so the QR's 300 s lifetime starts while it is on screen.
    /// `bearer` is this account's access token; the session is bound to the
    /// account it names.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] if the QR cannot be built, and
    /// [`PairingFault::Rendezvous`] for a relay URL that is not a relay
    /// origin, a relay refusing the session (at once, with its
    /// `Retry-After`), or one that cannot be reached within the wait.
    pub async fn offer(
        relay_url: &str,
        bearer: &str,
        account_email: &str,
    ) -> Result<Self, PairingFault> {
        let device = Arc::new(PairingDevice::offer(relay_url.to_owned(), account_email)?);
        let qr = device
            .qr_payload()
            .ok_or_else(|| PairingFault::Pairing("the new device has no QR".into()))?;
        let pair_id = decode_qr_payload(&qr)
            .map_err(|e| PairingFault::Pairing(e.to_string()))?
            .pair_id;
        let rendezvous = Rendezvous::new(relay_url, bearer, pair_id, RendezvousRole::NewDevice)?;
        let this = Self { device, rendezvous };
        let first = this.device.next_message()?;
        this.rendezvous.send(first).await?;
        Ok(this)
    }

    /// Begin on the **existing** device, from the code as scanned or pasted.
    ///
    /// `relay_url` is the relay this device is configured for: the bearer goes
    /// there and nowhere else, so a code whose relay is another origin is
    /// refused before any request is made.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Rendezvous`] for text that is not a pairing code, a
    /// relay URL that is not a relay origin, or a code naming another relay;
    /// [`PairingFault::Pairing`] for a code whose key does not decode.
    pub fn accept(qr_payload: &str, relay_url: &str, bearer: &str) -> Result<Self, PairingFault> {
        let pair_id = scanned_pair_id(qr_payload, relay_url)?;
        let device = Arc::new(PairingDevice::accept(qr_payload.trim())?);
        let rendezvous =
            Rendezvous::new(relay_url, bearer, pair_id, RendezvousRole::ExistingDevice)?;
        Ok(Self { device, rendezvous })
    }

    /// The QR payload to display, on the new device.
    #[must_use]
    pub fn qr_payload(&self) -> Option<String> {
        self.device.qr_payload()
    }

    /// This device's side.
    #[must_use]
    pub fn role(&self) -> RendezvousRole {
        self.device.role()
    }

    /// Run the Noise handshake over the relay and return the SAS both users
    /// compare. On the new device this waits for the other to scan the code.
    /// Any failure drops the relay session, so the other device stops waiting
    /// rather than running out its 300 s.
    ///
    /// # Errors
    ///
    /// [`PairingFault::Pairing`] when the transcript fails — on the existing
    /// device, a peer whose static key is not the QR's — and
    /// [`PairingFault::Rendezvous`] when the session is gone or the relay
    /// cannot be reached.
    pub async fn handshake(&self) -> Result<String, PairingFault> {
        let ran = self.run_handshake().await;
        self.abort_on_failure(ran).await
    }

    /// "Match", on the **existing** device: send this account's offer, wait
    /// for the cert request, issue the cert inside `core` and send the grant.
    ///
    /// # Errors
    ///
    /// As [`seal_offer`] and [`seal_grant`], and
    /// [`PairingFault::Rendezvous`] when the new device rejected the SAS or
    /// left.
    pub async fn sponsor(&self, core: &Core) -> Result<(), PairingFault> {
        let ran = self.run_sponsor(core).await;
        self.abort_on_failure(ran).await
    }

    /// "Match", on the **new** device: wait for the offer, mint this device's
    /// keys from `seed_s` and `seed_d` and ask for a cert, then open the
    /// grant into the payload a vault is opened with.
    ///
    /// # Errors
    ///
    /// As [`PairingDevice::request_device_cert`] and
    /// [`PairingDevice::open_pairing_grant`], and
    /// [`PairingFault::Rendezvous`] when the existing device rejected the SAS
    /// or left.
    pub async fn join(
        &self,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<PairingPayload, PairingFault> {
        let ran = self.run_join(nickname, platform, seed_s, seed_d).await;
        self.abort_on_failure(ran).await
    }

    /// "Don't match": discard the keys here and tell the other device through
    /// the relay, which is the spec's `pair_abort`.
    pub async fn reject(&self) {
        let _ = self.device.confirm(false);
        self.rendezvous.abort().await;
    }

    /// Stop at any point: end any wait in progress, discard the keys and drop
    /// the relay session. Idempotent.
    pub async fn cancel(&self) {
        self.rendezvous.cancel();
        let _ = self.device.confirm(false);
        self.rendezvous.abort().await;
    }

    async fn run_handshake(&self) -> Result<String, PairingFault> {
        match self.device.role() {
            RendezvousRole::NewDevice => {
                let second = self.rendezvous.next().await?;
                self.device.receive_message(&second)?;
                let third = self.device.next_message()?;
                self.rendezvous.send(third).await?;
            }
            RendezvousRole::ExistingDevice => {
                let first = self.rendezvous.next().await?;
                self.device.receive_message(&first)?;
                let second = self.device.next_message()?;
                self.rendezvous.send(second).await?;
                let third = self.rendezvous.next().await?;
                self.device.receive_message(&third)?;
            }
        }
        self.device.sas()
    }

    async fn run_sponsor(&self, core: &Core) -> Result<(), PairingFault> {
        self.device.confirm(true)?;
        let offer = seal_offer(core, &self.device)?;
        self.rendezvous.send(offer).await?;
        let request = self.rendezvous.next().await?;
        let grant = seal_grant(core, &self.device, &request)?;
        Ok(self.rendezvous.send(grant).await?)
    }

    async fn run_join(
        &self,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<PairingPayload, PairingFault> {
        self.device.confirm(true)?;
        let offer = self.rendezvous.next().await?;
        let request = self
            .device
            .request_device_cert(&offer, nickname, platform, seed_s, seed_d)?;
        self.rendezvous.send(request).await?;
        let grant = self.rendezvous.next().await?;
        self.device.open_pairing_grant(&grant)
    }

    /// Drop the relay session when a step failed, so the other device learns
    /// at its next poll rather than at the session's expiry.
    async fn abort_on_failure<T>(&self, ran: Result<T, PairingFault>) -> Result<T, PairingFault> {
        if ran.is_err() {
            let _ = self.device.confirm(false);
            self.rendezvous.abort().await;
        }
        ran
    }
}

/// Seal message 1 — this account's identity — into a confirmed pairing, on
/// the device that holds the vault. Nothing secret is in it.
///
/// # Errors
///
/// [`PairingFault::Vault`] when the vault cannot export its identity, and
/// [`PairingFault::Pairing`] before the SAS is confirmed or on the device
/// being added.
pub fn seal_offer(core: &Core, device: &PairingDevice) -> Result<String, PairingFault> {
    let offer = core
        .export_pairing_offer()
        .map_err(|e| PairingFault::Vault(e.to_string()))?;
    let encoded = offer
        .encode()
        .map_err(|e| PairingFault::Pairing(e.to_string()))?;
    device.seal_pairing_offer(&encoded)
}

/// Open the joiner's cert request, issue its `DeviceCert` inside `core` — the
/// only thing holding `ID_S_priv` — and seal message 3, the grant.
///
/// # Errors
///
/// [`PairingFault::Pairing`] before the SAS is confirmed, on the device being
/// added, for a request that does not open or decode, or a grant too large
/// for one transport message; [`PairingFault::Vault`] when this vault holds no
/// `ID_S_priv` or the request answers another account's offer.
pub fn seal_grant(
    core: &Core,
    device: &PairingDevice,
    sealed_request: &str,
) -> Result<String, PairingFault> {
    let raw = device.open_cert_request(sealed_request)?;
    let request = sunrise_pairing::decode_pairing_request(&raw)
        .map_err(|e| PairingFault::Pairing(e.to_string()))?;
    let grant = core
        .issue_pairing_grant(&request)
        .map_err(|e| PairingFault::Vault(e.to_string()))?;
    // `grant` zeroizes itself on drop; its encoding is the same secret
    // serialized, and the seal wipes it once the ciphertext exists.
    let mut encoded = grant
        .encode()
        .map_err(|e| PairingFault::Pairing(e.to_string()))?;
    let sealed = device.seal_pairing_grant(&encoded);
    zeroize::Zeroize::zeroize(&mut encoded);
    sealed
}

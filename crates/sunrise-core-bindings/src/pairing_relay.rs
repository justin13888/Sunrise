//! Pairing over the relay: the same state machine, with the relay carrying the
//! bytes instead of the user.
//!
//! [`DevicePairing`] is transport-agnostic, and until the rendezvous existed
//! its only transport was a person copying six base64 strings between two
//! screens. [`RelayPairing`] moves each of those strings through
//! `POST /api/v1/pairing/{send,receive,abort}` (`docs/06-server/api.md`
//! §Pairing rendezvous), in the order the protocol fixes. Nothing about the
//! crypto changes: the relay sees Noise ciphertext, the SAS binds the
//! transcript, and the existing device still refuses a transcript whose peer
//! static is not the scanned QR's `n_static_pub` — before the SAS screen,
//! inside [`DevicePairing::receive_message`].
//!
//! # The phases a client sees
//!
//! Three, which is the point of moving the transport down here:
//!
//! 1. **Code.** The new device calls [`RelayPairing::offer`] and shows
//!    [`RelayPairing::qr_payload`]; the existing device scans it and calls
//!    [`RelayPairing::accept`] with the text exactly as scanned. The seam
//!    decodes it — a client never parses a QR.
//! 2. **Compare.** Both call [`RelayPairing::handshake`], which returns the six
//!    digits once the transcript completes.
//! 3. **Done.** On "Match", the existing device calls
//!    [`RelayPairing::sponsor`] and the new one [`RelayPairing::join`]; on
//!    "Don't match", either calls [`RelayPairing::reject`], which also tells the
//!    other side through the relay.
//!
//! # Waiting, and where the bearer goes
//!
//! The rendezvous itself — the polling, the retry bound, the `429` that
//! refuses a new session, and the rule that the bearer goes only to this
//! device's relay origin — and the order the six messages cross in are the
//! client layer's [`sunrise_client_core::pairing::RelayPairing`], which the
//! CLI's `sunrise pair join` and `sunrise pair add` drive too, so every client
//! pairs the same way. This file lowers it onto [`BindingError`].
//!
//! [`DevicePairing`]: crate::pairing::DevicePairing
//! [`DevicePairing::receive_message`]: crate::pairing::DevicePairing::receive_message

use std::sync::Arc;

use sunrise_client_core::pairing::{self as client, RendezvousError};

use crate::pairing::{PairedBundle, PairingRole};
use crate::{BindingError, SunriseCore};

pub use sunrise_client_core::pairing::{POLL_INTERVAL, WAIT_CAP};

/// One device's half of a pairing whose messages travel through the relay.
#[derive(uniffi::Object)]
pub struct RelayPairing {
    inner: client::RelayPairing,
}

impl std::fmt::Debug for RelayPairing {
    /// The role only: see [`DevicePairing`]'s `Debug` for why.
    ///
    /// [`DevicePairing`]: crate::pairing::DevicePairing
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayPairing")
            .field("role", &self.role())
            .finish_non_exhaustive()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl RelayPairing {
    /// Begin on the **new** device: mint the QR and open the relay session.
    ///
    /// The first handshake message is sent here, before the QR is shown,
    /// because Noise XX starts with the new device and the message needs
    /// nothing from the other side. Sending it now is also what opens the
    /// session, so the QR's 300 s lifetime starts while it is on screen.
    ///
    /// `bearer` is this account's access token: the session is bound to the
    /// account it names, and the existing device must present one for the same
    /// account to join it.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] if the QR cannot be built, and
    /// [`BindingError::Relay`] for a relay URL that is not `https://` (or
    /// `http://` to a loopback host), if the relay refuses the session — at
    /// once, with its `Retry-After` in the message, for a pair attempt over
    /// the hourly or daily limit or a relay holding all the sessions it will —
    /// or if it cannot be reached within [`WAIT_CAP`].
    #[uniffi::constructor]
    pub async fn offer(
        relay_url: String,
        bearer: String,
        account_email: String,
    ) -> Result<Arc<Self>, BindingError> {
        let inner = client::RelayPairing::offer(&relay_url, &bearer, &account_email).await?;
        Ok(Arc::new(Self { inner }))
    }

    /// Begin on the **existing** device, from the QR as scanned or pasted.
    ///
    /// The text goes straight to the decoder that wrote it; the session to
    /// join is read from it. Its `n_static_pub` is kept and checked against the
    /// peer the handshake actually reaches, so a tampered code is refused
    /// before any SAS is shown.
    ///
    /// `relay_url` is the relay this device is configured for, and `bearer`
    /// this account's access token. The bearer goes to `relay_url` and nowhere
    /// else: the code's own `relay_url` must name the same origin, so a code
    /// printed to point elsewhere is refused before any request is made.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] with the decoder's own message for a payload
    /// that is not a pairing QR, and [`BindingError::Relay`] for a relay URL
    /// that is not `https://` (or `http://` to a loopback host), or a code
    /// whose relay is not this device's.
    #[uniffi::constructor]
    pub fn accept(
        qr_payload: String,
        relay_url: String,
        bearer: String,
    ) -> Result<Arc<Self>, BindingError> {
        let inner = client::RelayPairing::accept(&qr_payload, &relay_url, &bearer)?;
        Ok(Arc::new(Self { inner }))
    }

    /// The QR payload to display, on the new device.
    #[must_use]
    pub fn qr_payload(&self) -> Option<String> {
        self.inner.qr_payload()
    }

    /// This device's role.
    #[must_use]
    pub fn role(&self) -> PairingRole {
        self.inner.role().into()
    }

    /// Run the Noise handshake over the relay and return the SAS both users
    /// compare.
    ///
    /// On the new device this waits for the existing one to scan the code; on
    /// the existing device it runs at once. Any failure aborts the relay
    /// session, so the other device stops waiting rather than running out its
    /// 300 s.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the transcript fails — which on the
    /// existing device includes a peer whose static key is not the QR's — or
    /// when the session is gone; [`BindingError::Relay`] when the relay cannot
    /// be reached within [`WAIT_CAP`].
    pub async fn handshake(&self) -> Result<String, BindingError> {
        Ok(self.inner.handshake().await?)
    }

    /// The user tapped "Match" on the **existing** device: hand the new one
    /// this account's offer, wait for its cert request, issue the cert and send
    /// the grant.
    ///
    /// `core` is the open vault; the cert is signed inside it and nothing
    /// secret crosses into the client.
    ///
    /// # Errors
    ///
    /// As [`SunriseCore::send_pairing_offer`] and
    /// [`SunriseCore::send_pairing_grant`], and [`BindingError::Pairing`] when
    /// the new device rejected the SAS or left.
    pub async fn sponsor(&self, core: Arc<SunriseCore>) -> Result<(), BindingError> {
        Ok(self.inner.sponsor(core.core()).await?)
    }

    /// The user tapped "Match" on the **new** device: wait for the offer, mint
    /// this device's keys and ask for a cert, then open the grant.
    ///
    /// `seed_s` and `seed_d` are 32 unpredictable bytes each, drawn by the
    /// platform — see [`DevicePairing::request_device_cert`]. The bundle comes
    /// back for `SunriseCore::open`, exactly as the manual flow returns it.
    ///
    /// # Errors
    ///
    /// As [`DevicePairing::request_device_cert`] and
    /// [`DevicePairing::open_pairing_grant`], and [`BindingError::Pairing`]
    /// when the existing device rejected the SAS or left.
    ///
    /// [`DevicePairing::request_device_cert`]: crate::pairing::DevicePairing::request_device_cert
    /// [`DevicePairing::open_pairing_grant`]: crate::pairing::DevicePairing::open_pairing_grant
    pub async fn join(
        &self,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<PairedBundle, BindingError> {
        let payload = self.inner.join(nickname, platform, seed_s, seed_d).await?;
        PairedBundle::try_from(&payload)
    }

    /// The user tapped "Don't match": discard the keys here and tell the other
    /// device through the relay, which is the spec's `pair_abort`.
    pub async fn reject(&self) {
        self.inner.reject().await;
    }

    /// Stop at any point: end any wait in progress, discard the keys and drop
    /// the relay session. Idempotent.
    pub async fn cancel(&self) {
        self.inner.cancel().await;
    }
}

/// The rendezvous's failures, as the variants a client branches on: the
/// pairing's own (the session, the code, the other device) are
/// [`BindingError::Pairing`], and the relay's or its URL's
/// [`BindingError::Relay`] — the split this seam has always made.
impl From<RendezvousError> for BindingError {
    fn from(e: RendezvousError) -> Self {
        if e.is_pairing() {
            Self::Pairing(e.to_string())
        } else {
            Self::Relay(e.to_string())
        }
    }
}

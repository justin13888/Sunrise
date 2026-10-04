//! Pairing over the relay: the same state machine, with the relay carrying the
//! bytes instead of the user.
//!
//! [`DevicePairing`] is transport-agnostic, and until the rendezvous existed
//! its only transport was a person copying six base64 strings between two
//! screens. [`RelayPairing`] wraps one and moves each of those strings through
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
//! # Waiting
//!
//! The relay holds messages; it does not push them. Each wait polls
//! `receive` every [`POLL_INTERVAL`] until the other side's next message
//! arrives, the relay says the session is gone (it expired, or the other side
//! aborted), [`RelayPairing::cancel`] is called, or [`WAIT_CAP`] passes. A
//! transient failure — the network, a `5xx`, a `429` — is retried inside the
//! same bound rather than surfaced, because a pairing is a few seconds of a
//! person's attention and a dropped packet should not cost them a rescan.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sunrise_pairing::decode_qr_payload;
use sunrise_relay_client::api;

use crate::pairing::{DevicePairing, PairedBundle, PairingRole};
use crate::{BindingError, SunriseCore};

/// How often a wait asks the relay for the other side's next message.
pub const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// The longest any one wait lasts: the relay's 300 s session lifetime and a
/// margin. The relay ends the session first in every case but a silent
/// network, and this is what ends that one.
pub const WAIT_CAP: Duration = Duration::from_secs(330);

/// One device's half of a pairing whose messages travel through the relay.
#[derive(uniffi::Object)]
pub struct RelayPairing {
    pairing: Arc<DevicePairing>,
    rendezvous: Rendezvous,
}

impl std::fmt::Debug for RelayPairing {
    /// The role only: see [`DevicePairing`]'s `Debug` for why.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayPairing")
            .field("role", &self.pairing.role())
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
    /// [`BindingError::Relay`] if the relay refuses the session — a pair
    /// attempt over the hourly or daily limit among them — or cannot be
    /// reached within [`WAIT_CAP`].
    #[uniffi::constructor]
    pub async fn offer(
        relay_url: String,
        bearer: String,
        account_email: String,
    ) -> Result<Arc<Self>, BindingError> {
        let pairing = Arc::new(DevicePairing::offer(relay_url.clone(), account_email)?);
        let qr = pairing
            .qr_payload()
            .ok_or_else(|| BindingError::Pairing("the new device has no QR".into()))?;
        let pair_id = decode_qr_payload(&qr)
            .map_err(|e| BindingError::Pairing(e.to_string()))?
            .pair_id;
        let rendezvous = Rendezvous::new(&relay_url, &bearer, pair_id, PairingRole::NewDevice)?;
        let this = Self {
            pairing,
            rendezvous,
        };
        let first = this.pairing.next_message()?;
        this.rendezvous.send(first).await?;
        Ok(Arc::new(this))
    }

    /// Begin on the **existing** device, from the QR as scanned or pasted.
    ///
    /// The text goes straight to the decoder that wrote it; the relay to use
    /// and the session to join are read from it. Its `n_static_pub` is kept
    /// and checked against the peer the handshake actually reaches, so a
    /// tampered code is refused before any SAS is shown.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] with the decoder's own message for a payload
    /// that is not a pairing QR, and [`BindingError::Relay`] for a relay URL the
    /// client cannot use.
    #[uniffi::constructor]
    pub fn accept(qr_payload: String, bearer: String) -> Result<Arc<Self>, BindingError> {
        let scanned = decode_qr_payload(qr_payload.trim())
            .map_err(|e| BindingError::Pairing(e.to_string()))?;
        let pairing = Arc::new(DevicePairing::accept(qr_payload.trim().to_owned())?);
        let rendezvous = Rendezvous::new(
            &scanned.relay_url,
            &bearer,
            scanned.pair_id,
            PairingRole::ExistingDevice,
        )?;
        Ok(Arc::new(Self {
            pairing,
            rendezvous,
        }))
    }

    /// The QR payload to display, on the new device.
    #[must_use]
    pub fn qr_payload(&self) -> Option<String> {
        self.pairing.qr_payload()
    }

    /// This device's role.
    #[must_use]
    pub fn role(&self) -> PairingRole {
        self.pairing.role()
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
        let ran = self.run_handshake().await;
        self.abort_on_failure(ran).await
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
        let ran = self.run_sponsor(&core).await;
        self.abort_on_failure(ran).await
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
    pub async fn join(
        &self,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<PairedBundle, BindingError> {
        let ran = self.run_join(nickname, platform, seed_s, seed_d).await;
        self.abort_on_failure(ran).await
    }

    /// The user tapped "Don't match": discard the keys here and tell the other
    /// device through the relay, which is the spec's `pair_abort`.
    pub async fn reject(&self) {
        let _ = self.pairing.confirm(false);
        self.rendezvous.abort().await;
    }

    /// Stop at any point: end any wait in progress, discard the keys and drop
    /// the relay session. Idempotent.
    pub async fn cancel(&self) {
        self.rendezvous.cancelled.store(true, Ordering::SeqCst);
        let _ = self.pairing.confirm(false);
        self.rendezvous.abort().await;
    }
}

impl RelayPairing {
    async fn run_handshake(&self) -> Result<String, BindingError> {
        match self.pairing.role() {
            PairingRole::NewDevice => {
                let second = self.rendezvous.next().await?;
                self.pairing.receive_message(second)?;
                let third = self.pairing.next_message()?;
                self.rendezvous.send(third).await?;
            }
            PairingRole::ExistingDevice => {
                let first = self.rendezvous.next().await?;
                self.pairing.receive_message(first)?;
                let second = self.pairing.next_message()?;
                self.rendezvous.send(second).await?;
                let third = self.rendezvous.next().await?;
                self.pairing.receive_message(third)?;
            }
        }
        self.pairing.sas()
    }

    async fn run_sponsor(&self, core: &SunriseCore) -> Result<(), BindingError> {
        self.pairing.confirm(true)?;
        let offer = core.send_pairing_offer(Arc::clone(&self.pairing))?;
        self.rendezvous.send(offer).await?;
        let request = self.rendezvous.next().await?;
        let grant = core.send_pairing_grant(Arc::clone(&self.pairing), request)?;
        self.rendezvous.send(grant).await
    }

    async fn run_join(
        &self,
        nickname: String,
        platform: String,
        seed_s: Vec<u8>,
        seed_d: Vec<u8>,
    ) -> Result<PairedBundle, BindingError> {
        self.pairing.confirm(true)?;
        let offer = self.rendezvous.next().await?;
        let request = self
            .pairing
            .request_device_cert(offer, nickname, platform, seed_s, seed_d)?;
        self.rendezvous.send(request).await?;
        let grant = self.rendezvous.next().await?;
        self.pairing.open_pairing_grant(grant)
    }

    /// Drop the relay session when a step failed, so the other device learns
    /// at its next poll rather than at the session's expiry.
    async fn abort_on_failure<T>(&self, ran: Result<T, BindingError>) -> Result<T, BindingError> {
        if ran.is_err() {
            let _ = self.pairing.confirm(false);
            self.rendezvous.abort().await;
        }
        ran
    }
}

/// The relay end of one pairing: which session, which side, and how far this
/// side has read.
struct Rendezvous {
    client: api::Client,
    pair_id: String,
    role: api::types::PairRole,
    /// How many of the other side's messages this side has consumed.
    read: AtomicU32,
    cancelled: AtomicBool,
}

/// What a relay call's failure means for the wait it is in.
enum Fault {
    /// The session is gone: expired, aborted by the other side, or never
    /// there. Final.
    Gone,
    /// Worth asking again inside the wait's bound.
    Transient(String),
    /// Final, for any other reason.
    Fatal(String),
}

impl Rendezvous {
    fn new(
        relay_url: &str,
        bearer: &str,
        pair_id: String,
        role: PairingRole,
    ) -> Result<Self, BindingError> {
        let client = api::Client::new(relay_url.trim_end_matches('/'))
            .map_err(|e| BindingError::Relay(e.to_string()))?
            .with_credential(
                "AccountToken",
                api::Credential::Bearer(api::SecretString::from(bearer.to_owned())),
            );
        Ok(Self {
            client,
            pair_id,
            role: match role {
                PairingRole::NewDevice => api::types::PairRole::NewDevice,
                PairingRole::ExistingDevice => api::types::PairRole::ExistingDevice,
            },
            read: AtomicU32::new(0),
            cancelled: AtomicBool::new(false),
        })
    }

    /// Buffer one message for the other side.
    async fn send(&self, message: String) -> Result<(), BindingError> {
        let body = api::types::PairSendRequest {
            pair_id: self.pair_id.clone(),
            role: self.role.clone(),
            message,
        };
        let deadline = tokio::time::Instant::now() + WAIT_CAP;
        loop {
            self.check_cancelled()?;
            match self.client.send_pairing_message(None, &body).await {
                Ok(_) => return Ok(()),
                Err(e) => self.settle(classify(&e), deadline).await?,
            }
        }
    }

    /// Wait for the other side's next message.
    async fn next(&self) -> Result<String, BindingError> {
        let deadline = tokio::time::Instant::now() + WAIT_CAP;
        loop {
            self.check_cancelled()?;
            let body = api::types::PairReceiveRequest {
                pair_id: self.pair_id.clone(),
                role: self.role.clone(),
                after: i64::from(self.read.load(Ordering::SeqCst)),
            };
            match self.client.receive_pairing_messages(None, &body).await {
                Ok(res) => {
                    if let Some(message) = res.into_inner().messages.into_iter().next() {
                        self.read.fetch_add(1, Ordering::SeqCst);
                        return Ok(message);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(timed_out());
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Err(e) => self.settle(classify(&e), deadline).await?,
            }
        }
    }

    /// Drop the session. Best effort: the relay expires it anyway, and a
    /// failure here must not mask the failure that led to it.
    async fn abort(&self) {
        let body = api::types::PairAbortRequest {
            pair_id: self.pair_id.clone(),
            role: self.role.clone(),
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.client.abort_pairing(None, &body),
        )
        .await;
    }

    /// Turn a failed call into the wait's next move: sleep and retry, or stop.
    async fn settle(
        &self,
        fault: Fault,
        deadline: tokio::time::Instant,
    ) -> Result<(), BindingError> {
        match fault {
            Fault::Gone => Err(BindingError::Pairing(
                "the pairing ended: the other device cancelled it or it expired. \
                 Start again from a new code"
                    .into(),
            )),
            Fault::Fatal(why) => Err(BindingError::Relay(why)),
            Fault::Transient(why) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(BindingError::Relay(why));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                Ok(())
            }
        }
    }

    fn check_cancelled(&self) -> Result<(), BindingError> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(BindingError::Pairing("the pairing was cancelled".into()))
        } else {
            Ok(())
        }
    }
}

fn timed_out() -> BindingError {
    BindingError::Pairing("the other device did not answer in time".into())
}

fn classify<E: std::fmt::Display>(e: &api::Error<E>) -> Fault {
    if let api::Error::Api(value) = e {
        if value.status().as_u16() == 404 {
            return Fault::Gone;
        }
    }
    if e.is_transient() {
        Fault::Transient(e.to_string())
    } else {
        Fault::Fatal(e.to_string())
    }
}

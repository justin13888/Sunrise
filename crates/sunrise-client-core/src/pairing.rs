//! The relay end of a pairing: the rendezvous that carries its messages.
//!
//! A pairing's six messages — three Noise, then offer, request and grant —
//! cross either by the user's hand or through
//! `POST /api/v1/pairing/{send,receive,abort}` (`docs/06-server/api.md`
//! §Pairing rendezvous). This module is the second: a [`Rendezvous`] moves
//! one message at a time, in order, and waits for the other side's. Nothing
//! about the crypto is here — the relay sees Noise ciphertext and the SAS
//! binds the transcript — so a seam drives the state machine and hands each
//! message to this.
//!
//! # Waiting
//!
//! The relay holds messages; it does not push them. Each wait polls
//! `receive` every [`POLL_INTERVAL`] until the other side's next message
//! arrives, the relay says the session is gone (it expired, or the other side
//! aborted), [`Rendezvous::cancel`] is called, or [`WAIT_CAP`] passes. A
//! transient failure — the network, a `5xx`, a `429` — is retried inside the
//! same bound rather than surfaced: a pairing is a few seconds of a person's
//! attention and a dropped packet should not cost them a rescan.
//!
//! One `429` is not transient: the answer to the message that opens the
//! session. That is the relay refusing a new pairing — over the account's or
//! the address's pair-attempt limit, or holding all the sessions it will —
//! and its `Retry-After` is minutes, so [`Rendezvous::send`] returns it at
//! once as [`RendezvousError::Refused`].
//!
//! # Where the bearer goes
//!
//! Only to the relay this device is configured for, and only if that is a
//! relay origin by [`crate::relay_url`]'s rule. The scanned QR names a relay
//! too, but it is text anyone can print, so [`scanned_pair_id`] refuses a code
//! whose relay is another origin before any request is made.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use sunrise_pairing::decode_qr_payload;
use sunrise_relay_client::api;

use crate::relay_url::{parse_relay_url, RelayUrlIssue};

/// How often a wait asks the relay for the other side's next message.
pub const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// The longest any one wait lasts: the relay's 300 s session lifetime and a
/// margin. The relay ends the session first in every case but a silent
/// network, and this is what ends that one.
pub const WAIT_CAP: Duration = Duration::from_secs(330);

/// Which side of the rendezvous this device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RendezvousRole {
    /// The device being added: it shows the code and sends first.
    NewDevice,
    /// The device that holds the vault: it scans the code.
    ExistingDevice,
}

/// Why a rendezvous call stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RendezvousError {
    /// The relay URL is not one a bearer may go to.
    #[error("the relay URL {url} is refused ({issue}); a pairing sends this account's token only over TLS to a relay origin")]
    BadRelayUrl {
        /// The URL as given.
        url: String,
        /// Why.
        issue: RelayUrlIssue,
    },
    /// The scanned code is not a pairing code: the decoder's own message.
    #[error("{0}")]
    BadCode(String),
    /// The scanned code names a relay that is not this device's.
    #[error(
        "the code names the relay {scanned}, not this device's relay {own}; \
         pair only with a code shown by a device on the same relay"
    )]
    ForeignRelay {
        /// The relay the code names.
        scanned: String,
        /// This device's relay.
        own: String,
    },
    /// The relay refused to open a session: a `429` to the opening message.
    #[error(
        "the relay refused a new pairing: too many pair attempts, or too many \
         pairings in progress. Try again {}",
        retry_after.as_ref().map_or_else(|| "later".to_owned(), |s| format!("in {s} s (Retry-After)"))
    )]
    Refused {
        /// The relay's `Retry-After`, in seconds as it sent them.
        retry_after: Option<String>,
    },
    /// The session is gone: the other side cancelled it, or it expired.
    #[error(
        "the pairing ended: the other device cancelled it or it expired. \
         Start again from a new code"
    )]
    Gone,
    /// [`Rendezvous::cancel`] was called.
    #[error("the pairing was cancelled")]
    Cancelled,
    /// The other side did not send its next message within [`WAIT_CAP`].
    #[error("the other device did not answer in time")]
    TimedOut,
    /// The relay could not be reached, or answered with an error, within the
    /// bound.
    #[error("{0}")]
    Relay(String),
}

impl RendezvousError {
    /// Whether the failure is the pairing's — the session, the code, the
    /// other device — rather than the relay's or its URL's.
    #[must_use]
    pub const fn is_pairing(&self) -> bool {
        matches!(
            self,
            Self::BadCode(_) | Self::Gone | Self::Cancelled | Self::TimedOut
        )
    }
}

/// The session a scanned code names, once its relay is checked against this
/// device's.
///
/// `qr_payload` goes to the decoder that wrote it; the code's relay must be
/// the same origin as `own_relay_url`, so a code printed to point elsewhere
/// never receives this device's bearer.
///
/// # Errors
///
/// [`RendezvousError::BadCode`] for text that is not a pairing code,
/// [`RendezvousError::BadRelayUrl`] for a relay that is not a relay origin,
/// and [`RendezvousError::ForeignRelay`] for a code naming another one.
pub fn scanned_pair_id(qr_payload: &str, own_relay_url: &str) -> Result<String, RendezvousError> {
    let scanned = decode_qr_payload(qr_payload.trim())
        .map_err(|e| RendezvousError::BadCode(e.to_string()))?;
    let own = checked(own_relay_url)?;
    if checked(&scanned.relay_url)? != own {
        return Err(RendezvousError::ForeignRelay {
            scanned: scanned.relay_url,
            own: own_relay_url.to_owned(),
        });
    }
    Ok(scanned.pair_id)
}

fn checked(url: &str) -> Result<crate::relay_url::RelayOrigin, RendezvousError> {
    parse_relay_url(url).map_err(|issue| RendezvousError::BadRelayUrl {
        url: url.to_owned(),
        issue,
    })
}

/// The relay end of one pairing: which session, which side, and how far this
/// side has read.
pub struct Rendezvous {
    client: api::Client,
    pair_id: String,
    role: api::types::PairRole,
    /// How many of the other side's messages this side has consumed.
    read: AtomicU32,
    /// How many messages this side has had accepted.
    written: AtomicU32,
    cancelled: AtomicBool,
}

impl std::fmt::Debug for Rendezvous {
    /// The role only: the session id is a capability for the rendezvous.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendezvous")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// What a relay call's failure means for the wait it is in.
enum Fault {
    /// The session is gone. Final.
    Gone,
    /// Worth asking again inside the wait's bound.
    Transient(String),
    /// Final, for any other reason.
    Fatal(String),
}

impl Rendezvous {
    /// Join session `pair_id` at `relay_url` as `role`, presenting `bearer`.
    ///
    /// # Errors
    ///
    /// [`RendezvousError::BadRelayUrl`] for a URL that is not a relay origin,
    /// and [`RendezvousError::Relay`] if the client cannot be built.
    pub fn new(
        relay_url: &str,
        bearer: &str,
        pair_id: String,
        role: RendezvousRole,
    ) -> Result<Self, RendezvousError> {
        let origin = checked(relay_url)?;
        let client = api::Client::new(origin.as_str())
            .map_err(|e| RendezvousError::Relay(e.to_string()))?
            .with_credential(
                "AccountToken",
                api::Credential::Bearer(api::SecretString::from(bearer.to_owned())),
            );
        Ok(Self {
            client,
            pair_id,
            role: match role {
                RendezvousRole::NewDevice => api::types::PairRole::NewDevice,
                RendezvousRole::ExistingDevice => api::types::PairRole::ExistingDevice,
            },
            read: AtomicU32::new(0),
            written: AtomicU32::new(0),
            cancelled: AtomicBool::new(false),
        })
    }

    /// Buffer one message for the other side.
    ///
    /// Each message carries its index, and a retry carries the same one: when
    /// the relay accepted a message and its answer was lost, the resend is
    /// acknowledged rather than buffered twice, which would hand the other
    /// side a duplicate it reads as the protocol's next message.
    ///
    /// # Errors
    ///
    /// [`RendezvousError::Refused`] for a `429` to the new device's opening
    /// message; otherwise as [`Self::next`].
    pub async fn send(&self, message: String) -> Result<(), RendezvousError> {
        let index = self.written.load(Ordering::SeqCst);
        let opening = index == 0 && self.role == api::types::PairRole::NewDevice;
        let body = api::types::PairSendRequest {
            pair_id: self.pair_id.clone(),
            role: self.role.clone(),
            index: i64::from(index),
            message,
        };
        let deadline = tokio::time::Instant::now() + WAIT_CAP;
        loop {
            self.check_cancelled()?;
            match self.client.send_pairing_message(None, &body).await {
                Ok(_) => {
                    self.written.fetch_add(1, Ordering::SeqCst);
                    return Ok(());
                }
                Err(e) => {
                    if opening {
                        if let Some(refusal) = opening_refusal(&e) {
                            return Err(refusal);
                        }
                    }
                    self.settle(classify(&e), deadline).await?;
                }
            }
        }
    }

    /// Wait for the other side's next message.
    ///
    /// # Errors
    ///
    /// [`RendezvousError::Gone`], [`RendezvousError::Cancelled`],
    /// [`RendezvousError::TimedOut`], or [`RendezvousError::Relay`] for a
    /// relay that kept failing until [`WAIT_CAP`] or failed for good.
    pub async fn next(&self) -> Result<String, RendezvousError> {
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
                        return Err(RendezvousError::TimedOut);
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Err(e) => self.settle(classify(&e), deadline).await?,
            }
        }
    }

    /// Drop the session. Best effort: the relay expires it anyway, and a
    /// failure here must not mask the failure that led to it.
    pub async fn abort(&self) {
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

    /// End any wait in progress at its next poll, and every later one.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Turn a failed call into the wait's next move: sleep and retry, or stop.
    async fn settle(
        &self,
        fault: Fault,
        deadline: tokio::time::Instant,
    ) -> Result<(), RendezvousError> {
        match fault {
            Fault::Gone => Err(RendezvousError::Gone),
            Fault::Fatal(why) => Err(RendezvousError::Relay(why)),
            Fault::Transient(why) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(RendezvousError::Relay(why));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                Ok(())
            }
        }
    }

    fn check_cancelled(&self) -> Result<(), RendezvousError> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(RendezvousError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The relay refusing to open a session: a `429` to the opening message,
/// carrying how long the relay asked the device to wait.
fn opening_refusal<E>(e: &api::Error<E>) -> Option<RendezvousError> {
    let (status, headers) = match e {
        api::Error::Api(value) => (value.status(), value.headers()),
        api::Error::UnexpectedStatus {
            status, headers, ..
        } => (*status, headers),
        _ => return None,
    };
    if status.as_u16() != 429 {
        return None;
    }
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|secs| secs.trim().to_owned());
    Some(RendezvousError::Refused { retry_after })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendezvous_needs_a_relay_origin() {
        for ok in [
            "https://relay.example",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            assert!(
                Rendezvous::new(ok, "t", "p".into(), RendezvousRole::NewDevice).is_ok(),
                "{ok} is usable"
            );
        }
        for refused in [
            "http://relay.example",
            "wss://relay.example",
            "relay.example",
            "",
        ] {
            let err = Rendezvous::new(refused, "t", "p".into(), RendezvousRole::NewDevice)
                .expect_err(refused);
            assert!(
                matches!(err, RendezvousError::BadRelayUrl { .. }),
                "{refused}: {err}"
            );
            assert!(!err.is_pairing());
        }
    }

    /// The bearer goes only to this device's relay: a code naming another
    /// origin is refused before any request.
    #[test]
    fn a_code_naming_another_relay_is_refused() {
        let qr = |relay: &str| {
            sunrise_pairing::encode_qr_payload(&sunrise_pairing::QrPayload {
                magic_v1: sunrise_pairing::MAGIC_V1_HEX.to_owned(),
                pair_id: "AAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                n_static_pub: "A".repeat(43),
                account_email_hash: "00000000".to_owned(),
                relay_url: relay.to_owned(),
            })
            .unwrap()
        };
        let own = "https://relay.example";
        assert_eq!(
            scanned_pair_id(&qr("https://Relay.Example/"), own).as_deref(),
            Ok("AAAAAAAAAAAAAAAAAAAAAA")
        );
        assert!(matches!(
            scanned_pair_id(&qr("https://relay.example.attacker.test"), own),
            Err(RendezvousError::ForeignRelay { .. })
        ));
        assert!(matches!(
            scanned_pair_id("not a code", own),
            Err(RendezvousError::BadCode(_))
        ));
    }

    #[test]
    fn the_refusal_names_the_relays_wait() {
        let with = RendezvousError::Refused {
            retry_after: Some("600".into()),
        };
        assert!(
            with.to_string()
                .ends_with("Try again in 600 s (Retry-After)"),
            "{with}"
        );
        let without = RendezvousError::Refused { retry_after: None };
        assert!(
            without.to_string().ends_with("Try again later"),
            "{without}"
        );
    }
}

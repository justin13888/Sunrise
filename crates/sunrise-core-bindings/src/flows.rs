//! The client layer's state machines, lowered onto UniFFI: a pairing as a
//! screen walks it (this file), the vault session's lifecycle (`vault`), and
//! the account's OIDC session (`account`).
//!
//! Each is an object holding the [`sunrise_client_core`] machine behind a
//! mutex, and each method is one transition. The machines perform no IO: a
//! transition returns the seam calls to make (`PairingEffect`, `VaultStep`,
//! `SignInStep`, `RenewalStep`) and the client answers through the next
//! method. The account's synchronous store calls are the foreign
//! `AccountCredentialStore`.
//!
//! `CoreLockReason` and `CoreSignOutResidue` carry a `Core` prefix because the
//! Apple app already declares a `LockReason` and a `SignOutResidue`.

use std::sync::{Arc, Mutex, MutexGuard};

use sunrise_client_core::pairing_flow::{
    self as flow, ManualReason, PairingFailureKind, PairingIntent, PairingLeg, PairingTransport,
    PairingWork,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic inside a transition leaves the machine as it was before it;
    // there is no half-written state to refuse.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An answer that is text or a failure.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum TextAnswer {
    /// The text.
    Ok {
        /// It.
        text: String,
    },
    /// The seam's message.
    Err {
        /// It.
        message: String,
    },
}

impl From<TextAnswer> for Result<String, String> {
    fn from(a: TextAnswer) -> Self {
        match a {
            TextAnswer::Ok { text } => Ok(text),
            TextAnswer::Err { message } => Err(message),
        }
    }
}

/// An answer that is done or a failure.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum UnitAnswer {
    /// Done.
    Ok,
    /// The seam's message.
    Err {
        /// It.
        message: String,
    },
}

impl From<UnitAnswer> for Result<(), String> {
    fn from(a: UnitAnswer) -> Self {
        match a {
            UnitAnswer::Ok => Ok(()),
            UnitAnswer::Err { message } => Err(message),
        }
    }
}

// --- Pairing ---------------------------------------------------------------

/// See [`sunrise_client_core::pairing_flow::PairingIntent`].
#[uniffi::remote(Enum)]
pub enum PairingIntent {
    AddThisDevice,
    AddAnotherDevice,
}

/// See [`sunrise_client_core::pairing_flow::PairingLeg`].
#[uniffi::remote(Enum)]
pub enum PairingLeg {
    Code,
    First,
    Second,
    Third,
    Compare,
    Offer,
    Request,
    Grant,
}

/// See [`sunrise_client_core::pairing_flow::PairingTransport`].
#[uniffi::remote(Enum)]
pub enum PairingTransport {
    Relay,
    Manual,
}

/// See [`sunrise_client_core::pairing_flow::ManualReason`].
#[uniffi::remote(Enum)]
pub enum ManualReason {
    NoRelayConfigured,
    NotSignedIn,
    RelayDidNotAnswer,
    RelayDidNotAnswerHere,
    RelayRefused,
}

/// See [`sunrise_client_core::pairing_flow::PairingWork`].
#[uniffi::remote(Enum)]
pub enum PairingWork {
    ReachingRelay,
    Connecting,
    Finishing,
}

/// See [`sunrise_client_core::pairing_flow::PairingFailureKind`].
#[uniffi::remote(Enum)]
pub enum PairingFailureKind {
    General,
    RelayCode,
}

/// See [`sunrise_client_core::pairing_flow::PairingPhase`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PairingPhase {
    /// Nothing generated yet.
    Idle,
    /// Showing `text` at `leg`.
    HandOff {
        /// The leg.
        leg: PairingLeg,
        /// The text.
        text: String,
    },
    /// Waiting for a paste at `leg`.
    Awaiting {
        /// The leg.
        leg: PairingLeg,
    },
    /// Comparing the digits.
    Comparing {
        /// The digits.
        sas: String,
    },
    /// The seam is busy.
    Working {
        /// On what.
        work: PairingWork,
    },
    /// Paired.
    Done,
    /// The digits differed.
    Mismatch,
    /// Failed.
    Failed {
        /// The seam's message.
        message: String,
        /// How to word it.
        kind: PairingFailureKind,
    },
}

impl From<&flow::PairingPhase> for PairingPhase {
    fn from(p: &flow::PairingPhase) -> Self {
        match p {
            flow::PairingPhase::Idle => Self::Idle,
            flow::PairingPhase::HandOff { leg, text } => Self::HandOff {
                leg: *leg,
                text: text.clone(),
            },
            flow::PairingPhase::Awaiting(leg) => Self::Awaiting { leg: *leg },
            flow::PairingPhase::Comparing { sas } => Self::Comparing { sas: sas.clone() },
            flow::PairingPhase::Working(work) => Self::Working { work: *work },
            flow::PairingPhase::Done => Self::Done,
            flow::PairingPhase::Mismatch => Self::Mismatch,
            flow::PairingPhase::Failed { message, kind } => Self::Failed {
                message: message.clone(),
                kind: *kind,
            },
        }
    }
}

/// See [`sunrise_client_core::pairing_flow::PairingEffect`]: the seam call to
/// make, and the [`PairingFlow`] method that takes its answer.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PairingEffect {
    /// Probe the relay → [`PairingFlow::relay_probed`].
    ProbeRelay {
        /// The attempt.
        attempt: u64,
    },
    /// `RelayPairing.offer` → [`PairingFlow::relay_opened`].
    OpenRelay {
        /// The attempt.
        attempt: u64,
    },
    /// `RelayPairing.accept` → [`PairingFlow::relay_accepted`].
    AcceptRelayCode {
        /// The attempt.
        attempt: u64,
        /// The code as scanned.
        text: String,
    },
    /// `RelayPairing.handshake` → [`PairingFlow::handshake_finished`].
    RunHandshake {
        /// The attempt.
        attempt: u64,
    },
    /// `RelayPairing.sponsor` or `join` → [`PairingFlow::relay_finished`].
    FinishRelay {
        /// The attempt.
        attempt: u64,
    },
    /// `RelayPairing.reject`.
    RejectRelay,
    /// `RelayPairing.cancel`.
    CancelRelay,
    /// `DevicePairing.offer` → [`PairingFlow::code_minted`].
    MintCode,
    /// The manual leg's seam call → [`PairingFlow::submitted`].
    Submit {
        /// The leg.
        leg: PairingLeg,
        /// The pasted text, trimmed.
        text: String,
    },
    /// `DevicePairing.next_message` → [`PairingFlow::message_ready`].
    NextMessage {
        /// The leg.
        leg: PairingLeg,
    },
    /// `DevicePairing.sas` → [`PairingFlow::sas_ready`].
    ReadSas,
    /// `DevicePairing.confirm(true)` → [`PairingFlow::match_confirmed`].
    ConfirmMatch,
    /// The vault's `send_pairing_offer` → [`PairingFlow::offer_sealed`].
    SealOffer,
    /// `DevicePairing.confirm(false)`.
    Discard,
}

fn effects(list: Vec<flow::PairingEffect>) -> Vec<PairingEffect> {
    use flow::PairingEffect as E;
    list.into_iter()
        .map(|e| match e {
            E::ProbeRelay { attempt } => PairingEffect::ProbeRelay { attempt },
            E::OpenRelay { attempt } => PairingEffect::OpenRelay { attempt },
            E::AcceptRelayCode { attempt, text } => {
                PairingEffect::AcceptRelayCode { attempt, text }
            }
            E::RunHandshake { attempt } => PairingEffect::RunHandshake { attempt },
            E::FinishRelay { attempt } => PairingEffect::FinishRelay { attempt },
            E::RejectRelay => PairingEffect::RejectRelay,
            E::CancelRelay => PairingEffect::CancelRelay,
            E::MintCode => PairingEffect::MintCode,
            E::Submit { leg, text } => PairingEffect::Submit { leg, text },
            E::NextMessage(leg) => PairingEffect::NextMessage { leg },
            E::ReadSas => PairingEffect::ReadSas,
            E::ConfirmMatch => PairingEffect::ConfirmMatch,
            E::SealOffer => PairingEffect::SealOffer,
            E::Discard => PairingEffect::Discard,
        })
        .collect()
}

/// How far along a pairing is: step `leg` of `of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PairingProgress {
    /// One-based step.
    pub leg: u8,
    /// Of how many.
    pub of: u8,
}

/// See [`sunrise_client_core::pairing_flow::PairingFlow`].
#[derive(Debug, uniffi::Object)]
pub struct PairingFlow(Mutex<flow::PairingFlow>);

#[uniffi::export]
impl PairingFlow {
    /// A pairing for `intent`, over the relay when this device has a relay
    /// and a signed-in account.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(intent: PairingIntent, relay_configured: bool, signed_in: bool) -> Arc<Self> {
        Arc::new(Self(Mutex::new(flow::PairingFlow::new(
            intent,
            relay_configured,
            signed_in,
        ))))
    }

    /// The phase.
    #[must_use]
    pub fn phase(&self) -> PairingPhase {
        PairingPhase::from(lock(&self.0).phase())
    }

    /// The transport.
    #[must_use]
    pub fn transport(&self) -> PairingTransport {
        lock(&self.0).transport()
    }

    /// Why this is copy and paste, when that was not the user's choice.
    #[must_use]
    pub fn notice(&self) -> Option<ManualReason> {
        lock(&self.0).notice()
    }

    /// Where in the script this device is.
    #[must_use]
    pub fn progress(&self) -> Option<PairingProgress> {
        lock(&self.0)
            .progress()
            .map(|(leg, of)| PairingProgress { leg, of })
    }

    /// Whether to offer "Copy and paste instead".
    #[must_use]
    pub fn offers_manual_fallback(&self) -> bool {
        lock(&self.0).offers_manual_fallback()
    }

    /// Start as the device being added.
    #[must_use]
    pub fn begin(&self) -> Vec<PairingEffect> {
        effects(lock(&self.0).begin())
    }

    /// The manual code.
    pub fn code_minted(&self, code: TextAnswer) {
        lock(&self.0).code_minted(code.into());
    }

    /// Whether the relay answered.
    #[must_use]
    pub fn relay_probed(&self, attempt: u64, answered: bool) -> Vec<PairingEffect> {
        effects(lock(&self.0).relay_probed(attempt, answered))
    }

    /// The rendezvous opened: its QR payload, or why not.
    #[must_use]
    pub fn relay_opened(&self, attempt: u64, payload: TextAnswer) -> Vec<PairingEffect> {
        effects(lock(&self.0).relay_opened(attempt, payload.into()))
    }

    /// Whether the seam took the scanned code.
    #[must_use]
    pub fn relay_accepted(&self, attempt: u64, accepted: UnitAnswer) -> Vec<PairingEffect> {
        effects(lock(&self.0).relay_accepted(attempt, accepted.into()))
    }

    /// The digits, or why the handshake stopped.
    pub fn handshake_finished(&self, attempt: u64, sas: TextAnswer) {
        lock(&self.0).handshake_finished(attempt, sas.into());
    }

    /// How `sponsor` or `join` ended.
    #[must_use]
    pub fn relay_finished(&self, attempt: u64, finished: UnitAnswer) -> Vec<PairingEffect> {
        effects(lock(&self.0).relay_finished(attempt, finished.into()))
    }

    /// The user pasted `text`.
    #[must_use]
    pub fn submit(&self, text: String) -> Vec<PairingEffect> {
        effects(lock(&self.0).submit(&text))
    }

    /// The seam's answer to a paste at `leg`: `hand_off` is the text the next
    /// leg shows, when the paste produced one.
    #[must_use]
    pub fn submitted(
        &self,
        leg: PairingLeg,
        hand_off: Option<String>,
        error: Option<String>,
    ) -> Vec<PairingEffect> {
        let outcome = error.map_or(Ok(hand_off), Err);
        effects(lock(&self.0).submitted(leg, outcome))
    }

    /// The text on screen was handed over.
    #[must_use]
    pub fn advance(&self) -> Vec<PairingEffect> {
        effects(lock(&self.0).advance())
    }

    /// The message to show at `leg`.
    pub fn message_ready(&self, leg: PairingLeg, message: TextAnswer) {
        lock(&self.0).message_ready(leg, message.into());
    }

    /// The digits.
    pub fn sas_ready(&self, sas: TextAnswer) {
        lock(&self.0).sas_ready(sas.into());
    }

    /// This user's answer to the digits.
    #[must_use]
    pub fn confirm(&self, matched: bool) -> Vec<PairingEffect> {
        effects(lock(&self.0).confirm(matched))
    }

    /// Whether the seam took "Match".
    #[must_use]
    pub fn match_confirmed(&self, confirmed: UnitAnswer) -> Vec<PairingEffect> {
        effects(lock(&self.0).match_confirmed(confirmed.into()))
    }

    /// The sealed offer.
    pub fn offer_sealed(&self, offer: TextAnswer) {
        lock(&self.0).offer_sealed(offer.into());
    }

    /// End it.
    #[must_use]
    pub fn cancel(&self) -> Vec<PairingEffect> {
        effects(lock(&self.0).cancel())
    }

    /// Leave the relay for copy and paste.
    #[must_use]
    pub fn use_manual_instead(&self) -> Vec<PairingEffect> {
        effects(lock(&self.0).use_manual_instead())
    }

    /// The screen went away.
    #[must_use]
    pub fn dismissed(&self) -> Vec<PairingEffect> {
        effects(lock(&self.0).dismissed())
    }
}

mod account;
mod vault;

pub use account::*;
pub use vault::*;

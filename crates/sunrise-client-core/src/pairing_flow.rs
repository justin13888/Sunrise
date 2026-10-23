//! One device's half of a pairing, as a screen walks it: pure transitions.
//!
//! The protocol is the seam's (`DevicePairing`, `RelayPairing`) and the words
//! are each client's. What is left — which transport carries the pairing,
//! what this device does at each leg, which phase it is in, how far along it
//! is, and what a late answer from an abandoned attempt may still change —
//! was the Apple app's `PairingModel`, and is here so every client walks the
//! same script. Nothing here performs IO: each transition returns the
//! [`PairingEffect`]s the caller runs against the seam, and the caller feeds
//! each answer back through the matching method.
//!
//! # Two transports
//!
//! * **Over the relay** ([`PairingTransport::Relay`], whenever this device has
//!   a relay and a signed-in account): code, compare, done. The rendezvous
//!   moves every message.
//! * **By copy and paste** ([`PairingTransport::Manual`]): eight legs, at each
//!   of which one device shows text and the other pastes it. The fallback for
//!   no relay, no account, or a relay that does not answer, and
//!   [`PairingFlow::notice`] says which.
//!
//! # Why eight legs
//!
//! Three Noise messages, the SAS, then offer, request and grant: the device
//! that holds the vault cannot sign a certificate for keys the joining device
//! has not minted yet, so since #105 the account's signing key never travels
//! and the last leg is a round trip.
//!
//! # Stale answers
//!
//! Every start and every cancel bumps [`PairingFlow::attempt`]. A relay
//! answer carries the attempt it was asked under, and one that returns after
//! the user moved on changes nothing — or, for a session it opened, asks for
//! that session to be cancelled.

/// Which side of the pairing this device is asking to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingIntent {
    /// This device has no vault and wants one: the protocol's new device,
    /// which shows the code.
    AddThisDevice,
    /// This device holds the vault and is authorising another: the existing
    /// device, which scans the code and issues the certificate.
    AddAnotherDevice,
}

/// The legs of the copy-and-paste script, in the order the protocol forces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PairingLeg {
    /// The QR code (or its text).
    Code,
    /// Noise message 1, from the new device.
    First,
    /// Noise message 2, from the existing device.
    Second,
    /// Noise message 3, from the new device.
    Third,
    /// Both users compare the six digits.
    Compare,
    /// The account's public identity, from the existing device.
    Offer,
    /// The new device's freshly minted public keys.
    Request,
    /// The signed certificate and the vault key.
    Grant,
}

impl PairingLeg {
    /// Every leg, in order.
    pub const ALL: [Self; 8] = [
        Self::Code,
        Self::First,
        Self::Second,
        Self::Third,
        Self::Compare,
        Self::Offer,
        Self::Request,
        Self::Grant,
    ];

    /// One-based position, for a "step 3 of 8" line.
    #[must_use]
    pub const fn number(self) -> u8 {
        self as u8 + 1
    }

    /// The leg after this one, or `None` after the grant.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        Some(match self {
            Self::Code => Self::First,
            Self::First => Self::Second,
            Self::Second => Self::Third,
            Self::Third => Self::Compare,
            Self::Compare => Self::Offer,
            Self::Offer => Self::Request,
            Self::Request => Self::Grant,
            Self::Grant => return None,
        })
    }

    /// Whether a device with `intent` is the one **showing** text at this
    /// leg. Neither shows at the compare leg; both look at their digits.
    ///
    /// ```text
    /// leg      | being added | already has the vault
    /// code     | show        | paste
    /// first    | show        | paste
    /// second   | paste       | show
    /// third    | show        | paste
    /// offer    | paste       | show
    /// request  | show        | paste
    /// grant    | paste       | show
    /// ```
    #[must_use]
    pub const fn shown_by(self, intent: PairingIntent) -> bool {
        match self {
            Self::Code | Self::First | Self::Third | Self::Request => {
                matches!(intent, PairingIntent::AddThisDevice)
            }
            Self::Second | Self::Offer | Self::Grant => {
                matches!(intent, PairingIntent::AddAnotherDevice)
            }
            Self::Compare => false,
        }
    }
}

/// Which transport carries the pairing's messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingTransport {
    /// The relay's rendezvous: code, compare, done.
    Relay,
    /// The user, by copy and paste: eight legs.
    Manual,
}

/// Why a pairing runs by copy and paste when that was not the user's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManualReason {
    /// This device has no relay set up.
    NoRelayConfigured,
    /// It has a relay and no signed-in account to present to it.
    NotSignedIn,
    /// The relay did not answer the device being added.
    RelayDidNotAnswer,
    /// The relay did not answer the device that scanned the code: both have
    /// to start again by copy and paste, from a new code.
    RelayDidNotAnswerHere,
    /// The relay answered and would not open the rendezvous.
    RelayRefused,
}

/// What the seam is doing while the screen shows a spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingWork {
    /// Asking whether the relay answers.
    ReachingRelay,
    /// Waiting on the rendezvous for the other device.
    Connecting,
    /// Sealing the offer or the grant, or opening the vault.
    Finishing,
}

/// How a failure is worded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingFailureKind {
    /// Any failure.
    General,
    /// A scanned code the relay could not pair with, most often one the other
    /// device drew for copy and paste: the words point at that way out.
    RelayCode,
}

/// Where the screen is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingPhase {
    /// Nothing generated yet.
    Idle,
    /// Showing `text` for the other device at `leg`.
    HandOff {
        /// The leg.
        leg: PairingLeg,
        /// What to show: the QR payload or a sealed message.
        text: String,
    },
    /// Waiting for the user to paste the other device's text at this leg.
    Awaiting(PairingLeg),
    /// Both devices show `sas`; nothing advances without an answer.
    Comparing {
        /// The six digits.
        sas: String,
    },
    /// The seam is busy.
    Working(PairingWork),
    /// Paired.
    Done,
    /// The user said the digits differ: the check working, not a fault.
    Mismatch,
    /// Something failed; `message` is the seam's.
    Failed {
        /// The seam's message.
        message: String,
        /// How to word it.
        kind: PairingFailureKind,
    },
}

impl PairingPhase {
    /// Done, mismatched or failed: the pairing has an outcome and nothing runs.
    #[must_use]
    pub const fn is_outcome(&self) -> bool {
        matches!(self, Self::Done | Self::Mismatch | Self::Failed { .. })
    }
}

/// Work the caller does against the seam, answering through the method each
/// variant names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingEffect {
    /// Ask whether the relay answers (`GET /api/v1/health`) →
    /// [`PairingFlow::relay_probed`].
    ProbeRelay {
        /// The attempt to answer under.
        attempt: u64,
    },
    /// `RelayPairing::offer`, then its QR → [`PairingFlow::relay_opened`].
    OpenRelay {
        /// The attempt to answer under.
        attempt: u64,
    },
    /// `RelayPairing::accept` with the scanned `text` →
    /// [`PairingFlow::relay_accepted`].
    AcceptRelayCode {
        /// The attempt to answer under.
        attempt: u64,
        /// The code as scanned.
        text: String,
    },
    /// `RelayPairing::handshake` → [`PairingFlow::handshake_finished`].
    RunHandshake {
        /// The attempt to answer under.
        attempt: u64,
    },
    /// `RelayPairing::sponsor` or `join`, by intent →
    /// [`PairingFlow::relay_finished`].
    FinishRelay {
        /// The attempt to answer under.
        attempt: u64,
    },
    /// `RelayPairing::reject`. No answer.
    RejectRelay,
    /// `RelayPairing::cancel`. No answer.
    CancelRelay,
    /// `DevicePairing::offer` and its QR → [`PairingFlow::code_minted`].
    MintCode,
    /// The manual leg's seam call with the pasted `text` →
    /// [`PairingFlow::submitted`]: `accept` at the code, `receive_message`
    /// at a Noise leg, `request_device_cert` at the offer (answering with the
    /// request), the vault's `send_pairing_grant` at the request (answering
    /// with the grant), `open_pairing_grant` and adopting the vault at the
    /// grant.
    Submit {
        /// The leg pasted at.
        leg: PairingLeg,
        /// What was pasted, trimmed.
        text: String,
    },
    /// `DevicePairing::next_message` → [`PairingFlow::message_ready`].
    NextMessage(PairingLeg),
    /// `DevicePairing::sas` → [`PairingFlow::sas_ready`].
    ReadSas,
    /// `DevicePairing::confirm(true)` → [`PairingFlow::match_confirmed`].
    ConfirmMatch,
    /// The vault's `send_pairing_offer` → [`PairingFlow::offer_sealed`].
    SealOffer,
    /// `DevicePairing::confirm(false)`: drop the keys. No answer.
    Discard,
}

/// One device's half of a pairing. See the module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingFlow {
    intent: PairingIntent,
    transport: PairingTransport,
    notice: Option<ManualReason>,
    phase: PairingPhase,
    relay_step: u8,
    attempt: u64,
    relay_open: bool,
    /// The code scanned while the relay was being probed.
    pending_scan: Option<String>,
    /// Text a submit produced for the next leg to show: the request is built
    /// from the offer, the grant signed over the request.
    pending_hand_off: Option<String>,
}

/// The relay's progress line: code, digits, done.
pub const RELAY_STEPS: u8 = 3;

impl PairingFlow {
    /// A pairing for `intent`, over the relay when this device has one and a
    /// signed-in account to present to it.
    #[must_use]
    pub fn new(intent: PairingIntent, relay_configured: bool, signed_in: bool) -> Self {
        let (transport, notice) = match (relay_configured, signed_in) {
            (true, true) => (PairingTransport::Relay, None),
            (false, _) => (
                PairingTransport::Manual,
                Some(ManualReason::NoRelayConfigured),
            ),
            (true, false) => (PairingTransport::Manual, Some(ManualReason::NotSignedIn)),
        };
        Self {
            intent,
            transport,
            notice,
            phase: Self::rest(intent),
            relay_step: 1,
            attempt: 0,
            relay_open: false,
            pending_scan: None,
            pending_hand_off: None,
        }
    }

    fn rest(intent: PairingIntent) -> PairingPhase {
        match intent {
            PairingIntent::AddThisDevice => PairingPhase::Idle,
            PairingIntent::AddAnotherDevice => PairingPhase::Awaiting(PairingLeg::Code),
        }
    }

    /// The intent.
    #[must_use]
    pub const fn intent(&self) -> PairingIntent {
        self.intent
    }

    /// The transport.
    #[must_use]
    pub const fn transport(&self) -> PairingTransport {
        self.transport
    }

    /// Why this is copy and paste, when that was not the user's choice.
    #[must_use]
    pub const fn notice(&self) -> Option<ManualReason> {
        self.notice
    }

    /// The phase.
    #[must_use]
    pub const fn phase(&self) -> &PairingPhase {
        &self.phase
    }

    /// The current attempt.
    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    /// Where in the script this device is, as `(step, of)`, while a pairing
    /// is running.
    #[must_use]
    pub const fn progress(&self) -> Option<(u8, u8)> {
        let running = matches!(
            self.phase,
            PairingPhase::HandOff { .. }
                | PairingPhase::Awaiting(_)
                | PairingPhase::Working(_)
                | PairingPhase::Comparing { .. }
        );
        if !running {
            return None;
        }
        if matches!(self.transport, PairingTransport::Relay) {
            return Some((self.relay_step, RELAY_STEPS));
        }
        let leg = match &self.phase {
            PairingPhase::HandOff { leg, .. } | PairingPhase::Awaiting(leg) => *leg,
            PairingPhase::Comparing { .. } => PairingLeg::Compare,
            _ => PairingLeg::Offer,
        };
        Some((leg.number(), PairingLeg::Grant.number()))
    }

    /// Whether to offer "Copy and paste instead": on the relay, unless the
    /// digits did not match — that is the check working, not the transport
    /// failing.
    #[must_use]
    pub fn offers_manual_fallback(&self) -> bool {
        self.transport == PairingTransport::Relay && self.phase != PairingPhase::Mismatch
    }

    fn fail(&mut self, message: String, kind: PairingFailureKind) {
        self.phase = PairingPhase::Failed { message, kind };
    }

    fn fall_back(&mut self, reason: ManualReason) {
        self.transport = PairingTransport::Manual;
        self.notice = Some(reason);
        self.relay_open = false;
    }

    // --- Starting ---------------------------------------------------------

    /// Start as the device being added. Nothing for the other intent, or
    /// once started.
    pub fn begin(&mut self) -> Vec<PairingEffect> {
        if self.intent != PairingIntent::AddThisDevice || self.phase != PairingPhase::Idle {
            return Vec::new();
        }
        if self.transport == PairingTransport::Manual {
            return vec![PairingEffect::MintCode];
        }
        self.attempt += 1;
        self.relay_step = 1;
        self.phase = PairingPhase::Working(PairingWork::ReachingRelay);
        vec![PairingEffect::ProbeRelay {
            attempt: self.attempt,
        }]
    }

    /// The manual code, minted, or why not.
    pub fn code_minted(&mut self, code: Result<String, String>) {
        match code {
            Ok(text) => {
                self.phase = PairingPhase::HandOff {
                    leg: PairingLeg::Code,
                    text,
                }
            }
            Err(message) => self.fail(message, PairingFailureKind::General),
        }
    }

    /// Whether the relay answered the probe `attempt` asked.
    pub fn relay_probed(&mut self, attempt: u64, answered: bool) -> Vec<PairingEffect> {
        if attempt != self.attempt {
            return Vec::new();
        }
        let scanned = self.pending_scan.take();
        match (self.intent, answered) {
            (PairingIntent::AddThisDevice, false) => {
                self.fall_back(ManualReason::RelayDidNotAnswer);
                self.phase = PairingPhase::Idle;
                vec![PairingEffect::MintCode]
            }
            (PairingIntent::AddAnotherDevice, false) => {
                // Not "carry on with this code": the device that showed it is
                // waiting on the relay, not on a pasted message.
                self.fall_back(ManualReason::RelayDidNotAnswerHere);
                self.phase = PairingPhase::Awaiting(PairingLeg::Code);
                Vec::new()
            }
            (PairingIntent::AddThisDevice, true) => vec![PairingEffect::OpenRelay { attempt }],
            (PairingIntent::AddAnotherDevice, true) => vec![PairingEffect::AcceptRelayCode {
                attempt,
                text: scanned.unwrap_or_default(),
            }],
        }
    }
}

mod drive;

#[cfg(test)]
mod tests;

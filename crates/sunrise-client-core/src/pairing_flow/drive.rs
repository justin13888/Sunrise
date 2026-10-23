//! The transitions after the start: the relay's three steps, the manual
//! script's eight legs, the answer to the digits, and the ways out.

use super::{
    ManualReason, PairingEffect, PairingFailureKind, PairingFlow, PairingIntent, PairingLeg,
    PairingPhase, PairingTransport, PairingWork,
};

impl PairingFlow {
    // --- Over the relay ---------------------------------------------------

    /// The rendezvous `attempt` opened: its QR payload, or why it could not
    /// open. A session that opened for an attempt the user has left is
    /// cancelled.
    pub fn relay_opened(
        &mut self,
        attempt: u64,
        opened: Result<String, String>,
    ) -> Vec<PairingEffect> {
        if attempt != self.attempt {
            return match opened {
                Ok(_) => vec![PairingEffect::CancelRelay],
                Err(_) => Vec::new(),
            };
        }
        let Ok(payload) = opened else {
            // Every failure to open one — a relay gone since the probe, one
            // refusing new pairings for now, a URL the seam will not send a
            // bearer to — leaves the manual flow untouched.
            self.fall_back(ManualReason::RelayRefused);
            self.phase = PairingPhase::Idle;
            return vec![PairingEffect::MintCode];
        };
        self.relay_open = true;
        self.phase = PairingPhase::HandOff {
            leg: PairingLeg::Code,
            text: payload,
        };
        vec![PairingEffect::RunHandshake { attempt }]
    }

    /// Whether the seam took the scanned code, on the device with the vault.
    pub fn relay_accepted(
        &mut self,
        attempt: u64,
        accepted: Result<(), String>,
    ) -> Vec<PairingEffect> {
        if attempt != self.attempt {
            return Vec::new();
        }
        match accepted {
            Ok(()) => {
                self.relay_open = true;
                self.phase = PairingPhase::Working(PairingWork::Connecting);
                vec![PairingEffect::RunHandshake { attempt }]
            }
            Err(message) => {
                self.fail(message, PairingFailureKind::RelayCode);
                Vec::new()
            }
        }
    }

    /// The digits the relay handshake reached, or why it stopped.
    pub fn handshake_finished(&mut self, attempt: u64, sas: Result<String, String>) {
        if attempt != self.attempt {
            return;
        }
        match sas {
            Ok(sas) => {
                self.relay_step = 2;
                self.phase = PairingPhase::Comparing { sas };
            }
            Err(message) => {
                self.relay_open = false;
                // On the device that scanned, a handshake that fails before
                // the digits is most often a code drawn for copy and paste:
                // the relay answers that no such pairing exists, and "start
                // again from a new code" would only send the user round the
                // same rescan.
                let kind = match self.intent {
                    PairingIntent::AddAnotherDevice => PairingFailureKind::RelayCode,
                    PairingIntent::AddThisDevice => PairingFailureKind::General,
                };
                self.fail(message, kind);
            }
        }
    }

    /// How `sponsor` or `join` ended, after "Match" over the relay.
    pub fn relay_finished(
        &mut self,
        attempt: u64,
        finished: Result<(), String>,
    ) -> Vec<PairingEffect> {
        if attempt != self.attempt {
            return Vec::new();
        }
        self.relay_open = false;
        match finished {
            Ok(()) => {
                self.phase = PairingPhase::Done;
                Vec::new()
            }
            Err(message) => {
                self.fail(message, PairingFailureKind::General);
                vec![PairingEffect::CancelRelay]
            }
        }
    }

    // --- The manual script ------------------------------------------------

    /// The user pasted `text` at the leg on screen. Over the relay, the code
    /// leg goes to the rendezvous; every other paste is the manual script's.
    pub fn submit(&mut self, text: &str) -> Vec<PairingEffect> {
        let PairingPhase::Awaiting(leg) = self.phase else {
            return Vec::new();
        };
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        if leg == PairingLeg::Code && self.transport == PairingTransport::Relay {
            self.attempt += 1;
            self.relay_step = 1;
            self.pending_scan = Some(text.to_owned());
            self.phase = PairingPhase::Working(PairingWork::ReachingRelay);
            return vec![PairingEffect::ProbeRelay {
                attempt: self.attempt,
            }];
        }
        if matches!(leg, PairingLeg::Request | PairingLeg::Grant) {
            self.phase = PairingPhase::Working(PairingWork::Finishing);
        }
        vec![PairingEffect::Submit {
            leg,
            text: text.to_owned(),
        }]
    }

    /// What the seam made of a paste at `leg`: the text the next leg shows
    /// (the request at the offer, the grant at the request), nothing, or why
    /// it refused.
    pub fn submitted(
        &mut self,
        leg: PairingLeg,
        outcome: Result<Option<String>, String>,
    ) -> Vec<PairingEffect> {
        match outcome {
            Err(message) => {
                self.fail(message, PairingFailureKind::General);
                Vec::new()
            }
            Ok(_) if leg == PairingLeg::Grant => {
                self.phase = PairingPhase::Done;
                Vec::new()
            }
            Ok(hand_off) => {
                self.pending_hand_off = hand_off;
                self.step_after(leg)
            }
        }
    }

    /// The user has handed the text on screen over; move to the next leg.
    pub fn advance(&mut self) -> Vec<PairingEffect> {
        match self.phase {
            PairingPhase::HandOff { leg, .. } => self.step_after(leg),
            _ => Vec::new(),
        }
    }

    /// The Noise message to show at `leg`, or why the seam has none.
    pub fn message_ready(&mut self, leg: PairingLeg, message: Result<String, String>) {
        match message {
            Ok(text) => self.phase = PairingPhase::HandOff { leg, text },
            Err(message) => self.fail(message, PairingFailureKind::General),
        }
    }

    /// The digits, or why the seam has none.
    pub fn sas_ready(&mut self, sas: Result<String, String>) {
        match sas {
            Ok(sas) => self.phase = PairingPhase::Comparing { sas },
            Err(message) => self.fail(message, PairingFailureKind::General),
        }
    }

    /// This user's answer to the digits. Both users answer independently;
    /// "don't match" discards the keys and cannot be walked back.
    pub fn confirm(&mut self, matched: bool) -> Vec<PairingEffect> {
        if !matches!(self.phase, PairingPhase::Comparing { .. }) {
            return Vec::new();
        }
        if self.relay_open {
            if !matched {
                self.relay_open = false;
                self.phase = PairingPhase::Mismatch;
                return vec![PairingEffect::RejectRelay];
            }
            self.relay_step = 3;
            self.phase = PairingPhase::Working(PairingWork::Finishing);
            return vec![PairingEffect::FinishRelay {
                attempt: self.attempt,
            }];
        }
        if matched {
            vec![PairingEffect::ConfirmMatch]
        } else {
            self.phase = PairingPhase::Mismatch;
            vec![PairingEffect::Discard]
        }
    }

    /// Whether the seam took "Match" on the manual script.
    pub fn match_confirmed(&mut self, confirmed: Result<(), String>) -> Vec<PairingEffect> {
        match confirmed {
            Ok(()) => self.step_after(PairingLeg::Compare),
            Err(message) => {
                self.fail(message, PairingFailureKind::General);
                Vec::new()
            }
        }
    }

    /// The offer the vault sealed for the other device, or why not.
    pub fn offer_sealed(&mut self, offer: Result<String, String>) {
        self.message_ready(PairingLeg::Offer, offer);
    }

    /// What this device does at the leg after `leg`.
    fn step_after(&mut self, leg: PairingLeg) -> Vec<PairingEffect> {
        let Some(next) = leg.next() else {
            self.phase = PairingPhase::Done;
            return Vec::new();
        };
        let shows = next.shown_by(self.intent);
        match next {
            PairingLeg::Code => Vec::new(),
            PairingLeg::First | PairingLeg::Second | PairingLeg::Third if shows => {
                vec![PairingEffect::NextMessage(next)]
            }
            PairingLeg::Compare => vec![PairingEffect::ReadSas],
            // The device that seals passes through a spinner while it does.
            PairingLeg::Offer if shows => {
                self.phase = PairingPhase::Working(PairingWork::Finishing);
                vec![PairingEffect::SealOffer]
            }
            // Computed by the submit that just ran, so waiting, not produced.
            PairingLeg::Request | PairingLeg::Grant if shows => {
                match self.pending_hand_off.take() {
                    Some(text) => self.phase = PairingPhase::HandOff { leg: next, text },
                    None => self.fail(
                        "the pairing has no message to show".to_owned(),
                        PairingFailureKind::General,
                    ),
                }
                Vec::new()
            }
            _ => {
                self.phase = PairingPhase::Awaiting(next);
                Vec::new()
            }
        }
    }

    // --- Ways out ---------------------------------------------------------

    /// End it at any point, leaving nothing half-open.
    pub fn cancel(&mut self) -> Vec<PairingEffect> {
        self.attempt += 1;
        let mut effects = Vec::new();
        if std::mem::take(&mut self.relay_open) {
            effects.push(PairingEffect::CancelRelay);
        }
        effects.push(PairingEffect::Discard);
        self.relay_step = 1;
        self.pending_scan = None;
        self.pending_hand_off = None;
        self.phase = Self::rest(self.intent);
        effects
    }

    /// Leave the relay for copy and paste, by the user's choice: the way out
    /// when one device reaches the relay and the other does not.
    pub fn use_manual_instead(&mut self) -> Vec<PairingEffect> {
        let effects = self.cancel();
        self.transport = PairingTransport::Manual;
        self.notice = None;
        effects
    }

    /// The screen went away without Cancel. The same as Cancel unless the
    /// pairing finished: a relay wait otherwise outlives the screen.
    pub fn dismissed(&mut self) -> Vec<PairingEffect> {
        if self.phase == PairingPhase::Done {
            return Vec::new();
        }
        self.cancel()
    }
}

//! A relay pairing run to its end: [`PairingFlow`]'s transitions, with each
//! effect it names carried out against a [`PairingSeam`].
//!
//! [`PairingFlow`] is pure — it says what to call and takes the answer back.
//! A screen runs that loop on its own event loop; a caller that has no event
//! loop of its own (the CLI, one process per pairing) runs it here instead,
//! so neither restates which call answers which effect. [`PairingRun::next`]
//! carries out effects until there is something for the person to see or
//! decide: the code to show, the digits to compare, or the outcome.
//!
//! # Over the relay only
//!
//! A run never walks the copy-and-paste script. When the flow falls back to it
//! — the relay did not answer, or refused to open a session — the run stops
//! with [`RunStop::NoRelay`] and says why, and the caller offers whatever
//! other way it has (the CLI's file exchange).

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use sunrise_core::{Core, Rng};
use sunrise_pairing::PairingPayload;
use zeroize::Zeroize as _;

use super::relay::RelayPairing;
use crate::pairing_flow::{
    ManualReason, PairingEffect, PairingFailureKind, PairingFlow, PairingIntent, PairingLeg,
    PairingPhase, PairingTransport,
};
use crate::recovery::DeviceLabel;

/// The seam calls a relay pairing makes, one per [`PairingEffect`] it can
/// name. Errors are the seam's message, as [`PairingFlow`] takes them.
#[async_trait]
pub trait PairingSeam: Send {
    /// Whether the relay answers at all (`GET /api/v1/health`).
    async fn probe(&mut self) -> bool;
    /// On the new device: open the rendezvous and return the code to show.
    async fn open(&mut self) -> Result<String, String>;
    /// On the existing device: join the rendezvous the scanned code names.
    fn accept(&mut self, code: &str) -> Result<(), String>;
    /// Run the handshake and return the digits.
    async fn handshake(&mut self) -> Result<String, String>;
    /// After "match": sponsor the other device, or join through it.
    async fn finish(&mut self) -> Result<(), String>;
    /// After "don't match": discard the keys and tell the other device.
    async fn reject(&mut self);
    /// Stop: end any wait, discard the keys, drop the session.
    async fn cancel(&mut self);
}

/// Where a run stopped, for the caller to show or answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStop {
    /// The new device's code, for the other device to scan or paste. Reported
    /// once; the next [`PairingRun::next`] waits for the other device.
    ShowCode(String),
    /// Both devices show these digits; answer with [`PairingRun::answer`].
    Compare(String),
    /// Paired.
    Paired,
    /// This user said the digits differ. The other device has been told.
    Mismatch,
    /// The pairing failed: the seam's message, and how to word it.
    Failed {
        /// The seam's message.
        message: String,
        /// How to word it.
        kind: PairingFailureKind,
    },
    /// The relay cannot carry this pairing, and a run does not walk the
    /// copy-and-paste script. `detail` is the relay's refusal, when it gave
    /// one.
    NoRelay {
        /// Why.
        reason: ManualReason,
        /// What the relay said, if it said anything.
        detail: Option<String>,
    },
}

/// One pairing over the relay, run to its end. See the module comment.
#[derive(Debug)]
pub struct PairingRun<S> {
    flow: PairingFlow,
    seam: S,
    queue: VecDeque<PairingEffect>,
    code_shown: bool,
    refusal: Option<String>,
}

impl<S: PairingSeam> PairingRun<S> {
    /// A run on the device being added: it will show a code.
    pub fn add_this_device(seam: S) -> Self {
        let mut flow = PairingFlow::new(PairingIntent::AddThisDevice, true, true);
        let queue = flow.begin().into();
        Self::with(flow, seam, queue)
    }

    /// A run on the device that holds the vault, from the code as scanned.
    pub fn add_another_device(seam: S, code: &str) -> Self {
        let mut flow = PairingFlow::new(PairingIntent::AddAnotherDevice, true, true);
        let queue = flow.submit(code).into();
        Self::with(flow, seam, queue)
    }

    fn with(flow: PairingFlow, seam: S, queue: VecDeque<PairingEffect>) -> Self {
        Self {
            flow,
            seam,
            queue,
            code_shown: false,
            refusal: None,
        }
    }

    /// The flow, for a caller that shows progress.
    #[must_use]
    pub const fn flow(&self) -> &PairingFlow {
        &self.flow
    }

    /// The seam, for what it kept: the joined payload, say.
    pub fn seam_mut(&mut self) -> &mut S {
        &mut self.seam
    }

    /// Carry out effects until there is something to show or decide.
    pub async fn next(&mut self) -> RunStop {
        loop {
            if self.flow.transport() == PairingTransport::Manual {
                self.queue.clear();
                return RunStop::NoRelay {
                    reason: self.flow.notice().unwrap_or(ManualReason::RelayRefused),
                    detail: self.refusal.take(),
                };
            }
            let idle = self.queue.is_empty();
            match self.flow.phase() {
                PairingPhase::HandOff {
                    leg: PairingLeg::Code,
                    text,
                } if !self.code_shown => {
                    self.code_shown = true;
                    return RunStop::ShowCode(text.clone());
                }
                PairingPhase::Comparing { sas } if idle => return RunStop::Compare(sas.clone()),
                PairingPhase::Done if idle => return RunStop::Paired,
                PairingPhase::Mismatch if idle => return RunStop::Mismatch,
                PairingPhase::Failed { message, kind } if idle => {
                    return RunStop::Failed {
                        message: message.clone(),
                        kind: *kind,
                    }
                }
                _ => {}
            }
            let Some(effect) = self.queue.pop_front() else {
                // An empty code: the flow does nothing with it.
                return RunStop::Failed {
                    message: "the pairing has nothing to do: no code was given".to_owned(),
                    kind: PairingFailureKind::RelayCode,
                };
            };
            self.carry_out(effect).await;
        }
    }

    /// This user's answer to the digits [`RunStop::Compare`] showed.
    pub fn answer(&mut self, matched: bool) {
        let effects = self.flow.confirm(matched);
        self.queue.extend(effects);
    }

    /// Stop now, leaving nothing half-open on the relay.
    pub async fn cancel(&mut self) {
        self.queue.clear();
        for effect in self.flow.cancel() {
            self.carry_out(effect).await;
        }
    }

    async fn carry_out(&mut self, effect: PairingEffect) {
        let more = match effect {
            PairingEffect::ProbeRelay { attempt } => {
                let answered = self.seam.probe().await;
                self.flow.relay_probed(attempt, answered)
            }
            PairingEffect::OpenRelay { attempt } => {
                let opened = self.seam.open().await;
                if let Err(why) = &opened {
                    self.refusal = Some(why.clone());
                }
                self.flow.relay_opened(attempt, opened)
            }
            PairingEffect::AcceptRelayCode { attempt, text } => {
                let accepted = self.seam.accept(&text);
                self.flow.relay_accepted(attempt, accepted)
            }
            PairingEffect::RunHandshake { attempt } => {
                let sas = self.seam.handshake().await;
                self.flow.handshake_finished(attempt, sas);
                Vec::new()
            }
            PairingEffect::FinishRelay { attempt } => {
                let finished = self.seam.finish().await;
                self.flow.relay_finished(attempt, finished)
            }
            PairingEffect::RejectRelay => {
                self.seam.reject().await;
                Vec::new()
            }
            PairingEffect::CancelRelay => {
                self.seam.cancel().await;
                Vec::new()
            }
            // `Discard`: the keys are the seam's, and `cancel` and `reject`
            // discard them. The rest are the copy-and-paste script's: the
            // transport check in `next` stops the run before any of them
            // could be asked for.
            PairingEffect::Discard
            | PairingEffect::MintCode
            | PairingEffect::Submit { .. }
            | PairingEffect::NextMessage(_)
            | PairingEffect::ReadSas
            | PairingEffect::ConfirmMatch
            | PairingEffect::SealOffer => Vec::new(),
        };
        self.queue.extend(more);
    }
}

/// Which device this is, and what it needs to finish.
#[derive(Debug)]
pub enum PairingSide {
    /// The device that holds the vault; the cert is signed inside it.
    Sponsor(Arc<Core>),
    /// The device being added: what to call itself, and where its keys come
    /// from.
    Joiner {
        /// The nickname and platform the cert will carry.
        label: DeviceLabel,
        /// The CSPRNG `D_S` and `D_D` are drawn from.
        rng: Arc<dyn Rng>,
    },
}

/// The real seam: the relay at `relay_url`, through [`RelayPairing`].
pub struct RelaySeam {
    relay_url: String,
    bearer: String,
    account_email: String,
    side: PairingSide,
    pairing: Option<RelayPairing>,
    joined: Option<PairingPayload>,
}

impl std::fmt::Debug for RelaySeam {
    /// No bearer and no payload: the one is a credential, the other the vault
    /// root and every Stream key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaySeam")
            .field("relay_url", &self.relay_url)
            .field("pairing", &self.pairing)
            .finish_non_exhaustive()
    }
}

impl RelaySeam {
    /// A seam for `side`, against the relay origin `relay_url`, presenting
    /// `bearer`. `account_email` goes into the new device's code as a
    /// four-byte hash; the existing device ignores it.
    #[must_use]
    pub fn new(relay_url: &str, bearer: &str, account_email: &str, side: PairingSide) -> Self {
        Self {
            relay_url: relay_url.to_owned(),
            bearer: bearer.to_owned(),
            account_email: account_email.to_owned(),
            side,
            pairing: None,
            joined: None,
        }
    }

    /// What a joiner's finished pairing handed it: the payload its vault is
    /// opened with. `None` on the sponsor and before the run is paired.
    pub fn take_joined(&mut self) -> Option<PairingPayload> {
        self.joined.take()
    }

    fn open_pairing(&self) -> Result<&RelayPairing, String> {
        self.pairing
            .as_ref()
            .ok_or_else(|| "this pairing has no relay session".to_owned())
    }
}

#[async_trait]
impl PairingSeam for RelaySeam {
    async fn probe(&mut self) -> bool {
        super::relay_answers(&self.relay_url).await
    }

    async fn open(&mut self) -> Result<String, String> {
        let pairing = RelayPairing::offer(&self.relay_url, &self.bearer, &self.account_email)
            .await
            .map_err(|e| e.to_string())?;
        let qr = pairing
            .qr_payload()
            .ok_or_else(|| "the new device has no QR".to_owned())?;
        self.pairing = Some(pairing);
        Ok(qr)
    }

    fn accept(&mut self, code: &str) -> Result<(), String> {
        let pairing =
            RelayPairing::accept(code, &self.relay_url, &self.bearer).map_err(|e| e.to_string())?;
        self.pairing = Some(pairing);
        Ok(())
    }

    async fn handshake(&mut self) -> Result<String, String> {
        self.open_pairing()?
            .handshake()
            .await
            .map_err(|e| e.to_string())
    }

    async fn finish(&mut self) -> Result<(), String> {
        let pairing = self.open_pairing()?;
        match &self.side {
            PairingSide::Sponsor(core) => pairing.sponsor(core).await.map_err(|e| e.to_string()),
            PairingSide::Joiner { label, rng } => {
                let mut seed_s = vec![0u8; 32];
                let mut seed_d = vec![0u8; 32];
                rng.fill_bytes(&mut seed_s);
                rng.fill_bytes(&mut seed_d);
                let joined = pairing
                    .join(
                        label.nickname.clone(),
                        label.platform.clone(),
                        seed_s.clone(),
                        seed_d.clone(),
                    )
                    .await;
                seed_s.zeroize();
                seed_d.zeroize();
                self.joined = Some(joined.map_err(|e| e.to_string())?);
                Ok(())
            }
        }
    }

    async fn reject(&mut self) {
        if let Some(pairing) = &self.pairing {
            pairing.reject().await;
        }
    }

    async fn cancel(&mut self) {
        if let Some(pairing) = &self.pairing {
            pairing.cancel().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seam that answers from a script and records what it was asked.
    #[derive(Debug, Default)]
    struct Fake {
        relay_down: bool,
        open: Option<Result<String, String>>,
        accept: Option<Result<(), String>>,
        handshake: Option<Result<String, String>>,
        finish: Option<Result<(), String>>,
        calls: Vec<&'static str>,
    }

    impl Fake {
        fn happy() -> Self {
            Self {
                open: Some(Ok("CODE".into())),
                accept: Some(Ok(())),
                handshake: Some(Ok("123456".into())),
                finish: Some(Ok(())),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl PairingSeam for Fake {
        async fn probe(&mut self) -> bool {
            self.calls.push("probe");
            !self.relay_down
        }
        async fn open(&mut self) -> Result<String, String> {
            self.calls.push("open");
            self.open.take().expect("open is asked once")
        }
        fn accept(&mut self, code: &str) -> Result<(), String> {
            assert_eq!(
                code, "CODE",
                "the code goes to the seam as scanned, trimmed"
            );
            self.calls.push("accept");
            self.accept.take().expect("accept is asked once")
        }
        async fn handshake(&mut self) -> Result<String, String> {
            self.calls.push("handshake");
            self.handshake.take().expect("handshake is asked once")
        }
        async fn finish(&mut self) -> Result<(), String> {
            self.calls.push("finish");
            self.finish.take().expect("finish is asked once")
        }
        async fn reject(&mut self) {
            self.calls.push("reject");
        }
        async fn cancel(&mut self) {
            self.calls.push("cancel");
        }
    }

    #[tokio::test]
    async fn the_new_device_shows_its_code_then_the_digits_then_pairs() {
        let mut run = PairingRun::add_this_device(Fake::happy());
        assert_eq!(run.next().await, RunStop::ShowCode("CODE".into()));
        assert_eq!(run.next().await, RunStop::Compare("123456".into()));
        assert_eq!(run.flow().progress(), Some((2, 3)));
        run.answer(true);
        assert_eq!(run.next().await, RunStop::Paired);
        assert_eq!(
            run.seam_mut().calls,
            ["probe", "open", "handshake", "finish"]
        );
    }

    #[tokio::test]
    async fn the_existing_device_goes_from_the_code_to_the_digits() {
        let mut run = PairingRun::add_another_device(Fake::happy(), "  CODE \n");
        assert_eq!(run.next().await, RunStop::Compare("123456".into()));
        run.answer(true);
        assert_eq!(run.next().await, RunStop::Paired);
        assert_eq!(
            run.seam_mut().calls,
            ["probe", "accept", "handshake", "finish"]
        );
    }

    #[tokio::test]
    async fn digits_that_differ_tell_the_other_device_and_finish_nothing() {
        let mut run = PairingRun::add_another_device(Fake::happy(), "CODE");
        assert!(matches!(run.next().await, RunStop::Compare(_)));
        run.answer(false);
        assert_eq!(run.next().await, RunStop::Mismatch);
        assert_eq!(
            run.seam_mut().calls,
            ["probe", "accept", "handshake", "reject"]
        );
    }

    #[tokio::test]
    async fn a_relay_that_does_not_answer_stops_the_run_before_any_session() {
        let fake = Fake {
            relay_down: true,
            ..Fake::happy()
        };
        let mut run = PairingRun::add_this_device(fake);
        assert_eq!(
            run.next().await,
            RunStop::NoRelay {
                reason: ManualReason::RelayDidNotAnswer,
                detail: None
            }
        );
        let mut run = PairingRun::add_another_device(
            Fake {
                relay_down: true,
                ..Fake::happy()
            },
            "CODE",
        );
        assert_eq!(
            run.next().await,
            RunStop::NoRelay {
                reason: ManualReason::RelayDidNotAnswerHere,
                detail: None
            }
        );
        assert_eq!(run.seam_mut().calls, ["probe"]);
    }

    #[tokio::test]
    async fn a_refused_session_carries_the_relays_words() {
        let fake = Fake {
            open: Some(Err("Try again in 600 s (Retry-After)".into())),
            ..Fake::happy()
        };
        let mut run = PairingRun::add_this_device(fake);
        assert_eq!(
            run.next().await,
            RunStop::NoRelay {
                reason: ManualReason::RelayRefused,
                detail: Some("Try again in 600 s (Retry-After)".into())
            }
        );
    }

    #[tokio::test]
    async fn a_handshake_that_fails_on_the_scanning_device_points_at_the_code() {
        let fake = Fake {
            handshake: Some(Err("the pairing ended".into())),
            ..Fake::happy()
        };
        let mut run = PairingRun::add_another_device(fake, "CODE");
        assert_eq!(
            run.next().await,
            RunStop::Failed {
                message: "the pairing ended".into(),
                kind: PairingFailureKind::RelayCode
            }
        );
    }

    #[tokio::test]
    async fn a_finish_that_fails_drops_the_session() {
        let fake = Fake {
            finish: Some(Err("the other device left".into())),
            ..Fake::happy()
        };
        let mut run = PairingRun::add_this_device(fake);
        assert!(matches!(run.next().await, RunStop::ShowCode(_)));
        assert!(matches!(run.next().await, RunStop::Compare(_)));
        run.answer(true);
        assert_eq!(
            run.next().await,
            RunStop::Failed {
                message: "the other device left".into(),
                kind: PairingFailureKind::General
            }
        );
        assert_eq!(run.seam_mut().calls.last(), Some(&"cancel"));
    }

    #[tokio::test]
    async fn cancelling_at_the_digits_drops_the_session() {
        let mut run = PairingRun::add_this_device(Fake::happy());
        assert!(matches!(run.next().await, RunStop::ShowCode(_)));
        assert!(matches!(run.next().await, RunStop::Compare(_)));
        run.cancel().await;
        assert_eq!(run.seam_mut().calls.last(), Some(&"cancel"));
    }

    #[tokio::test]
    async fn an_empty_code_is_refused_without_asking_the_relay() {
        let mut run = PairingRun::add_another_device(Fake::happy(), "   ");
        assert!(matches!(run.next().await, RunStop::Failed { .. }));
        assert!(run.seam_mut().calls.is_empty());
    }
}

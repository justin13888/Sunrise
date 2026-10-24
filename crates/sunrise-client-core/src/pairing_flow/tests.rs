//! The script, walked with fake seam answers. Ported from the Apple app's
//! `PairingModelTests` and `RelayPairingModelTests`, minus what needed the
//! real seam (those stay where the seam is).

use super::*;

use PairingEffect as E;
use PairingIntent::{AddAnotherDevice, AddThisDevice};

fn relay(intent: PairingIntent) -> PairingFlow {
    PairingFlow::new(intent, true, true)
}

fn manual(intent: PairingIntent) -> PairingFlow {
    PairingFlow::new(intent, false, false)
}

fn hand_off(leg: PairingLeg, text: &str) -> PairingPhase {
    PairingPhase::HandOff {
        leg,
        text: text.into(),
    }
}

#[test]
fn the_table_of_who_shows_what() {
    use PairingLeg::*;
    let added: Vec<_> = PairingLeg::ALL
        .iter()
        .map(|l| l.shown_by(AddThisDevice))
        .collect();
    let holder: Vec<_> = PairingLeg::ALL
        .iter()
        .map(|l| l.shown_by(AddAnotherDevice))
        .collect();
    assert_eq!(added, [true, true, false, true, false, false, true, false]);
    assert_eq!(
        holder,
        [false, false, true, false, false, true, false, true]
    );
    assert_eq!(Grant.next(), None);
    assert_eq!(Code.number(), 1);
    assert_eq!(Grant.number(), 8);
}

#[test]
fn without_a_relay_or_an_account_it_is_copy_and_paste_and_says_which() {
    let none = PairingFlow::new(AddThisDevice, false, true);
    assert_eq!(none.transport(), PairingTransport::Manual);
    assert_eq!(none.notice(), Some(ManualReason::NoRelayConfigured));
    let signed_out = PairingFlow::new(AddThisDevice, true, false);
    assert_eq!(signed_out.transport(), PairingTransport::Manual);
    assert_eq!(signed_out.notice(), Some(ManualReason::NotSignedIn));
    let both = relay(AddThisDevice);
    assert_eq!(
        (both.transport(), both.notice()),
        (PairingTransport::Relay, None)
    );
    assert_eq!(
        *relay(AddAnotherDevice).phase(),
        PairingPhase::Awaiting(PairingLeg::Code)
    );
}

#[test]
fn the_new_device_shows_the_code_then_the_digits_then_finishes_over_the_relay() {
    let mut f = relay(AddThisDevice);
    assert_eq!(f.begin(), vec![E::ProbeRelay { attempt: 1 }]);
    assert_eq!(f.progress(), Some((1, 3)));
    assert_eq!(f.relay_probed(1, true), vec![E::OpenRelay { attempt: 1 }]);
    assert_eq!(
        f.relay_opened(1, Ok("QR".into())),
        vec![E::RunHandshake { attempt: 1 }]
    );
    assert_eq!(*f.phase(), hand_off(PairingLeg::Code, "QR"));
    f.handshake_finished(1, Ok("123456".into()));
    assert_eq!(
        *f.phase(),
        PairingPhase::Comparing {
            sas: "123456".into()
        }
    );
    assert_eq!(f.progress(), Some((2, 3)), "scan, compare, done");
    assert_eq!(f.confirm(true), vec![E::FinishRelay { attempt: 1 }]);
    assert_eq!(f.progress(), Some((3, 3)));
    assert!(f.relay_finished(1, Ok(())).is_empty());
    assert_eq!(*f.phase(), PairingPhase::Done);
    assert_eq!(f.progress(), None);
}

#[test]
fn the_existing_device_hands_the_scanned_text_to_the_relay() {
    let mut f = relay(AddAnotherDevice);
    assert_eq!(f.progress(), Some((1, 3)));
    assert_eq!(
        f.submit("  SUNRISE-PAIR-SCANNED \n"),
        vec![E::ProbeRelay { attempt: 1 }]
    );
    assert_eq!(
        f.relay_probed(1, true),
        vec![E::AcceptRelayCode {
            attempt: 1,
            text: "SUNRISE-PAIR-SCANNED".into()
        }]
    );
    assert_eq!(
        f.relay_accepted(1, Ok(())),
        vec![E::RunHandshake { attempt: 1 }]
    );
    assert_eq!(*f.phase(), PairingPhase::Working(PairingWork::Connecting));
    f.handshake_finished(1, Ok("123456".into()));
    assert_eq!(f.confirm(true), vec![E::FinishRelay { attempt: 1 }]);
}

#[test]
fn saying_the_digits_differ_rejects_through_the_relay_and_cannot_be_walked_back() {
    let mut f = relay(AddThisDevice);
    f.begin();
    f.relay_probed(1, true);
    f.relay_opened(1, Ok("QR".into()));
    f.handshake_finished(1, Ok("123456".into()));
    assert_eq!(f.confirm(false), vec![E::RejectRelay]);
    assert_eq!(*f.phase(), PairingPhase::Mismatch);
    assert!(
        f.confirm(true).is_empty(),
        "there is nothing left to confirm"
    );
    assert_eq!(*f.phase(), PairingPhase::Mismatch);
    assert!(
        !f.offers_manual_fallback(),
        "a mismatch is the check working"
    );
}

#[test]
fn a_relay_that_does_not_answer_falls_back_to_copy_and_paste_and_says_so() {
    let mut f = relay(AddThisDevice);
    f.begin();
    assert_eq!(f.relay_probed(1, false), vec![E::MintCode]);
    assert_eq!(f.transport(), PairingTransport::Manual);
    assert_eq!(f.notice(), Some(ManualReason::RelayDidNotAnswer));
    f.code_minted(Ok("CODE".into()));
    assert_eq!(*f.phase(), hand_off(PairingLeg::Code, "CODE"));
    assert_eq!(f.progress(), Some((1, 8)), "the eight-leg script from here");
}

#[test]
fn a_relay_that_refuses_the_session_falls_back_too() {
    let mut f = relay(AddThisDevice);
    f.begin();
    f.relay_probed(1, true);
    assert_eq!(f.relay_opened(1, Err("429".into())), vec![E::MintCode]);
    assert_eq!(f.notice(), Some(ManualReason::RelayRefused));
    assert_eq!(f.transport(), PairingTransport::Manual);
}

#[test]
fn the_existing_device_that_cannot_reach_the_relay_asks_for_a_fresh_code() {
    let mut f = relay(AddAnotherDevice);
    f.submit("SCANNED");
    assert!(f.relay_probed(1, false).is_empty());
    assert_eq!(f.notice(), Some(ManualReason::RelayDidNotAnswerHere));
    assert_eq!(*f.phase(), PairingPhase::Awaiting(PairingLeg::Code));
    // The next paste is the manual script's.
    assert_eq!(
        f.submit("CODE"),
        vec![E::Submit {
            leg: PairingLeg::Code,
            text: "CODE".into()
        }]
    );
}

#[test]
fn a_scanned_code_the_relay_cannot_pair_with_is_worded_as_one() {
    let mut f = relay(AddAnotherDevice);
    f.submit("SCANNED");
    f.relay_probed(1, true);
    f.relay_accepted(1, Err("not a pairing code".into()));
    assert_eq!(
        *f.phase(),
        PairingPhase::Failed {
            message: "not a pairing code".into(),
            kind: PairingFailureKind::RelayCode
        }
    );
    assert!(f.phase().is_outcome());
}

#[test]
fn a_late_answer_for_an_abandoned_attempt_changes_nothing() {
    let mut f = relay(AddThisDevice);
    f.begin();
    assert_eq!(f.cancel(), vec![E::Discard]);
    assert_eq!(*f.phase(), PairingPhase::Idle);
    assert!(f.relay_probed(1, false).is_empty());
    assert_eq!(
        f.transport(),
        PairingTransport::Relay,
        "a stale probe does not fall back"
    );
    // A session opened for the abandoned attempt is cancelled, not shown.
    assert_eq!(f.relay_opened(1, Ok("QR".into())), vec![E::CancelRelay]);
    assert_eq!(*f.phase(), PairingPhase::Idle);
}

#[test]
fn choosing_copy_and_paste_cancels_the_rendezvous() {
    let mut f = relay(AddThisDevice);
    f.begin();
    f.relay_probed(1, true);
    f.relay_opened(1, Ok("QR".into()));
    assert!(f.offers_manual_fallback());
    assert_eq!(f.use_manual_instead(), vec![E::CancelRelay, E::Discard]);
    assert_eq!(f.transport(), PairingTransport::Manual);
    assert_eq!(f.notice(), None);
    assert_eq!(*f.phase(), PairingPhase::Idle);
    assert_eq!(f.progress(), None);
    assert!(!f.offers_manual_fallback(), "already copy and paste");
}

#[test]
fn only_done_mismatch_and_failed_are_outcomes() {
    let failed = PairingPhase::Failed {
        message: String::new(),
        kind: PairingFailureKind::General,
    };
    for p in [PairingPhase::Done, PairingPhase::Mismatch, failed] {
        assert!(p.is_outcome(), "{p:?}");
    }
    for p in [
        PairingPhase::Idle,
        hand_off(PairingLeg::Code, "x"),
        PairingPhase::Awaiting(PairingLeg::Code),
        PairingPhase::Comparing { sas: "1".into() },
        PairingPhase::Working(PairingWork::Finishing),
    ] {
        assert!(!p.is_outcome(), "{p:?}");
    }
}

/// Both halves of the manual script, walked against each other: what one
/// device shows the other pastes, leg by leg, until the grant.
#[test]
fn the_manual_script_walks_eight_legs_between_two_devices() {
    let mut added = manual(AddThisDevice);
    let mut holder = manual(AddAnotherDevice);
    assert_eq!(added.begin(), vec![E::MintCode]);
    added.code_minted(Ok("CODE".into()));
    assert_eq!(added.progress(), Some((1, 8)));

    // Code: the holder pastes it, then expects the first message.
    assert_eq!(
        holder.submit("CODE"),
        vec![E::Submit {
            leg: PairingLeg::Code,
            text: "CODE".into()
        }]
    );
    assert!(holder.submitted(PairingLeg::Code, Ok(None)).is_empty());
    assert_eq!(*holder.phase(), PairingPhase::Awaiting(PairingLeg::First));
    assert_eq!(added.advance(), vec![E::NextMessage(PairingLeg::First)]);
    added.message_ready(PairingLeg::First, Ok("M1".into()));

    // First: holder pastes, then shows the second.
    holder.submit("M1");
    assert_eq!(
        holder.submitted(PairingLeg::First, Ok(None)),
        vec![E::NextMessage(PairingLeg::Second)]
    );
    holder.message_ready(PairingLeg::Second, Ok("M2".into()));
    assert!(added.advance().is_empty());
    assert_eq!(*added.phase(), PairingPhase::Awaiting(PairingLeg::Second));

    // Second: added pastes, then shows the third.
    added.submit("M2");
    assert_eq!(
        added.submitted(PairingLeg::Second, Ok(None)),
        vec![E::NextMessage(PairingLeg::Third)]
    );
    added.message_ready(PairingLeg::Third, Ok("M3".into()));
    assert!(holder.advance().is_empty());
    holder.submit("M3");
    assert_eq!(
        holder.submitted(PairingLeg::Third, Ok(None)),
        vec![E::ReadSas]
    );
    holder.sas_ready(Ok("424242".into()));
    assert_eq!(added.advance(), vec![E::ReadSas]);
    added.sas_ready(Ok("424242".into()));
    assert_eq!(added.progress(), Some((5, 8)), "the SAS is the fifth leg");

    // Compare: both say match. The holder seals the offer; the added waits.
    assert_eq!(holder.confirm(true), vec![E::ConfirmMatch]);
    assert_eq!(holder.match_confirmed(Ok(())), vec![E::SealOffer]);
    assert_eq!(
        *holder.phase(),
        PairingPhase::Working(PairingWork::Finishing)
    );
    holder.offer_sealed(Ok("OFFER".into()));
    assert_eq!(added.confirm(true), vec![E::ConfirmMatch]);
    assert!(added.match_confirmed(Ok(())).is_empty());
    assert_eq!(*added.phase(), PairingPhase::Awaiting(PairingLeg::Offer));

    // Offer → request: the request is what the added device's paste made.
    added.submit("OFFER");
    assert!(added
        .submitted(PairingLeg::Offer, Ok(Some("REQUEST".into())))
        .is_empty());
    assert_eq!(*added.phase(), hand_off(PairingLeg::Request, "REQUEST"));
    assert!(holder.advance().is_empty());
    assert_eq!(*holder.phase(), PairingPhase::Awaiting(PairingLeg::Request));

    // Request → grant, sealed by the holder while it shows a spinner.
    assert_eq!(
        holder.submit("REQUEST"),
        vec![E::Submit {
            leg: PairingLeg::Request,
            text: "REQUEST".into()
        }]
    );
    assert_eq!(
        *holder.phase(),
        PairingPhase::Working(PairingWork::Finishing)
    );
    holder.submitted(PairingLeg::Request, Ok(Some("GRANT".into())));
    assert_eq!(*holder.phase(), hand_off(PairingLeg::Grant, "GRANT"));
    assert!(added.advance().is_empty());
    assert_eq!(*added.phase(), PairingPhase::Awaiting(PairingLeg::Grant));

    // Grant: the added device opens it and is done; the holder moves past it.
    added.submit("GRANT");
    added.submitted(PairingLeg::Grant, Ok(None));
    assert_eq!(*added.phase(), PairingPhase::Done);
    assert!(holder.advance().is_empty());
    assert_eq!(*holder.phase(), PairingPhase::Done);
}

#[test]
fn a_manual_mismatch_discards_the_keys() {
    let mut f = manual(AddThisDevice);
    f.sas_ready(Ok("1".into()));
    assert_eq!(f.confirm(false), vec![E::Discard]);
    assert_eq!(*f.phase(), PairingPhase::Mismatch);
}

#[test]
fn a_refused_paste_fails_and_cancel_returns_to_the_start() {
    let mut holder = manual(AddAnotherDevice);
    holder.submit("garbage");
    holder.submitted(PairingLeg::Code, Err("not a pairing code".into()));
    assert!(holder.phase().is_outcome());
    assert_eq!(holder.cancel(), vec![E::Discard]);
    assert_eq!(*holder.phase(), PairingPhase::Awaiting(PairingLeg::Code));
    assert!(
        holder.submit("   ").is_empty(),
        "a blank paste is not a submit"
    );
}

#[test]
fn a_done_screen_going_away_cancels_nothing() {
    let mut f = manual(AddThisDevice);
    f.code_minted(Ok("C".into()));
    assert_eq!(f.dismissed(), vec![E::Discard]);
    let mut done = manual(AddAnotherDevice);
    done.submitted(PairingLeg::Grant, Ok(None));
    assert!(done.dismissed().is_empty());
}

#[test]
fn the_digits_are_read_in_two_threes() {
    assert_eq!(grouped_sas("123456"), "123 456");
    assert_eq!(grouped_sas("12345"), "12345", "not six digits: as given");
    assert_eq!(grouped_sas("12a456"), "12a456");
}

#[test]
fn a_typed_answer_matches_only_the_same_six_digits() {
    for typed in ["123456", "123 456", " 123-456\n"] {
        assert!(sas_matches(typed, "123456"), "{typed:?}");
    }
    for typed in ["", "  ", "123457", "12345", "1234567", "654321"] {
        assert!(!sas_matches(typed, "123456"), "{typed:?}");
    }
}

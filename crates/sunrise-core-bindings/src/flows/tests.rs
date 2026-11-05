//! The lowering, driven: each exported object walked along its shortest paths.
//!
//! What the machines decide is pinned by [`sunrise_client_core`]'s own tests.
//! These pin what this layer adds, which until now only Swift reached: that
//! each phase, effect and step crosses as itself, and that an answer's arguments
//! become the `Result` the machine reads.

use std::sync::Mutex;

use sunrise_client_core::vault_session::{LockRepair, VaultStep};

use super::{
    AccountCredentialStore, AccountSession, AccountSessionFailure, AccountSessionState,
    CoreLockReason, CoreSignOutResidue, CredentialRead, ManualReason, PairingEffect,
    PairingFailureKind, PairingFlow, PairingIntent, PairingLeg, PairingPhase, PairingProgress,
    PairingTransport, PairingWork, RenewalAnswer, RenewalStep, SignInStep, StoredRoot, TextAnswer,
    UnitAnswer, VaultPhase, VaultSession,
};
use crate::LoginCredentials;

fn text(text: &str) -> TextAnswer {
    TextAnswer::Ok { text: text.into() }
}

fn text_err(message: &str) -> TextAnswer {
    TextAnswer::Err {
        message: message.into(),
    }
}

fn unit_err(message: &str) -> UnitAnswer {
    UnitAnswer::Err {
        message: message.into(),
    }
}

fn failed(message: &str, kind: PairingFailureKind) -> PairingPhase {
    PairingPhase::Failed {
        message: message.into(),
        kind,
    }
}

// --- Answers ---------------------------------------------------------------

#[test]
fn an_answer_becomes_the_result_it_names() {
    assert_eq!(Result::from(text("t")), Ok("t".to_owned()));
    assert_eq!(
        Result::<String, _>::from(text_err("e")),
        Err("e".to_owned())
    );
    assert_eq!(Result::from(UnitAnswer::Ok), Ok::<_, String>(()));
    assert_eq!(Result::<(), _>::from(unit_err("e")), Err("e".to_owned()));
}

// --- Pairing ---------------------------------------------------------------

#[test]
fn a_new_pairing_says_which_transport_and_why() {
    let relay = PairingFlow::new(PairingIntent::AddThisDevice, true, true);
    assert_eq!(relay.transport(), PairingTransport::Relay);
    assert_eq!(relay.notice(), None);
    assert!(relay.offers_manual_fallback());
    assert_eq!(relay.phase(), PairingPhase::Idle);
    assert_eq!(relay.progress(), None);

    let no_relay = PairingFlow::new(PairingIntent::AddAnotherDevice, false, true);
    assert_eq!(no_relay.transport(), PairingTransport::Manual);
    assert_eq!(no_relay.notice(), Some(ManualReason::NoRelayConfigured));
    assert!(!no_relay.offers_manual_fallback());
    assert_eq!(
        no_relay.phase(),
        PairingPhase::Awaiting {
            leg: PairingLeg::Code
        }
    );

    let signed_out = PairingFlow::new(PairingIntent::AddThisDevice, true, false);
    assert_eq!(signed_out.notice(), Some(ManualReason::NotSignedIn));
}

#[test]
fn the_device_being_added_pairs_over_the_relay() {
    let flow = PairingFlow::new(PairingIntent::AddThisDevice, true, true);

    assert_eq!(flow.begin(), [PairingEffect::ProbeRelay { attempt: 1 }]);
    assert_eq!(
        flow.phase(),
        PairingPhase::Working {
            work: PairingWork::ReachingRelay
        }
    );
    assert_eq!(flow.progress(), Some(PairingProgress { leg: 1, of: 3 }));

    assert_eq!(
        flow.relay_probed(1, true),
        [PairingEffect::OpenRelay { attempt: 1 }]
    );
    assert_eq!(
        flow.relay_opened(1, text("qr")),
        [PairingEffect::RunHandshake { attempt: 1 }]
    );
    assert_eq!(
        flow.phase(),
        PairingPhase::HandOff {
            leg: PairingLeg::Code,
            text: "qr".into()
        }
    );

    flow.handshake_finished(1, text("123456"));
    assert_eq!(
        flow.phase(),
        PairingPhase::Comparing {
            sas: "123456".into()
        }
    );
    assert_eq!(flow.progress(), Some(PairingProgress { leg: 2, of: 3 }));

    assert_eq!(
        flow.confirm(true),
        [PairingEffect::FinishRelay { attempt: 1 }]
    );
    assert_eq!(flow.progress(), Some(PairingProgress { leg: 3, of: 3 }));
    assert_eq!(flow.relay_finished(1, UnitAnswer::Ok), []);
    assert_eq!(flow.phase(), PairingPhase::Done);
    assert_eq!(flow.progress(), None);
    assert_eq!(flow.dismissed(), [], "a finished pairing leaves nothing");
}

#[test]
fn the_device_with_the_vault_scans_and_rejects_the_digits_over_the_relay() {
    let flow = PairingFlow::new(PairingIntent::AddAnotherDevice, true, true);

    assert_eq!(
        flow.submit("  code ".into()),
        [PairingEffect::ProbeRelay { attempt: 1 }]
    );
    assert_eq!(
        flow.relay_probed(1, true),
        [PairingEffect::AcceptRelayCode {
            attempt: 1,
            text: "code".into()
        }]
    );
    assert_eq!(
        flow.relay_accepted(1, UnitAnswer::Ok),
        [PairingEffect::RunHandshake { attempt: 1 }]
    );
    assert_eq!(
        flow.phase(),
        PairingPhase::Working {
            work: PairingWork::Connecting
        }
    );
    flow.handshake_finished(1, text("123456"));

    assert_eq!(flow.confirm(false), [PairingEffect::RejectRelay]);
    assert_eq!(flow.phase(), PairingPhase::Mismatch);
    assert!(!flow.offers_manual_fallback());
}

#[test]
fn the_device_being_added_pairs_by_copy_and_paste() {
    let flow = PairingFlow::new(PairingIntent::AddThisDevice, false, false);

    assert_eq!(flow.begin(), [PairingEffect::MintCode]);
    flow.code_minted(text("code"));
    assert_eq!(
        flow.progress(),
        Some(PairingProgress { leg: 1, of: 8 }),
        "the code is the first of eight legs"
    );

    assert_eq!(
        flow.advance(),
        [PairingEffect::NextMessage {
            leg: PairingLeg::First
        }]
    );
    flow.message_ready(PairingLeg::First, text("m1"));
    assert_eq!(flow.advance(), []);
    assert_eq!(
        flow.submit(" m2 ".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::Second,
            text: "m2".into()
        }]
    );
    assert_eq!(
        flow.submitted(PairingLeg::Second, None, None),
        [PairingEffect::NextMessage {
            leg: PairingLeg::Third
        }]
    );
    flow.message_ready(PairingLeg::Third, text("m3"));
    assert_eq!(flow.advance(), [PairingEffect::ReadSas]);
    flow.sas_ready(text("123456"));
    assert_eq!(flow.confirm(true), [PairingEffect::ConfirmMatch]);
    assert_eq!(flow.match_confirmed(UnitAnswer::Ok), []);
    assert_eq!(
        flow.phase(),
        PairingPhase::Awaiting {
            leg: PairingLeg::Offer
        }
    );

    assert_eq!(
        flow.submit("offer".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::Offer,
            text: "offer".into()
        }]
    );
    assert_eq!(
        flow.submitted(PairingLeg::Offer, Some("request".into()), None),
        []
    );
    assert_eq!(
        flow.phase(),
        PairingPhase::HandOff {
            leg: PairingLeg::Request,
            text: "request".into()
        },
        "the hand-off a submit returned is what the next leg shows"
    );
    assert_eq!(flow.advance(), []);
    assert_eq!(
        flow.submit("grant".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::Grant,
            text: "grant".into()
        }]
    );
    assert_eq!(flow.submitted(PairingLeg::Grant, None, None), []);
    assert_eq!(flow.phase(), PairingPhase::Done);
}

#[test]
fn the_device_with_the_vault_pairs_by_copy_and_paste() {
    let flow = PairingFlow::new(PairingIntent::AddAnotherDevice, false, false);

    assert_eq!(
        flow.submit("code".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::Code,
            text: "code".into()
        }]
    );
    assert_eq!(flow.submitted(PairingLeg::Code, None, None), []);
    assert_eq!(
        flow.submit("m1".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::First,
            text: "m1".into()
        }]
    );
    assert_eq!(
        flow.submitted(PairingLeg::First, None, None),
        [PairingEffect::NextMessage {
            leg: PairingLeg::Second
        }]
    );
    flow.message_ready(PairingLeg::Second, text("m2"));
    assert_eq!(flow.advance(), []);
    assert_eq!(
        flow.submit("m3".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::Third,
            text: "m3".into()
        }]
    );
    assert_eq!(
        flow.submitted(PairingLeg::Third, None, None),
        [PairingEffect::ReadSas]
    );
    flow.sas_ready(text("123456"));
    assert_eq!(flow.confirm(true), [PairingEffect::ConfirmMatch]);
    assert_eq!(
        flow.match_confirmed(UnitAnswer::Ok),
        [PairingEffect::SealOffer]
    );
    flow.offer_sealed(text("offer"));
    assert_eq!(
        flow.phase(),
        PairingPhase::HandOff {
            leg: PairingLeg::Offer,
            text: "offer".into()
        }
    );
    assert_eq!(flow.advance(), []);
    assert_eq!(
        flow.submit("request".into()),
        [PairingEffect::Submit {
            leg: PairingLeg::Request,
            text: "request".into()
        }]
    );
    assert_eq!(
        flow.submitted(PairingLeg::Request, Some("grant".into()), None),
        []
    );
    assert_eq!(flow.advance(), [], "the grant is the last leg");
    assert_eq!(flow.phase(), PairingPhase::Done);
}

/// `submitted`'s two optionals are the one place here that decides something:
/// an error wins, and whatever hand-off came with it is never shown.
#[test]
fn a_submit_that_failed_fails_whatever_else_it_carried() {
    let flow = PairingFlow::new(PairingIntent::AddThisDevice, false, false);
    assert_eq!(
        flow.submitted(
            PairingLeg::Offer,
            Some("request".into()),
            Some("refused".into())
        ),
        []
    );
    assert_eq!(flow.phase(), failed("refused", PairingFailureKind::General));

    let flow = PairingFlow::new(PairingIntent::AddThisDevice, false, false);
    assert_eq!(
        flow.submitted(PairingLeg::Second, None, Some("refused".into())),
        []
    );
    assert_eq!(flow.phase(), failed("refused", PairingFailureKind::General));
}

#[test]
fn a_seam_failure_at_each_answer_is_the_failed_phase() {
    let general = failed("no", PairingFailureKind::General);
    let new = || PairingFlow::new(PairingIntent::AddThisDevice, true, true);

    let flow = new();
    flow.code_minted(text_err("no"));
    assert_eq!(flow.phase(), general);

    let flow = new();
    flow.message_ready(PairingLeg::First, text_err("no"));
    assert_eq!(flow.phase(), general);

    let flow = new();
    flow.sas_ready(text_err("no"));
    assert_eq!(flow.phase(), general);

    let flow = new();
    flow.offer_sealed(text_err("no"));
    assert_eq!(flow.phase(), general);

    let flow = new();
    assert_eq!(flow.match_confirmed(unit_err("no")), []);
    assert_eq!(flow.phase(), general);

    let flow = new();
    assert_eq!(flow.begin(), [PairingEffect::ProbeRelay { attempt: 1 }]);
    flow.handshake_finished(1, text_err("no"));
    assert_eq!(flow.phase(), general);

    let flow = new();
    assert_eq!(flow.begin(), [PairingEffect::ProbeRelay { attempt: 1 }]);
    assert_eq!(
        flow.relay_finished(1, unit_err("no")),
        [PairingEffect::CancelRelay]
    );
    assert_eq!(flow.phase(), general);

    let flow = PairingFlow::new(PairingIntent::AddAnotherDevice, true, true);
    assert_eq!(
        flow.submit("code".into()),
        [PairingEffect::ProbeRelay { attempt: 1 }]
    );
    assert_eq!(flow.relay_accepted(1, unit_err("no")), []);
    assert_eq!(flow.phase(), failed("no", PairingFailureKind::RelayCode));
}

#[test]
fn a_relay_that_fails_falls_back_to_copy_and_paste() {
    let flow = PairingFlow::new(PairingIntent::AddThisDevice, true, true);
    assert_eq!(flow.begin(), [PairingEffect::ProbeRelay { attempt: 1 }]);
    assert_eq!(flow.relay_probed(1, false), [PairingEffect::MintCode]);
    assert_eq!(flow.transport(), PairingTransport::Manual);
    assert_eq!(flow.notice(), Some(ManualReason::RelayDidNotAnswer));

    let flow = PairingFlow::new(PairingIntent::AddThisDevice, true, true);
    assert_eq!(flow.begin(), [PairingEffect::ProbeRelay { attempt: 1 }]);
    assert_eq!(
        flow.relay_opened(1, text_err("no")),
        [PairingEffect::MintCode]
    );
    assert_eq!(flow.notice(), Some(ManualReason::RelayRefused));
}

#[test]
fn every_way_out_says_what_to_tear_down() {
    let open_relay = || {
        let flow = PairingFlow::new(PairingIntent::AddThisDevice, true, true);
        assert_eq!(flow.begin(), [PairingEffect::ProbeRelay { attempt: 1 }]);
        assert_eq!(
            flow.relay_opened(1, text("qr")),
            [PairingEffect::RunHandshake { attempt: 1 }]
        );
        flow
    };

    let flow = open_relay();
    assert_eq!(
        flow.cancel(),
        [PairingEffect::CancelRelay, PairingEffect::Discard]
    );
    assert_eq!(flow.phase(), PairingPhase::Idle);
    assert_eq!(flow.transport(), PairingTransport::Relay);
    assert_eq!(
        flow.relay_opened(1, text("late")),
        [PairingEffect::CancelRelay],
        "a session that opened for an attempt the user left is cancelled"
    );

    let flow = open_relay();
    assert_eq!(
        flow.use_manual_instead(),
        [PairingEffect::CancelRelay, PairingEffect::Discard]
    );
    assert_eq!(flow.transport(), PairingTransport::Manual);
    assert_eq!(flow.notice(), None, "the user chose it");

    let flow = open_relay();
    assert_eq!(
        flow.dismissed(),
        [PairingEffect::CancelRelay, PairingEffect::Discard]
    );

    let flow = PairingFlow::new(PairingIntent::AddThisDevice, false, false);
    assert_eq!(flow.cancel(), [PairingEffect::Discard]);
}

// --- Account ---------------------------------------------------------------

/// A store that remembers what it was asked and refuses when told to.
#[derive(Default)]
struct FakeStore {
    saved: Mutex<Vec<LoginCredentials>>,
    clears: Mutex<u32>,
    refusal: Mutex<Option<String>>,
}

impl FakeStore {
    fn refusing(message: &str) -> Self {
        Self {
            refusal: Mutex::new(Some(message.into())),
            ..Self::default()
        }
    }

    fn saved(&self) -> Vec<LoginCredentials> {
        self.saved.lock().expect("lock").clone()
    }

    fn clears(&self) -> u32 {
        *self.clears.lock().expect("lock")
    }

    fn answer(&self) -> Option<String> {
        self.refusal.lock().expect("lock").clone()
    }
}

impl AccountCredentialStore for FakeStore {
    fn save(&self, credentials: LoginCredentials) -> Option<String> {
        self.saved.lock().expect("lock").push(credentials);
        self.answer()
    }

    fn clear(&self) -> Option<String> {
        *self.clears.lock().expect("lock") += 1;
        self.answer()
    }
}

fn session(store: FakeStore) -> (std::sync::Arc<FakeStore>, std::sync::Arc<AccountSession>) {
    let store = std::sync::Arc::new(store);
    let session = AccountSession::new(store.clone());
    (store, session)
}

fn credentials(token: &str, refresh: Option<&str>) -> LoginCredentials {
    LoginCredentials {
        access_token: token.into(),
        refresh_token: refresh.map(str::to_owned),
        expires_at_ms: 2_000,
        renew_at_ms: 1_000,
    }
}

const SIGNED_IN: AccountSessionState = AccountSessionState::SignedIn {
    expires_at_ms: 2_000,
};

fn sign_in(session: &AccountSession, credentials: LoginCredentials) {
    let SignInStep::Login { ticket } = session.begin_sign_in("issuer".into(), "client".into())
    else {
        panic!("a configured sign-in logs in");
    };
    assert!(session.browser_opening(ticket));
    assert_eq!(session.state(), AccountSessionState::AwaitingBrowser);
    session.login_finished(ticket, Some(credentials), None);
}

#[test]
fn a_sign_in_saves_the_credential_and_presents_its_bearer() {
    let (store, session) = session(FakeStore::default());
    assert_eq!(session.state(), AccountSessionState::SignedOut);
    assert_eq!(session.access_token(), None);

    let fresh = credentials("bearer", Some("refresh"));
    sign_in(&session, fresh.clone());

    assert_eq!(session.state(), SIGNED_IN);
    assert_eq!(session.access_token().as_deref(), Some("bearer"));
    assert_eq!(store.saved(), [fresh]);
    assert!(
        !format!("{session:?}").contains("bearer"),
        "the session's Debug must not print the bearer it holds"
    );
}

#[test]
fn a_login_answer_reads_its_error_first_and_needs_a_credential() {
    let finish = |answer: Option<LoginCredentials>, error: Option<String>| {
        let (store, session) = session(FakeStore::default());
        session.login_finished(0, answer, error);
        assert_eq!(store.saved(), [], "a failed login saves nothing");
        session.state()
    };
    let failed = |message: &str| AccountSessionState::Failed {
        failure: AccountSessionFailure::Failed {
            message: message.into(),
        },
    };

    assert_eq!(
        finish(Some(credentials("t", None)), Some("no".into())),
        failed("no")
    );
    assert_eq!(finish(None, Some("no".into())), failed("no"));
    assert_eq!(
        finish(None, None),
        failed("the login returned no credential")
    );
}

#[test]
fn a_save_the_store_refuses_is_the_login_failing() {
    let (_store, session) = session(FakeStore::refusing("full"));
    sign_in(&session, credentials("t", None));
    assert_eq!(
        session.state(),
        AccountSessionState::Failed {
            failure: AccountSessionFailure::Failed {
                message: "full".into()
            }
        }
    );
    assert_eq!(session.access_token(), None);
}

#[test]
fn a_sign_in_with_no_issuer_is_not_configured() {
    let (_store, session) = session(FakeStore::default());
    assert_eq!(
        session.begin_sign_in(" ".into(), "client".into()),
        SignInStep::Rest
    );
    assert_eq!(
        session.state(),
        AccountSessionState::Failed {
            failure: AccountSessionFailure::NotConfigured
        }
    );
}

#[test]
fn a_store_read_crosses_in_all_three_shapes() {
    let (_store, session) = session(FakeStore::default());
    session.restore(CredentialRead::Found {
        credentials: credentials("kept", None),
    });
    assert_eq!(session.state(), SIGNED_IN);
    assert_eq!(session.access_token().as_deref(), Some("kept"));

    session.restore(CredentialRead::Absent);
    assert_eq!(session.state(), AccountSessionState::SignedOut);

    let refused = CredentialRead::Refused {
        message: "locked".into(),
        may_have_left_copy_unread: true,
    };
    session.restore(refused.clone());
    assert_eq!(
        session.state(),
        AccountSessionState::Failed {
            failure: AccountSessionFailure::StoreRefused {
                message: "locked".into()
            }
        }
    );

    // A copy nobody read means looking again before a sign-in writes a second.
    let look = |loaded| session.looked_again(loaded, "issuer".into(), "client".into());
    assert_eq!(
        session.begin_sign_in("issuer".into(), "client".into()),
        SignInStep::LookAgain
    );
    assert_eq!(look(refused), SignInStep::Rest);
    assert_eq!(
        look(CredentialRead::Found {
            credentials: credentials("kept", None)
        }),
        SignInStep::Rest
    );
    assert_eq!(
        look(CredentialRead::Absent),
        SignInStep::Login { ticket: 0 }
    );
}

#[test]
fn a_renewal_comes_due_and_replaces_the_bearer() {
    let (store, session) = session(FakeStore::default());
    sign_in(&session, credentials("old", Some("refresh")));

    assert_eq!(session.renewal_due(999), RenewalStep::NotDue);
    let RenewalStep::Refresh {
        ticket,
        refresh_token,
    } = session.renewal_due(1_000)
    else {
        panic!("a credential past its renewal time with a refresh token refreshes");
    };
    assert_eq!(refresh_token, "refresh");

    let fresh = credentials("new", Some("refresh"));
    session.renewed(
        ticket,
        RenewalAnswer::Renewed {
            credentials: fresh.clone(),
        },
        1_000,
    );
    assert_eq!(session.access_token().as_deref(), Some("new"));
    assert_eq!(store.saved().last(), Some(&fresh));
}

#[test]
fn an_expired_credential_with_no_refresh_token_signs_out() {
    let (_store, session) = session(FakeStore::default());
    sign_in(&session, credentials("t", None));
    assert_eq!(session.renewal_due(1_000), RenewalStep::NotDue);
    assert_eq!(session.renewal_due(2_000), RenewalStep::SignOut);
}

#[test]
fn a_renewal_failure_crosses_with_whether_the_issuer_refused() {
    let renew = |refused_by_issuer| {
        let (store, session) = session(FakeStore::default());
        sign_in(&session, credentials("t", Some("refresh")));
        session.renewed(
            0,
            RenewalAnswer::Failed {
                message: "no".into(),
                refused_by_issuer,
            },
            2_000,
        );
        (session.state(), store.clears())
    };

    assert_eq!(
        renew(true),
        (
            AccountSessionState::Failed {
                failure: AccountSessionFailure::RefusedByIssuer
            },
            1
        ),
        "a refused refresh token is cleared from the store"
    );
    assert_eq!(
        renew(false),
        (
            AccountSessionState::Failed {
                failure: AccountSessionFailure::Failed {
                    message: "no".into()
                }
            },
            0
        ),
        "any other failure keeps the refresh token for the next tick"
    );
}

#[test]
fn a_sign_out_ends_what_was_in_flight_and_reports_its_residue() {
    let (store, session) = session(FakeStore::refusing("stuck"));
    assert_eq!(session.residue(), CoreSignOutResidue::None);

    let SignInStep::Login { ticket } = session.begin_sign_in("issuer".into(), "client".into())
    else {
        panic!("a configured sign-in logs in");
    };
    session.sign_out();
    assert_eq!(store.clears(), 1);
    assert_eq!(session.state(), AccountSessionState::SignedOut);
    assert!(
        !session.browser_opening(ticket),
        "the login the sign-out overtook opens nothing"
    );

    assert_eq!(
        session.residue(),
        CoreSignOutResidue::Unread {
            message: "stuck".into()
        }
    );
    session.dismiss_sign_out_incomplete();
    assert_eq!(session.residue(), CoreSignOutResidue::Acknowledged);
    session.dismiss_sign_out_retry();
    assert_eq!(session.residue(), CoreSignOutResidue::Retired);
}

// --- Vault -----------------------------------------------------------------

#[test]
fn a_first_run_creates_the_vault_and_a_lock_closes_it() {
    let vault = VaultSession::new(None);
    assert_eq!(vault.phase(), VaultPhase::Starting);

    assert_eq!(vault.start(StoredRoot::Absent, false), VaultStep::Rest);
    assert_eq!(vault.phase(), VaultPhase::FirstRun);
    assert_eq!(vault.create(), VaultStep::GenerateAndStore);
    assert_eq!(vault.created(UnitAnswer::Ok), VaultStep::Open);
    vault.opened(UnitAnswer::Ok);
    assert_eq!(vault.phase(), VaultPhase::Unlocked);

    assert_eq!(vault.lock(), VaultStep::Close);
    assert_eq!(
        vault.phase(),
        VaultPhase::Locked {
            reason: CoreLockReason::LockedByUser,
            repair: LockRepair::Unlock
        }
    );
}

#[test]
fn a_vault_that_cannot_open_says_why_and_offers_no_new_key() {
    let vault = VaultSession::new(None);
    assert_eq!(
        vault.start(
            StoredRoot::Refused {
                detail: "locked".into()
            },
            true
        ),
        VaultStep::Rest
    );
    assert_eq!(
        vault.phase(),
        VaultPhase::Locked {
            reason: CoreLockReason::KeyStoreUnavailable {
                detail: "locked".into()
            },
            repair: LockRepair::TryAgain
        }
    );
    assert_eq!(vault.create(), VaultStep::Rest, "only a first run creates");

    assert_eq!(vault.start(StoredRoot::Absent, true), VaultStep::Rest);
    assert_eq!(
        vault.phase(),
        VaultPhase::Locked {
            reason: CoreLockReason::KeyMissingForExistingVault,
            repair: LockRepair::TryAgain
        }
    );

    assert_eq!(vault.start(StoredRoot::Found, true), VaultStep::Open);
    assert_eq!(vault.phase(), VaultPhase::Starting);
}

#[test]
fn a_paired_root_is_stored_before_it_opens() {
    let vault = VaultSession::new(None);
    assert_eq!(vault.adopt(vec![7; 32]), VaultStep::StoreAdopted);
    assert_eq!(vault.adopted(UnitAnswer::Ok), VaultStep::Open);

    let vault = VaultSession::new(None);
    assert_eq!(vault.adopt(vec![7; 3]), VaultStep::Rest);
    assert_eq!(
        vault.phase(),
        VaultPhase::Failed {
            message: "vault root must be 32 bytes, got 3".into()
        }
    );
}

#[test]
fn a_failure_no_retry_fixes_is_the_failed_phase() {
    let failed = VaultPhase::Failed {
        message: "no".into(),
    };

    let vault = VaultSession::new(Some("no".into()));
    assert_eq!(vault.start(StoredRoot::Found, true), VaultStep::Rest);
    assert_eq!(vault.phase(), failed);

    let vault = VaultSession::new(None);
    assert_eq!(vault.created(unit_err("no")), VaultStep::Rest);
    assert_eq!(vault.phase(), failed);

    let vault = VaultSession::new(None);
    assert_eq!(vault.adopted(unit_err("no")), VaultStep::Rest);
    assert_eq!(vault.phase(), failed);

    let vault = VaultSession::new(None);
    vault.opened(unit_err("no"));
    assert_eq!(vault.phase(), failed);
}

#[test]
fn a_switch_closes_the_open_vault_only_for_another_one_it_can_bind() {
    let open = || {
        let vault = VaultSession::new(None);
        assert_eq!(vault.start(StoredRoot::Found, true), VaultStep::Open);
        vault.opened(UnitAnswer::Ok);
        vault
    };

    assert_eq!(open().switch(true, None), VaultStep::Rest);
    assert_eq!(open().switch(false, None), VaultStep::Close);

    let vault = open();
    assert_eq!(vault.switch(false, Some("no".into())), VaultStep::Rest);
    assert_eq!(
        vault.phase(),
        VaultPhase::Failed {
            message: "no".into()
        }
    );
}

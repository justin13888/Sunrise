//! Ported from the Apple app's `AccountModelTests`, `AccountRenewalTickTests`
//! and `AccountDisclosureTests`: the decisions, with the store and the issuer
//! as closures and values.

use super::*;

const NOW: u64 = 1_700_000_000_000;
const HOUR_MS: u64 = 3_600_000;

fn creds(token: &str, refresh: Option<&str>, at: u64) -> Credentials {
    Credentials::new(token.into(), refresh.map(Into::into), Some(3600), at)
}

/// A store that takes every save and every clear.
const fn ok_save(_: &Credentials) -> Result<(), String> {
    STORED
}

const fn ok_clear() -> Result<(), String> {
    STORED
}

const STORED: Result<(), String> = Ok(());

fn signed_in(token: &str, refresh: Option<&str>) -> AccountSession {
    let mut s = AccountSession::new();
    s.restore(Ok(Some(creds(token, refresh, NOW))));
    s
}

fn login(s: &mut AccountSession) -> SessionTicket {
    match s.begin_sign_in("https://issuer.example", "sunrise") {
        SignInStep::Login(ticket) => ticket,
        other => panic!("expected a login, got {other:?}"),
    }
}

#[test]
fn a_completed_login_is_stored_and_becomes_the_bearer() {
    let mut s = AccountSession::new();
    let ticket = login(&mut s);
    assert!(s.browser_opening(ticket));
    assert_eq!(s.state(), &AccountState::AwaitingBrowser);
    let mut saved = None;
    s.login_finished(ticket, Ok(creds("tok", Some("r"), NOW)), |c| {
        saved = Some(c.access_token.clone());
        Ok(())
    });
    assert_eq!(saved.as_deref(), Some("tok"));
    assert_eq!(s.access_token(), Some("tok"));
    assert_eq!(
        s.state(),
        &AccountState::SignedIn {
            expires_at_ms: NOW + HOUR_MS
        }
    );
}

#[test]
fn an_unconfigured_issuer_is_refused_before_any_network_call() {
    let mut s = AccountSession::new();
    assert_eq!(s.begin_sign_in(" ", "sunrise"), SignInStep::Rest);
    assert_eq!(
        s.state(),
        &AccountState::Failed(AccountFailure::NotConfigured)
    );
    assert_eq!(s.begin_sign_in("https://i", ""), SignInStep::Rest);
}

#[test]
fn a_failed_login_or_save_leaves_no_half_session() {
    let mut s = AccountSession::new();
    let t = login(&mut s);
    s.login_finished(t, Err("denied".into()), ok_save);
    assert_eq!(
        s.state(),
        &AccountState::Failed(AccountFailure::Failed("denied".into()))
    );
    assert_eq!(s.access_token(), None);
    let t = login(&mut s);
    s.login_finished(t, Ok(creds("tok", None, NOW)), |_| Err("locked".into()));
    assert_eq!(
        s.access_token(),
        None,
        "a token that was not stored is not used"
    );
    assert!(s.credentials().is_none());
}

#[test]
fn a_stored_token_is_restored_and_a_refusal_is_reported_not_signed_out() {
    let s = signed_in("tok", Some("r"));
    assert_eq!(s.access_token(), Some("tok"));
    let mut s = AccountSession::new();
    s.restore(Err(StoreRefusal {
        message: "the other keychain is locked".into(),
        may_have_left_copy_unread: true,
    }));
    assert_eq!(
        s.state(),
        &AccountState::Failed(AccountFailure::StoreRefused(
            "the other keychain is locked".into()
        ))
    );
}

/// A sign-in while the store may hold a token nobody read looks again first,
/// and logs in only once the store answers that nothing is there.
#[test]
fn a_store_that_may_hold_an_unread_token_is_asked_again_before_any_login() {
    let refusal = || {
        Err(StoreRefusal {
            message: "locked".into(),
            may_have_left_copy_unread: true,
        })
    };
    let mut s = AccountSession::new();
    s.restore(refusal());
    assert_eq!(s.begin_sign_in("https://i", "c"), SignInStep::LookAgain);
    assert_eq!(
        s.looked_again(refusal(), "https://i", "c"),
        SignInStep::Rest
    );
    assert_eq!(
        s.looked_again(Ok(Some(creds("found", None, NOW))), "https://i", "c"),
        SignInStep::Rest,
        "restored, not replaced"
    );
    assert_eq!(s.access_token(), Some("found"));
    let mut s = AccountSession::new();
    s.restore(refusal());
    assert!(matches!(
        s.looked_again(Ok(None), "https://i", "c"),
        SignInStep::Login(_)
    ));
}

/// Both copies read and compared: signing in is the repair, not a second
/// token, so there is no second look.
#[test]
fn a_refusal_that_read_both_copies_lets_the_sign_in_repair_it() {
    let mut s = AccountSession::new();
    s.restore(Err(StoreRefusal {
        message: "two tokens disagree".into(),
        may_have_left_copy_unread: false,
    }));
    assert!(matches!(
        s.begin_sign_in("https://i", "c"),
        SignInStep::Login(_)
    ));
}

#[test]
fn renewal_is_silent_and_only_happens_at_the_renewal_point() {
    let mut s = signed_in("old", Some("r"));
    assert_eq!(
        s.renewal_due(NOW + HOUR_MS * 3 / 4 - 1),
        RenewalStep::NotDue
    );
    let at = NOW + HOUR_MS * 3 / 4;
    let RenewalStep::Refresh {
        ticket,
        refresh_token,
    } = s.renewal_due(at)
    else {
        panic!("due at 75%");
    };
    assert_eq!(refresh_token, "r");
    s.renewed(
        ticket,
        Ok(creds("new", Some("r2"), at)),
        at,
        ok_save,
        ok_clear,
    );
    assert_eq!(s.access_token(), Some("new"));
    assert!(matches!(s.state(), AccountState::SignedIn { .. }));
}

#[test]
fn a_failed_renewal_keeps_the_still_valid_token_and_drops_an_expired_bearer() {
    let mut s = signed_in("tok", Some("r"));
    let at = NOW + HOUR_MS * 3 / 4;
    let RenewalStep::Refresh { ticket, .. } = s.renewal_due(at) else {
        panic!()
    };
    let offline = || {
        Err(RenewalFailure {
            message: "offline".into(),
            refused_by_issuer: false,
        })
    };
    s.renewed(ticket, offline(), at, ok_save, ok_clear);
    assert_eq!(s.access_token(), Some("tok"), "still valid");
    let expired = NOW + HOUR_MS;
    s.renewed(ticket, offline(), expired, ok_save, ok_clear);
    assert_eq!(s.access_token(), None, "the relay refuses it anyway");
    assert_eq!(
        s.state(),
        &AccountState::Failed(AccountFailure::Failed("offline".into()))
    );
    assert!(
        s.credentials().is_some_and(|c| c.refresh_token.is_some()),
        "the refresh token stays"
    );
    // And the next look renews once the network is back.
    assert!(matches!(
        s.renewal_due(expired + 1),
        RenewalStep::Refresh { .. }
    ));
}

#[test]
fn a_refused_renewal_ends_the_session_while_the_token_is_still_valid() {
    let mut s = signed_in("tok", Some("r"));
    let at = NOW + HOUR_MS * 3 / 4;
    let RenewalStep::Refresh { ticket, .. } = s.renewal_due(at) else {
        panic!()
    };
    let mut cleared = false;
    let refused = Err(RenewalFailure {
        message: "invalid_grant".into(),
        refused_by_issuer: true,
    });
    s.renewed(ticket, refused, at, ok_save, || {
        cleared = true;
        Ok(())
    });
    assert!(cleared, "out of the store too");
    assert_eq!(
        s.state(),
        &AccountState::Failed(AccountFailure::RefusedByIssuer)
    );
    assert_eq!(s.access_token(), None);
}

#[test]
fn a_refused_renewal_whose_clear_is_refused_says_so() {
    let mut s = signed_in("tok", Some("r"));
    let at = NOW + HOUR_MS * 3 / 4;
    let RenewalStep::Refresh { ticket, .. } = s.renewal_due(at) else {
        panic!()
    };
    let refused = Err(RenewalFailure {
        message: "invalid_grant".into(),
        refused_by_issuer: true,
    });
    s.renewed(ticket, refused, at, ok_save, || Err("locked".into()));
    assert_eq!(s.residue(), &SignOutResidue::Unread("locked".into()));
}

/// A renewal failing while a login is in the browser leaves its spinner.
#[test]
fn a_renewal_failing_during_a_login_leaves_the_login_to_finish() {
    let mut s = signed_in("tok", Some("r"));
    let expired = NOW + HOUR_MS;
    let RenewalStep::Refresh { ticket, .. } = s.renewal_due(expired) else {
        panic!()
    };
    let login_ticket = login(&mut s);
    s.browser_opening(login_ticket);
    for refused in [true, false] {
        let failure = Err(RenewalFailure {
            message: "x".into(),
            refused_by_issuer: refused,
        });
        s.renewed(ticket, failure, expired, ok_save, ok_clear);
        assert_eq!(
            s.state(),
            &AccountState::AwaitingBrowser,
            "refused: {refused}"
        );
    }
}

#[test]
fn no_refresh_token_means_nothing_until_expiry_then_sign_out() {
    let s = signed_in("tok", None);
    assert_eq!(s.renewal_due(NOW + HOUR_MS * 3 / 4), RenewalStep::NotDue);
    assert_eq!(s.renewal_due(NOW + HOUR_MS), RenewalStep::SignOut);
    assert_eq!(AccountSession::new().renewal_due(NOW), RenewalStep::NotDue);
}

#[test]
fn signing_out_clears_the_store_and_a_refused_clear_is_disclosed() {
    let mut s = signed_in("tok", Some("r"));
    let mut cleared = false;
    s.sign_out(|| {
        cleared = true;
        Ok(())
    });
    assert!(cleared);
    assert_eq!(
        (s.state(), s.residue()),
        (&AccountState::SignedOut, &SignOutResidue::None)
    );
    let mut s = signed_in("tok", Some("r"));
    s.sign_out(|| Err("locked".into()));
    assert_eq!(
        s.state(),
        &AccountState::SignedOut,
        "the local half is unconditional"
    );
    assert_eq!(s.access_token(), None);
    assert_eq!(s.residue(), &SignOutResidue::Unread("locked".into()));
}

#[test]
fn the_disclosure_is_dismissed_retired_and_re_armed() {
    let mut s = signed_in("tok", Some("r"));
    s.sign_out(|| Err("locked".into()));
    s.dismiss_sign_out_incomplete();
    assert_eq!(s.residue(), &SignOutResidue::Acknowledged);
    s.dismiss_sign_out_incomplete();
    assert_eq!(
        s.residue(),
        &SignOutResidue::Acknowledged,
        "only an unread one"
    );
    s.dismiss_sign_out_retry();
    assert_eq!(s.residue(), &SignOutResidue::Retired);
    s.sign_out(|| Err("still locked".into()));
    assert_eq!(
        s.residue(),
        &SignOutResidue::Unread("still locked".into()),
        "a fresh refusal"
    );
}

/// A save replaces the credential the residue is about; a failure does not.
#[test]
fn a_sign_in_or_renewal_that_saves_retires_the_residue_and_one_that_fails_does_not() {
    let mut s = signed_in("tok", Some("r"));
    s.sign_out(|| Err("locked".into()));
    let t = login(&mut s);
    s.login_finished(t, Err("cancelled".into()), ok_save);
    assert_eq!(s.residue(), &SignOutResidue::Unread("locked".into()));
    let t = login(&mut s);
    s.login_finished(t, Ok(creds("tok", Some("r"), NOW)), ok_save);
    assert_eq!(s.residue(), &SignOutResidue::None);

    let mut s = signed_in("tok", Some("r"));
    let at = NOW + HOUR_MS * 3 / 4;
    let RenewalStep::Refresh { ticket, .. } = s.renewal_due(at) else {
        panic!()
    };
    s.residue = SignOutResidue::Acknowledged;
    s.renewed(
        ticket,
        Ok(creds("new", Some("r"), at)),
        at,
        ok_save,
        ok_clear,
    );
    assert_eq!(s.residue(), &SignOutResidue::None);
}

/// A sign-out while the browser is open discards the login that lands late:
/// no browser, no spinner, no token, no error.
#[test]
fn a_sign_out_discards_the_login_and_the_renewal_in_flight() {
    let mut s = AccountSession::new();
    let t = login(&mut s);
    s.sign_out(ok_clear);
    assert!(
        !s.browser_opening(t),
        "a login the user ended opens nothing"
    );
    s.login_finished(t, Ok(creds("late", None, NOW)), |_| panic!("not saved"));
    assert_eq!(s.state(), &AccountState::SignedOut);
    s.login_finished(t, Err("timed out".into()), ok_save);
    assert_eq!(
        s.state(),
        &AccountState::SignedOut,
        "no error for a login walked away from"
    );

    let mut s = signed_in("tok", Some("r"));
    let at = NOW + HOUR_MS * 3 / 4;
    let RenewalStep::Refresh { ticket, .. } = s.renewal_due(at) else {
        panic!()
    };
    s.sign_out(ok_clear);
    s.renewed(
        ticket,
        Ok(creds("renewed", None, at)),
        at,
        |_| panic!("not saved"),
        ok_clear,
    );
    assert_eq!(s.access_token(), None);
}

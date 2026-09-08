//! The OIDC step-up gate in front of the recovery blob.
//!
//! # What is being guarded, and why an ordinary bearer is not enough
//!
//! The recovery blob plus the user's recovery code is the whole account:
//! `ID_S_priv` and `ID_D_priv`, and with them every `key_envelope` op the relay
//! holds. The blob is ciphertext the server cannot open, so serving it to the
//! wrong party is not immediately fatal — but it converts an *online* attack,
//! bounded by whatever the IdP throttles, into an *offline* one bounded only by
//! Argon2id at m = 64 MiB. `docs/03-crypto/recovery.md` §Recovery flow names
//! exactly that risk and asks for a second gate in front of the fetch.
//!
//! An ordinary bearer cannot be that gate. A bearer is precisely what a stolen
//! session has, and its `exp` says nothing about when the human authenticated:
//! a refresh-token exchange mints a token with a fresh `iat` and `exp` from a
//! login that may be months old, so "this token is valid" and "this person is
//! at the keyboard" are different statements.
//!
//! # Why not the email OTP the spec asked for
//!
//! `docs/03-crypto/recovery.md`:103 specifies a 6-digit email OTP here, and
//! `docs/00-product/non-goals.md` forbids building one — "we do not implement
//! password storage, email-OTP delivery, magic links, MFA, captcha" — as does
//! `docs/06-server/auth.md` §What we explicitly do not build. Those two are in
//! direct contradiction and #56 resolves it in the direction the rest of the
//! product already goes: the IdP owns every authentication ceremony, so the
//! server asks the IdP for a *stronger, fresher* authentication rather than
//! running one of its own. The client re-runs its authorization request with
//! `max_age=0` (or `prompt=login`, or an `acr_values` the operator configured),
//! the IdP does whatever it does — password, passkey, OTP, hardware token — and
//! comes back with claims that say so. Sunrise stores no OTP secret, sends no
//! mail, and gains a gate the operator can make as strong as their IdP allows.
//!
//! # The three claims
//!
//! - **`auth_time`** — required, and the load-bearing one, because it is the
//!   only claim a refresh cannot move. Within
//!   [`StepUpPolicy::max_auth_age_secs`] of now.
//! - **`acr`** — checked when the operator lists accepted values. Its meaning
//!   is the IdP's, so this server cannot rank them; it can only match what an
//!   operator who knows their IdP wrote down.
//! - **`amr`** — checked the same way, on intersection: any one accepted method
//!   satisfies it.
//!
//! With no `acr`/`amr` policy configured, freshness alone stands. That is the
//! useful default: `max_age` needs no knowledge of the deployment's IdP, while
//! an `acr` value this server invented would match nothing anywhere.
//!
//! # Failing closed
//!
//! Every absence is a refusal. A token with no `auth_time` has said nothing
//! about freshness, and an issuer that does not emit the claim is a deployment
//! that has not configured its step-up rather than one that is exempt from it.
//! The single exemption is the self-host [`crate::NullVerifier`], which has no
//! IdP to ask, maps every caller to one account, and which
//! `ServerConfig::validate` already refuses to bind anywhere but loopback — and
//! that exemption is keyed on the *verifier*, so no configuration of a real
//! OIDC deployment can reach it.

use super::StepUp;

/// What a deployment demands before it will serve the recovery blob.
#[derive(Debug, Clone)]
pub struct StepUpPolicy {
    /// How recently the end user must have authenticated, in seconds.
    pub max_auth_age_secs: u64,
    /// Accepted `acr` values. Empty means "any", not "none".
    pub acr_values: Vec<String>,
    /// Accepted `amr` values, satisfied by intersection. Empty means "any".
    pub amr_values: Vec<String>,
    /// Clock skew tolerated on `auth_time`, in seconds.
    ///
    /// The same allowance the token's own `exp` gets. Without it a user whose
    /// device clock runs a minute fast authenticates "in the future" and is
    /// refused for it.
    pub leeway_secs: u64,
}

/// Why a step-up was refused.
///
/// Distinguished here and **not** on the wire: the client is told one code. The
/// server logs which, because an operator debugging "nobody can fetch their
/// blob" needs to know whether their IdP is omitting `auth_time` or their
/// `acr` list names a value the IdP never emits, and those look identical from
/// outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepUpFailure {
    /// The token carries no `auth_time`, so it makes no claim about freshness.
    AuthTimeMissing,
    /// The user authenticated longer ago than the policy allows.
    AuthTimeStale,
    /// No `acr` claim while the policy lists accepted values.
    AcrMissing,
    /// The `acr` claim is not one the policy accepts.
    AcrRejected,
    /// No `amr` value intersects the policy's list.
    AmrRejected,
}

impl StepUpFailure {
    /// A closed set of static discriminants for the `reason` log field.
    ///
    /// String literals chosen here rather than interpolated from the claim, so
    /// nothing an issuer put in a token can reach a log record through this.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::AuthTimeMissing => "auth_time_missing",
            Self::AuthTimeStale => "auth_time_stale",
            Self::AcrMissing => "acr_missing",
            Self::AcrRejected => "acr_rejected",
            Self::AmrRejected => "amr_rejected",
        }
    }
}

/// Whether `claims` satisfy `policy` at `now_ms`.
///
/// # Errors
/// The first requirement that failed, in the order freshness, `acr`, `amr`.
pub fn check(policy: &StepUpPolicy, claims: &StepUp, now_ms: u64) -> Result<(), StepUpFailure> {
    let auth_time = claims
        .auth_time_secs
        .ok_or(StepUpFailure::AuthTimeMissing)?;
    let now_secs = now_ms / 1000;
    // `saturating_sub` both ways: an `auth_time` in the future is a skewed
    // clock rather than a fresher login, and it is tolerated only as far as the
    // leeway — beyond that the age reads as zero, which is the same answer a
    // just-completed login gets and the honest one, since nothing here can tell
    // the two apart.
    let age = now_secs.saturating_sub(auth_time);
    if age > policy.max_auth_age_secs.saturating_add(policy.leeway_secs) {
        return Err(StepUpFailure::AuthTimeStale);
    }

    if !policy.acr_values.is_empty() {
        let acr = claims.acr.as_deref().ok_or(StepUpFailure::AcrMissing)?;
        if !policy.acr_values.iter().any(|want| want == acr) {
            return Err(StepUpFailure::AcrRejected);
        }
    }

    if !policy.amr_values.is_empty()
        && !claims
            .amr
            .iter()
            .any(|got| policy.amr_values.iter().any(|want| want == got))
    {
        return Err(StepUpFailure::AmrRejected);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_800_000_000_000;

    fn policy() -> StepUpPolicy {
        StepUpPolicy {
            max_auth_age_secs: 300,
            acr_values: Vec::new(),
            amr_values: Vec::new(),
            leeway_secs: 60,
        }
    }

    fn fresh() -> StepUp {
        StepUp {
            acr: None,
            amr: Vec::new(),
            auth_time_secs: Some(NOW_MS / 1000 - 10),
        }
    }

    #[test]
    fn a_fresh_login_passes_with_no_acr_policy() {
        assert_eq!(check(&policy(), &fresh(), NOW_MS), Ok(()));
    }

    /// The failure that matters most, because it is the one a stolen session
    /// produces: a token minted seconds ago from a login six months old.
    #[test]
    fn a_refreshed_token_from_an_old_login_is_refused() {
        let stale = StepUp {
            auth_time_secs: Some(NOW_MS / 1000 - 60 * 60 * 24 * 180),
            ..fresh()
        };
        assert_eq!(
            check(&policy(), &stale, NOW_MS),
            Err(StepUpFailure::AuthTimeStale)
        );
    }

    /// An issuer that emits no `auth_time` has said nothing about freshness,
    /// and nothing is not "recent enough".
    #[test]
    fn an_absent_auth_time_is_a_refusal_and_not_a_pass() {
        let silent = StepUp {
            auth_time_secs: None,
            ..fresh()
        };
        assert_eq!(
            check(&policy(), &silent, NOW_MS),
            Err(StepUpFailure::AuthTimeMissing)
        );
    }

    /// The boundary, both sides of it, including the leeway.
    #[test]
    fn the_freshness_window_is_max_age_plus_leeway() {
        let at = |age: u64| StepUp {
            auth_time_secs: Some(NOW_MS / 1000 - age),
            ..fresh()
        };
        assert_eq!(check(&policy(), &at(360), NOW_MS), Ok(()));
        assert_eq!(
            check(&policy(), &at(361), NOW_MS),
            Err(StepUpFailure::AuthTimeStale)
        );
    }

    /// A clock that says the login happens next week does not make it fresher
    /// than one that happened now, and does not fail either.
    #[test]
    fn an_auth_time_in_the_future_is_treated_as_now() {
        let ahead = StepUp {
            auth_time_secs: Some(NOW_MS / 1000 + 60 * 60 * 24 * 7),
            ..fresh()
        };
        assert_eq!(check(&policy(), &ahead, NOW_MS), Ok(()));
    }

    #[test]
    fn an_acr_policy_demands_a_listed_value() {
        let mut p = policy();
        p.acr_values = vec!["urn:sunrise:mfa".into()];

        assert_eq!(
            check(&p, &fresh(), NOW_MS),
            Err(StepUpFailure::AcrMissing),
            "a configured acr policy is not satisfied by a token that carries none"
        );

        let wrong = StepUp {
            acr: Some("0".into()),
            ..fresh()
        };
        assert_eq!(check(&p, &wrong, NOW_MS), Err(StepUpFailure::AcrRejected));

        let right = StepUp {
            acr: Some("urn:sunrise:mfa".into()),
            ..fresh()
        };
        assert_eq!(check(&p, &right, NOW_MS), Ok(()));
    }

    /// `amr` is satisfied by intersection: one accepted method is enough, and
    /// the other methods a token lists do not weaken it.
    #[test]
    fn an_amr_policy_is_satisfied_by_any_listed_method() {
        let mut p = policy();
        p.amr_values = vec!["otp".into(), "hwk".into()];

        assert_eq!(check(&p, &fresh(), NOW_MS), Err(StepUpFailure::AmrRejected));

        let pwd_only = StepUp {
            amr: vec!["pwd".into()],
            ..fresh()
        };
        assert_eq!(
            check(&p, &pwd_only, NOW_MS),
            Err(StepUpFailure::AmrRejected)
        );

        let pwd_and_otp = StepUp {
            amr: vec!["pwd".into(), "otp".into()],
            ..fresh()
        };
        assert_eq!(check(&p, &pwd_and_otp, NOW_MS), Ok(()));
    }

    /// Freshness is checked before the operator's policy, so an operator
    /// reading the log sees the requirement the *user* can fix first.
    #[test]
    fn freshness_is_reported_before_the_acr_policy() {
        let mut p = policy();
        p.acr_values = vec!["urn:sunrise:mfa".into()];
        let stale = StepUp {
            auth_time_secs: Some(0),
            ..fresh()
        };
        assert_eq!(check(&p, &stale, NOW_MS), Err(StepUpFailure::AuthTimeStale));
    }

    /// The `reason` values are the closed set the log field's contract needs.
    #[test]
    fn every_failure_names_itself_with_a_literal() {
        for f in [
            StepUpFailure::AuthTimeMissing,
            StepUpFailure::AuthTimeStale,
            StepUpFailure::AcrMissing,
            StepUpFailure::AcrRejected,
            StepUpFailure::AmrRejected,
        ] {
            assert!(f
                .reason()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }
}

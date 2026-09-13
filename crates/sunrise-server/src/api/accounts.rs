//! `POST /api/v1/accounts`, `GET /api/v1/accounts/me`, and the recovery blob.
//!
//! Both resolve the caller from the verified token's `(iss, sub)`. **Nothing
//! identifying is read out of a request body** — the account id used to be the
//! first sixteen characters of the caller's own submitted public key, and
//! `GET /accounts/me` used to answer `200 {"identity_id":"unauthenticated"}` to
//! anybody. Those are regression-tested in `routes::accounts` and the property
//! is preserved here by construction: the only account id in scope comes from
//! [`Principal`](crate::api::auth::Principal), which only
//! [`Auth`](kynos::security::auth::Auth) can produce.
//!
//! # Why the request types live here
//!
//! `sunrise-onboarding` declares an `AccountCreateRequest` and an `AccountInfo`
//! that nothing has ever sent or received — the bootstrap flow was specified,
//! served, and never invoked, which is the void ADR-0021 adopted a client
//! generator to fill. The typed surface is the contract now, so the shapes are
//! declared where they are described, and carry the two things the old ones
//! could not: a `Schema` for the document, and `deny_unknown_fields`.
//!
//! That last one is load-bearing rather than tidiness. `header_sig_v2` signs a
//! re-serialisation of the parsed value, and a signature over a lossy parse is
//! a signature over something the client did not send. Rejecting the unknown
//! member turns an inscrutable signature mismatch into a 400 that names it.

use crate::api::error::ApiError;
use crate::api::signed::{SignedBootstrap, SignedParts};
use crate::state::ServerState;
use crate::store::Account;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::response::status::Created;
use serde::{Deserialize, Serialize};

/// `POST /api/v1/accounts` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct AccountCreateRequest {
    /// User email. Normalized server-side, and only a fallback: the IdP owns
    /// the address, and this is read solely when the verifier emits no `email`
    /// claim at all.
    pub email: String,
    /// Identity Ed25519 public key, base64url no-pad.
    pub identity_signing_pub: String,
    /// Identity X25519 public key, base64url no-pad.
    pub identity_dh_pub: String,
    /// Encrypted recovery blob. Opaque to the server, base64url no-pad.
    pub recovery_blob: String,
    /// Terms acceptance timestamp, milliseconds since the epoch.
    pub terms_at_ms: u64,
}

/// `GET /api/v1/accounts/me/recovery_blob` response body.
///
/// One field, deliberately: the blob is the whole answer, and the salt and
/// nonce the client needs to open it are inside it — `magic(5) || salt(16) ||
/// nonce(24) || ct`, per `docs/03-crypto/recovery.md`. Reporting them
/// separately would be the server describing a format it does not parse.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBlobResponse {
    /// The sealed blob, base64url no-pad, exactly as it was uploaded.
    pub recovery_blob: String,
}

/// An account, as this surface reports it.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct AccountInfo {
    /// 16-byte identity id.
    pub identity_id: String,
    /// Normalized email.
    pub email: String,
    /// Subscription tier.
    pub tier: String,
    /// Number of registered, unrevoked devices.
    pub device_count: u32,
    /// Account creation time, milliseconds since the epoch.
    pub created_at_ms: u64,
}

fn info(state: &ServerState, account: &Account) -> Result<AccountInfo, ApiError> {
    Ok(AccountInfo {
        identity_id: account.account_id.clone(),
        email: account.email.clone().unwrap_or_default(),
        tier: account.tier.clone(),
        device_count: state.store.active_device_count(&account.account_id)?,
        created_at_ms: account.created_at_ms,
    })
}

/// Set the account's identity keys and recovery blob.
///
/// Idempotent: the store coalesces, so a client that retries after a dropped
/// response does not get a second account. The account itself already exists by
/// the time this runs — it is minted when the token first resolves.
#[kynos::post("/api/v1/accounts", operation_id = "createAccount")]
pub async fn create(
    Inject(state): Inject<ServerState>,
    // Bootstrap exemption: a device cannot sign before it exists, so this route
    // accepts an absent binding even where the server demands one elsewhere. A
    // binding that *is* supplied is verified in full, so this weakens nothing
    // for a client that already has a device.
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<AccountCreateRequest>,
) -> Result<Created<Json<AccountInfo>>, ApiError> {
    let principal = &caller.principal;

    if body.email.trim().is_empty() {
        return Err(ApiError::validation("email required"));
    }
    if body.identity_signing_pub.trim().is_empty() || body.identity_dh_pub.trim().is_empty() {
        return Err(ApiError::validation("identity keys required"));
    }

    let account = state.store.set_identity(
        &principal.account.account_id,
        body.identity_signing_pub.trim(),
        body.identity_dh_pub.trim(),
        Some(body.recovery_blob.as_str()).filter(|b| !b.is_empty()),
        body.terms_at_ms,
    )?;

    state.metrics.incr("sunrise_account_create_total");
    let mut out = info(&state, &account)?;
    // The IdP owns the email; the body's copy is the self-host fallback.
    out.email = principal
        .subject
        .email
        .clone()
        .unwrap_or_else(|| body.email.trim().to_ascii_lowercase());
    Ok(Created::at("/api/v1/accounts/me", Json(out)))
}

/// The calling account.
#[kynos::get("/api/v1/accounts/me", operation_id = "getAccount")]
pub async fn me(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<AccountInfo>, ApiError> {
    Ok(Json(info(&state, &caller.principal.account)?))
}

/// The calling account's sealed recovery blob.
///
/// # Why this route exists
///
/// It is the one thing missing between a user who has lost every device and
/// their data. `POST /api/v1/accounts` stored `recovery_blob` and nothing read
/// it back — `docs/03-crypto/recovery.md` §Implementation status called it "a
/// write-only column" — so the recovery flow's step 2 had no route to call and
/// a recovery code was not usable even once one existed.
///
/// # The step-up, and why it is not the email OTP the spec asked for
///
/// `recovery.md`:103 specifies a 6-digit email OTP in front of this fetch, and
/// `docs/00-product/non-goals.md` forbids implementing OTP delivery. #56
/// resolves the contradiction the way the rest of the product resolves every
/// authentication question: the IdP performs the ceremony and this server reads
/// the claims that describe it. The client re-runs its authorization request
/// with `max_age=0` — or `prompt=login`, or an operator-configured
/// `acr_values` — and the resulting token carries an `auth_time` this route
/// checks. [`crate::auth::step_up`] carries the full reasoning, including why
/// `exp` cannot stand in for `auth_time`.
///
/// **An ordinary bearer is refused.** That is the whole point: a stolen session
/// is exactly an ordinary bearer, and the blob plus a guessable recovery code
/// is the account.
///
/// # `403` rather than `401`
///
/// The credential is not wrong; a stronger one is required. A `401` drives a
/// client's token-refresh path, and a refresh mints a token with the same
/// `auth_time` — so the client would refresh into the identical refusal
/// forever, which is precisely the failure `AUTH_DEVICE_SIG_INVALID` exists to
/// avoid one route away. RFC 9470 defines a `401` with
/// `insufficient_user_authentication` for this, and this surface's error type
/// carries no `WWW-Authenticate` parameters to make that answer actionable, so
/// the code says it instead.
#[kynos::get("/api/v1/accounts/me/recovery_blob", operation_id = "getRecoveryBlob")]
pub async fn recovery_blob(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<RecoveryBlobResponse>, ApiError> {
    let principal = &caller.principal;

    // Keyed on the verifier rather than on configuration. Self-host has no IdP
    // to ask for a step-up, maps every caller to one synthetic account, and
    // `ServerConfig::validate` refuses to bind it anywhere but loopback — so
    // there is nothing here for a step-up to distinguish. No configuration of a
    // real OIDC deployment can reach this branch, which is why it is a property
    // of the verifier and not a setting an operator can turn on.
    if !state.token_verifier.is_single_tenant() {
        if let Err(failure) = crate::auth::step_up::check(
            &state.config.recovery_step_up(),
            &principal.step_up,
            state.clock.now_ms(),
        ) {
            state.metrics.incr("sunrise_recovery_step_up_refused_total");
            tracing::warn!(
                ev = "srv.auth.step_up_required",
                account_h = %crate::logging::account_h(&principal.account.account_id),
                reason = failure.reason(),
                "recovery blob withheld: the token does not carry a fresh enough authentication"
            );
            return Err(ApiError::forbidden(
                crate::api::error::codes::AUTH_STEP_UP_REQUIRED,
                "this operation needs a fresh authentication: sign in again through your \
                 identity provider and retry"
                    .to_owned(),
            ));
        }
    }

    let blob = state
        .store
        .recovery_blob(&principal.account.account_id)?
        .ok_or_else(|| {
            ApiError::not_found(
                crate::api::error::codes::RECOVERY_BLOB_NOT_FOUND,
                "this account has no recovery blob".to_owned(),
            )
        })?;

    state.metrics.incr("sunrise_recovery_blob_fetch_total");
    Ok(Json(RecoveryBlobResponse {
        recovery_blob: blob,
    }))
}

#[cfg(test)]
mod tests {
    use crate::api::error::codes::{
        AUTH_STEP_UP_REQUIRED, RECOVERY_BLOB_EXISTS, RECOVERY_BLOB_NOT_FOUND, VALIDATION_INVALID,
    };
    use crate::api::testing::{code_of, register_device, send_signed, Client, BEARER};
    use crate::state::{Clock, ServerState};
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::Arc;

    /// A fixed instant, so "how long ago did this person authenticate" is a
    /// property the test states rather than one it races.
    const T0_MS: u64 = 1_800_000_000_000;

    #[derive(Debug)]
    struct FixedClock;
    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            T0_MS
        }
    }

    /// A client behind a **multi-tenant** verifier whose one bearer carries
    /// `auth_time` at `authenticated_secs_ago`.
    ///
    /// Multi-tenant matters: the self-host `NullVerifier` is the one exemption
    /// from the step-up, so a test that used the default client would prove
    /// nothing about the gate.
    fn client_with_login_age(secs_ago: u64) -> Client {
        let step_up = crate::auth::StepUp {
            acr: None,
            amr: vec!["pwd".into()],
            auth_time_secs: Some(T0_MS / 1000 - secs_ago),
        };
        let state = ServerState::with_clock(ServerConfig::default(), Arc::new(FixedClock))
            .with_verifier(Arc::new(StaticVerifier::default().with_step_up(
                "test",
                Subject::new("https://idp.example", "alice"),
                step_up,
            )));
        Client::from_state(state)
    }

    /// A client behind a multi-tenant verifier whose bearer carries no step-up
    /// claims at all — an ordinary session token, which is what a stolen one
    /// is.
    fn client_with_a_plain_bearer() -> Client {
        let state = ServerState::with_clock(ServerConfig::default(), Arc::new(FixedClock))
            .with_verifier(Arc::new(
                StaticVerifier::default()
                    .with("test", Subject::new("https://idp.example", "alice")),
            ));
        Client::from_state(state)
    }

    async fn create_account(client: &Client, blob: &str) -> Res {
        client
            .send(
                Method::POST,
                "/api/v1/accounts",
                Some(&serde_json::json!({
                    "email": "alice@example.com",
                    "identity_signing_pub": "aWRfc19wdWI",
                    "identity_dh_pub": "aWRfZF9wdWI",
                    "recovery_blob": blob,
                    "terms_at_ms": 1,
                })),
            )
            .await
    }

    type Res = crate::api::testing::Res;

    /// A client whose one bearer carries `email` as its IdP claim.
    fn client_claiming_email(email: &str) -> Client {
        let state = ServerState::with_clock(ServerConfig::default(), Arc::new(FixedClock))
            .with_verifier(Arc::new(StaticVerifier::default().with(
                "test",
                Subject::new("https://idp.example", "alice").with_email(email),
            )));
        Client::from_state(state)
    }

    /// `POST /api/v1/accounts` with one member replaced.
    async fn create_with(client: &Client, field: &str, value: serde_json::Value) -> Res {
        let mut body = serde_json::json!({
            "email": "alice@example.com",
            "identity_signing_pub": "aWRfc19wdWI",
            "identity_dh_pub": "aWRfZF9wdWI",
            "recovery_blob": "",
            "terms_at_ms": 1,
        });
        body[field] = value;
        client
            .send(Method::POST, "/api/v1/accounts", Some(&body))
            .await
    }

    /// An email that is absent in everything but shape buys no account.
    ///
    /// The helper every other test in this module uses always sends a
    /// well-formed body, so this branch and the one below had never been taken.
    /// Whitespace as well as the empty string: the check is `trim().is_empty()`
    /// and a space would otherwise be stored as an address.
    #[tokio::test]
    async fn account_creation_requires_an_email() {
        let client = client_with_a_plain_bearer();
        for empty in ["", "   ", "\t\n"] {
            let res = create_with(&client, "email", serde_json::json!(empty)).await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert_eq!(code_of(&res), VALIDATION_INVALID, "for {empty:?}");
        }
    }

    /// Both identity keys are required, and each is checked.
    ///
    /// One refusal covers the pair, so a test that only blanked the signing key
    /// would leave the `identity_dh_pub` half of the condition unexercised.
    #[tokio::test]
    async fn account_creation_requires_both_identity_keys() {
        let client = client_with_a_plain_bearer();
        for field in ["identity_signing_pub", "identity_dh_pub"] {
            let res = create_with(&client, field, serde_json::json!("  ")).await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert_eq!(code_of(&res), VALIDATION_INVALID, "for {field}");
        }

        // And a well-formed pair still creates, so the checks above refuse the
        // blank value rather than everything.
        create_account(&client, "")
            .await
            .assert_status(StatusCode::CREATED);
    }

    /// The response email rule, in both directions.
    ///
    /// The IdP owns the address: when the token carries an `email` claim that
    /// is what comes back, verbatim, whatever the body said. Only when the
    /// verifier emits no claim at all is the body read, and then it is trimmed
    /// and lowercased so one account cannot be addressed two ways.
    #[tokio::test]
    async fn the_idp_email_wins_and_the_bodys_is_the_fallback() {
        let with_claim = client_claiming_email("Alice@IdP.example");
        let res = create_with(
            &with_claim,
            "email",
            serde_json::json!("  SOMEONE-ELSE@example.com  "),
        )
        .await;
        res.assert_status(StatusCode::CREATED);
        assert_eq!(
            res.json()["email"],
            serde_json::json!("Alice@IdP.example"),
            "the claim wins, and is not re-cased on the way out"
        );

        let without_claim = client_with_a_plain_bearer();
        let res = create_with(
            &without_claim,
            "email",
            serde_json::json!("  Alice@Example.COM  "),
        )
        .await;
        res.assert_status(StatusCode::CREATED);
        assert_eq!(
            res.json()["email"],
            serde_json::json!("alice@example.com"),
            "the fallback is trimmed and lowercased"
        );

        // Pinned, not endorsed: the fallback decorates *this response only*.
        // The account row's email comes from the token's claim and nothing
        // else, so with no claim the address the client just sent is not
        // stored, and the very next `GET /accounts/me` reports the empty
        // string `info` defaults to.
        let me = without_claim
            .send(Method::GET, "/api/v1/accounts/me", None)
            .await;
        me.assert_status(StatusCode::OK);
        assert_eq!(me.json()["email"], serde_json::json!(""));
    }

    /// **The body of `GET /api/v1/accounts/me`, which nothing ever asserted.**
    ///
    /// Every one of its five members was only ever reached as a signature
    /// target, where the test asserts the status and throws the body away.
    ///
    /// This module's own header says the `200 {"identity_id":"unauthenticated"}`
    /// regression is "regression-tested in `routes::accounts`". No `routes`
    /// module exists anywhere in the workspace — that string occurs exactly
    /// once, in the comment claiming it — so the regression had no test at all.
    /// This is it: the id is the resolved account's, it is stable across calls,
    /// and two principals do not share one.
    #[tokio::test]
    async fn the_account_route_reports_the_identity_the_token_resolved_to() {
        let client = client_claiming_email("alice@idp.example");
        let created = create_account(&client, "U1IDAAFibG9i").await;
        created.assert_status(StatusCode::CREATED);
        let identity_id = created.json()["identity_id"]
            .as_str()
            .expect("an identity id")
            .to_owned();

        let res = client.send(Method::GET, "/api/v1/accounts/me", None).await;
        res.assert_status(StatusCode::OK);
        let me = res.json();
        assert_ne!(
            me["identity_id"],
            serde_json::json!("unauthenticated"),
            "the id this route once answered to anybody"
        );
        assert_eq!(me["identity_id"], serde_json::json!(identity_id));
        assert_eq!(me["email"], serde_json::json!("alice@idp.example"));
        assert_eq!(me["tier"], serde_json::json!("free"));
        assert_eq!(me["device_count"], serde_json::json!(0));
        assert_eq!(
            me["created_at_ms"],
            serde_json::json!(T0_MS),
            "the row's own creation time, off the server's clock"
        );

        // A second principal is a second account: the id comes from the
        // verified token, never from anything the caller sent.
        let state = ServerState::with_clock(ServerConfig::default(), Arc::new(FixedClock))
            .with_verifier(Arc::new(
                StaticVerifier::default()
                    .with("test", Subject::new("https://idp.example", "alice"))
                    .with("other", Subject::new("https://idp.example", "bob")),
            ));
        let two = Client::from_state(state);
        let alice = two.send(Method::GET, "/api/v1/accounts/me", None).await;
        let bob = two
            .send_as(
                Method::GET,
                "/api/v1/accounts/me",
                Some("Bearer other"),
                None,
            )
            .await;
        alice.assert_status(StatusCode::OK);
        bob.assert_status(StatusCode::OK);
        assert_ne!(alice.json()["identity_id"], bob.json()["identity_id"]);
    }

    /// `device_count` counts the account's *unrevoked* devices, which is the
    /// number a user reads as "where am I signed in".
    #[tokio::test]
    async fn the_device_count_follows_registration_and_revocation() {
        const PHONE_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";

        async fn count(client: &Client) -> serde_json::Value {
            let res = client.send(Method::GET, "/api/v1/accounts/me", None).await;
            res.assert_status(StatusCode::OK);
            res.json()["device_count"].clone()
        }

        let client = client_with_a_plain_bearer();
        assert_eq!(count(&client).await, serde_json::json!(0));
        let (_phone, _) = register_device(&client, 40, "phone", Some(PHONE_VAULT_ID)).await;
        assert_eq!(count(&client).await, serde_json::json!(1));
        let (laptop, laptop_key) = register_device(&client, 41, "laptop", None).await;
        assert_eq!(count(&client).await, serde_json::json!(2));

        send_signed(
            &client,
            "DELETE",
            &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
            &laptop,
            &laptop_key,
            None,
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(
            count(&client).await,
            serde_json::json!(1),
            "a revoked device is not one the user is signed in on"
        );
    }

    /// No credential at all, against a verifier that actually checks.
    ///
    /// The default `NullVerifier` accepts an absent bearer by design, so the
    /// `None` requests elsewhere prove nothing about whether these routes are
    /// authenticated. `no_bearer_gets_no_blob` covered the recovery route; the
    /// other two had no equivalent.
    #[tokio::test]
    async fn every_account_route_requires_a_bearer() {
        let client = client_with_a_plain_bearer();
        for (method, path) in [
            (Method::GET, "/api/v1/accounts/me"),
            (Method::POST, "/api/v1/accounts"),
            (Method::GET, "/api/v1/accounts/me/recovery_blob"),
        ] {
            let res = client.send_as(method.clone(), path, None, None).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} must refuse an unauthenticated caller"
            );
        }
    }

    /// The route the whole recovery flow was missing: a blob goes up at
    /// account creation and comes back down to a device that has just proved a
    /// fresh login.
    #[tokio::test]
    async fn a_freshly_authenticated_caller_gets_the_blob_back() {
        let client = client_with_login_age(30);
        create_account(&client, "U1IDAAFibG9i")
            .await
            .assert_status(StatusCode::CREATED);

        let res = client
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(
            res.json()["recovery_blob"].as_str(),
            Some("U1IDAAFibG9i"),
            "the blob must come back byte for byte; the server never re-encodes it"
        );
    }

    /// **The property this route exists to have.** A stolen session is exactly
    /// a valid bearer with no fresh authentication behind it, and the blob plus
    /// a guessable recovery code is the whole account.
    #[tokio::test]
    async fn an_ordinary_bearer_does_not_get_the_blob() {
        let client = client_with_a_plain_bearer();
        create_account(&client, "U1IDAAFibG9i")
            .await
            .assert_status(StatusCode::CREATED);

        let res = client
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        res.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&res), AUTH_STEP_UP_REQUIRED);
        assert!(
            !String::from_utf8_lossy(&res.bytes).contains("U1IDAAFibG9i"),
            "a refusal must not carry the thing it refused"
        );
    }

    /// And a token minted seconds ago from a login six months old is an
    /// ordinary bearer too — which is the case `exp` cannot see, because a
    /// refresh moves `exp` and leaves `auth_time` alone.
    #[tokio::test]
    async fn a_live_token_from_a_stale_login_does_not_get_the_blob() {
        let client = client_with_login_age(60 * 60 * 24 * 180);
        create_account(&client, "U1IDAAFibG9i")
            .await
            .assert_status(StatusCode::CREATED);

        let res = client
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        res.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&res), AUTH_STEP_UP_REQUIRED);
    }

    /// The step-up is checked *before* the lookup, so the route cannot answer
    /// "does this account have a recovery blob?" to a caller who has not
    /// stepped up. Both accounts below answer `403`, not `403` and `404`.
    #[tokio::test]
    async fn the_step_up_is_checked_before_the_blob_is_looked_up() {
        let without = client_with_a_plain_bearer();
        let res = without
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        res.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(
            code_of(&res),
            AUTH_STEP_UP_REQUIRED,
            "an account with no blob must be indistinguishable from one that has one"
        );

        let with = client_with_login_age(10);
        let res = with
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(code_of(&res), RECOVERY_BLOB_NOT_FOUND);
    }

    /// Self-host is the one exemption, and it is keyed on the verifier: there
    /// is no IdP to ask for a step-up, every caller maps to one synthetic
    /// account, and `ServerConfig::validate` refuses to bind that mode off
    /// loopback. No configuration of an OIDC deployment reaches this branch.
    #[tokio::test]
    async fn self_host_serves_the_blob_with_no_step_up() {
        let client = Client::new(ServerConfig::default());
        create_account(&client, "U1IDAAFibG9i")
            .await
            .assert_status(StatusCode::CREATED);

        let res = client
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["recovery_blob"].as_str(), Some("U1IDAAFibG9i"));
    }

    /// No credential at all is still a credential failure, ahead of everything
    /// this route decides.
    #[tokio::test]
    async fn no_bearer_gets_no_blob() {
        let client = client_with_login_age(10);
        client
            .send_as(Method::GET, "/api/v1/accounts/me/recovery_blob", None, None)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// The write-once column, from the route rather than from the store: a
    /// second, different blob is refused rather than accepted and dropped, so a
    /// client that showed its user twenty-four words for it learns that they
    /// are not the account's.
    #[tokio::test]
    async fn a_second_different_blob_is_refused_by_the_route() {
        let client = client_with_login_age(10);
        create_account(&client, "U1IDAAFibG9i")
            .await
            .assert_status(StatusCode::CREATED);

        let res = create_account(&client, "U1IDAAFvdGhlcg").await;
        res.assert_status(StatusCode::CONFLICT);
        assert_eq!(code_of(&res), RECOVERY_BLOB_EXISTS);

        // Re-sending the same bytes is the dropped-response retry and still
        // succeeds, which is the property `POST /accounts` promises.
        create_account(&client, "U1IDAAFibG9i")
            .await
            .assert_status(StatusCode::CREATED);

        let res = client
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        assert_eq!(res.json()["recovery_blob"].as_str(), Some("U1IDAAFibG9i"));
    }

    /// The bearer is not what is wrong, so the answer is not `401`.
    ///
    /// A `401` drives a client's refresh path, and a refresh does not move
    /// `auth_time` — the client would refresh into the identical refusal
    /// forever. What it must do instead is re-run the authorization request.
    #[tokio::test]
    async fn the_refusal_is_not_the_one_that_makes_a_client_refresh() {
        let client = client_with_a_plain_bearer();
        let res = client
            .send(Method::GET, "/api/v1/accounts/me/recovery_blob", None)
            .await;
        assert_ne!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "401 sends the client to its token refresh, which cannot fix this"
        );
        assert_eq!(res.status, StatusCode::FORBIDDEN);
        let _ = BEARER;
    }
}

//! `POST /api/v1/accounts` and `GET /api/v1/accounts/me`.
//!
//! Per `docs/06-server/api.md` §account. Both routes resolve the caller from
//! the verified token's `(iss, sub)`; nothing identifying is read out of the
//! request body. `POST` is the call the *first* device makes after OIDC login
//! to attach identity material to the account the token already names — it does
//! not choose an account, and it cannot choose an account id.

use axum::{
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    routing::{get, post},
    Json, Router,
};
use sunrise_onboarding::{AccountCreateRequest, AccountInfo};

use crate::auth::request::{authenticate, authenticate_bootstrap, RequestContext};
use crate::error::ApiError;
use crate::state::ServerState;
use crate::store::Account;

/// Mount account routes.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/accounts", post(create))
        .route("/accounts/me", get(me))
}

/// Render an account row as the API's `AccountInfo`.
fn info(state: &ServerState, account: &Account) -> Result<AccountInfo, ApiError> {
    Ok(AccountInfo {
        identity_id: account.account_id.clone(),
        email: account.email.clone().unwrap_or_default(),
        tier: account.tier.clone(),
        device_count: state.store.active_device_count(&account.account_id)?,
        created_at_ms: account.created_at_ms,
    })
}

async fn create(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<AccountInfo>), ApiError> {
    // The body is taken as raw bytes rather than `Json<T>` because those exact
    // bytes are what `X-Sunrise-Device-Sig` covers; deserialising first would
    // leave the signature checking a re-serialisation.
    let caller = authenticate_bootstrap(
        &state,
        RequestContext {
            method: &method,
            uri: &uri,
            headers: &headers,
            body: &body,
        },
    )
    .await?;

    let req: AccountCreateRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("malformed account body: {e}")))?;
    if req.email.trim().is_empty() {
        return Err(ApiError::validation("email required"));
    }
    if req.identity_signing_pub.trim().is_empty() || req.identity_dh_pub.trim().is_empty() {
        return Err(ApiError::validation("identity keys required"));
    }

    let account = state.store.set_identity(
        &caller.account.account_id,
        req.identity_signing_pub.trim(),
        req.identity_dh_pub.trim(),
        Some(req.recovery_blob.as_str()).filter(|b| !b.is_empty()),
        req.terms_at_ms,
    )?;

    // The IdP owns the email. The body's copy is only a fallback for self-host
    // deployments whose verifier emits no `email` claim at all.
    let email = caller
        .subject
        .email
        .clone()
        .unwrap_or_else(|| req.email.trim().to_ascii_lowercase());

    state.metrics.incr("sunrise_account_create_total");
    let mut out = info(&state, &account)?;
    out.email = email;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn me(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AccountInfo>, ApiError> {
    let caller = authenticate(
        &state,
        RequestContext {
            method: &method,
            uri: &uri,
            headers: &headers,
            body: &body,
        },
    )
    .await?;
    Ok(Json(info(&state, &caller.account)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Subject;
    use crate::{build_router, config::ServerConfig, StaticVerifier};
    use axum::body::to_bytes;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn create_body() -> serde_json::Value {
        serde_json::json!({
            "email": "user@example.com",
            "identity_signing_pub": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "identity_dh_pub": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "recovery_blob": "Cg",
            "terms_at_ms": 1_700_000_000_000_u64,
        })
    }

    fn post_accounts(body: &serde_json::Value, bearer: Option<&str>) -> Request<axum::body::Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/api/v1/accounts")
            .header("content-type", "application/json");
        if let Some(t) = bearer {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(axum::body::Body::from(body.to_string())).unwrap()
    }

    fn get_me(bearer: Option<&str>) -> Request<axum::body::Body> {
        let mut b = Request::builder().method("GET").uri("/api/v1/accounts/me");
        if let Some(t) = bearer {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    async fn json(res: axum::response::Response) -> serde_json::Value {
        let body = to_bytes(res.into_body(), 65536).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn account_create_round_trip() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let response = app
            .oneshot(post_accounts(&create_body(), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let v = json(response).await;
        assert_eq!(v["email"], "user@example.com");
        assert_eq!(v["tier"], "free");
    }

    #[tokio::test]
    async fn empty_email_rejected() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "email": "  ",
            "identity_signing_pub": "x",
            "identity_dh_pub": "y",
            "recovery_blob": "z",
            "terms_at_ms": 1_u64,
        });
        let response = app.oneshot(post_accounts(&body, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// The account id used to be the first 16 characters of the caller's own
    /// submitted public key — a value the caller chose, and therefore a value
    /// two callers could collide on deliberately.
    #[tokio::test]
    async fn the_account_id_is_not_derived_from_the_request_body() {
        let state = ServerState::new(ServerConfig::default());
        let app = build_router(state);
        let v = json(
            app.oneshot(post_accounts(&create_body(), None))
                .await
                .unwrap(),
        )
        .await;
        let id = v["identity_id"].as_str().unwrap();
        assert!(
            !"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".starts_with(id),
            "identity_id {id} is still an echo of the submitted key"
        );
    }

    /// `GET /accounts/me` used to answer `200 {"identity_id":"unauthenticated"}`
    /// to anybody. It must now report the caller's own persisted account.
    #[tokio::test]
    async fn me_reports_the_account_that_was_created() {
        let state = ServerState::new(ServerConfig::default());
        let app = build_router(state);
        let created = json(
            app.clone()
                .oneshot(post_accounts(&create_body(), None))
                .await
                .unwrap(),
        )
        .await;
        let res = app.oneshot(get_me(None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let me = json(res).await;
        assert_eq!(me["identity_id"], created["identity_id"]);
        assert_ne!(me["identity_id"], "unauthenticated");
    }

    fn two_tenants() -> StaticVerifier {
        let mut allowed = std::collections::HashMap::new();
        allowed.insert(
            "alice".to_string(),
            Subject::new("https://idp.example", "alice").with_email("alice@example.com"),
        );
        allowed.insert(
            "bob".to_string(),
            Subject::new("https://idp.example", "bob").with_email("bob@example.com"),
        );
        StaticVerifier { allowed }
    }

    /// Two OIDC subjects on one server must not see each other's account.
    #[tokio::test]
    async fn each_subject_gets_its_own_account() {
        let state =
            ServerState::new(ServerConfig::default()).with_verifier(Arc::new(two_tenants()));
        let app = build_router(state);
        let alice = json(app.clone().oneshot(get_me(Some("alice"))).await.unwrap()).await;
        let bob = json(app.oneshot(get_me(Some("bob"))).await.unwrap()).await;
        assert_ne!(alice["identity_id"], bob["identity_id"]);
        assert_eq!(alice["email"], "alice@example.com");
        assert_eq!(bob["email"], "bob@example.com");
    }

    /// The same subject twice is the same account — the id is stable across
    /// logins, which is the whole point of keying on `(iss, sub)`.
    #[tokio::test]
    async fn a_returning_subject_keeps_its_account_id() {
        let state =
            ServerState::new(ServerConfig::default()).with_verifier(Arc::new(two_tenants()));
        let app = build_router(state);
        let first = json(app.clone().oneshot(get_me(Some("alice"))).await.unwrap()).await;
        let second = json(app.oneshot(get_me(Some("alice"))).await.unwrap()).await;
        assert_eq!(first["identity_id"], second["identity_id"]);
    }

    #[tokio::test]
    async fn an_unknown_bearer_is_rejected() {
        let state =
            ServerState::new(ServerConfig::default()).with_verifier(Arc::new(two_tenants()));
        let app = build_router(state);
        let res = app.oneshot(get_me(Some("forged"))).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json(res).await["error"]["code"], "AUTH_TOKEN_INVALID");
    }

    /// `allow_signup = false`: the IdP still issues tokens, but a subject with
    /// no account here gets nothing.
    #[tokio::test]
    async fn signup_disabled_refuses_a_first_login() {
        let cfg = ServerConfig {
            allow_signup: false,
            ..Default::default()
        };
        let state = ServerState::new(cfg).with_verifier(Arc::new(two_tenants()));
        let app = build_router(state);
        let res = app.oneshot(get_me(Some("alice"))).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(json(res).await["error"]["code"], "AUTH_SIGNUP_DISABLED");
    }

    /// …but an account that already exists keeps working: the flag gates
    /// provisioning, not login.
    #[tokio::test]
    async fn signup_disabled_still_admits_an_existing_account() {
        let state = {
            let cfg = ServerConfig {
                allow_signup: false,
                ..Default::default()
            };
            ServerState::new(cfg).with_verifier(Arc::new(two_tenants()))
        };
        // Provision out of band, exactly as an operator would.
        let existing = state
            .store
            .resolve_account(
                &Subject::new("https://idp.example", "alice"),
                true,
                state.clock.now_ms(),
            )
            .unwrap();
        let app = build_router(state);
        let res = app.oneshot(get_me(Some("alice"))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json(res).await["identity_id"], existing.account_id);
    }

    /// A retried `POST /accounts` must not mint a second account or let a
    /// later caller swap the identity key out from under the first.
    #[tokio::test]
    async fn a_retried_create_is_idempotent() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let first = json(
            app.clone()
                .oneshot(post_accounts(&create_body(), None))
                .await
                .unwrap(),
        )
        .await;
        let mut second_body = create_body();
        second_body["identity_signing_pub"] = serde_json::json!("ATTACKER_KEY");
        let second = json(
            app.clone()
                .oneshot(post_accounts(&second_body, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(first["identity_id"], second["identity_id"]);
    }
}

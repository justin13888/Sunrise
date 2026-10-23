//! Deleting an account: `POST /api/v1/accounts/me/delete/initiate`, then
//! `DELETE /api/v1/accounts/me`.
//!
//! # Two calls, and what each proves
//!
//! Both are bearer + device signature, and both demand the OIDC step-up the
//! recovery blob does (`super::accounts::require_step_up`): a stolen session
//! is exactly an ordinary bearer, and deletion is the one act more final than
//! reading the recovery blob. The step-up is what proves *who* is asking. The
//! token is what makes the deletion deliberate: `initiate` mints a single-use
//! phrase, valid for fifteen minutes, and only a `DELETE` that hands it back
//! is honoured, so a client bug, a retried request or a mis-tap cannot delete
//! an account with one call.
//!
//! # Why the phrase comes back in the response, not by email
//!
//! `docs/06-server/api.md` described the phrase as "relayed by the OIDC issuer
//! to the user's verified email". No issuer offers that, and
//! `docs/00-product/non-goals.md` forbids this relay delivering email itself.
//! The email leg was never what made the request authentic — an attacker who
//! holds the session can read the inbox no better than the step-up lets them
//! sign in — so the phrase is returned to the caller who passed the step-up,
//! and the step-up carries the proof of presence the email was standing in
//! for. The same resolution #56 made for the recovery blob's OTP.
//!
//! # What a confirmed deletion does
//!
//! It marks the account at once. From then on the account may open no sync
//! session, publish no op, and start or finish no upload
//! (`refuse_if_pending_deletion`), and the maintenance pass erases every row
//! and file it owns once `[storage] account_delete_grace_days` have passed
//! (`crate::admin::maintenance`). Nothing undoes the mark: there is no route
//! for it, and the operator's `admin account delete --immediately` only moves
//! the erasure earlier.

use crate::api::error::{codes, ApiError};
use crate::api::signed::{Signed, SignedParts};
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::response::status::Accepted;
use serde::{Deserialize, Serialize};

/// How long a confirmation phrase stays valid: `api.md`'s fifteen minutes.
pub const PHRASE_TTL_MS: u64 = 15 * 60 * 1000;

/// Bytes of entropy in a phrase. 32, so its 52 Crockford characters are
/// unguessable inside the rate limit whatever the TTL.
const PHRASE_BYTES: usize = 32;

/// `POST /api/v1/accounts/me/delete/initiate` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeleteInitiateResponse {
    /// The single-use phrase `DELETE /api/v1/accounts/me` must carry: 52
    /// Crockford base-32 characters. Initiating again replaces it.
    #[schema(pattern = "^[0-9A-HJKMNP-TV-Z]{52}$")]
    pub confirm_phrase: String,
    /// When the phrase stops being accepted, ms since the epoch.
    pub expires_at_ms: u64,
}

/// `DELETE /api/v1/accounts/me` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct AccountDeleteRequest {
    /// The phrase `initiate` returned. Case-insensitive.
    pub confirm_phrase: String,
}

/// `DELETE /api/v1/accounts/me` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct AccountDeleteResponse {
    /// When the deletion was first confirmed, ms since the epoch. A repeated
    /// confirmation reports the first one: it does not move the erasure.
    pub requested_at_ms: u64,
    /// The earliest the account's data is erased, ms since the epoch; the
    /// first maintenance pass after it does so.
    pub erase_after_ms: u64,
}

/// Mint a deletion phrase for the calling account.
#[kynos::post(
    "/api/v1/accounts/me/delete/initiate",
    operation_id = "initiateAccountDeletion"
)]
pub async fn initiate(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Accepted<Json<DeleteInitiateResponse>>, ApiError> {
    let principal = &caller.principal;
    super::accounts::require_step_up(&state, principal, "account deletion")?;

    let mut raw = [0u8; PHRASE_BYTES];
    getrandom::getrandom(&mut raw).map_err(|_| ApiError::internal())?;
    let confirm_phrase = crockford(&raw);
    let expires_at_ms = state.clock.now_ms().saturating_add(PHRASE_TTL_MS);
    state
        .store
        .put_delete_token(
            &principal.account.account_id,
            &phrase_hash(&confirm_phrase),
            expires_at_ms,
        )
        .await?;

    tracing::info!(
        ev = "srv.account.delete_initiated",
        account_h = %crate::logging::account_h(&principal.account.account_id),
        expires_at_ms,
        "account deletion phrase issued"
    );
    Ok(Accepted::new(Json(DeleteInitiateResponse {
        confirm_phrase,
        expires_at_ms,
    })))
}

/// Confirm the calling account's deletion.
#[kynos::delete("/api/v1/accounts/me", operation_id = "deleteAccount")]
pub async fn delete(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<AccountDeleteRequest>,
) -> Result<Accepted<Json<AccountDeleteResponse>>, ApiError> {
    let principal = &caller.principal;
    let account_id = &principal.account.account_id;
    super::accounts::require_step_up(&state, principal, "account deletion")?;

    let now_ms = state.clock.now_ms();
    if !state
        .store
        .consume_delete_token(account_id, &phrase_hash(&body.confirm_phrase), now_ms)
        .await?
    {
        return Err(ApiError::forbidden(
            codes::ACCOUNT_DELETE_PHRASE_INVALID,
            "the confirmation phrase was already used, has expired, or was never issued: \
             initiate the deletion again",
        ));
    }
    let requested_at_ms = state
        .store
        .request_account_deletion(account_id, now_ms)
        .await?;
    let erase_after_ms =
        requested_at_ms.saturating_add(state.config.retention().account_delete_grace_ms);

    tracing::info!(
        ev = "srv.account.delete_requested",
        account_h = %crate::logging::account_h(account_id),
        delay_ms = erase_after_ms.saturating_sub(now_ms),
        "account deletion confirmed; new work is refused from now on"
    );
    Ok(Accepted::new(Json(AccountDeleteResponse {
        requested_at_ms,
        erase_after_ms,
    })))
}

/// Refuse new work for an account whose deletion has been confirmed.
///
/// Called by the routes that start something the deletion would then have to
/// chase: opening a sync session, publishing ops, and every step of an upload.
/// Reads and the account's own routes stay open, so a client can still learn
/// why it is being refused.
pub(crate) async fn refuse_if_pending_deletion(
    state: &ServerState,
    account_id: &str,
) -> Result<(), ApiError> {
    match state.store.account_deletion_requested(account_id).await? {
        None => Ok(()),
        Some(_) => Err(ApiError::forbidden(
            codes::ACCOUNT_PENDING_DELETION,
            "this account is being deleted and accepts no new sessions, ops or uploads",
        )),
    }
}

/// The stored form of a phrase: BLAKE3 of it, uppercased, so a database copy
/// cannot confirm a deletion and a phrase typed in lower case still matches.
fn phrase_hash(phrase: &str) -> [u8; 32] {
    *blake3::hash(phrase.trim().to_ascii_uppercase().as_bytes()).as_bytes()
}

/// Crockford base-32 of `bytes`, most significant bit first, the final group
/// zero-padded. 32 bytes is 256 bits, hence 52 characters.
fn crockford(bytes: &[u8]) -> String {
    let alphabet = sunrise_id::crockford::ALPHABET;
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(alphabet[((acc >> bits) & 31) as usize]));
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(alphabet[((acc << (5 - bits)) & 31) as usize]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{code_of, Client, BEARER};
    use crate::state::Clock;
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    const T0_MS: u64 = 1_800_000_000_000;

    #[derive(Debug)]
    struct TestClock(AtomicU64);
    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn self_host() -> (Client, Arc<TestClock>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock(AtomicU64::new(T0_MS)));
        let state = ServerState::with_clock(
            ServerConfig {
                blob_root: Some(dir.path().to_path_buf()),
                ..ServerConfig::default()
            },
            clock.clone(),
        );
        (Client::from_state(state), clock, dir)
    }

    async fn initiate(client: &Client) -> String {
        let res = client
            .send(Method::POST, "/api/v1/accounts/me/delete/initiate", None)
            .await;
        res.assert_status(StatusCode::ACCEPTED);
        res.json()["confirm_phrase"].as_str().unwrap().to_owned()
    }

    async fn confirm(client: &Client, phrase: &str) -> crate::api::testing::Res {
        client
            .send(
                Method::DELETE,
                "/api/v1/accounts/me",
                Some(&serde_json::json!({ "confirm_phrase": phrase })),
            )
            .await
    }

    /// The whole two-step flow: a 52-character phrase with a fifteen-minute
    /// life, consumed once, case-insensitively, and an answer that names when
    /// the data goes.
    #[tokio::test]
    async fn a_phrase_confirms_the_deletion_once() {
        let (client, _clock, _dir) = self_host();
        let res = client
            .send(Method::POST, "/api/v1/accounts/me/delete/initiate", None)
            .await;
        res.assert_status(StatusCode::ACCEPTED);
        let phrase = res.json()["confirm_phrase"].as_str().unwrap().to_owned();
        assert_eq!(phrase.len(), 52);
        assert_eq!(res.json()["expires_at_ms"], T0_MS + PHRASE_TTL_MS);

        let res = confirm(&client, &phrase.to_ascii_lowercase()).await;
        res.assert_status(StatusCode::ACCEPTED);
        let thirty_days = 30 * 24 * 60 * 60 * 1000;
        assert_eq!(res.json()["requested_at_ms"], T0_MS);
        assert_eq!(res.json()["erase_after_ms"], T0_MS + thirty_days);

        let again = confirm(&client, &phrase).await;
        again.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&again), codes::ACCOUNT_DELETE_PHRASE_INVALID);
    }

    /// A phrase never issued and a phrase past its fifteen minutes are both
    /// refused, and a wrong guess does not burn the right one.
    #[tokio::test]
    async fn a_wrong_or_expired_phrase_is_refused() {
        let (client, clock, _dir) = self_host();
        let phrase = initiate(&client).await;

        let wrong = confirm(&client, &crockford(&[7; 32])).await;
        assert_eq!(code_of(&wrong), codes::ACCOUNT_DELETE_PHRASE_INVALID);

        clock.0.store(T0_MS + PHRASE_TTL_MS, Ordering::SeqCst);
        let late = confirm(&client, &phrase).await;
        late.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&late), codes::ACCOUNT_DELETE_PHRASE_INVALID);

        // A fresh initiate replaces it, and the new one works.
        let phrase = initiate(&client).await;
        confirm(&client, &phrase)
            .await
            .assert_status(StatusCode::ACCEPTED);
    }

    /// An ordinary bearer — what a stolen session is — can neither start nor
    /// finish a deletion on a deployment with an identity provider.
    #[tokio::test]
    async fn both_steps_demand_a_fresh_authentication() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
        ));
        let client = Client::from_state(state);
        let res = client
            .send(Method::POST, "/api/v1/accounts/me/delete/initiate", None)
            .await;
        res.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&res), codes::AUTH_STEP_UP_REQUIRED);

        let res = confirm(&client, &crockford(&[1; 32])).await;
        res.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&res), codes::AUTH_STEP_UP_REQUIRED);
    }

    /// **Pending deletion refuses new work at once**: a sync session, an op
    /// publish on a session opened before the confirmation, and every upload
    /// step: a new init, and a chunk PUT and finalize on an upload begun
    /// before it.
    /// The account's own routes stay open.
    #[tokio::test]
    async fn an_account_pending_deletion_is_refused_new_work() {
        let (client, _clock, _dir) = self_host();
        let hello = serde_json::json!({
            "client_app_v": "0.1.0",
            "client_platform": "test",
            "wire_proto_supported": [u32::from(sunrise_cbor::version::WIRE_PROTO_V)],
            "doc_schema_min": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
            "doc_schema_max": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
            "crypto_suite_supported": [u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)],
            "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0,
            "trace": "01J000000000000000000000000",
        });
        let open = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello))
            .await;
        open.assert_status(StatusCode::CREATED);
        let session = open.json()["session_id"].as_str().unwrap().to_owned();

        // An upload begun before the mark, one of its two chunks stored.
        let chunks: [&[u8]; 2] = [b"first", b"second"];
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(&serde_json::json!({
                    "stream_id": "str_test",
                    "chunk_count": 2,
                    "size_bytes": 11,
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        let upload_id = res.json()["upload_id"].as_str().unwrap().to_owned();
        let chunk_paths = [0, 1].map(|idx| format!("/api/v1/blobs/{upload_id}/{idx}"));
        let put_chunk = |idx: usize| {
            client.send_bytes(
                Method::PUT,
                &chunk_paths[idx],
                "application/octet-stream",
                chunks[idx],
                &[],
            )
        };
        put_chunk(0).await.assert_status(StatusCode::NO_CONTENT);

        let phrase = initiate(&client).await;
        confirm(&client, &phrase)
            .await
            .assert_status(StatusCode::ACCEPTED);

        let refused = |res: &crate::api::testing::Res, what: &str| {
            assert_eq!(res.status, StatusCode::FORBIDDEN, "{what}");
            assert_eq!(code_of(res), codes::ACCOUNT_PENDING_DELETION, "{what}");
        };
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello))
            .await;
        refused(&res, "sync/session");
        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": "11111111111111111111111111111111",
                    "batch_id": 1,
                    "ops": [],
                })),
                &[("x-sunrise-session", &session)],
            )
            .await;
        refused(&res, "sync/ops");
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(&serde_json::json!({
                    "stream_id": "str_test",
                    "chunk_count": 1,
                    "size_bytes": 1,
                })),
            )
            .await;
        refused(&res, "blobs/init");
        refused(&put_chunk(1).await, "blobs chunk PUT");
        let hash = |bytes: &[u8]| hex::encode(blake3::hash(bytes).as_bytes());
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hash(&chunks.concat()),
                    "chunk_hashes": chunks.iter().map(|c| hash(c)).collect::<Vec<_>>(),
                })),
            )
            .await;
        refused(&res, "blobs/finalize");

        client
            .send(Method::GET, "/api/v1/accounts/me", None)
            .await
            .assert_status(StatusCode::OK);
    }

    /// 256 bits is 52 characters of the Crockford alphabet, with the last
    /// group padded, and the encoding is the usual big-endian one.
    #[test]
    fn the_phrase_is_crockford_base32() {
        assert_eq!(crockford(&[0; 32]), "0".repeat(52));
        assert_eq!(crockford(&[0xff; 32]).len(), 52);
        assert!(crockford(&[0xff; 32]).starts_with("ZZZZ"));
        // 0x08 0x42 = 00001 00001 00001 0(0000) -> "1110"
        assert_eq!(crockford(&[0x08, 0x42]), "1110");
        assert_eq!(phrase_hash(" abc "), phrase_hash("ABC"));
    }
}

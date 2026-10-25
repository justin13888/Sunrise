//! The account's key bundle, pinned (`/api/v1/accounts/me/key_bundle`).
//!
//! Per [ADR-0069](../../../../docs/11-adr/0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md)
//! and `docs/06-server/api.md` §Key bundle. A key bundle is every current
//! Stream key of the account, sealed to the account identity's `ID_D_pub` and
//! signed by the device that published it. A device restored from the
//! recovery code holds the identity and nothing else, and the
//! identity-addressed `key_envelope` ops that used to be its only source of
//! keys are trimmed with the rest of the op log. The bundle is not.
//!
//! It travels as a snapshot record does (ADR-0068): uploaded through the blob
//! routes, then pinned here by its committed `blob_id`. The relay keeps one per
//! account, outside op retention, and never collects the blob it names.
//!
//! # What the relay reads
//!
//! The five-byte magic prefix, and only its kind (`0x08`), so a replaced pin's
//! blob is never an attachment. The bundle is sealed to a key the relay does
//! not hold and signed by a key it does not hold; it cannot read, write or
//! alter one.
//!
//! # A replacement names what it replaces
//!
//! A publisher reads the pin, checks that the bundle it is about to pin holds
//! every key the pinned one does, and pins naming the blob it read. If another
//! device pinned in between, the relay answers `409 KEY_BUNDLE_SUPERSEDED` and
//! writes nothing; the publisher reads again. Two honest devices therefore
//! never replace a bundle with one that holds less. The relay itself can still
//! serve any bundle it ever held: freshness is not something it can be made to
//! prove, and ADR-0069 §Rollback says what that costs.

use crate::api::blobs::{account, hex_pattern, not_found, parse_blob_id, refuse_if_pending};
use crate::api::error::{codes, ApiError};
use crate::api::signed::{Signed, SignedParts};
use crate::blob::Manifest;
use crate::state::ServerState;
use crate::store::{KeyBundlePinned, PinnedKeyBundle};
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::openapi::Schema as OpenApiSchema;
use serde::{Deserialize, Serialize};
use sunrise_cbor::magic::{decode_prefix, MagicKind};

/// A manifest digest: 64 lowercase hex characters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DigestHex(pub String);

impl kynos::schema::Schema for DigestHex {
    fn schema(registry: &mut kynos::schema::registry::Registry) -> OpenApiSchema {
        hex_pattern(registry, "^[0-9a-f]{64}$")
    }
}

/// `PUT /api/v1/accounts/me/key_bundle` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PinKeyBundleRequest {
    /// The committed blob holding the bundle: `blb_` + 32 lowercase hex
    /// characters, as `POST /blobs/finalize` returned it.
    #[schema(pattern = "^blb_[0-9a-f]{32}$")]
    pub blob_id: String,
    /// The publisher's digest of the bundle's manifest. Opaque to the relay,
    /// which stores it and hands it back.
    pub manifest_digest: DigestHex,
    /// The `blob_id` of the pin this one replaces, as the publisher last read
    /// it; absent or `null` when it read none. A pin that does not match is
    /// `409 KEY_BUNDLE_SUPERSEDED`.
    #[serde(default)]
    #[schema(pattern = "^blb_[0-9a-f]{32}$")]
    pub replaces: Option<String>,
}

/// The account's pinned key bundle: the body of both operations.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct KeyBundlePin {
    /// The blob holding the bundle; `GET /blobs/{blob_id}` reads it.
    #[schema(pattern = "^blb_[0-9a-f]{32}$")]
    pub blob_id: String,
    /// The bundle's size in bytes.
    pub size_bytes: u64,
    /// The publisher's digest of the bundle's manifest, as it sent it.
    pub manifest_digest: DigestHex,
    /// When it was pinned, ms since the epoch, by the relay's clock.
    pub published_at_ms: u64,
}

impl KeyBundlePin {
    fn of(pin: &PinnedKeyBundle) -> Self {
        Self {
            blob_id: format!("blb_{}", hex::encode(pin.blob_key)),
            size_bytes: pin.size_bytes,
            manifest_digest: DigestHex(hex::encode(pin.manifest_digest)),
            published_at_ms: pin.published_at_ms,
        }
    }
}

fn parse_digest(s: &str) -> Result<[u8; 32], ApiError> {
    let mut out = [0u8; 32];
    if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(ApiError::validation(
            "manifest_digest must be 64 lowercase hex characters",
        ));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation("manifest_digest is not hex"))?;
    Ok(out)
}

/// Make a committed blob the account's key bundle.
///
/// The blob must be this account's, readable, and start with the key bundle's
/// magic prefix, and `replaces` must name the pin in place. The blob the
/// replaced pin named is then deleted: the relay keeps one bundle per account.
#[kynos::put("/api/v1/accounts/me/key_bundle", operation_id = "pinKeyBundle")]
pub async fn pin(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<PinKeyBundleRequest>,
) -> Result<Json<KeyBundlePin>, ApiError> {
    let blob = parse_blob_id(&body.blob_id)?;
    let digest = parse_digest(&body.manifest_digest.0)?;
    let replaces = body.replaces.as_deref().map(parse_blob_id).transpose()?;
    refuse_if_pending(&state, &caller).await?;
    let account_key = account(&caller);
    // The same `404` as a fetch: no oracle for another account's ciphertext.
    let Manifest {
        chunk_count,
        size_bytes,
    } = state
        .blobs
        .manifest(account_key, blob)
        .await?
        .ok_or_else(not_found)?;
    let first = state
        .blobs
        .get_committed(account_key, blob, 0)
        .await?
        .ok_or_else(not_found)?;
    if chunk_count == 0 || !matches!(decode_prefix(&first), Ok(p) if p.kind == MagicKind::KeyBundle)
    {
        return Err(ApiError::validation("blob_id does not name a key bundle"));
    }

    let account_id = &caller.principal.account.account_id;
    let pinned = PinnedKeyBundle {
        blob_key: blob,
        size_bytes,
        manifest_digest: digest,
        published_by: caller.device.as_ref().map(|d| d.device_id.clone()),
        published_at_ms: state.clock.now_ms(),
    };
    let released = match state
        .store
        .pin_key_bundle(account_id, &pinned, replaces)
        .await?
    {
        KeyBundlePinned::Pinned { released } => released,
        KeyBundlePinned::Superseded => {
            return Err(ApiError::conflict(
                codes::KEY_BUNDLE_SUPERSEDED,
                "another key bundle was pinned since this one was read",
            ))
        }
    };
    tracing::info!(
        ev = "srv.key_bundle.pinned",
        account_h = %crate::logging::account_h(account_id),
        blob_h = %crate::logging::id_h(&blob),
        n_bytes = size_bytes,
        "the account's key bundle is pinned"
    );
    state.metrics.incr("sunrise_key_bundle_pin_total");
    if let Some(old) = released {
        // Best effort, after the pin committed, as a replaced snapshot's is.
        match state.blobs.delete_committed(account_key, old).await {
            Ok(()) => {
                let _ = state.store.forget_blob(account_id, &old).await;
            }
            Err(e) => tracing::warn!(
                ev = "srv.key_bundle.release_failed",
                account_h = %crate::logging::account_h(account_id),
                blob_h = %crate::logging::id_h(&old),
                cause = %e,
                "a replaced key bundle could not be deleted; it is unreferenced"
            ),
        }
    }
    Ok(Json(KeyBundlePin::of(&pinned)))
}

/// The account's pinned key bundle, or `404` when none is.
#[kynos::get("/api/v1/accounts/me/key_bundle", operation_id = "fetchKeyBundle")]
pub async fn latest(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<KeyBundlePin>, ApiError> {
    let account_id = &caller.principal.account.account_id;
    let pin = state
        .store
        .pinned_key_bundle(account_id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(KeyBundlePin::of(&pin)))
}

#[cfg(test)]
mod tests {
    use crate::api::error::codes;
    use crate::api::testing::{Client, Res, BEARER, SECOND_BEARER};
    use kynos::http::{Method, StatusCode};

    const STREAM: &str = "00000000000000000000000000000000";
    const ROUTE: &str = "/api/v1/accounts/me/key_bundle";

    fn hex_hash(bytes: &[u8]) -> String {
        hex::encode(blake3::hash(bytes).as_bytes())
    }

    /// A bundle as the relay sees one: the key bundle magic, then opaque bytes.
    fn bundle(tag: u8) -> Vec<u8> {
        let mut r = b"SR\x08\x00\x01".to_vec();
        r.extend(std::iter::repeat_n(tag, 64));
        r
    }

    async fn upload(client: &Client, bearer: &str, bytes: &[u8]) -> String {
        let res = client
            .send_as(
                Method::POST,
                "/api/v1/blobs/init",
                Some(bearer),
                Some(&serde_json::json!({
                    "stream_id": STREAM, "chunk_count": 1, "size_bytes": bytes.len(),
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        let upload_id = res.json()["upload_id"].as_str().unwrap().to_owned();
        client
            .send_bytes(
                Method::PUT,
                &format!("/api/v1/blobs/{upload_id}/0"),
                "application/octet-stream",
                bytes,
                &[("authorization", bearer)],
            )
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let res = client
            .send_as(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(bearer),
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex_hash(bytes),
                    "chunk_hashes": [hex_hash(bytes)],
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()["blob_id"].as_str().unwrap().to_owned()
    }

    async fn pin(client: &Client, bearer: &str, blob_id: &str, replaces: Option<&str>) -> Res {
        client
            .send_as(
                Method::PUT,
                ROUTE,
                Some(bearer),
                Some(&serde_json::json!({
                    "blob_id": blob_id,
                    "manifest_digest": "ab".repeat(32),
                    "replaces": replaces,
                })),
            )
            .await
    }

    async fn latest(client: &Client, bearer: &str) -> Res {
        client.send_as(Method::GET, ROUTE, Some(bearer), None).await
    }

    #[tokio::test]
    async fn a_pinned_bundle_is_found_and_fetched() {
        let (client, _guard) = Client::with_blob_root();
        let bytes = bundle(1);
        let blob_id = upload(&client, BEARER, &bytes).await;
        latest(&client, BEARER)
            .await
            .assert_status(StatusCode::NOT_FOUND);

        let res = pin(&client, BEARER, &blob_id, None).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["blob_id"], blob_id.as_str());
        assert_eq!(res.json()["manifest_digest"], "ab".repeat(32));

        let res = latest(&client, BEARER).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["blob_id"], blob_id.as_str());
        assert_eq!(res.json()["size_bytes"], bytes.len());
        let fetched = client
            .send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
            .await;
        fetched.assert_status(StatusCode::OK);
        assert_eq!(fetched.bytes, bytes);
        assert_eq!(client.metrics.get("sunrise_key_bundle_pin_total"), 1);
    }

    /// A replacement names the pin it read. One that read a stale pin is
    /// refused and writes nothing; one that read the pin in place replaces it
    /// and the replaced blob is deleted.
    #[tokio::test]
    async fn a_replacement_must_name_the_pin_in_place() {
        let (client, _guard) = Client::with_blob_root();
        let first = upload(&client, BEARER, &bundle(1)).await;
        let second = upload(&client, BEARER, &bundle(2)).await;
        let third = upload(&client, BEARER, &bundle(3)).await;
        pin(&client, BEARER, &first, None)
            .await
            .assert_status(StatusCode::OK);

        let res = pin(&client, BEARER, &second, None).await;
        res.assert_status(StatusCode::CONFLICT);
        assert_eq!(res.json()["code"], codes::KEY_BUNDLE_SUPERSEDED);

        pin(&client, BEARER, &second, Some(&first))
            .await
            .assert_status(StatusCode::OK);
        let res = pin(&client, BEARER, &third, Some(&first)).await;
        res.assert_status(StatusCode::CONFLICT);

        assert_eq!(
            latest(&client, BEARER).await.json()["blob_id"],
            second.as_str()
        );
        client
            .send(Method::GET, &format!("/api/v1/blobs/{first}"), None)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    /// Only a key bundle is pinned: a snapshot record or an attachment is
    /// refused, so replacing the pin can never delete either.
    #[tokio::test]
    async fn only_a_committed_key_bundle_is_pinned() {
        let (client, _guard) = Client::with_blob_root();
        let attachment = upload(&client, BEARER, b"opaque attachment ciphertext").await;
        let res = pin(&client, BEARER, &attachment, None).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json()["code"], codes::VALIDATION_INVALID);

        let mut snapshot = b"SR\x04\x00\x01".to_vec();
        snapshot.extend([9u8; 32]);
        let snapshot = upload(&client, BEARER, &snapshot).await;
        pin(&client, BEARER, &snapshot, None)
            .await
            .assert_status(StatusCode::BAD_REQUEST);

        let res = pin(&client, BEARER, &format!("blb_{}", "0".repeat(32)), None).await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(res.json()["code"], codes::BLOB_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_malformed_digest_is_refused() {
        let (client, _guard) = Client::with_blob_root();
        let blob_id = upload(&client, BEARER, &bundle(1)).await;
        let res = client
            .send_as(
                Method::PUT,
                ROUTE,
                Some(BEARER),
                Some(&serde_json::json!({
                    "blob_id": blob_id, "manifest_digest": "AB".repeat(32),
                })),
            )
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
    }

    /// Tenant isolation: another account neither sees the pin nor can pin a
    /// blob it does not hold, and its own pin does not touch this one.
    #[tokio::test]
    async fn a_bundle_is_the_accounts_own() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        let blob_id = upload(&client, BEARER, &bundle(7)).await;
        pin(&client, BEARER, &blob_id, None)
            .await
            .assert_status(StatusCode::OK);

        latest(&client, SECOND_BEARER)
            .await
            .assert_status(StatusCode::NOT_FOUND);
        pin(&client, SECOND_BEARER, &blob_id, None)
            .await
            .assert_status(StatusCode::NOT_FOUND);
        let theirs = upload(&client, SECOND_BEARER, &bundle(8)).await;
        pin(&client, SECOND_BEARER, &theirs, None)
            .await
            .assert_status(StatusCode::OK);
        assert_eq!(
            latest(&client, BEARER).await.json()["blob_id"],
            blob_id.as_str()
        );
        assert_eq!(
            latest(&client, SECOND_BEARER).await.json()["blob_id"],
            theirs.as_str()
        );
    }

    /// Auth: no bearer, no pin and no read.
    #[tokio::test]
    async fn an_unauthenticated_caller_is_refused() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        let res = client.send_as(Method::GET, ROUTE, None, None).await;
        res.assert_status(StatusCode::UNAUTHORIZED);
        let res = client
            .send_as(
                Method::PUT,
                ROUTE,
                None,
                Some(&serde_json::json!({
                    "blob_id": format!("blb_{}", "0".repeat(32)),
                    "manifest_digest": "ab".repeat(32),
                })),
            )
            .await;
        res.assert_status(StatusCode::UNAUTHORIZED);
    }
}

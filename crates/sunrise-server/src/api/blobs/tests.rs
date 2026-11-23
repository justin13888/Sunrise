//! The blob routes through the router: upload, finalize, fetch and their
//! refusals.

use crate::api::error::codes;
use crate::api::testing::{Client, Res, BEARER, SECOND_BEARER};
use crate::state::{Clock, ServerState};
use crate::ServerConfig;
use kynos::http::{Method, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The ciphertext a test uploads. Opaque to the server by construction —
/// what matters is only that the hashes agree.
const CHUNKS: [&[u8]; 2] = [b"first-chunk-ciphertext", b"second-chunk-ciphertext"];

fn hex_hash(bytes: &[u8]) -> String {
    hex::encode(blake3::hash(bytes).as_bytes())
}

async fn init(client: &Client, chunk_count: u32) -> String {
    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/init",
            Some(&serde_json::json!({
                "stream_id": "str_test",
                "chunk_count": chunk_count,
                "size_bytes": 64,
            })),
        )
        .await;
    res.assert_status(StatusCode::OK);
    res.json()["upload_id"]
        .as_str()
        .expect("an upload id")
        .to_owned()
}

async fn put_chunk(client: &Client, upload_id: &str, idx: u32, body: &[u8]) -> StatusCode {
    client
        .send_bytes(
            Method::PUT,
            &format!("/api/v1/blobs/{upload_id}/{idx}"),
            "application/octet-stream",
            body,
            &[],
        )
        .await
        .status
}

/// Init, PUT every chunk in order, and finalize — the whole happy path,
/// returning the committed blob id.
async fn upload(client: &Client, chunks: &[&[u8]]) -> String {
    let count = u32::try_from(chunks.len()).expect("a sane chunk count");
    let upload_id = init(client, count).await;
    for (idx, chunk) in chunks.iter().enumerate() {
        let idx = u32::try_from(idx).expect("a sane chunk index");
        assert_eq!(
            put_chunk(client, &upload_id, idx, chunk).await,
            StatusCode::NO_CONTENT
        );
    }
    let whole: Vec<u8> = chunks.concat();
    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex_hash(&whole),
                "chunk_hashes": chunks.iter().map(|c| hex_hash(c)).collect::<Vec<_>>(),
            })),
        )
        .await;
    res.assert_status(StatusCode::OK);
    res.json()["blob_id"]
        .as_str()
        .expect("a blob id")
        .to_owned()
}

/// The whole two-phase commit, end to end.
#[tokio::test]
async fn chunks_finalize_and_fetch_round_trip() {
    let (client, _guard) = Client::with_blob_root();
    let upload_id = init(&client, 2).await;

    for (idx, chunk) in CHUNKS.iter().enumerate() {
        let idx = u32::try_from(idx).expect("two chunks fit in a u32");
        assert_eq!(
            put_chunk(&client, &upload_id, idx, chunk).await,
            StatusCode::NO_CONTENT
        );
    }

    let whole: Vec<u8> = CHUNKS.concat();
    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex_hash(&whole),
                "chunk_hashes": CHUNKS.iter().map(|c| hex_hash(c)).collect::<Vec<_>>(),
            })),
        )
        .await;
    res.assert_status(StatusCode::OK);
    let body = res.json();
    let blob_id = body["blob_id"].as_str().expect("a blob id").to_owned();
    assert_eq!(body["chunk_count"], 2);
    assert_eq!(body["size_bytes"], whole.len());

    // The read streams chunk by chunk; what comes back must still be the
    // exact ciphertext that went in.
    let fetched = client
        .send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
        .await;
    fetched.assert_status(StatusCode::OK);
    assert_eq!(fetched.bytes, whole, "the streamed body must reassemble");

    // Both directions counted the ciphertext they moved, and nothing else.
    let bytes = |direction| {
        client
            .metrics
            .get_with("sunrise_blob_bytes_total", &[("direction", direction)])
    };
    assert_eq!(bytes("upload"), whole.len() as u64);
    assert_eq!(bytes("download"), whole.len() as u64);
}

/// The client's hashes are a claim; finalize is the check.
#[tokio::test]
async fn finalize_rejects_a_chunk_that_does_not_hash_to_its_claim() {
    let (client, _guard) = Client::with_blob_root();
    let upload_id = init(&client, 1).await;
    assert_eq!(
        put_chunk(&client, &upload_id, 0, CHUNKS[0]).await,
        StatusCode::NO_CONTENT,
        "the chunk has to be stored, or finalize refuses it for the wrong reason"
    );

    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex_hash(b"something else"),
                "chunk_hashes": [hex_hash(b"something else")],
            })),
        )
        .await;

    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json()["code"], codes::BLOB_HASH_MISMATCH);
}

/// A chunk that never arrived is a 409, not a 400: the request is
/// well-formed and the upload simply is not finishable yet.
#[tokio::test]
async fn finalize_rejects_a_chunk_that_never_arrived() {
    let (client, _guard) = Client::with_blob_root();
    let upload_id = init(&client, 2).await;
    assert_eq!(
        put_chunk(&client, &upload_id, 0, CHUNKS[0]).await,
        StatusCode::NO_CONTENT,
        "chunk 0 has to be stored, or the 409 is about the wrong chunk"
    );

    let whole: Vec<u8> = CHUNKS.concat();
    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex_hash(&whole),
                "chunk_hashes": CHUNKS.iter().map(|c| hex_hash(c)).collect::<Vec<_>>(),
            })),
        )
        .await;

    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json()["code"], codes::BLOB_CHUNK_MISSING);
}

/// The refusal of a chunk under an upload that is not open, as the client
/// reads it: `sunrise_core::blob_sync` forgets a reserved upload id only on a
/// code that parses to `SyncOpInvalid`.
fn assert_upload_refused(res: &Res, why: &str) {
    res.assert_status(StatusCode::NOT_FOUND);
    let code = res.json()["code"].as_str().map(ToOwned::to_owned);
    assert_eq!(code.as_deref(), Some(codes::SYNC_OP_INVALID), "{why}");
    assert_eq!(
        code.as_deref()
            .and_then(sunrise_error::ErrorCode::from_wire_str),
        Some(sunrise_error::ErrorCode::SyncOpInvalid),
        "the client must read it as a refused upload id: {why}"
    );
}

async fn put_chunk_res(client: &Client, upload_id: &str, idx: u32, body: &[u8]) -> Res {
    client
        .send_bytes(
            Method::PUT,
            &format!("/api/v1/blobs/{upload_id}/{idx}"),
            "application/octet-stream",
            body,
            &[],
        )
        .await
}

/// A well-formed id `init` never handed out is refused, and the refused
/// chunk leaves nothing a `finalize` could commit: an upload that skips
/// `init` cannot skip its bounds or its open-upload ledger.
#[tokio::test]
async fn a_chunk_under_an_upload_never_opened_is_refused() {
    let (client, _guard) = Client::with_blob_root();
    let never = format!("up_{}", "0".repeat(32));
    let res = put_chunk_res(&client, &never, 0, CHUNKS[0]).await;
    assert_upload_refused(&res, "an id init never handed out");

    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": never,
                "content_hash": hex_hash(CHUNKS[0]),
                "chunk_hashes": [hex_hash(CHUNKS[0])],
            })),
        )
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json()["code"],
        codes::BLOB_CHUNK_MISSING,
        "the refused chunk was not stored"
    );
    assert_eq!(client.metrics.get("sunrise_blob_chunk_total"), 0);
}

/// An id that was opened and then finalized is spent: a late chunk under it
/// is refused rather than reopening the upload.
#[tokio::test]
async fn a_chunk_under_a_finalized_upload_is_refused() {
    let (client, _guard) = Client::with_blob_root();
    let upload_id = init(&client, 1).await;
    assert_eq!(
        put_chunk(&client, &upload_id, 0, CHUNKS[0]).await,
        StatusCode::NO_CONTENT
    );
    client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex_hash(CHUNKS[0]),
                "chunk_hashes": [hex_hash(CHUNKS[0])],
            })),
        )
        .await
        .assert_status(StatusCode::OK);

    let res = put_chunk_res(&client, &upload_id, 0, CHUNKS[1]).await;
    assert_upload_refused(&res, "an upload already finalized");
}

/// One account's open upload is not another's to write into.
#[tokio::test]
async fn a_chunk_under_another_accounts_upload_is_refused() {
    let (client, _guard) = Client::with_blob_root_and_verifier();
    let upload_id = init(&client, 1).await;
    let res = client
        .send_bytes(
            Method::PUT,
            &format!("/api/v1/blobs/{upload_id}/0"),
            "application/octet-stream",
            CHUNKS[0],
            &[("authorization", SECOND_BEARER)],
        )
        .await;
    assert_upload_refused(&res, "another account's upload");
}

/// A blob nobody committed is a 404 — the same 404 another account's blob
/// produces, so the route is not an existence oracle.
#[tokio::test]
async fn fetch_of_an_uncommitted_blob_is_404() {
    let (client, _guard) = Client::with_blob_root();
    let res = client
        .send(
            Method::GET,
            &format!("/api/v1/blobs/blb_{}", "0".repeat(32)),
            None,
        )
        .await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(res.json()["code"], codes::BLOB_NOT_FOUND);
}

/// A traversing upload id never reaches the store — and the exact status
/// says *where* it stops, which the previous `is_client_error` could not.
///
/// It is a `404` from the router, not the handler's `400`: `up_../../etc`
/// spans several path segments, so it matches no route and
/// `parse_upload_id` is never called. The refusal is real and the
/// traversal is contained, but the parse is not what contains it, and the
/// loose assertion read as though it were. The body carries no `code`
/// member at all, which is the other half of the same fact — it is kynos's
/// refusal rather than this surface's.
#[tokio::test]
async fn a_traversing_upload_id_is_rejected() {
    let (client, _guard) = Client::with_blob_root();
    let res = client
        .send_bytes(
            Method::PUT,
            "/api/v1/blobs/up_../../etc/0",
            "application/octet-stream",
            CHUNKS[0],
            &[],
        )
        .await;
    res.assert_status(StatusCode::NOT_FOUND);
}

/// And the parse itself, on an id that *does* occupy one segment, so this
/// is the assertion the test above was mistaken for: a well-shaped segment
/// that is not `up_` plus 32 lowercase hex is the handler's own `400`.
#[tokio::test]
async fn a_malformed_single_segment_upload_id_is_the_handlers_400() {
    let (client, _guard) = Client::with_blob_root();

    for id in ["up_zz", "up_..", "not_an_upload_id"] {
        let res = client
            .send_bytes(
                Method::PUT,
                &format!("/api/v1/blobs/{id}/0"),
                "application/octet-stream",
                CHUNKS[0],
                &[],
            )
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json()["code"], codes::VALIDATION_INVALID, "for {id}");
    }
}

/// An **uppercase** hex upload id is refused, at the route.
///
/// It used to be accepted: `parse_upload_id` is documented as "`up_` + 32
/// lowercase hex characters" and its refusal says "must be lowercase hex",
/// but the decode underneath is case-insensitive, so the message was a
/// claim nothing enforced. The parsers now enforce it, which is the
/// decision this test records rather than the gap the old one pinned.
///
/// Asserted through the route, not against `parse_hex16`, because the
/// refusal has to be the surface's `400 VALIDATION_INVALID` and not a
/// `404` from the router or a `500` from somewhere downstream — an
/// uppercase id occupies one path segment, so it reaches the handler.
#[tokio::test]
async fn an_uppercase_hex_upload_id_is_rejected() {
    let (client, _guard) = Client::with_blob_root();
    let res = client
        .send_bytes(
            Method::PUT,
            &format!("/api/v1/blobs/up_{}/0", "A".repeat(32)),
            "application/octet-stream",
            CHUNKS[0],
            &[],
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json()["code"], codes::VALIDATION_INVALID);
}

/// **Content addressing, which nothing checked.**
///
/// `FinalizeResponse` documents "re-uploading identical ciphertext
/// converges on the same id", and no test ever finalized the same content
/// twice or related `blob_id` to `content_hash`. The id is `blb_` plus the
/// first sixteen bytes of the BLAKE3 of the *concatenation*, so how a
/// client chose to split those bytes into chunks cannot move it — which is
/// what makes dedup across one account's devices work at all.
#[tokio::test]
async fn the_same_bytes_converge_on_one_blob_id_however_they_are_split() {
    let (client, _guard) = Client::with_blob_root();
    let whole: Vec<u8> = CHUNKS.concat();

    let as_one = upload(&client, &[&whole]).await;
    let as_two = upload(&client, &CHUNKS).await;
    assert_eq!(
        as_one, as_two,
        "the chunk split is the client's business, not the address's"
    );

    // And the id is the content address rather than an opaque handle the
    // server minted: it is derivable from the bytes alone.
    assert_eq!(as_one, format!("blb_{}", &hex_hash(&whole)[..32]));
}

/// The other direction, which is what makes the convergence above a
/// content address rather than a constant: any differing byte gives a
/// different id, including one in the tail that a truncated hash would
/// miss.
#[tokio::test]
async fn a_single_differing_byte_gives_a_different_blob_id() {
    let (client, _guard) = Client::with_blob_root();
    let mut flipped: Vec<u8> = CHUNKS.concat();
    let last = flipped.len() - 1;
    flipped[last] ^= 0x01;

    let original = upload(&client, &CHUNKS).await;
    let altered = upload(&client, &[&flipped]).await;
    assert_ne!(original, altered);

    // Both are readable and each reads back its own bytes, so the two ids
    // name two committed blobs rather than one overwriting the other.
    let fetched = client
        .send(Method::GET, &format!("/api/v1/blobs/{original}"), None)
        .await;
    fetched.assert_status(StatusCode::OK);
    assert_eq!(fetched.bytes, CHUNKS.concat());
}

/// **Blob paths are namespaced per caller, and nothing constrained it.**
///
/// `account_root` keys every pending and committed path by a BLAKE3 of the
/// account id, because content addressing across accounts would be a
/// cross-tenant read primitive: one account could name another's blob by
/// its hash. `fetch_of_an_uncommitted_blob_is_404` claims its refusal is
/// "the same 404 another account's blob produces" — but no blob test ever
/// had two accounts, so nothing would have failed if `account_root`
/// stopped keying on the caller.
#[tokio::test]
async fn one_account_cannot_fetch_anothers_blob_and_cannot_tell_it_apart() {
    let (client, _guard) = Client::with_blob_root_and_verifier();
    let blob_id = upload(&client, &CHUNKS).await;

    // The account that committed it reads it back.
    client
        .send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
        .await
        .assert_status(StatusCode::OK);

    // A second account naming the very same content address does not —
    // and gets byte for byte what a blob nobody ever committed produces,
    // so the route answers nothing about whose ciphertext exists.
    let theirs = client
        .send_as(
            Method::GET,
            &format!("/api/v1/blobs/{blob_id}"),
            Some(SECOND_BEARER),
            None,
        )
        .await;
    let nothing = client
        .send_as(
            Method::GET,
            &format!("/api/v1/blobs/blb_{}", "0".repeat(32)),
            Some(SECOND_BEARER),
            None,
        )
        .await;

    theirs.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(theirs.json()["code"], codes::BLOB_NOT_FOUND);
    assert_eq!(
        (theirs.status, theirs.bytes),
        (nothing.status, nothing.bytes),
        "the two refusals must be indistinguishable, or this is an oracle"
    );
}

/// `init` bounds the upload before any byte is written.
#[tokio::test]
async fn init_rejects_zero_chunks_and_an_oversized_blob() {
    let (client, _guard) = Client::with_blob_root();

    client
        .send(
            Method::POST,
            "/api/v1/blobs/init",
            Some(&serde_json::json!({
                "stream_id": "str_test",
                "chunk_count": 0,
                "size_bytes": 1,
            })),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    client
        .send(
            Method::POST,
            "/api/v1/blobs/init",
            Some(&serde_json::json!({
                "stream_id": "str_test",
                "chunk_count": 1,
                "size_bytes": super::MAX_BLOB_BYTES + 1,
            })),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// An empty chunk hashes to something, so it would finalize cleanly while
/// contributing nothing. Refused at the write instead.
#[tokio::test]
async fn an_empty_chunk_is_rejected() {
    let (client, _guard) = Client::with_blob_root();
    let upload_id = init(&client, 1).await;
    assert_eq!(
        put_chunk(&client, &upload_id, 0, b"").await,
        StatusCode::BAD_REQUEST
    );
}

/// Every blob operation is authenticated. This module ran unauthenticated
/// once, so the check is a test rather than a reading.
///
/// Against a verifier that actually checks. `ServerConfig::default()`
/// installs `NullVerifier`, for which an absent header verifies the empty
/// string and succeeds — deliberately, so that enabling authentication is
/// purely a matter of configuring a verifier. Asserting a 401 there would
/// be asserting the self-host escape hatch is broken.
#[tokio::test]
async fn every_blob_route_requires_a_bearer() {
    let (client, _guard) = Client::with_blob_root_and_verifier();
    for (method, path) in [
        (Method::POST, "/api/v1/blobs/init"),
        (Method::POST, "/api/v1/blobs/finalize"),
        (
            Method::PUT,
            "/api/v1/blobs/up_00000000000000000000000000000000/0",
        ),
        (
            Method::GET,
            "/api/v1/blobs/blb_00000000000000000000000000000000",
        ),
    ] {
        let res = client.send_as(method.clone(), path, None, None).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} must refuse an unauthenticated caller"
        );
    }
}

// ---------------------------------------------------------------------------
// Reads through a viewer grant (ADR-0074), shared with the snapshot tests
// ---------------------------------------------------------------------------

/// A third account: neither the owner nor the viewer a grant names.
pub(crate) const THIRD_BEARER: &str = "Bearer third";

/// The Stream the owner grants, 32 lowercase hex characters.
pub(crate) const SHARED_STREAM: &str = "0123456789abcdef0123456789abcdef";

/// When every sharing test starts, by the relay's clock.
pub(crate) const T0_MS: u64 = 1_704_067_200_000;

/// A clock a test moves by hand, so a grant's expiry is a value, not a wait.
#[derive(Debug)]
pub(crate) struct HandClock(AtomicU64);

impl Clock for HandClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl HandClock {
    pub(crate) fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

/// A relay on a temp blob root with three accounts: [`BEARER`] (alice) owns,
/// [`SECOND_BEARER`] (bob) views, [`THIRD_BEARER`] (carol) holds no grant.
pub(crate) fn sharing_relay() -> (Client, Arc<HandClock>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let clock = Arc::new(HandClock(AtomicU64::new(T0_MS)));
    let state = ServerState::with_clock(
        ServerConfig {
            blob_root: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        },
        clock.clone(),
    )
    .with_verifier(Arc::new(
        crate::StaticVerifier::default()
            .with("test", crate::Subject::new("https://idp.example", "alice"))
            .with("second", crate::Subject::new("https://idp.example", "bob"))
            .with("third", crate::Subject::new("https://idp.example", "carol")),
    ));
    (Client::from_state(state), clock, dir)
}

/// Alice registers a grant on `stream` under claim token `tag`; its id.
pub(crate) async fn register_grant(
    client: &Client,
    stream: &str,
    tag: u8,
    expires_at_ms: Option<u64>,
) -> String {
    let res = client
        .send_as(
            Method::POST,
            "/api/v1/sync/grants",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": stream,
                "claim": hex::encode([tag; 32]),
                "expires_at_ms": expires_at_ms,
            })),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()["grant_id"].as_str().unwrap().to_owned()
}

/// [`register_grant`], then bob claims it; its id.
pub(crate) async fn grant_to_bob(
    client: &Client,
    stream: &str,
    tag: u8,
    expires_at_ms: Option<u64>,
) -> String {
    let grant_id = register_grant(client, stream, tag, expires_at_ms).await;
    client
        .send_as(
            Method::POST,
            "/api/v1/sync/grants/claim",
            Some(SECOND_BEARER),
            Some(&serde_json::json!({ "claim": hex::encode([tag; 32]) })),
        )
        .await
        .assert_status(StatusCode::OK);
    grant_id
}

/// `bearer` withdraws `grant_id`.
pub(crate) async fn withdraw_grant(client: &Client, bearer: &str, grant_id: &str) {
    client
        .send_as(
            Method::DELETE,
            &format!("/api/v1/sync/grants/{grant_id}"),
            Some(bearer),
            None,
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

/// `GET path`, as `bearer`.
pub(crate) async fn get_as(client: &Client, bearer: &str, path: &str) -> Res {
    client.send_as(Method::GET, path, Some(bearer), None).await
}

/// The one refusal a read through a grant not in force gets, whatever the
/// case: asserted on status, code and the whole body, so no case can be told
/// from another.
pub(crate) fn assert_not_in_force(res: &Res, reference: &Res, case: &str) {
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json()["code"], codes::RELAY_GRANT_REVOKED, "{case}");
    assert_eq!(res.bytes, reference.bytes, "{case} must not be told apart");
}

/// The route the issue asked for: an attachment in a shared Stream is read
/// by the viewer, from the owner's account, through the grant it claimed.
#[tokio::test]
async fn a_viewer_fetches_the_owners_blob_through_a_grant() {
    let (client, _clock, _guard) = sharing_relay();
    let blob_id = upload(&client, &CHUNKS).await;
    let grant_id = grant_to_bob(&client, SHARED_STREAM, 1, None).await;

    let res = get_as(
        &client,
        SECOND_BEARER,
        &format!("/api/v1/blobs/{blob_id}?grant={grant_id}"),
    )
    .await;
    res.assert_status(StatusCode::OK);
    assert_eq!(res.bytes, CHUNKS.concat());

    // Without the grant bob reads his own account, which holds nothing.
    let own = get_as(&client, SECOND_BEARER, &format!("/api/v1/blobs/{blob_id}")).await;
    own.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(own.json()["code"], codes::BLOB_NOT_FOUND);

    // Through the grant, a blob the owner does not hold is the ordinary 404.
    let missing = get_as(
        &client,
        SECOND_BEARER,
        &format!("/api/v1/blobs/blb_{}?grant={grant_id}", "0".repeat(32)),
    )
    .await;
    missing.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["code"], codes::BLOB_NOT_FOUND);
}

/// Every grant not in force for the caller is the same `403`: unknown,
/// unclaimed, someone else's, the owner's own, withdrawn and expired alike,
/// and decided before the blob is looked up, so a refused read says nothing
/// about which blobs the owner holds either.
#[tokio::test]
async fn a_blob_read_through_a_grant_not_in_force_is_refused_alike() {
    let (client, clock, _guard) = sharing_relay();
    let blob_id = upload(&client, &CHUNKS).await;
    let held = grant_to_bob(&client, SHARED_STREAM, 1, Some(T0_MS + 60_000)).await;
    let unclaimed = register_grant(&client, SHARED_STREAM, 2, None).await;
    let fetch = |grant: &str| format!("/api/v1/blobs/{blob_id}?grant={grant}");

    let unknown = get_as(&client, SECOND_BEARER, &fetch(&"0".repeat(32))).await;
    assert_not_in_force(&unknown, &unknown, "an unknown grant");
    let nothing = get_as(
        &client,
        SECOND_BEARER,
        &format!(
            "/api/v1/blobs/blb_{}?grant={}",
            "0".repeat(32),
            "0".repeat(32)
        ),
    )
    .await;
    assert_not_in_force(&nothing, &unknown, "an unknown grant on a missing blob");
    for (bearer, grant, case) in [
        (SECOND_BEARER, unclaimed.as_str(), "an unclaimed grant"),
        (THIRD_BEARER, held.as_str(), "another account's grant"),
        (BEARER, held.as_str(), "the owner naming her own grant"),
    ] {
        let res = get_as(&client, bearer, &fetch(grant)).await;
        assert_not_in_force(&res, &unknown, case);
    }

    let withdrawn = grant_to_bob(&client, SHARED_STREAM, 3, None).await;
    get_as(&client, SECOND_BEARER, &fetch(&withdrawn))
        .await
        .assert_status(StatusCode::OK);
    withdraw_grant(&client, BEARER, &withdrawn).await;
    let res = get_as(&client, SECOND_BEARER, &fetch(&withdrawn)).await;
    assert_not_in_force(&res, &unknown, "a withdrawn grant");

    get_as(&client, SECOND_BEARER, &fetch(&held))
        .await
        .assert_status(StatusCode::OK);
    clock.set(T0_MS + 60_000);
    let expired = get_as(&client, SECOND_BEARER, &fetch(&held)).await;
    assert_not_in_force(&expired, &unknown, "an expired grant");
}

/// A grant id with a second spelling would be a second grant.
#[tokio::test]
async fn a_malformed_grant_is_a_validation_error() {
    let (client, _clock, _guard) = sharing_relay();
    let res = get_as(
        &client,
        SECOND_BEARER,
        &format!(
            "/api/v1/blobs/blb_{}?grant={}",
            "0".repeat(32),
            "AB".repeat(16)
        ),
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

//! `sunrise recover`, driven against a real relay.
//!
//! The unit tests in [`sunrise_cli::recover`] cover the decisions a recovery
//! makes before it touches anything — which directory it will accept, how the
//! AAD is derived, what it does with a code that arrives on stdin. What they
//! cannot cover is the sequence, and the sequence is where #180 says the bug
//! lives: *a recovery that mints the root before it can decrypt anything
//! produces a vault that opens and is empty, which looks exactly like success.*
//!
//! So this runs the two halves of the command against a live relay, in order,
//! and asserts on the vault they leave behind: the same account, a different
//! device, `ID_D_priv` in hand, and — the one that would catch an empty-vault
//! "success" — the task the founding device wrote before any of this started.
//!
//! The relay runs the default self-host `NullVerifier`, which is the single
//! configuration exempt from the recovery step-up (it maps every caller to one
//! account, so there is no second account for a stolen session to reach). The
//! gate itself is covered against a real verifier in
//! `sunrise-e2e/tests/recovery_blob_round_trip.rs`; what is under test here is
//! everything after the blob comes back.

use std::net::SocketAddr;
use std::time::Duration;

use sunrise_cli::recover;
use sunrise_core::{Command, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{StreamDraft, TaskDraft};
use sunrise_server::{ServerConfig, ServerState};
use tokio::task::JoinHandle;

const BEARER: &str = "self-host";
const FOUNDING_ROOT: [u8; 32] = [0x61; 32];

async fn spawn_relay() -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let state = ServerState::new(ServerConfig::default());
    let handle = tokio::spawn(async move {
        let _ = sunrise_server::serve(state, listener).await;
    });
    (addr, handle)
}

/// The founding half of `sunrise bootstrap`, without the process boundary:
/// seal a blob, upload it, write something, push it.
async fn found_account(dir: &std::path::Path, base_url: &str) -> (String, String) {
    let mut cfg = CoreConfig::production(dir.to_path_buf(), "test");
    cfg.sync = Some(sunrise_core::SyncConfig::new(base_url.to_owned()));
    let core = std::sync::Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(FOUNDING_ROOT),
                paired: None,
            },
        )
        .await
        .expect("open the founding vault"),
    );

    let seed = [0x77u8; sunrise_crypto::bip39::RECOVERY_ENTROPY_LEN];
    let blob = core.seal_recovery_blob(&seed).expect("the creator seals");
    let code = sunrise_crypto::bip39::encode_recovery_code(&seed);

    let outcome = sunrise_relay_client::bootstrap(
        base_url,
        BEARER,
        sunrise_onboarding::AccountCreateRequest {
            email: "alice@example.com".into(),
            identity_signing_pub: core.identity_signing_pub(),
            identity_dh_pub: core.identity_dh_pub(),
            recovery_blob: Some(blob),
            terms_at_ms: core.now_ms(),
        },
        sunrise_relay_client::DeviceIdentity {
            device_pub_s: core.device_signing_pub(),
            device_pub_d: None,
            device_cert: None,
            vault_device_id: Some(sunrise_id::crockford::encode_bytes(&core.device_id())),
            nickname: "founder".into(),
            platform: "linux".into(),
            app_version: None,
        },
    )
    .await
    .expect("the account registers");

    let stream = core
        .submit(Command::CreateStream(StreamDraft {
            name: "Travel".into(),
            ..Default::default()
        }))
        .await
        .expect("create stream")
        .entity;
    core.submit(Command::CreateTask(TaskDraft {
        title: "Renew passport".into(),
        stream_id: Some(stream),
        ..Default::default()
    }))
    .await
    .expect("create task");

    let plan = sunrise_cli::livesync::SyncPlan {
        sync: Some(
            sunrise_core::SyncConfig::new(base_url.to_owned())
                .with_credential(sunrise_core::TokenSource::new(Some(BEARER.to_owned()))),
        ),
        device_id: Some(outcome.device_id.clone()),
    };
    let _ = sunrise_cli::livesync::apply_plan(&core, &plan);
    wait_drained(&core).await;
    core.shutdown().await;

    (code.reveal().to_owned(), outcome.identity_id)
}

async fn wait_drained(core: &Core) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let QueryResult::SyncStatus(s) = core.query(Query::SyncStatus).await.expect("status")
            else {
                panic!("expected SyncStatus")
            };
            if s.outbox_pending == 0 && s.state == sunrise_sync::SyncState::Live {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the founding device never pushed its ops");
}

/// The command, end to end: words in, working vault out.
#[tokio::test]
async fn sunrise_recover_rebuilds_the_account_from_the_words_alone() {
    let (addr, relay) = spawn_relay().await;
    let base_url = format!("http://{addr}");

    let founding_dir = tempfile::tempdir().expect("a vault dir");
    let (code, _) = found_account(founding_dir.path(), &base_url).await;

    // Everything is gone. The words are all that is left.
    drop(founding_dir);

    let recovered_dir = tempfile::tempdir().expect("a vault dir");
    recover::require_empty(recovered_dir.path()).expect("a fresh directory is acceptable");

    // Steps 3-5. `restore_identity` fetches the account record, derives the
    // AAD from the `ID_S_pub` it carries, fetches the blob, and spends the
    // code — none of which has a vault to read anything out of.
    let identity = recover::restore_identity(&base_url, BEARER, &code)
        .await
        .expect("the code opens the blob the relay serves");

    // A keystore of this test's own, so the recovery registers its minted root
    // where nothing else will find it.
    let keystore = tempfile::tempdir().expect("a keystore");
    // Process-global, and safe here because this is the only test in this
    // binary: `SUNRISE_KEYSTORE` is the documented way to say where
    // `vault::resolve` puts the root a recovery mints, and minting it is step 6
    // rather than something the caller can hand in.
    std::env::set_var("SUNRISE_KEYSTORE", keystore.path());
    std::env::remove_var("SUNRISE_VAULT_ROOT");

    let mut lines: Vec<String> = Vec::new();
    let done = recover::rebuild_vault(
        recovered_dir.path(),
        identity,
        &base_url,
        BEARER,
        "test",
        &mut |l| lines.push(l.to_owned()),
    )
    .await
    .expect("the vault is rebuilt and catches up");

    assert!(!done.identity_id.is_empty());
    assert!(!done.relay_device_id.is_empty());
    assert!(
        lines.iter().any(|l| l.contains("re-keyed as device")),
        "step 6 has to say it re-keyed: {lines:?}"
    );

    // Reopen what the command left on disk — the state a user is in when they
    // next run `sunrise today` — and assert the two things that distinguish a
    // recovery from an empty vault that opened cleanly.
    let root = sunrise_cli::vault::resolve(recovered_dir.path(), &SystemRng).expect("the root");
    let reopened = Core::open(
        CoreConfig::production(recovered_dir.path().to_path_buf(), "test"),
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(root),
            paired: None,
        },
    )
    .await
    .expect("the restored vault reopens");

    assert!(
        reopened.holds_identity_key(),
        "the restored ID_D_priv must survive the command exiting, or the next \
         launch opens no identity envelope"
    );

    // The assertion that matters: content written before the recovery is
    // readable after it. Queried through the stream listing rather than a
    // dated view, so nothing here depends on when the task was scheduled.
    let QueryResult::Streams(streams) = reopened.query(Query::StreamList).await.expect("streams")
    else {
        panic!("expected Streams")
    };
    assert!(
        streams.iter().any(|s| s.name == "Travel"),
        "the recovered vault never read the Stream written before the recovery — \
         `recovery.md` §Recovery flow step 8. Saw {:?}",
        streams.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    reopened.shutdown().await;
    relay.abort();
}

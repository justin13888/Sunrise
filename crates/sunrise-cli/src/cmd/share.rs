//! `share key`, `share <stream> <share-key>` and `accept <link>`: one Stream
//! shared, read-only, with another account
//! (`docs/03-crypto/sharing-with-others.md` §Granting access).
//!
//! The owner's side writes the grant through `Core::share_stream`, registers
//! the relay viewer grant under a fresh claim token, and prints the
//! `sunrise://share/<token>` link. The recipient's side opens the grant
//! first, so a link made for someone else or signed by another key claims
//! nothing, and only then claims the relay grant.
//!
//! stdout carries what a script reads (the key, the link); the narration is
//! on stderr.

use sunrise_cli::i18n::strings;
use sunrise_cli::share::{
    decode_share_key, encode_share_key, expiry_after_days, mismatch_refusal, open_refusal,
};
use sunrise_cli::{livesync, login};
use sunrise_client_core::links::{DeepLink, ShareLink};
use sunrise_core::config::Rng as _;
use sunrise_relay_client::grants::{self, GrantCaller, GrantError};

use super::{flag_value, hex16, positionals, resolve_stream, Archived, CmdResult, Ctx};

/// The relay every grant call goes to, and who calls it.
struct Relay {
    url: String,
    bearer: String,
    device: String,
}

impl Relay {
    /// Read from the same places live sync does, before anything is written,
    /// so a share with nowhere to register leaves no op behind.
    fn from_env(ctx: &Ctx<'_>) -> CmdResult<Self> {
        let env = livesync::SyncEnv::from_process_env()
            .with_stored(&login::store_for(ctx.vault_dir), ctx.core.now_ms())
            .with_relay_device(ctx.vault_dir);
        let needs = || strings::share::needs_relay(livesync::ENV_SYNC_URL);
        let url = livesync::relay_origin(env.url.as_deref().ok_or_else(needs)?)?;
        let bearer = env
            .token
            .or(env.stored_token)
            .ok_or_else(strings::share::not_signed_in)?;
        let device = env.device_id.or(env.stored_device_id).ok_or_else(needs)?;
        Ok(Self {
            url,
            bearer,
            device,
        })
    }

    fn caller(&self) -> GrantCaller<'_> {
        GrantCaller {
            base_url: &self.url,
            bearer: &self.bearer,
            relay_device_id: &self.device,
        }
    }
}

/// `share key` | `share <stream> <share-key> [--days <n>]`.
pub(super) async fn share(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let words = positionals(rest, &["--days"], &[]);
    match words.as_slice() {
        [key] if key == "key" => {
            let core = ctx.core;
            println!(
                "{}",
                encode_share_key(&core.identity_signing_pub(), &core.identity_dh_pub())
            );
            eprintln!("{}", strings::share::key_note(&hex16(&core.identity_id())));
            Ok(())
        }
        [stream, key] => share_stream(ctx, stream, key, flag_value(rest, "--days")).await,
        _ => Err(strings::share::usage().into()),
    }
}

async fn share_stream(ctx: &Ctx<'_>, stream: &str, key: &str, days: Option<String>) -> CmdResult {
    let core = ctx.core;
    let stream = resolve_stream(core, stream, Archived::Exclude).await?;
    let (id_s_pub, id_d_pub) = decode_share_key(key).ok_or_else(strings::share::bad_key)?;
    let expiry = days
        .map(|d| expiry_after_days(core.now_ms(), &d).ok_or_else(strings::share::bad_days))
        .transpose()?;
    let relay = Relay::from_env(ctx)?;

    let grant = core.share_stream(stream, &id_s_pub, id_d_pub, expiry.map(|(_, at)| at))?;
    let mut claim = [0u8; 32];
    sunrise_core::SystemRng.fill_bytes(&mut claim);
    let core_for_sign = std::sync::Arc::clone(ctx.shared);
    grants::create_viewer_grant(
        &relay.caller(),
        *stream.bytes(),
        &claim,
        expiry.map(|(_, at)| at),
        core.now_ms(),
        move |m| core_for_sign.sign_with_device_key(m),
    )
    .await
    .map_err(|e| strings::share::unregistered(&e.to_string()))?;

    let link = ShareLink::new(claim, core.identity_signing_pub(), grant)?;
    println!("{}", DeepLink::Share(Box::new(link)).to_url());
    eprintln!(
        "{}",
        strings::share::shared(
            &stream.to_str(),
            &hex16(&sunrise_crypto::identity_id_from_pub(&id_s_pub)),
            &hex16(&core.identity_id()),
        )
    );
    if let Some((n, _)) = expiry {
        eprintln!("{}", strings::share::shared_until(i64::from(n)));
    }
    publish(core).await
}

/// Push the share_grant op to the relay before the command exits
/// (`docs/03-crypto/sharing-with-others.md` §Granting access, step 4). The
/// recipient reads the grant from the link, not the op; the op is the
/// account's record of the share, which the owner's other devices fold to
/// re-seal each later epoch to it, so an unpushed one is a share they do not
/// know of. Where the push does not finish, the owner is told the link is
/// made and that `sunrise sync` carries the rest.
async fn publish(core: &sunrise_core::Core) -> CmdResult {
    let waited = sunrise_client_core::sync::wait_until_caught_up(
        core,
        super::plumbing::DEADLINE_MS,
        &mut |_| {},
    )
    .await
    .map_err(|e| strings::share::unpublished(&e.to_string()))?;
    waited.map_err(|last| {
        strings::share::unpublished(&super::plumbing::not_caught_up(
            &last,
            core.sync_last_refusal(),
        ))
        .into()
    })
}

/// `accept <sunrise://share/...>`.
pub(super) async fn accept(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let [raw] = rest else {
        return Err(strings::share::usage().into());
    };
    let Some(DeepLink::Share(link)) = DeepLink::parse(raw.trim()) else {
        return Err(strings::share::not_a_link().into());
    };
    let core = ctx.core;
    // Opened before anything reaches the relay: a link for another account,
    // or one whose grant its key did not sign, claims nothing.
    let offer = core
        .open_share_grant(link.granter_id_s_pub(), link.grant_bytes())
        .map_err(|e| open_refusal(&e))?;
    eprintln!(
        "{}",
        strings::share::offered(&offer.name, &hex16(&offer.granter_identity_id))
    );
    if let Some(at) = offer.expires_at_ms {
        let date = jiff::Timestamp::from_millisecond(i64::try_from(at).unwrap_or(i64::MAX))
            .map(|t| {
                t.to_zoned(ctx.reader())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        eprintln!("{}", strings::share::offered_until(&date));
    }

    let relay = Relay::from_env(ctx)?;
    let core_for_sign = std::sync::Arc::clone(ctx.shared);
    let held = grants::claim_viewer_grant(
        &relay.caller(),
        link.claim_token(),
        core.now_ms(),
        move |m| core_for_sign.sign_with_device_key(m),
    )
    .await
    .map_err(|e| match e {
        GrantError::NotHeld => strings::share::not_held(),
        other => other.to_string(),
    })?;
    if held.stream_id != offer.stream_id {
        // The claim already bound the relay grant to this account; decline
        // it again so a refused link leaves nothing held, and say so where
        // the relay would not let go of it.
        let core_for_sign = std::sync::Arc::clone(ctx.shared);
        let declined = grants::withdraw_viewer_grant(
            &relay.caller(),
            held.grant_id,
            core.now_ms(),
            move |m| core_for_sign.sign_with_device_key(m),
        )
        .await
        .map_err(|e| e.to_string());
        return Err(mismatch_refusal(declined, &hex16(&held.grant_id)).into());
    }
    println!("{}", hex16(&held.grant_id));
    eprintln!("{}", strings::share::claimed(&hex16(&held.grant_id)));
    eprintln!(
        "{}",
        strings::general::note(&strings::share::not_yet_read())
    );
    Ok(())
}

//! `share key`, `share <stream> <share-key>`, `accept <link>` and the verbs
//! that list and end a share: one Stream shared, read-only, with another
//! account (`docs/03-crypto/sharing-with-others.md` §Granting access,
//! §Revoking access).
//!
//! The owner's side writes the grant through `Core::share_stream`, registers
//! the relay viewer grant under a fresh claim token, records that grant's id,
//! and prints the `sunrise://share/<token>` link. The recipient's side opens
//! the grant first, so a link made for someone else or signed by another key
//! claims nothing, then claims the relay grant and holds the Stream key
//! beside its id (`Core::accept_share`).
//!
//! `share list` shows a Stream's shares; `share revoke` ends one with a key
//! rotation and withdraws the relay grants this device recorded for it;
//! `share declined` records a recipient's decline; and the recipient's
//! `share decline` withdraws its relay grant and drops the held key.
//!
//! stdout carries what a script reads (the key, the link, the list); the
//! narration is on stderr.

use sunrise_cli::i18n::strings;
use sunrise_cli::share::{
    decode_share_key, encode_share_key, expiry_after_days, held_matching, held_stream_ref,
    mismatch_refusal, open_refusal, parse_identity_id, withdrawal_settled,
};
use sunrise_cli::{livesync, login};
use sunrise_client_core::links::{DeepLink, ShareLink};
use sunrise_core::config::Rng as _;
use sunrise_core::ShareEnd;
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

/// `share key` | `share <stream> <share-key> [--days <n>]` |
/// `share list <stream>` | `share revoke <stream> <identity>` |
/// `share declined <stream> <identity>` | `share decline <stream>`.
///
/// The verbs are matched before a Stream name, as `key` always was: a
/// Stream named `list` or `decline` is shared by its `str_` id or `#name`.
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
        [verb, stream] if verb == "list" => list(ctx, stream).await,
        [verb, stream] if verb == "decline" => decline(ctx, stream).await,
        [verb, stream, identity] if verb == "revoke" => {
            end(ctx, stream, identity, End::Revoke).await
        }
        [verb, stream, identity] if verb == "declined" => {
            end(ctx, stream, identity, End::Declined).await
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
    let registered = grants::create_viewer_grant(
        &relay.caller(),
        *stream.bytes(),
        &claim,
        expiry.map(|(_, at)| at),
        core.now_ms(),
        move |m| core_for_sign.sign_with_device_key(m),
    )
    .await
    .map_err(|e| strings::share::unregistered(&e.to_string()))?;
    // Kept so `share revoke` can withdraw it: the relay names it by this id
    // alone, and no op carries it.
    let recipient = sunrise_crypto::identity_id_from_pub(&id_s_pub);
    core.record_share_relay_grant(stream, recipient, registered.grant_id)?;

    let link = ShareLink::new(claim, core.identity_signing_pub(), grant)?;
    println!("{}", DeepLink::Share(Box::new(link)).to_url());
    eprintln!(
        "{}",
        strings::share::shared(
            &stream.to_str(),
            &hex16(&recipient),
            &hex16(&core.identity_id()),
        )
    );
    if let Some((n, _)) = expiry {
        eprintln!("{}", strings::share::shared_until(i64::from(n)));
    }
    publish(core, strings::share::unpublished).await
}

/// Push the op just written to the relay before the command exits
/// (`docs/03-crypto/sharing-with-others.md` §Granting access, step 4). For a
/// share, the recipient reads the grant from the link, not the op; the op is
/// the account's record of the share, which the owner's other devices fold
/// to re-seal each later epoch to it, so an unpushed one is a share they do
/// not know of. An ending op is what keeps them from re-sealing to it.
/// Where the push does not finish, `unpushed` says what stands and that
/// `sunrise sync` carries the rest.
async fn publish(core: &sunrise_core::Core, unpushed: fn(&str) -> String) -> CmdResult {
    let waited = sunrise_client_core::sync::wait_until_caught_up(
        core,
        super::plumbing::DEADLINE_MS,
        &mut |_| {},
    )
    .await
    .map_err(|e| unpushed(&e.to_string()))?;
    waited.map_err(|last| {
        unpushed(&super::plumbing::not_caught_up(
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
        eprintln!("{}", strings::share::offered_until(&local_time(ctx, at)));
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
    // Opened again here rather than trusting `offer`, so what is held is
    // what this call verified. A failure leaves the claim standing; a rerun
    // claims the same grant and holds the key then.
    core.accept_share(link.granter_id_s_pub(), link.grant_bytes(), held.grant_id)
        .map_err(|e| strings::share::hold_failed(&hex16(&held.grant_id), &e.to_string()))?;
    println!("{}", hex16(&held.grant_id));
    eprintln!("{}", strings::share::claimed(&hex16(&held.grant_id)));
    let stream = sunrise_id::EntityRef::new(sunrise_id::EntityKind::Stream, offer.stream_id);
    eprintln!("{}", strings::share::decline_hint(&stream.to_str()));
    eprintln!(
        "{}",
        strings::general::note(&strings::share::not_yet_read())
    );
    Ok(())
}

/// `share list <stream>`: the shares in force, then the ended ones, one
/// line each on stdout.
async fn list(ctx: &Ctx<'_>, stream: &str) -> CmdResult {
    let core = ctx.core;
    let stream = resolve_stream(core, stream, Archived::Include).await?;
    let shares = core.stream_shares(stream)?;
    let ended = core.ended_stream_shares(stream)?;
    if shares.is_empty() && ended.is_empty() {
        eprintln!("{}", strings::share::list_none(&stream.to_str()));
        return Ok(());
    }
    for share in &shares {
        let who = hex16(&share.recipient_identity_id);
        println!(
            "{}",
            match share.expires_at_ms {
                None => strings::share::list_forever(&who),
                Some(at) => strings::share::list_until(&who, &local_time(ctx, at)),
            }
        );
    }
    for gone in &ended {
        let who = hex16(&gone.recipient_identity_id);
        let at = local_time(ctx, gone.at_ms);
        println!(
            "{}",
            match gone.end {
                ShareEnd::Revoked => strings::share::list_revoked(&who, &at),
                ShareEnd::Declined => strings::share::list_declined(&who, &at),
            }
        );
    }
    Ok(())
}

/// How an owner ends a share.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// `share revoke`: the `ShareRevoke` op and a key rotation.
    Revoke,
    /// `share declined`: the `ShareDecline` op the recipient's word records.
    Declined,
}

/// The message for a grant the relay did not withdraw. Only a revocation
/// rotates the key, so only its message says the new key stops them reading.
fn withdraw_failed(how: End, grant: &str, error: &str) -> String {
    match how {
        End::Revoke => strings::share::withdraw_failed(grant, error),
        End::Declined => strings::share::withdraw_failed_declined(grant, error),
    }
}

/// `share revoke <stream> <identity>` | `share declined <stream> <identity>`.
///
/// The share is ended first, offline if need be: the rotation is the
/// guarantee, and a relay that cannot be reached must not hold it back
/// (`docs/03-crypto/sharing-with-others.md` §Revoking access step 3). Then
/// every relay grant this device registered for it is withdrawn, and the op
/// pushed. A share already ended whose relay grants this device still
/// records is not ended again; its grants are withdrawn, so a rerun after a
/// failed withdrawal finishes the job.
async fn end(ctx: &Ctx<'_>, stream: &str, identity: &str, how: End) -> CmdResult {
    let core = ctx.core;
    let stream = resolve_stream(core, stream, Archived::Include).await?;
    let recipient =
        parse_identity_id(identity).ok_or_else(|| strings::share::bad_identity(identity))?;
    let recorded = core.share_relay_grants(stream, recipient)?;
    let in_force = core
        .stream_shares(stream)?
        .iter()
        .any(|s| s.recipient_identity_id == recipient);
    let (who, at) = (hex16(&recipient), stream.to_str());
    let wrote = if in_force || recorded.is_empty() {
        // Not in force and nothing to withdraw: the core says why.
        match how {
            End::Revoke => {
                let epoch = core.revoke_share(stream, recipient)?;
                eprintln!("{}", strings::share::revoked(&who, &at, &epoch.to_string()));
            }
            End::Declined => {
                core.record_share_decline(stream, recipient)?;
                eprintln!("{}", strings::share::decline_recorded(&who, &at));
            }
        }
        true
    } else {
        eprintln!("{}", strings::share::already_ended());
        false
    };

    let mut failed = Vec::new();
    if recorded.is_empty() {
        if how == End::Revoke {
            eprintln!(
                "{}",
                strings::general::note(&strings::share::no_relay_grant())
            );
        }
    } else {
        // The share is already ended above, so a relay this device cannot
        // reach is a grant left to withdraw, not a reason to stop.
        match Relay::from_env(ctx) {
            Ok(relay) => {
                for grant in recorded {
                    let core_for_sign = std::sync::Arc::clone(ctx.shared);
                    let outcome = grants::withdraw_viewer_grant(
                        &relay.caller(),
                        grant,
                        core.now_ms(),
                        move |m| core_for_sign.sign_with_device_key(m),
                    )
                    .await;
                    if withdrawal_settled(&outcome) {
                        core.forget_share_relay_grant(stream, recipient, grant)?;
                        eprintln!("{}", strings::share::withdrawn(&hex16(&grant)));
                    } else if let Err(e) = outcome {
                        failed.push(withdraw_failed(how, &hex16(&grant), &e.to_string()));
                    }
                }
            }
            Err(e) => {
                let why = e.to_string();
                failed.extend(
                    recorded
                        .iter()
                        .map(|grant| withdraw_failed(how, &hex16(grant), &why)),
                );
            }
        }
    }
    if wrote {
        if let Err(e) = publish(core, strings::share::end_unpublished).await {
            failed.push(e.to_string());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("\n").into())
    }
}

/// `share decline <stream>`: the recipient's decline. Every relay grant the
/// held share records is withdrawn first; only once the relay has let go of
/// each is the key dropped, so a refusal leaves the share as it was for a
/// rerun.
async fn decline(ctx: &Ctx<'_>, typed: &str) -> CmdResult {
    let core = ctx.core;
    let held = core.held_shares()?;
    let named = held_matching(&held, typed);
    let mut streams: Vec<String> = named.iter().map(|h| held_stream_ref(h).to_str()).collect();
    streams.sort_unstable();
    streams.dedup();
    match streams.len() {
        0 => return Err(strings::share::nothing_held(typed).into()),
        1 => {}
        _ => return Err(strings::share::held_ambiguous(typed, &streams.join(", ")).into()),
    }
    let relay = Relay::from_env(ctx)?;
    for share in named {
        for grant in &share.relay_grant_ids {
            let core_for_sign = std::sync::Arc::clone(ctx.shared);
            let outcome =
                grants::withdraw_viewer_grant(&relay.caller(), *grant, core.now_ms(), move |m| {
                    core_for_sign.sign_with_device_key(m)
                })
                .await;
            if !withdrawal_settled(&outcome) {
                let why = outcome.err().map(|e| e.to_string()).unwrap_or_default();
                return Err(strings::share::decline_failed(&hex16(grant), &why).into());
            }
        }
        core.drop_held_share(share.granter_identity_id, share.stream_id)?;
        eprintln!(
            "{}",
            strings::share::declined(&share.name, &hex16(&share.granter_identity_id))
        );
    }
    Ok(())
}

/// `ms` as `YYYY-MM-DD HH:MM` in the reader's zone.
fn local_time(ctx: &Ctx<'_>, ms: u64) -> String {
    jiff::Timestamp::from_millisecond(i64::try_from(ms).unwrap_or(i64::MAX))
        .map(|t| {
            t.to_zoned(ctx.reader())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

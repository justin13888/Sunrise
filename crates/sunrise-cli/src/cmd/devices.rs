//! `devices`, `device revoke`, `device withdraw` and `identity`: who is on the
//! account.

use sunrise_cli::i18n::strings;
use sunrise_core::{
    Command, Core, Query, QueryResult, RevokeReason, Withdrawal, WithdrawalOutcome,
};
use sunrise_id::{EntityKind, EntityRef};

use super::{flag_value, hex16, CmdResult, Ctx};

/// Report the devices the account has stopped calling revoked while still
/// giving them no keys.
///
/// Printed on **both** branches of `devices revoke` — the gated one and the
/// ordinary one — because it is not about the revocation the user just asked
/// for. `device_revocations` is a fold (ADR-0041), so applying any revocation
/// can discard an older one whose own author the ledger revokes, and the
/// devices that drops go back to reading as ordinary members on `sunrise
/// devices`. `core.device.revocation_unwound` says so in NDJSON; this is the
/// same fact where the person who ran the command can see it.
///
/// Silent when the set is empty, which is the overwhelmingly common case.
fn print_revocation_unwound(unwound: &[String]) {
    if unwound.is_empty() {
        return;
    }
    // The indentation is this report's layout, not the message's: it stays
    // here, and the catalog holds only the words.
    println!(
        "  - {}",
        strings::devices::unwound(i64::try_from(unwound.len()).unwrap_or(i64::MAX))
    );
    let row_note = strings::devices::unwound_row();
    for id in unwound {
        println!("      {id}  {row_note}");
    }
    println!("    {}", strings::devices::unwound_remedy());
}

/// Say which kind of revocation `devices revoke` made (ADR-0058 §7).
///
/// One signed by the account identity is the only kind no later op can
/// unwind; only the device holding that identity can withdraw it, while it is
/// in force. Every other kind is ordinary, and a later op can take it back off
/// the list (ADR-0041). Printed on the effective branch only: an authority revocation
/// is never gated, so a gated one is ordinary and already says that nothing
/// was revoked. When the holder could not sign only because some devices'
/// builds cannot read a signed revocation, those devices are named, since
/// updating them is what stands between the user and one nothing can unwind.
fn print_revocation_authority(outcome: &sunrise_core::CommandResult) {
    for line in revocation_authority_lines(outcome) {
        println!("{line}");
    }
}

/// The lines [`print_revocation_authority`] prints, separate so a test can
/// read them without a second device to revoke.
fn revocation_authority_lines(outcome: &sunrise_core::CommandResult) -> Vec<String> {
    if outcome.revoked_by_authority {
        return vec![format!("  - {}", strings::devices::authority_signed())];
    }
    if outcome.authority_awaits_update.is_empty() {
        return vec![format!("  - {}", strings::devices::authority_ordinary())];
    }
    let row_note = strings::devices::authority_awaits_update_row();
    let mut out = vec![format!(
        "  - {}",
        strings::devices::authority_awaits_update()
    )];
    out.extend(
        outcome
            .authority_awaits_update
            .iter()
            .map(|id| format!("      {id}  {row_note}")),
    );
    out.push(format!(
        "    {}",
        strings::devices::authority_awaits_update_note()
    ));
    out
}

/// Resolve a device id from a hex **prefix**, refusing an ambiguous one.
///
/// Full 32-character ids are not something a user retypes, and this CLI already
/// resolves streams and contexts by partial name. Ambiguity is an error rather
/// than a first match: the whole point of the command that uses this is to cut
/// off one specific device, and picking the wrong one is both destructive and
/// silent.
async fn resolve_device(core: &Core, prefix: &str) -> CmdResult<[u8; 16]> {
    let prefix = prefix.trim().to_ascii_lowercase();
    if prefix.is_empty() {
        return Err(strings::devices::no_prefix().into());
    }
    let QueryResult::Devices(rows) = core.query(Query::DeviceList).await? else {
        return Err(strings::general::expected_device_list().into());
    };
    let hits: Vec<[u8; 16]> = rows
        .iter()
        .filter(|d| hex16(&d.device_id).starts_with(&prefix))
        .map(|d| d.device_id)
        .collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(strings::devices::no_match(&prefix).into()),
        many => Err(strings::devices::ambiguous(
            i64::try_from(many.len()).unwrap_or(i64::MAX),
            &prefix,
        )
        .into()),
    }
}

/// `devices` — every device on the account, with the marks that tell their
/// states apart.
///
/// Two states, printed distinctly, because they are different facts and a
/// user who conflates them will draw the wrong conclusion. `revoked` is a
/// register entry naming a device id; `current` is whether the identity that
/// certified the device is the one in force. A device that left and certified
/// itself back in under a fresh id is `revoked: no, current: no` — the
/// register has never heard of the new id, and `current` is the only column
/// that shows it (ADR-0032, #105).
pub(super) async fn list(ctx: &Ctx<'_>) -> CmdResult {
    let core = ctx.core;
    let QueryResult::Devices(rows) = core.query(Query::DeviceList).await? else {
        return Err(strings::general::expected_device_list().into());
    };
    let me = core.device_id();
    if ctx.json {
        sunrise_cli::json::print(
            &rows
                .iter()
                .map(|d| sunrise_cli::json::device(d, &me))
                .collect(),
        );
        return Ok(());
    }
    for d in rows {
        let mut marks: Vec<String> = Vec::new();
        if d.device_id == me {
            marks.push(strings::devices::mark_this_device());
        }
        if d.revoked && d.revoked_by_authority {
            // ADR-0058 §7: an authority row names this device, so no op from
            // any device unwinds it while the identity that signed it is in
            // force. A plain "revoked" is the ordinary kind, which a later op
            // can take back off the list (ADR-0041). Gated on `revoked` too,
            // though the core says the converse cannot happen: a mark that
            // says "revoked" belongs only on a row the register calls revoked.
            marks.push(strings::devices::mark_revoked_by_authority());
        } else if d.revoked {
            marks.push(strings::devices::mark_revoked());
        } else if d.read_bounded {
            // The two revocation facts have come apart, which since ADR-0041
            // is ordinary rather than exotic: the register is a fold, so a
            // revocation stops being believed once the ledger shows its own
            // author was revoked first, while `device_read_bounds` never gives
            // the key bound back. Printing only `revoked` hid this device
            // entirely — it read as a plain member while receiving nothing.
            // The remedy is named because there is one.
            marks.push(strings::devices::mark_read_bounded());
        }
        if !d.current {
            // Deliberately not "compromised" or "impostor". An honest device
            // that has not yet applied a rotation reads exactly the same way
            // for a moment, and nothing on this replica can separate the two.
            marks.push(strings::devices::mark_not_current());
        }
        if d.admitted_after_revocation {
            // The third state, and the only one that is invisible in the two
            // above when the revocation could not rotate the identity: the
            // fresh id is then certified under the identity in force and reads
            // `revoked: no, current: yes`. Worded as an event rather than a
            // verdict, because the ordinary cause is an honest pairing (#144).
            marks.push(strings::devices::mark_admitted_after_revocation());
        }
        let suffix = if marks.is_empty() {
            String::new()
        } else {
            format!("  [{}]", marks.join(", "))
        };
        println!(
            "{}  {:<20} {}{suffix}",
            hex16(&d.device_id),
            d.nickname,
            d.platform
        );
    }
    // The other half of what a device list is for: not only "what has access"
    // but "what happens if this one goes". `ID_D_priv` lives on the device
    // that created the account and on one restored from the recovery code, and
    // nowhere else; the consequence of losing it is permanent and no recovery
    // feature added later can undo it, so it belongs under the list rather
    // than in a document (`docs/03-crypto/recovery.md`, #144).
    //
    // Printed only when it is true. A device that does not hold the key has
    // nothing to be careful about here, and a line saying so on every listing
    // is how a warning stops being read.
    if core.holds_identity_key() {
        println!();
        println!("{}", strings::devices::identity_key());
        println!("  - {}", strings::devices::identity_key_remedy());
    }
    Ok(())
}

/// `device revoke <id-prefix> [--reason …]` and `device withdraw <id-prefix>`.
pub(super) async fn device(core: &Core, rest: &[String]) -> CmdResult {
    let Some(verb) = rest.first().map(String::as_str) else {
        return Err(strings::devices::revoke_usage().into());
    };
    match verb {
        "revoke" => revoke(core, &rest[1..]).await,
        "withdraw" => withdraw(core, &rest[1..]).await,
        _ => Err(strings::devices::unknown_subcommand(verb).into()),
    }
}

/// `device withdraw <id-prefix>`: withdraw this device's own revocation
/// (ADR-0056).
///
/// The report says which of the outcomes the core returned, because only one
/// of them is "the device is back", and the relay's half separately, because
/// a relay already told has no reinstatement yet.
async fn withdraw(core: &Core, args: &[String]) -> CmdResult {
    let prefix = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(strings::devices::revoke_usage)?;
    let target = resolve_device(core, prefix).await?;
    let id = hex16(&target);
    // Not recorded for undo: its inverse is a new revocation, which the user
    // makes deliberately with `device revoke`.
    let outcome = match core
        .submit(Command::WithdrawRevocation {
            device_id: EntityRef::new(EntityKind::Device, target),
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(sunrise_core::CoreError::Engine(
            sunrise_core::EngineError::FeatureUnsupportedByDevices { devices, .. },
        )) => {
            let rows: Vec<String> = devices
                .iter()
                .map(|d| format!("      {}", hex16(d)))
                .collect();
            return Err(format!(
                "{}\n{}",
                strings::devices::withdraw_awaits_update(),
                rows.join("\n")
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    };
    let Some(w) = outcome.withdrawal else {
        return Err(strings::devices::withdraw_not_author(&id).into());
    };
    if w.outcome == WithdrawalOutcome::NotAuthor {
        return Err(strings::devices::withdraw_not_author(&id).into());
    }
    for line in withdrawal_lines(&id, w) {
        println!("{line}");
    }
    Ok(())
}

/// The lines [`withdraw`] prints for a withdrawal that was emitted, separate
/// so a test can read them without a vault. Empty for `NotAuthor`, which is
/// an error instead.
fn withdrawal_lines(id: &str, w: Withdrawal) -> Vec<String> {
    if w.outcome == WithdrawalOutcome::NotAuthor {
        return Vec::new();
    }
    let mut out = vec![strings::devices::withdraw_title(id)];
    out.push(format!(
        "  - {}",
        match w.outcome {
            WithdrawalOutcome::StillClaimed => strings::devices::withdraw_still_claimed(id),
            WithdrawalOutcome::RepairRequired => strings::devices::withdraw_repair(id),
            WithdrawalOutcome::Reinstated | WithdrawalOutcome::NotAuthor => {
                strings::devices::withdraw_reinstated(id)
            }
        }
    ));
    if w.keys_resent > 0 {
        out.push(format!(
            "  - {}",
            strings::devices::withdraw_keys(i64::from(w.keys_resent))
        ));
    }
    if w.outcome == WithdrawalOutcome::Reinstated && !w.read_bound_released {
        out.push(format!(
            "  - {}",
            strings::devices::withdraw_still_bounded()
        ));
    }
    out.push(format!(
        "  - {}",
        if w.relay_intent_cancelled {
            strings::devices::withdraw_relay_cancelled()
        } else {
            strings::devices::withdraw_relay_told()
        }
    ));
    out
}

/// `device revoke <id-prefix> [--reason …]`.
async fn revoke(core: &Core, args: &[String]) -> CmdResult {
    let prefix = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(strings::devices::revoke_usage)?;
    let reason = match flag_value(args, "--reason").as_deref() {
        None | Some("lost") => RevokeReason::Lost,
        Some("stolen") => RevokeReason::Stolen,
        Some("retired") => RevokeReason::Retired,
        Some("compromised") => RevokeReason::Compromised,
        Some(other) => return Err(strings::devices::unknown_reason(other).into()),
    };
    let target = resolve_device(core, prefix).await?;
    // Not recorded for undo: its inverse is `device withdraw`, which is
    // refused until every device reads one (ADR-0056 §7), so an undo that
    // could fail on the feature gate is not offered as one.
    let outcome = core
        .submit(Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, target),
            reason,
        })
        .await?;

    // The two-state disclosure `docs/03-crypto/key-rotation.md` §Revocation
    // asks for, and issue #160. These are different guarantees and the second
    // is the one a user pressing the button believes they are getting: the
    // local half is done and durable, the relay half is a queued intent that
    // needs a session. Printing only "revoked" would let a user with no
    // network believe a stolen laptop had been cut off from the server, which
    // it has not. Before any of it, because it makes the rest of the report
    // moot. The fold judges the sender of every `device_revoke` row it holds,
    // including the one this command has just written, and when this device
    // is itself revoked the op is stored and skipped on every replica — so
    // nothing was cut. The keys still rotated and the target is still a
    // recipient of the new epochs, the register saying it is current, and the
    // relay half was deliberately not queued. Printing "revoked" over that
    // would be the same disclosure failure as printing it over a queued relay
    // intent.
    //
    // The relay line is fixed copy rather than a read of
    // `relay_revocation_pending`, and that is exact rather than assumed: a
    // gated command leaves the target current in the register, and an intent
    // is owed only while the register calls its device revoked. So an older
    // intent for the same device — one an earlier, effective revocation queued
    // before it unwound — is held and not sent
    // (`crates/sunrise-core/src/relay_intents.rs`, #257).
    if outcome.revocation_gated {
        println!("{}", strings::devices::gated_title());
        println!(
            "  - {}",
            strings::devices::gated_still_current(&hex16(&target))
        );
        println!("  - {}", strings::devices::gated_relay());
        println!("  - {}", strings::devices::gated_kept());
        println!("  - {}", strings::devices::gated_untouched());
        print_revocation_unwound(&outcome.revocation_unwound);
        return Ok(());
    }
    println!("{}", strings::devices::revoked_title(&hex16(&target)));
    // Not "every Stream key rotated" unconditionally: that was a claim this
    // command could not always make. A vault row whose stream id is malformed
    // names no stream to rotate, and the revoked device goes on holding
    // whatever key it was last given for it.
    if outcome.unrotated_streams.is_empty() {
        println!("  - {}", strings::devices::revoked_all_rotated());
    } else {
        println!("  - {}", strings::devices::revoked_some_unrotated());
        let row_note = strings::devices::unrotated_row();
        for raw in &outcome.unrotated_streams {
            println!("      {raw}  {row_note}");
        }
        println!("    {}", strings::devices::unrotated_note());
    }
    print_revocation_authority(&outcome);
    println!("  - {}", strings::devices::no_recertify());
    // Said on the effective branch only: a gated revocation revoked nothing,
    // so there is nothing to undo. An ordinary revocation is withdrawn by
    // this device (ADR-0056); one signed by the account identity is not, so
    // for it pairing again is the only way back.
    if outcome.revoked_by_authority {
        println!("  - {}", strings::devices::irreversible());
    } else {
        println!("  - {}", strings::devices::withdrawable(&hex16(&target)));
    }
    print_revocation_unwound(&outcome.revocation_unwound);
    let pending = core.relay_revocation_pending(&target)?;
    if pending {
        println!("  - {}", strings::devices::relay_pending());
    } else {
        println!("  - {}", strings::devices::relay_told());
    }
    Ok(())
}

/// `identity [status|rotate [--keep-code]|new-code]`.
pub(super) async fn identity(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let core = ctx.core;
    let verb = rest.first().map_or("status", String::as_str);
    match verb {
        "status" => {
            let QueryResult::Identity(st) = core.query(Query::IdentityStatus).await? else {
                return Err(strings::general::expected_identity_status().into());
            };
            if ctx.json {
                sunrise_cli::json::print(&serde_json::json!({
                    "account": hex16(&st.genesis_identity_id),
                    "current": hex16(&st.current_identity_id),
                    "rotations": st.transitions,
                    "this_device_is_current": st.this_device_is_current,
                    "holds_recovery_key": st.holds_recovery_key,
                    "can_sponsor": st.can_sponsor,
                }));
                return Ok(());
            }
            // The genesis first and labelled "account": it is the value that
            // does not move, and the one two devices compare to decide they
            // belong together. The identity in force is a key, not a name.
            println!("account   {}", hex16(&st.genesis_identity_id));
            println!("current   {}", hex16(&st.current_identity_id));
            println!("rotations {}", st.transitions);
            println!(
                "this device {}",
                if st.this_device_is_current {
                    strings::identity::speaks()
                } else {
                    strings::identity::does_not_speak()
                }
            );
            println!(
                "recovery  {}",
                if st.holds_recovery_key {
                    strings::identity::can_seal()
                } else {
                    strings::identity::cannot_seal()
                }
            );
            // The line above and this one look alike and answer different
            // questions: that one is `ID_D_priv` and what this device can read
            // back, this is `ID_S_priv` and what it can say. A device added by
            // pairing is a full member that can admit nobody, and it should
            // not have to discover that from a failed `pair offer`.
            println!(
                "pairing   {}",
                if st.can_sponsor {
                    strings::identity::can_sponsor()
                } else {
                    strings::identity::cannot_sponsor()
                }
            );
            Ok(())
        }
        "rotate" => {
            // A new code is the default, because both reasons to rotate — the
            // code may be in someone else's hands, or `ID_S_priv` was taken
            // from the device that also holds `ID_D_priv` — are reasons the
            // old code's holder must not follow the account to the successor
            // (ADR-0070 §1). `--keep-code` is the deliberate exception, and
            // `--new-recovery-code` is the default's old explicit spelling.
            let keep = rest.iter().any(|a| a == "--keep-code");
            if !keep {
                // Checked before anything moves: a rotation that retires the
                // code and cannot issue the next one leaves the account with
                // no code at all.
                if !core.holds_identity_key() {
                    return Err(strings::identity::cannot_seal_code().into());
                }
                relay_transport(ctx).await?;
            }
            core.submit(Command::RotateIdentity {
                keep_recovery_code: keep,
            })
            .await?;
            let QueryResult::Identity(st) = core.query(Query::IdentityStatus).await? else {
                return Err(strings::general::expected_identity_status().into());
            };
            println!(
                "{}",
                strings::identity::rotated(&hex16(&st.genesis_identity_id))
            );
            println!(
                "  {}",
                strings::identity::current_now(&hex16(&st.current_identity_id))
            );
            if keep {
                println!("  {}", strings::identity::code_kept());
                if std::env::var(sunrise_cli::livesync::ENV_SYNC_URL).is_ok() {
                    push_rotation(core).await?;
                }
                return Ok(());
            }
            // The rotation reaches the relay first, with the chain record and
            // the key bundle a session publishes before it is `Live`; only
            // then is the new code's blob worth replacing the old one with.
            if let Err(e) = push_rotation(core).await {
                return Err(strings::identity::reissue_failed(&e.to_string()).into());
            }
            issue_code(ctx).await
        }
        "new-code" => {
            if !core.holds_identity_key() {
                return Err(strings::identity::cannot_seal_code().into());
            }
            push_rotation(core).await?;
            issue_code(ctx).await
        }
        other => Err(strings::identity::unknown_subcommand(other).into()),
    }
}

/// Wait until this vault's outbox is on the relay and the session is `Live`.
async fn push_rotation(core: &sunrise_core::Core) -> CmdResult {
    let waited = sunrise_client_core::sync::wait_until_caught_up(
        core,
        super::plumbing::DEADLINE_MS,
        &mut |_| {},
    )
    .await?;
    waited.map_err(|last| super::plumbing::not_caught_up(&last, core.sync_last_refusal()).into())
}

/// A transport to the relay bound to this device, for one account call
/// outside the sync driver.
async fn relay_transport(ctx: &Ctx<'_>) -> CmdResult<sunrise_core::BoxTransport> {
    let needs = || strings::identity::needs_relay(sunrise_cli::livesync::ENV_SYNC_URL);
    let url = std::env::var(sunrise_cli::livesync::ENV_SYNC_URL).map_err(|_| needs())?;
    let url = sunrise_cli::livesync::relay_origin(&url)?;
    let device = std::env::var(sunrise_cli::livesync::ENV_SYNC_DEVICE_ID)
        .ok()
        .or_else(|| sunrise_cli::livesync::load_relay_device_id(ctx.vault_dir))
        .ok_or_else(needs)?;
    let signer = ctx.shared.device_signer(device);
    let factory = sunrise_cli::livesync::sse_factory(&url, Some(signer));
    Ok(factory(ctx.core.sync_credential().read()).await?)
}

/// Seal a new recovery code, put its blob on the relay, and show the words.
async fn issue_code(ctx: &Ctx<'_>) -> CmdResult {
    let mut transport = relay_transport(ctx).await?;
    match sunrise_client_core::account::reissue_recovery_code(
        ctx.core,
        &sunrise_core::SystemRng,
        transport.as_mut(),
    )
    .await
    {
        Ok(code) => {
            eprintln!("  {}", strings::identity::code_retired());
            super::account::print_recovery_code(&code);
            Ok(())
        }
        Err(e) => Err(strings::identity::reissue_failed(&e.to_string()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::CommandResult;

    fn revocation(signed: bool, awaiting: Vec<String>) -> CommandResult {
        CommandResult::new(
            EntityRef::new(EntityKind::Device, [0x11; 16]),
            None,
            [0x22; 16],
            1,
        )
        .with_revocation_authority(signed, awaiting)
    }

    /// The three kinds ADR-0058 §7 asks a user to be able to tell apart each
    /// print their own line, and only the signed one says that just the device
    /// holding the account identity can undo it.
    #[test]
    fn a_revocation_says_whether_it_carries_the_accounts_authority() {
        let signed = revocation_authority_lines(&revocation(true, Vec::new()));
        assert_eq!(
            signed,
            vec![format!("  - {}", strings::devices::authority_signed())]
        );

        let ordinary = revocation_authority_lines(&revocation(false, Vec::new()));
        assert_eq!(
            ordinary,
            vec![format!("  - {}", strings::devices::authority_ordinary())],
            "a revocation from a device that cannot sign must not be called permanent"
        );
    }

    /// When only out-of-date builds kept the holder from signing, every one of
    /// them is named, so the user knows what to update.
    #[test]
    fn an_unsigned_revocation_names_the_devices_to_update() {
        let ids = vec!["b1".repeat(16), "b2".repeat(16)];
        let lines = revocation_authority_lines(&revocation(false, ids.clone()));
        assert_eq!(
            lines.first(),
            Some(&format!(
                "  - {}",
                strings::devices::authority_awaits_update()
            ))
        );
        for id in &ids {
            assert!(
                lines.iter().any(|l| l.contains(id.as_str())),
                "{id} must be named, got {lines:?}"
            );
        }
        assert_eq!(lines.len(), ids.len() + 2, "header, one row each, note");
        assert!(
            !lines.contains(&format!("  - {}", strings::devices::authority_signed())),
            "an unsigned revocation must not claim to be signed"
        );
    }

    fn withdrawal(
        outcome: WithdrawalOutcome,
        released: bool,
        keys: u32,
        cancelled: bool,
    ) -> Withdrawal {
        Withdrawal {
            outcome,
            read_bound_released: released,
            keys_resent: keys,
            relay_intent_cancelled: cancelled,
        }
    }

    /// **Only a reinstatement says the device is back** (ADR-0056 §6), and
    /// each of the other outcomes says why not. The keys line says the device
    /// can read what was written while it was out, and the relay line which
    /// half of the relay's telling was undone.
    #[test]
    fn a_withdrawal_says_whether_the_device_is_back() {
        let id = "a1".repeat(16);
        let back = withdrawal_lines(
            &id,
            withdrawal(WithdrawalOutcome::Reinstated, true, 3, true),
        );
        assert_eq!(
            back,
            vec![
                strings::devices::withdraw_title(&id),
                format!("  - {}", strings::devices::withdraw_reinstated(&id)),
                format!("  - {}", strings::devices::withdraw_keys(3)),
                format!("  - {}", strings::devices::withdraw_relay_cancelled()),
            ]
        );

        let claimed = withdrawal_lines(
            &id,
            withdrawal(WithdrawalOutcome::StillClaimed, false, 0, false),
        );
        assert_eq!(
            claimed[1],
            format!("  - {}", strings::devices::withdraw_still_claimed(&id))
        );
        assert!(
            claimed.contains(&format!("  - {}", strings::devices::withdraw_relay_told())),
            "a relay that may already refuse the device is said to"
        );
        assert!(!claimed
            .iter()
            .any(|l| l.contains(&strings::devices::withdraw_reinstated(&id))));

        let repair = withdrawal_lines(
            &id,
            withdrawal(WithdrawalOutcome::RepairRequired, true, 0, true),
        );
        assert_eq!(
            repair[1],
            format!("  - {}", strings::devices::withdraw_repair(&id))
        );

        let bounded = withdrawal_lines(
            &id,
            withdrawal(WithdrawalOutcome::Reinstated, false, 0, true),
        );
        assert!(
            bounded.contains(&format!(
                "  - {}",
                strings::devices::withdraw_still_bounded()
            )),
            "a reinstatement this device sends no keys to says so"
        );

        assert!(
            withdrawal_lines(
                &id,
                withdrawal(WithdrawalOutcome::NotAuthor, false, 0, false)
            )
            .is_empty(),
            "nothing was withdrawn, and the command fails instead"
        );
    }
}

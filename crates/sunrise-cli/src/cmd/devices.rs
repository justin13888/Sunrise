//! `devices`, `device revoke` and `identity`: who is on the account.

use sunrise_cli::i18n::strings;
use sunrise_core::{Command, Core, Query, QueryResult, RevokeReason};
use sunrise_id::{EntityKind, EntityRef};

use super::{flag_value, hex16, CmdResult};

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
pub(super) async fn list(core: &Core) -> CmdResult {
    let QueryResult::Devices(rows) = core.query(Query::DeviceList).await? else {
        return Err(strings::general::expected_device_list().into());
    };
    let me = core.device_id();
    for d in rows {
        let mut marks: Vec<String> = Vec::new();
        if d.device_id == me {
            marks.push(strings::devices::mark_this_device());
        }
        if d.revoked {
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

/// `device revoke <id-prefix> [--reason …]`.
pub(super) async fn device(core: &Core, rest: &[String]) -> CmdResult {
    let Some(verb) = rest.first().map(String::as_str) else {
        return Err(strings::devices::revoke_usage().into());
    };
    if verb != "revoke" {
        return Err(strings::devices::unknown_subcommand(verb).into());
    }
    let args = &rest[1..];
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
    // Not recorded for undo: a revocation has no inverse (ADR-0056 decides a
    // withdrawal, and until #383 builds it, pairing again is the way back).
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
    println!("  - {}", strings::devices::no_recertify());
    // Said on the effective branch only: a gated revocation revoked nothing,
    // so there is nothing to be unable to undo. ADR-0056 decides a withdrawal
    // by this device, and until #383 builds it, pairing again is the only way
    // back.
    println!("  - {}", strings::devices::irreversible());
    print_revocation_unwound(&outcome.revocation_unwound);
    let pending = core.relay_revocation_pending(&target)?;
    if pending {
        println!("  - {}", strings::devices::relay_pending());
    } else {
        println!("  - {}", strings::devices::relay_told());
    }
    Ok(())
}

/// `identity [status|rotate [--new-recovery-code]]`.
pub(super) async fn identity(core: &Core, rest: &[String]) -> CmdResult {
    let verb = rest.first().map_or("status", String::as_str);
    match verb {
        "status" => {
            let QueryResult::Identity(st) = core.query(Query::IdentityStatus).await? else {
                return Err(strings::general::expected_identity_status().into());
            };
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
            // The flag is phrased as what the user wants to happen — a new
            // code — rather than as the internal `keep_recovery_code`, which
            // is a double negative at the one moment a user must not misread
            // it.
            let keep = !rest.iter().any(|a| a == "--new-recovery-code");
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
            } else {
                eprintln!("  {}", strings::identity::code_retired());
            }
            Ok(())
        }
        other => Err(strings::identity::unknown_subcommand(other).into()),
    }
}

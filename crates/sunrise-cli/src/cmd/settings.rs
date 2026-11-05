//! Undo and redo, and preferences: the client's own state. Saved views are
//! `views.rs`.

use sunrise_cli::i18n::strings;
use sunrise_cli::prefs_text;
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::{PrefScope, PrefSource, PrefTarget};

use super::{has, positionals, CmdResult, Ctx};

/// `undo` — reverse the most recent write this vault recorded.
///
/// The history is this device's, kept across commands in the vault
/// directory (`sunrise_cli::undo_file`). An undo is a new write: it merges
/// like any other, so a field another device has changed since loses the
/// tie exactly as a manual edit would.
pub(super) async fn undo(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    if !rest.is_empty() {
        return Err(strings::undo::usage().into());
    }
    let label = ctx.history.undo(ctx.core).await?;
    ctx.dirty.set(true);
    match label {
        Some(l) => {
            println!("{}", strings::undo::undid(&l));
            Ok(())
        }
        None => Err(strings::undo::nothing().into()),
    }
}

/// `redo` — replay the most recently undone step.
pub(super) async fn redo(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    if !rest.is_empty() {
        return Err(strings::undo::redo_usage().into());
    }
    let label = ctx.history.redo(ctx.core).await?;
    ctx.dirty.set(true);
    match label {
        Some(l) => {
            println!("{}", strings::undo::redid(&l));
            Ok(())
        }
        None => Err(strings::undo::nothing_to_redo().into()),
    }
}

/// Where a resolved value came from, as printed.
pub(super) const fn source_name(s: PrefSource) -> &'static str {
    match s {
        PrefSource::Overlay => "device",
        PrefSource::Vault => "vault",
        PrefSource::Default => "default",
    }
}

/// `prefs [get <key>|set <key> <value> [--device]|clear <key> [--device]]`.
///
/// Writes go to the vault (synced) unless `--device` names this device's
/// overlay, or the key can live nowhere else (`docs/02-domain/preferences.md`
/// §Scope and resolution). The value is read in [`prefs_text`]'s spelling and
/// checked against the key's type before anything is sent.
pub(super) async fn prefs(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let words = positionals(rest, &[], &["--device"]);
    let device = has(rest, "--device");
    match words.first().map(String::as_str) {
        None | Some("list") => {
            let all = read_prefs(ctx).await?;
            if ctx.json {
                sunrise_cli::json::print(&all.iter().map(pref_json).collect());
                return Ok(());
            }
            for p in all {
                println!(
                    "{:<36} {:<24} {}",
                    p.key,
                    p.value
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), prefs_text::render),
                    source_name(p.source)
                );
            }
            Ok(())
        }
        Some("get") => {
            let [_, key] = words.as_slice() else {
                return Err(strings::prefs::usage().into());
            };
            prefs_text::spec(key)?;
            if let Some(entry) = sunrise_domain::DayScheduleEntry::parse_key(key) {
                return super::day_schedule::get_entry(ctx, key, &entry).await;
            }
            let p = read_prefs(ctx)
                .await?
                .into_iter()
                .find(|p| p.key == key.as_str())
                .ok_or_else(|| prefs_text::PrefTextError::UnknownKey(key.clone()))?;
            if ctx.json {
                sunrise_cli::json::print(&pref_json(&p));
                return Ok(());
            }
            println!(
                "{}",
                p.value
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), prefs_text::render)
            );
            Ok(())
        }
        Some("set") => {
            let [_, key, value @ ..] = words.as_slice() else {
                return Err(strings::prefs::usage().into());
            };
            if value.is_empty() {
                return Err(strings::prefs::usage().into());
            }
            let spec = prefs_text::spec(key)?;
            let value = prefs_text::parse_for(key, spec, &value.join(" "))?;
            let target = target(spec.scope, device);
            ctx.core
                .submit(Command::SetPreference {
                    key: key.clone(),
                    value: value.clone(),
                    target,
                })
                .await?;
            println!("{key} = {}", prefs_text::render(&value));
            Ok(())
        }
        Some("clear") => {
            let [_, key] = words.as_slice() else {
                return Err(strings::prefs::usage().into());
            };
            let spec = prefs_text::spec(key)?;
            ctx.core
                .submit(Command::ClearPreference {
                    key: key.clone(),
                    target: target(spec.scope, device),
                })
                .await?;
            println!("{key} cleared");
            Ok(())
        }
        Some(other) => Err(strings::prefs::unknown_verb(other).into()),
    }
}

/// One preference as `--json` prints it: `{key, value, source, scope}`, the
/// value in the spelling `prefs set` reads.
fn pref_json(p: &sunrise_domain::ResolvedPref) -> serde_json::Value {
    serde_json::json!({
        "key": p.key,
        "value": p.value.as_ref().map(prefs_text::render),
        "source": source_name(p.source),
        "scope": p.scope.as_str(),
    })
}

/// The store a write goes to: the device overlay when asked for or when the
/// key lives nowhere else, the vault otherwise.
pub(super) const fn target(scope: PrefScope, device: bool) -> PrefTarget {
    match scope {
        PrefScope::Device => PrefTarget::Device,
        PrefScope::VaultOverridable if device => PrefTarget::Device,
        _ => PrefTarget::Vault,
    }
}

pub(super) async fn read_prefs(ctx: &Ctx<'_>) -> CmdResult<Vec<sunrise_domain::ResolvedPref>> {
    match ctx.core.query(Query::Preferences).await? {
        QueryResult::Preferences(p) => Ok(p),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// `tz [show|set <IANA>|clear]` — the zone this device reads times in.
///
/// `set` keeps the zone in the vault directory and reports it to the core
/// through `Core::on_time_zone_changed`, which rewrites nothing stored: a
/// floating 09:00 is 09:00 in the new zone, and a zoned one keeps its own
/// (`docs/11-adr/0066-times-survive-travel.md`). `--tz` and `SUNRISE_TZ` still
/// win over it for the command they are given to.
pub(super) fn tz(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    use sunrise_cli::zone::{self, ZoneSource};
    match rest.first().map(String::as_str) {
        None | Some("show") => {
            let reader = ctx.reader();
            let name = reader.iana_name().unwrap_or("UTC").to_owned();
            let unknown = ctx.core.zone_unknown();
            let source = ctx.zone_source.get().as_str();
            if ctx.json {
                sunrise_cli::json::print(&serde_json::json!({
                    "zone": name,
                    "source": source,
                    "zone_unknown": unknown,
                }));
            } else if unknown {
                println!("{}", strings::tz::show_unknown(&name, source));
            } else {
                println!("{}", strings::tz::show(&name, source));
            }
            Ok(())
        }
        Some("set") => {
            let [_, name] = rest else {
                return Err(strings::tz::usage().into());
            };
            zone::check(name, ZoneSource::Set)?;
            zone::store(ctx.vault_dir, Some(name))?;
            let change =
                ctx.core
                    .on_time_zone_changed(name)
                    .map_err(|_| zone::ZoneError::Unknown {
                        name: name.clone(),
                        from: ZoneSource::Set,
                    })?;
            ctx.zone_source.set(ZoneSource::Set);
            println!(
                "{}",
                strings::tz::set(
                    &change.zone,
                    &change.previous,
                    i64::from(change.affected_tasks)
                )
            );
            if std::env::var(zone::ENV_TZ).is_ok_and(|v| !v.trim().is_empty()) {
                eprintln!(
                    "{}",
                    strings::general::note(&strings::tz::env_overrides(zone::ENV_TZ))
                );
            }
            Ok(())
        }
        Some("clear") => {
            zone::store(ctx.vault_dir, None)?;
            println!("{}", strings::tz::cleared());
            Ok(())
        }
        Some(_) => Err(strings::tz::usage().into()),
    }
}

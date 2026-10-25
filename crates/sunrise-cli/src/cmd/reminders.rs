//! `reminders [plan|kinds|on|off]`: this device's notification schedule,
//! every kind of the catalog (`docs/08-features/notifications.md`), and its
//! toggles.
//!
//! The plan is the client layer's ([`sunrise_client_core::reminders::schedule`]),
//! so the CLI shows what the apps would hand their OS. A terminal posts
//! nothing, so printing an alert is what showing it means here: the ledger
//! that remembers what was shown is kept beside the vault
//! ([`LEDGER_FILE`]), and an alert printed once is not printed again.

use std::path::Path;

use sunrise_cli::i18n::strings;
use sunrise_client_core::reminders::{
    self, catalog_entries, CatalogNotification, ClockStyle, NotificationLedger, ScheduleRequest,
};
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::{NotificationKind, ReminderKind};

use super::{CmdResult, Ctx};

/// The ledger's file name inside the vault directory. It holds ids, a zone
/// name and a fingerprint, no content, so it is not sealed.
pub(super) const LEDGER_FILE: &str = "notification-ledger.json";

/// The name a reminder kind is printed under. A kind name, not prose.
const fn kind_name(kind: ReminderKind) -> &'static str {
    match kind {
        ReminderKind::Task => "task",
        ReminderKind::Block => "block",
        ReminderKind::Routine => "routine",
    }
}

fn load_ledger(vault_dir: &Path) -> NotificationLedger {
    let json = std::fs::read_to_string(vault_dir.join(LEDGER_FILE)).ok();
    NotificationLedger::from_json(json.as_deref())
}

/// The plan for this device: what it would hand the OS over the next day,
/// and the alerts it would post now.
async fn plan(ctx: &Ctx<'_>) -> CmdResult<reminders::Schedule> {
    let QueryResult::Preferences(prefs) = ctx.core.query(Query::Preferences).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    let time_format = prefs
        .iter()
        .find(|p| p.key == "time_format")
        .and_then(|p| match &p.value {
            Some(sunrise_domain::PrefValue::Text(t)) => Some(t.clone()),
            _ => None,
        });
    let req = ScheduleRequest {
        now_ms: ctx.core.now_ms(),
        // A terminal has no notification permission to refuse.
        authorized: true,
        focus_scope: None,
        // Nothing is held by an OS on this surface, so the reconciliation
        // against it is everything desired.
        pending: Vec::new(),
        // `locale` reads as 24-hour on a terminal: there is no OS locale
        // setting to ask, and 24-hour is what every other time here prints.
        clock: ClockStyle::from_time_format(time_format.as_deref(), true),
        ledger: load_ledger(ctx.vault_dir),
        // A terminal cannot ask; the stall rule then reads the outbox's age.
        network_available: true,
    };
    let s = reminders::schedule(ctx.core, req).await?;
    std::fs::write(ctx.vault_dir.join(LEDGER_FILE), s.ledger.to_json())?;
    Ok(s)
}

fn catalog_json(n: &CatalogNotification) -> serde_json::Value {
    serde_json::json!({
        "identifier": n.identifier,
        "kind": n.kind.tag(),
        "fire_at_ms": n.fire_at_ms,
        "deferred_from_ms": n.deferred_from_ms,
        "entity": n.entity.map(|e| e.to_str()),
        // A device as `sunrise devices` names it.
        "device": n
            .entity
            .filter(|e| e.kind() == sunrise_id::EntityKind::Device)
            .map(|e| sunrise_client_core::ids::hex_id(e.bytes())),
        "subject": n.subject,
        "count": n.count,
        "title": n.title,
        "body": n.body,
        "link": n.link,
        "level": n.level,
        "actions": n.actions,
    })
}

/// `reminders [plan|kinds|on <kind>...|off <kind>...] [--device]`.
pub(super) async fn reminders(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let words = super::positionals(rest, &[], &["--device"]);
    match words.first().map(String::as_str) {
        None | Some("plan") => print_plan(ctx).await,
        Some("kinds") => kinds(ctx).await,
        Some(verb @ ("on" | "off")) => toggle(ctx, verb == "on", &words[1..], rest).await,
        Some(other) => Err(strings::reminders::usage(other).into()),
    }
}

/// `reminders kinds`: every kind of the catalog, whether it is on here, and
/// how it is delivered.
async fn kinds(ctx: &Ctx<'_>) -> CmdResult {
    let policy = reminders::read_catalog_policy(ctx.core).await?;
    let entries = catalog_entries(&policy);
    if ctx.json {
        sunrise_cli::json::print(&serde_json::json!(entries
            .iter()
            .map(|e| serde_json::json!({
                "kind": e.kind.tag(),
                "enabled": e.enabled,
                "always": e.kind.is_security(),
                "key": e.kind.enabled_key(),
                "level": e.kind.level(),
                "delivery": e.kind.delivery(),
                "audience": e.kind.audience(),
                "actions": e.kind.actions(),
            }))
            .collect::<Vec<_>>()));
        return Ok(());
    }
    for e in entries {
        let state = match (e.kind.is_security(), e.enabled) {
            (true, _) => "always",
            (false, true) => "on",
            (false, false) => "off",
        };
        println!(
            "{:<20} {:<7} {}",
            e.kind.tag(),
            state,
            serde_json::to_value(e.kind.audience())
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// `reminders on|off <kind>... [--device]`: set each kind's `.enabled` key,
/// for the account or, with `--device`, for this device only.
async fn toggle(ctx: &Ctx<'_>, on: bool, names: &[String], rest: &[String]) -> CmdResult {
    if names.is_empty() {
        return Err(strings::reminders::toggle_usage().into());
    }
    let mut keys = Vec::new();
    for name in names {
        let kind = NotificationKind::from_tag(name)
            .ok_or_else(|| strings::reminders::unknown_kind(name))?;
        let key = kind
            .enabled_key()
            .ok_or_else(|| strings::reminders::always_on(name))?;
        keys.push(key);
    }
    for key in keys {
        let spec =
            sunrise_domain::pref_spec(key).ok_or_else(strings::general::unexpected_result)?;
        let target = super::settings::target(spec.scope, super::has(rest, "--device"));
        // A preference, like `prefs set`: not recorded for undo.
        ctx.core
            .submit(Command::SetPreference {
                key: key.to_owned(),
                value: sunrise_domain::PrefValue::Bool(on),
                target,
            })
            .await?;
        println!("{key} = {on}");
    }
    Ok(())
}

/// `reminders [plan]`: this device's schedule for the next day, as the
/// client layer plans it for an OS: capped, earliest first, with the body
/// each would show, the ones it could not place with why, and the alerts it
/// posts now.
async fn print_plan(ctx: &Ctx<'_>) -> CmdResult {
    let s = plan(ctx).await?;
    let reader = ctx.reader();
    if ctx.json {
        sunrise_cli::json::print(&serde_json::json!({
            "enabled": s.policy.enabled,
            "zone_unknown": s.zone_unknown,
            "planned": s.desired.iter().map(|n| serde_json::json!({
                "identifier": n.identifier,
                "fire_at_ms": n.fire_at_ms,
                "kind": kind_name(n.kind),
                "entity": n.entity.to_str(),
                "title": n.title,
                "body": n.body,
            })).collect::<Vec<_>>(),
            "unscheduled": s.unscheduled.iter().map(|u| serde_json::json!({
                "entity": u.entity.to_str(),
                "kind": kind_name(u.kind),
                "reason": u.reason.to_string(),
            })).collect::<Vec<_>>(),
            "wind_down": s.wind_down.as_ref().map(|w| serde_json::json!({
                "identifier": w.identifier,
                "date": w.date.to_string(),
                "fire_at_ms": w.fire_at_ms,
                "sleep_at_ms": w.sleep_at_ms,
            })),
            "catalog": s.catalog.iter().map(catalog_json).collect::<Vec<_>>(),
            "alerts": s.alerts.iter().map(catalog_json).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    if !s.policy.enabled {
        eprintln!(
            "{}",
            strings::general::note(&strings::reminders::disabled())
        );
    }
    if s.zone_unknown {
        eprintln!("{}", strings::general::note(&strings::tz::unknown_note()));
    }
    let at = |ms: i64| {
        sunrise_client_core::vocab::instant_at(u64::try_from(ms).unwrap_or(0))
            .to_zoned(reader.clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string()
    };
    for a in &s.alerts {
        println!(
            "{}  {:<16} {}  {}",
            at(a.fire_at_ms),
            a.kind.tag(),
            a.title,
            a.body
        );
    }
    for n in &s.desired {
        println!(
            "{}  {:<16} {}  {}  {}",
            at(n.fire_at_ms),
            kind_name(n.kind),
            n.entity.to_str(),
            n.title,
            n.body
        );
    }
    for n in &s.catalog {
        println!(
            "{}  {:<16} {}  {}",
            at(n.fire_at_ms),
            n.kind.tag(),
            n.title,
            n.body
        );
    }
    if let Some(w) = &s.wind_down {
        let sleep =
            sunrise_client_core::vocab::instant_at(u64::try_from(w.sleep_at_ms).unwrap_or(0))
                .to_zoned(reader.clone())
                .strftime("%H:%M")
                .to_string();
        println!(
            "{}  {:<16} {}  {}",
            at(w.fire_at_ms),
            "wind-down",
            w.identifier,
            strings::reminders::wind_down(&sleep)
        );
    }
    for u in &s.unscheduled {
        eprintln!(
            "{}",
            strings::general::note(&strings::reminders::unscheduled(
                &u.entity.to_str(),
                &u.reason.to_string()
            ))
        );
    }
    Ok(())
}

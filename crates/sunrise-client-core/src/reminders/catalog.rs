//! The catalog kinds beyond the planner's three sources
//! (`docs/08-features/notifications.md` §Catalog, #347): the deadline
//! reminder, the two briefs and the triage nudge, scheduled and reconciled;
//! the security, sync and zone alerts, posted once ([`super::alerts`]).
//!
//! # Beside the reminders, not among them
//!
//! [`sunrise_domain::ReminderKind`] is the planner's source and a client
//! switches over it, so a kind added to it breaks every such client. The
//! catalog kinds are [`NotificationKind`]s instead, carried by
//! [`CatalogNotification`] and held under identifiers of their own:
//! [`CATALOG_PREFIX`] for the scheduled ones, which [`reconcile_catalog`]
//! manages, and [`ALERT_PREFIX`] for the posted ones, which nothing cancels.
//! [`super::reconcile`] never names either, and neither names a reminder.
//!
//! # Identity
//!
//! An entity's kind is `<prefix><kind>.<entity>`; a kind with one pending
//! request at a time (a brief, the nudge) is `<prefix><kind>`. Never a fire
//! time, so a re-plan in a new zone replaces rather than duplicates
//! (`docs/11-adr/0066-times-survive-travel.md` §5).

use std::collections::{BTreeMap, BTreeSet};

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_core::DeadlineCandidate;
use sunrise_domain::{
    deadline_fire_at, next_brief, place, Brief, DaySchedule, FocusWindow, InterruptionLevel,
    NotificationAction, NotificationKind, PrefValue, QuietHours, ResolvedPref, SunriseTime,
};
use sunrise_id::EntityRef;

use super::ledger::{NotificationLedger, NudgeRecord};
use super::policy::{flag, value};
use super::{ClockStyle, PendingNotification, ReminderPolicy};

/// Every scheduled catalog identifier starts with it.
pub const CATALOG_PREFIX: &str = "sunrise.notify.";

/// Every posted alert's identifier starts with it.
pub const ALERT_PREFIX: &str = "sunrise.alert.";

/// The morning brief's title.
pub const MORNING_TITLE: &str = "Good morning";
/// The morning brief's line when nothing needs a decision.
pub const MORNING_BODY: &str = "Plan your day";
/// The evening plan's title.
pub const EVENING_TITLE: &str = "Evening plan";
/// The evening plan's line.
pub const EVENING_BODY: &str = "Move what is open to tomorrow, or plan the week";
/// The triage nudge's title.
pub const TRIAGE_TITLE: &str = "Triage";

/// The catalog preferences: each kind's toggle, and the deadline's lead and
/// the briefs' offsets (§Preference keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPolicy {
    /// Each toggleable kind's `.enabled`, resolved (overlay, vault, default).
    pub enabled: BTreeMap<NotificationKind, bool>,
    /// `notifications.deadline.lead_s`.
    pub deadline_lead_s: u32,
    /// `notifications.morning_brief.offset_s`: after wake.
    pub morning_offset_s: i32,
    /// `notifications.evening_plan.offset_s`: before sleep.
    pub evening_offset_s: i32,
}

impl CatalogPolicy {
    /// Whether `kind` is on. A security event always is.
    #[must_use]
    pub fn is_on(&self, kind: NotificationKind) -> bool {
        kind.is_security()
            || self
                .enabled
                .get(&kind)
                .copied()
                .unwrap_or_else(|| kind.default_enabled())
    }
}

fn int(prefs: &[ResolvedPref], key: &str, default: i64) -> i64 {
    match value(prefs, key) {
        Some(PrefValue::Int(v)) => v,
        Some(PrefValue::Uint(v)) => i64::try_from(v).unwrap_or(i64::MAX),
        _ => default,
    }
}

/// The catalog preferences `prefs` describe, with the key table's defaults
/// for anything unset.
#[must_use]
pub fn catalog_policy(prefs: &[ResolvedPref]) -> CatalogPolicy {
    let enabled = NotificationKind::ALL
        .into_iter()
        .filter_map(|k| Some((k, flag(prefs, k.enabled_key()?, k.default_enabled()))))
        .collect();
    let clamp_i32 = |v: i64| i32::try_from(v.clamp(-86_400, 86_400)).unwrap_or(0);
    CatalogPolicy {
        enabled,
        deadline_lead_s: u32::try_from(int(prefs, "notifications.deadline.lead_s", 3600))
            .unwrap_or(u32::MAX),
        morning_offset_s: clamp_i32(int(prefs, "notifications.morning_brief.offset_s", 900)),
        evening_offset_s: clamp_i32(int(prefs, "notifications.evening_plan.offset_s", 7200)),
    }
}

/// One catalog notification: scheduled ([`CATALOG_PREFIX`]) or posted
/// ([`ALERT_PREFIX`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogNotification {
    /// Stable across re-plans; see the module docs.
    pub identifier: String,
    /// Changes whenever anything shown or fired on changes.
    pub revision: String,
    /// Which kind of the catalog.
    pub kind: NotificationKind,
    /// The title, in English.
    pub title: String,
    /// The line under it, in English; a client with its own catalog builds
    /// it from [`Self::kind`], [`Self::count`] and [`Self::subject`].
    pub body: String,
    /// The entity it is about: the task, the device. `None` for a brief.
    pub entity: Option<EntityRef>,
    /// A name it carries: a device's nickname, a zone.
    pub subject: Option<String>,
    /// A number it carries: the triage queue's length.
    pub count: Option<u32>,
    /// What a tap opens, as a `sunrise://` link.
    pub link: String,
    /// Its buttons, in order.
    pub actions: Vec<NotificationAction>,
    /// How loud it is.
    pub level: InterruptionLevel,
    /// The absolute instant to fire at, epoch ms, after focus and quiet
    /// hours.
    pub fire_at_ms: i64,
    /// When focus or quiet hours moved it, the instant it was due, epoch ms.
    pub deferred_from_ms: Option<i64>,
}

/// The parts of a notification before it is placed.
pub(super) struct Draft {
    pub(super) identifier: String,
    pub(super) kind: NotificationKind,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) entity: Option<EntityRef>,
    pub(super) subject: Option<String>,
    pub(super) count: Option<u32>,
    pub(super) link: String,
    pub(super) actions: Vec<NotificationAction>,
}

impl Draft {
    /// The draft for `kind`, with its own actions.
    pub(super) fn new(
        identifier: String,
        kind: NotificationKind,
        title: &str,
        body: String,
    ) -> Self {
        Self {
            identifier,
            kind,
            title: title.to_owned(),
            body,
            entity: None,
            subject: None,
            count: None,
            link: String::new(),
            actions: kind.actions().to_vec(),
        }
    }

    /// The same, opening `link` on a tap.
    pub(super) fn linked(mut self, link: &str) -> Self {
        link.clone_into(&mut self.link);
        self
    }

    /// Fired at `fire_at`, due at `raw`.
    pub(super) fn at(self, fire_at: Timestamp, raw: Timestamp) -> CatalogNotification {
        let fire_at_ms = fire_at.as_millisecond();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let actions: Vec<String> = self.actions.iter().map(|a| format!("{a:?}")).collect();
        for part in [self.kind.tag(), self.title.as_str(), self.body.as_str()]
            .into_iter()
            .chain(actions.iter().map(String::as_str))
        {
            hash = fnv(hash, part);
        }
        CatalogNotification {
            revision: format!("{fire_at_ms}.{hash:016x}"),
            identifier: self.identifier,
            kind: self.kind,
            title: self.title,
            body: self.body,
            entity: self.entity,
            subject: self.subject,
            count: self.count,
            link: self.link,
            actions: self.actions,
            level: self.kind.level(),
            fire_at_ms,
            deferred_from_ms: (fire_at != raw).then(|| raw.as_millisecond()),
        }
    }
}

/// FNV-1a over `part` and a terminating zero, so the same on every build.
pub(super) fn fnv(mut hash: u64, part: &str) -> u64 {
    for byte in part.bytes().chain(std::iter::once(0)) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The identifier of a scheduled kind about `entity`, or of the one pending
/// request a kind has.
#[must_use]
pub fn catalog_identifier(kind: NotificationKind, entity: Option<EntityRef>) -> String {
    match entity {
        Some(e) => format!("{CATALOG_PREFIX}{}.{e}", kind.tag()),
        None => format!("{CATALOG_PREFIX}{}", kind.tag()),
    }
}

/// A fingerprint of the triage queue: its task ids, in any order.
#[must_use]
pub fn triage_fingerprint(queue: &[EntityRef]) -> String {
    let ids: BTreeSet<String> = queue.iter().map(ToString::to_string).collect();
    format!(
        "{:016x}",
        ids.iter().fold(0xcbf2_9ce4_8422_2325, |h, id| fnv(h, id))
    )
}

/// What a catalog plan reads.
#[derive(Debug, Clone, Copy)]
pub struct CatalogInputs<'a> {
    /// "Now".
    pub now: Timestamp,
    /// The end of the window.
    pub horizon: Timestamp,
    /// The reader's zone.
    pub tz: &'a TimeZone,
    /// The day schedule, for the briefs and a dated deadline.
    pub schedule: &'a DaySchedule,
    /// [`sunrise_core::Core::deadline_candidates`].
    pub deadlines: &'a [DeadlineCandidate],
    /// The triage queue's task ids.
    pub triage: &'a [EntityRef],
    /// The focus sessions running.
    pub sessions: &'a [FocusWindow],
    /// How a time is written in a body.
    pub clock: ClockStyle,
}

/// The scheduled catalog notifications for this device, earliest first:
/// deadlines, the morning brief or the nudge in its place, the evening plan.
/// Updates `ledger`'s nudge records.
///
/// Every one is primary-only (§Multi-device dedup), so a device that is not
/// primary, or has notifications off, plans none.
#[must_use]
pub fn plan_catalog(
    policy: &ReminderPolicy,
    catalog: &CatalogPolicy,
    inputs: &CatalogInputs<'_>,
    ledger: &mut NotificationLedger,
) -> Vec<CatalogNotification> {
    ledger.settle_nudge(inputs.now.as_millisecond());
    let mut out = Vec::new();
    if !(policy.enabled && policy.settings.is_primary_device) {
        ledger.nudge_planned = None;
        return out;
    }
    let quiet = policy.settings.quiet_hours.as_ref();
    let placed = |raw: Timestamp, draft: Draft| -> Option<CatalogNotification> {
        if raw < inputs.now || raw > inputs.horizon {
            return None;
        }
        let at = place(raw, draft.kind, inputs.sessions, quiet, inputs.tz)?;
        Some(draft.at(at, raw))
    };
    if catalog.is_on(NotificationKind::Deadline) {
        out.extend(inputs.deadlines.iter().filter_map(|d| {
            let raw = deadline_fire_at(
                inputs.schedule,
                &d.hard_due_at,
                inputs.tz,
                catalog.deadline_lead_s,
                catalog.morning_offset_s,
            )?;
            placed(raw, deadline_draft(d, inputs))
        }));
    }
    out.extend(morning(catalog, inputs, quiet, ledger));
    if catalog.is_on(NotificationKind::EveningPlan) {
        let next = next_brief(
            inputs.schedule,
            inputs.now,
            inputs.tz,
            Brief::Evening,
            catalog.evening_offset_s,
        );
        if let Ok(Some((_, raw))) = next {
            let mut d = Draft::new(
                catalog_identifier(NotificationKind::EveningPlan, None),
                NotificationKind::EveningPlan,
                EVENING_TITLE,
                EVENING_BODY.to_owned(),
            );
            d = d.linked("sunrise://evening");
            out.extend(placed(raw, d));
        }
    }
    out.sort_by(|a, b| {
        a.fire_at_ms
            .cmp(&b.fire_at_ms)
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
    out
}

fn deadline_draft(d: &DeadlineCandidate, inputs: &CatalogInputs<'_>) -> Draft {
    let body = match &d.hard_due_at {
        SunriseTime::AllDay { .. } => "Due today".to_owned(),
        timed => timed.resolve_in(inputs.tz).map_or_else(
            || "Due".to_owned(),
            |at| format!("Due {}", inputs.clock.format(at, inputs.tz)),
        ),
    };
    let mut draft = Draft::new(
        catalog_identifier(NotificationKind::Deadline, Some(d.task)),
        NotificationKind::Deadline,
        &d.title,
        body,
    );
    draft.entity = Some(d.task);
    draft.link = format!("sunrise://task/{}", d.task);
    draft
}

/// "3 tasks need a decision".
fn needs_decision(n: usize) -> String {
    if n == 1 {
        "1 task needs a decision".to_owned()
    } else {
        format!("{n} tasks need a decision")
    }
}

/// The morning brief, with the queue folded in when the nudge is on; or,
/// with the brief off, the nudge alone, at most once a day and only for a
/// queue that changed since the last one.
fn morning(
    catalog: &CatalogPolicy,
    inputs: &CatalogInputs<'_>,
    quiet: Option<&QuietHours>,
    ledger: &mut NotificationLedger,
) -> Option<CatalogNotification> {
    let brief_on = catalog.is_on(NotificationKind::MorningBrief);
    let nudge_on = catalog.is_on(NotificationKind::TriageNudge) && !inputs.triage.is_empty();
    if !brief_on {
        // Planned again below when it still applies.
        ledger.nudge_planned = None;
    }
    if !(brief_on || nudge_on) {
        return None;
    }
    let (date, raw) = next_brief(
        inputs.schedule,
        inputs.now,
        inputs.tz,
        Brief::Morning,
        catalog.morning_offset_s,
    )
    .ok()??;
    if raw > inputs.horizon {
        return None;
    }
    let count = u32::try_from(inputs.triage.len()).unwrap_or(u32::MAX);
    let mut draft = if brief_on {
        let mut d = Draft::new(
            catalog_identifier(NotificationKind::MorningBrief, None),
            NotificationKind::MorningBrief,
            MORNING_TITLE,
            MORNING_BODY.to_owned(),
        );
        d = d.linked("sunrise://morning");
        if nudge_on {
            d.body = needs_decision(inputs.triage.len());
            d.count = Some(count);
            d.actions.push(NotificationAction::OpenTriage);
        }
        d
    } else {
        let fingerprint = triage_fingerprint(inputs.triage);
        let seen = ledger
            .nudged
            .as_ref()
            .is_some_and(|n| n.date == date || n.fingerprint == fingerprint);
        if seen {
            return None;
        }
        let mut d = Draft::new(
            catalog_identifier(NotificationKind::TriageNudge, None),
            NotificationKind::TriageNudge,
            TRIAGE_TITLE,
            needs_decision(inputs.triage.len()),
        );
        d = d.linked("sunrise://triage");
        d.count = Some(count);
        d.subject = Some(fingerprint);
        d
    };
    let at = place(raw, draft.kind, inputs.sessions, quiet, inputs.tz)?;
    if let Some(fingerprint) = draft.subject.take() {
        ledger.nudge_planned = Some(NudgeRecord {
            date,
            fingerprint,
            fire_at_ms: at.as_millisecond(),
        });
    }
    Some(draft.at(at, raw))
}

/// What the OS must change for the scheduled catalog kinds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogChanges {
    /// Not held yet: schedule each.
    pub add: Vec<CatalogNotification>,
    /// Held with another revision: schedule each again, replacing it.
    pub replace: Vec<CatalogNotification>,
    /// Held under [`CATALOG_PREFIX`] and no longer wanted: withdraw each.
    pub cancel: Vec<String>,
    /// Held and still right.
    pub kept: Vec<String>,
}

impl CatalogChanges {
    /// Whether there is nothing to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.replace.is_empty() && self.cancel.is_empty()
    }
}

/// `desired` against `pending`, over [`CATALOG_PREFIX`] only.
#[must_use]
pub fn reconcile_catalog(
    desired: &[CatalogNotification],
    pending: &[PendingNotification],
) -> CatalogChanges {
    let held: BTreeMap<&str, Option<&str>> = pending
        .iter()
        .filter(|p| p.identifier.starts_with(CATALOG_PREFIX))
        .map(|p| (p.identifier.as_str(), p.revision.as_deref()))
        .collect();
    let mut out = CatalogChanges::default();
    let mut wanted = BTreeSet::new();
    for d in desired {
        wanted.insert(d.identifier.as_str());
        match held.get(d.identifier.as_str()) {
            None => out.add.push(d.clone()),
            Some(Some(rev)) if *rev == d.revision => out.kept.push(d.identifier.clone()),
            Some(_) => out.replace.push(d.clone()),
        }
    }
    out.cancel = held
        .keys()
        .filter(|id| !wanted.contains(*id))
        .map(|id| (*id).to_owned())
        .collect();
    out.kept.sort();
    out
}

/// One row of Settings → Notifications: a kind, what it does, and whether
/// it is on here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The kind.
    pub kind: NotificationKind,
    /// Whether it is on, resolved for this device.
    pub enabled: bool,
}

/// Every kind in catalog order, with whether it is on.
#[must_use]
pub fn catalog_entries(catalog: &CatalogPolicy) -> Vec<CatalogEntry> {
    NotificationKind::ALL
        .into_iter()
        .map(|kind| CatalogEntry {
            kind,
            enabled: catalog.is_on(kind),
        })
        .collect()
}

#[cfg(test)]
mod tests;

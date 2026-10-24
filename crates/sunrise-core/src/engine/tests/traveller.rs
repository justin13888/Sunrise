//! The traveller harness: the acceptance test of ADR-0066
//! (`docs/11-adr/0066-times-survive-travel.md` §Consequences,
//! `docs/10-cross-cutting/time.md` §8 travel scenarios).
//!
//! One person, two devices. The traveller's phone (A) follows a scripted
//! itinerary across New York's spring-forward night, Kathmandu (+05:45),
//! Chatham (+13:45), an offline stretch, and Lord Howe across its half-hour
//! fall-back; the desk machine at home (B) stays in New York, keeps
//! materializing and completing its own routine while A is offline, and the
//! two sync whenever A is online. The clock and the zone are injected; no
//! test reads the host's.
//!
//! Every day the traveller's floating 09:00 habit must be planned at 09:00
//! wherever they are, its reminder must fire at 09:00 there, the pinned
//! 07:30 New York standup must stay at 07:30 New York, a DST-gap occurrence
//! must keep its intended key for the task id, the skip and the streak, no
//! occurrence may be generated twice or reset by a peer's late
//! materialization, and both replicas must end on the same rows.

use super::*;
use jiff::civil::{date, Date, DateTime};
use jiff::tz::TimeZone;
use sunrise_domain::{ReminderSettings, SunriseTime};

/// A clock whose instant and zone the script moves.
#[derive(Debug)]
struct TravelClock {
    now: PLMutex<u64>,
    zone: PLMutex<&'static str>,
}

impl Clock for TravelClock {
    fn now_ms(&self) -> u64 {
        *self.now.lock()
    }
    fn timezone(&self) -> String {
        (*self.zone.lock()).to_string()
    }
}

impl TravelClock {
    fn new(zone: &'static str, local: DateTime) -> Arc<Self> {
        let c = Arc::new(Self {
            now: PLMutex::new(0),
            zone: PLMutex::new(zone),
        });
        c.at(zone, local);
        c
    }

    /// Be at wall clock `local` in `zone`.
    fn at(&self, zone: &'static str, local: DateTime) -> u64 {
        let tz = TimeZone::get(zone).unwrap();
        let ms = u64::try_from(tz.to_zoned(local).unwrap().timestamp().as_millisecond()).unwrap();
        *self.zone.lock() = zone;
        *self.now.lock() = ms;
        ms
    }
}

fn device(seed: u8, clock: Arc<TravelClock>) -> Engine {
    let kc = Arc::new(Keychain::for_test_seeded(
        VaultRootKey::from_bytes(ROOT),
        [seed; 32],
    ));
    Engine::from_clock(clock, Arc::new(SystemRng), kc)
}

/// Deliver every op `from` wrote to `to`, in log order, as a relay would.
/// Delivery is idempotent, so the whole of it is offered every time.
fn sync(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    let me = from.keychain.device_id();
    let mut stmt = from_db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY rowid")
        .unwrap();
    let envs: Vec<Vec<u8>> = stmt
        .query_map(params![&me[..]], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for env in envs {
        to.apply_remote(to_db, &env).unwrap();
    }
}

/// Sync the two devices both ways. Each keeps its own zone; their clocks
/// meet at the later of the two instants first, as two devices' clocks do
/// in the world, because a receiver refuses an op stamped minutes in its
/// future.
fn sync_both(
    (ea, ca): (&Engine, &TravelClock),
    dba: &mut Db,
    (eb, cb): (&Engine, &TravelClock),
    dbb: &mut Db,
) {
    let now = ca.now_ms().max(cb.now_ms());
    *ca.now.lock() = now;
    *cb.now.lock() = now;
    sync(ea, dba, eb, dbb);
    sync(eb, dbb, ea, dba);
}

fn routine(e: &Engine, db: &mut Db, title: &str, anchor: SunriseTime, tz: &str) -> EntityRef {
    let draft = RoutineDraft {
        template: TaskTemplate {
            title: title.into(),
            stream_id: sunrise_domain::inbox_stream_ref(),
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        },
        rrule: sunrise_domain::RRule::parse("FREQ=DAILY").unwrap(),
        timezone: tz.into(),
        starts_at: anchor,
        ends_at: None,
        scheduling_constraints: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
    };
    e.apply(db, Command::CreateRoutine(draft)).unwrap().entity
}

fn key(d: Date, h: i8, m: i8) -> String {
    format!("{d}T{h:02}:{m:02}")
}

fn materialize(e: &Engine, db: &mut Db) {
    let now_ms = e.clock.now_ms();
    e.apply(db, Command::MaterializeRoutines { now_ms })
        .unwrap();
}

/// The live tasks routine `rid` generated, by id.
fn occurrences(db: &Db, rid: EntityRef) -> BTreeSet<[u8; 16]> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id FROM tasks WHERE routine_id = ? AND deleted = 0")
        .unwrap();
    stmt.query_map(params![&rid.bytes()[..]], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|r| crate::engine::ids::blob16(&r.unwrap()))
        .collect()
}

/// One leg of the itinerary: a day, the zone the traveller wakes up in, and
/// whether the phone is online that day.
struct Leg {
    day: Date,
    zone: &'static str,
    online: bool,
}

fn itinerary() -> Vec<Leg> {
    let mut legs = Vec::new();
    let mut d = date(2026, 3, 6);
    while d <= date(2026, 4, 7) {
        let zone = match d {
            _ if d < date(2026, 3, 10) => "America/New_York",
            _ if d < date(2026, 3, 13) => "Asia/Kathmandu",
            _ if d < date(2026, 3, 18) => "Pacific/Chatham",
            _ => "Australia/Lord_Howe",
        };
        // Offline across the Chatham leg and the flight onward.
        let online = !(date(2026, 3, 14) <= d && d <= date(2026, 3, 19));
        legs.push(Leg {
            day: d,
            zone,
            online,
        });
        d = d.tomorrow().unwrap();
    }
    legs
}

#[test]
fn a_traveller_and_a_desk_at_home_agree_on_every_routine_occurrence() {
    let start = date(2026, 3, 6);
    let ca = TravelClock::new("America/New_York", start.at(8, 0, 0, 0));
    let cb = TravelClock::new("America/New_York", start.at(8, 0, 0, 0));
    let ea = device(1, ca.clone());
    let eb = device(2, cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    // Both devices say what they support, so the phone may write a routine
    // that follows the traveller (`routine.civil_times`).
    assert!(ea.advertise_features(&mut dba).unwrap());
    assert!(eb.advertise_features(&mut dbb).unwrap());
    sync_both((&ea, &ca), &mut dba, (&eb, &cb), &mut dbb);

    // The habit follows the traveller; the standup is pinned to New York;
    // the night dose is pinned to 02:30 New York, in the spring-forward gap
    // on 03-08.
    let stretch = routine(
        &ea,
        &mut dba,
        "Stretch",
        SunriseTime::floating(start.at(9, 0, 0, 0)),
        "America/New_York",
    );
    let standup = routine(
        &eb,
        &mut dbb,
        "Standup",
        SunriseTime::zoned(start.at(7, 30, 0, 0), "America/New_York"),
        "America/New_York",
    );
    let night = routine(
        &ea,
        &mut dba,
        "Night dose",
        SunriseTime::zoned(start.at(2, 30, 0, 0), "America/New_York"),
        "America/New_York",
    );
    assert!(
        ea.vault_requires(dba.conn(), "routine.civil_times")
            .unwrap(),
        "a floating anchor requires the feature"
    );
    sync_both((&ea, &ca), &mut dba, (&eb, &cb), &mut dbb);
    let ny = TimeZone::get("America/New_York").unwrap();
    let primary = ReminderSettings {
        is_primary_device: true,
        ..ReminderSettings::default()
    };

    let mut stretch_done = 0i64;
    let mut standup_done = 0i64;
    for leg in itinerary() {
        let tz = TimeZone::get(leg.zone).unwrap();

        // A wakes up at 08:00 local, materializes, and reads its plan.
        let now = ca.at(leg.zone, leg.day.at(8, 0, 0, 0));
        materialize(&ea, &mut dba);
        let tid = occurrence_task_id(&stretch, &key(leg.day, 9, 0));
        let t = read_task_t(&ea, &dba, tid);
        assert_eq!(
            t.scheduled_at,
            Some(SunriseTime::floating(leg.day.at(9, 0, 0, 0))),
            "{}: planned floating at its key",
            leg.day
        );
        let at = t.scheduled_at.as_ref().unwrap().resolve_in(&tz).unwrap();
        assert_eq!(
            at.to_zoned(tz.clone()).datetime(),
            leg.day.at(9, 0, 0, 0),
            "{}",
            leg.day
        );
        // Its reminder fires at 09:00 where the traveller is.
        let intents = reminders(
            &ea,
            &dba,
            now as i64,
            now as i64 + 4 * 3_600_000,
            primary.clone(),
        );
        let intent = intents
            .iter()
            .find(|i| i.entity == tid)
            .unwrap_or_else(|| panic!("{}: no reminder for the habit", leg.day));
        assert_eq!(intent.fire_at, at, "{}", leg.day);

        // The pinned standup stays at 07:30 New York, whatever A's zone.
        let sid = occurrence_task_id(&standup, &key(leg.day, 7, 30));
        if let Some(s) = read_task(dba.conn(), sid.bytes()).unwrap() {
            let at = s.scheduled_at.as_ref().unwrap().resolve_in(&tz).unwrap();
            assert_eq!(at.to_zoned(ny.clone()).datetime(), leg.day.at(7, 30, 0, 0));
        }

        // A completes the habit at 09:05 local.
        ca.at(leg.zone, leg.day.at(9, 5, 0, 0));
        ea.apply(&mut dba, Command::CompleteTask(tid)).unwrap();
        stretch_done += 1;

        // B, at home, completes the standup at 07:35 New York, online or
        // not: it never stops working.
        cb.at("America/New_York", leg.day.at(7, 35, 0, 0));
        materialize(&eb, &mut dbb);
        // The first day's standup was already past when it was set up, and
        // the catch-up policy skips it.
        if leg.day > start {
            eb.apply(&mut dbb, Command::CompleteTask(sid)).unwrap();
            standup_done += 1;
        }

        if leg.online {
            sync_both((&ea, &ca), &mut dba, (&eb, &cb), &mut dbb);
        }

        // The night dose on the spring-forward night fires at 03:30 EDT and
        // keeps its key `…T02:30` for the task id and the streak (#331).
        if leg.day == date(2026, 3, 8) {
            let nid = occurrence_task_id(&night, &key(leg.day, 2, 30));
            let n = read_task_t(&ea, &dba, nid);
            let fired = n.scheduled_at.as_ref().unwrap().resolve_in(&tz).unwrap();
            assert_eq!(
                fired.to_zoned(ny.clone()).datetime(),
                leg.day.at(3, 30, 0, 0)
            );
            ea.apply(&mut dba, Command::CompleteTask(nid)).unwrap();
            let r = routine_of(&ea, &dba, night);
            assert!(
                r.streak_keys.contains(&key(leg.day, 2, 30)),
                "the streak counts the intended key: {:?}",
                r.streak_keys
            );
        }
    }
    sync_both((&ea, &ca), &mut dba, (&eb, &cb), &mut dbb);
    // One more round: an op folded on the second leg of the first round
    // (A's tasks that B generated while A was away) reaches A.
    sync_both((&ea, &ca), &mut dba, (&eb, &cb), &mut dbb);

    // Streak continuity: a completion every day, each inside its grace
    // window wherever it was due.
    for (e, db) in [(&ea, &dba), (&eb, &dbb)] {
        let s = routine_of(e, db, stretch);
        assert_eq!(
            s.streak_counter, stretch_done,
            "the habit's streak never reset"
        );
        let u = routine_of(e, db, standup);
        assert_eq!(
            u.streak_counter, standup_done,
            "the standup's streak never reset"
        );
    }

    // No occurrence generated twice, none lost, none reset: one live task per
    // day of the itinerary, each completed one still done on both devices,
    // even where the desk generated it while the phone was offline.
    for leg in itinerary() {
        let tid = occurrence_task_id(&stretch, &key(leg.day, 9, 0));
        for (e, db) in [(&ea, &dba), (&eb, &dbb)] {
            let t = read_task_t(e, db, tid);
            assert_eq!(t.state, TaskState::Done, "{}: not reset by a peer", leg.day);
        }
    }
    let a_occ = occurrences(&dba, stretch);
    assert_eq!(a_occ, occurrences(&dbb, stretch), "the same occurrences");
    // Every one is an occurrence of a distinct key: the ids are a function of
    // the key, so a duplicate would be a second id for one civil day.
    let days: BTreeSet<Date> = a_occ
        .iter()
        .map(|id| {
            let t = read_task(dba.conn(), id).unwrap().unwrap();
            match t.scheduled_at {
                Some(SunriseTime::Floating { civil }) => civil.date(),
                other => panic!("an occurrence planned {other:?}"),
            }
        })
        .collect();
    assert_eq!(days.len(), a_occ.len(), "one occurrence per civil day");

    // Convergence: the same task rows and the same routines on both.
    assert_eq!(tasks_projection(&dba), tasks_projection(&dbb));
    for rid in [stretch, standup, night] {
        let ra = routine_of(&ea, &dba, rid);
        let rb = routine_of(&eb, &dbb, rid);
        assert_eq!(ra.starts_at, rb.starts_at);
        assert_eq!(ra.streak_keys, rb.streak_keys);
        assert_eq!(ra.streak_counter, rb.streak_counter);
    }
}

/// Re-pinning a zoned routine from Los Angeles to New York keeps it at 09:00
/// on the wall clock, re-pins the occurrences already planned, and changes no
/// key, so nothing is duplicated or lost (time.md §8, ADR-0066 §1).
#[test]
fn rezoning_a_routine_keeps_its_wall_clock_and_its_occurrences() {
    let start = date(2026, 3, 2);
    let c = TravelClock::new("America/Los_Angeles", start.at(8, 0, 0, 0));
    let e = device(3, c.clone());
    let mut db = db_root(ROOT);
    let rid = routine(
        &e,
        &mut db,
        "Water",
        SunriseTime::zoned(start.at(9, 0, 0, 0), "America/Los_Angeles"),
        "America/Los_Angeles",
    );
    let before = occurrences(&db, rid);
    e.apply(
        &mut db,
        Command::UpdateRoutine {
            id: rid,
            patch: RoutinePatch {
                timezone: Some("America/New_York".into()),
                ..RoutinePatch::default()
            },
        },
    )
    .unwrap();
    let r = routine_of(&e, &db, rid);
    assert_eq!(
        r.starts_at,
        SunriseTime::zoned(start.at(9, 0, 0, 0), "America/New_York")
    );
    // No key changed, so every occurrence is still there, once; an earlier
    // instant can only bring one more inside the horizon.
    let after = occurrences(&db, rid);
    assert!(before.is_subset(&after), "no occurrence lost");
    assert!(after.len() <= before.len() + 1, "none duplicated");
    let ny = TimeZone::get("America/New_York").unwrap();
    for id in &before {
        let t = read_task(db.conn(), id).unwrap().unwrap();
        let at = t.scheduled_at.as_ref().unwrap();
        if at
            .resolve_in(&ny)
            .is_some_and(|at| at.as_millisecond() > c.now_ms() as i64)
        {
            assert!(
                matches!(at, SunriseTime::Zoned { tz, civil } if tz == "America/New_York" && civil.time() == jiff::civil::time(9, 0, 0, 0)),
                "{at:?}"
            );
        }
    }
}

/// A zone change re-checks the hard constraints of planned tasks (#333): a
/// floating 21:00 plan inside a hard `20:00–23:00` window is fine at home and
/// still fine abroad, while an instant plan for 21:00 New York falls outside
/// it once read in Kathmandu.
#[test]
fn a_zone_change_flags_a_plan_that_breaks_a_hard_window() {
    let c = TravelClock::new("America/New_York", date(2026, 3, 2).at(8, 0, 0, 0));
    let e = device(4, c.clone());
    let mut db = db_root(ROOT);
    let window = sunrise_domain::ScheduleConstraint {
        time_of_day: Some(sunrise_domain::TimeOfDayRange::new(
            jiff::civil::time(20, 0, 0, 0),
            jiff::civil::time(23, 0, 0, 0),
        )),
        days_of_week: sunrise_domain::WeekdaySet::new(),
        date_range: None,
        severity: sunrise_domain::ConstraintSeverity::Hard,
        unknown: Unknowns::new(),
    };
    let evening = date(2026, 3, 3).at(21, 0, 0, 0);
    let ny = TimeZone::get("America/New_York").unwrap();
    let mut ids = Vec::new();
    for scheduled in [
        SunriseTime::floating(evening),
        SunriseTime::instant(ny.to_zoned(evening).unwrap().timestamp()),
    ] {
        let id = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "call home".into(),
                    scheduled_at: Some(scheduled),
                    scheduling_constraints: vec![window.clone()],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        ids.push(id);
    }
    assert!(e.hard_constraint_violations(&db, &ny).unwrap().is_empty());
    let ktm = TimeZone::get("Asia/Kathmandu").unwrap();
    assert_eq!(
        e.hard_constraint_violations(&db, &ktm).unwrap(),
        vec![ids[1]]
    );
}

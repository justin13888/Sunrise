//! `CoreConfig` — the dependency-injection bundle.
//!
//! Per `docs/01-architecture/shared-core.md` §determinism. Every clock /
//! RNG / storage handle the core needs is injected via this struct so
//! tests can substitute deterministic fakes.

use std::path::PathBuf;
use std::sync::Arc;
use sunrise_cbor::hlc::{Hlc, HlcError};

/// Pluggable wall-clock source. Tests inject a fake; production wraps
/// `std::time::SystemTime::now()` behind a narrowly-scoped `#[allow]` on
/// [`SystemClock::now_ms`] — the single relaxation of the determinism gate in
/// this crate's clock path.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current unix time in milliseconds.
    fn now_ms(&self) -> u64;

    /// IANA name of the *device-local* timezone, used to pin the civil
    /// (zone-less) window dimensions of a Task's scheduling constraints to real
    /// instants — per `docs/02-domain/scheduling-constraints.md` §Evaluation
    /// timezone, a Task evaluates in the device zone (a Routine evaluates in
    /// its own).
    ///
    /// Defaulted to `"UTC"` so existing implementors keep compiling and so a
    /// test that injects only a clock still gets a deterministic zone. Ambient
    /// timezone state is as much a determinism hazard as an ambient clock, so
    /// it enters the core through this one injected seam and nowhere else.
    fn timezone(&self) -> String {
        "UTC".to_string()
    }
}

/// Pluggable hybrid-logical-clock source.
///
/// Separate from [`Clock`] because it is *stateful*: it is the only thing in
/// the core that carries mutable state between calls, and the `(physical,
/// logical)` pair it returns is not a function of the wall clock alone. It is
/// injected for the same reason [`Clock`] is — `clippy.toml`'s
/// disallowed-methods list and the CI determinism gate forbid reading an
/// ambient clock inside crate source, and a test that wants to model a skewed
/// replica needs to supply the skew rather than discover it.
pub trait HlcClock: Send + Sync + std::fmt::Debug {
    /// Stamp an op this device is emitting. Strictly increasing per call.
    fn send(&self) -> Hlc;

    /// Absorb an `Hlc` received from a peer, advancing local state so that
    /// everything this device emits afterwards sorts after `received`.
    ///
    /// # Errors
    /// [`HlcError::DriftTooLarge`] when `received` is too far in the future to
    /// be a legitimate reading; the caller rejects the op rather than adopting
    /// the bad clock.
    fn observe(&self, received: Hlc) -> Result<(), HlcError>;

    /// Restore durable state at open: advance to `observed` if it is ahead,
    /// without stamping anything and without the drift gate.
    ///
    /// [`Self::observe`] is not a substitute. It refuses a reading more than
    /// `MAX_DRIFT_MS` beyond the wall clock, which is right for a peer's claim
    /// and wrong for this replica's own history — a device whose clock has been
    /// set back would have its own past refused and start emitting beneath it,
    /// which is the failure this call exists to prevent. It also adds one to
    /// the logical counter, which would make reopening a vault twice with no
    /// writes in between drift the clock upwards for nothing.
    ///
    /// `observed` is the greatest stamp this replica has durably recorded, and
    /// [`Engine::prime_hlc`](crate::engine::Engine::prime_hlc) is what reads it.
    fn prime(&self, observed: Hlc);

    /// The current local reading, without advancing it. Diagnostics only.
    ///
    /// Not a basis for comparing a value another device stamped. It reads 0
    /// on a [`MonotonicHlc`] that has not been primed, and after priming it is
    /// a *local* reading of a distributed order: it says nothing about what
    /// peers this replica has not heard from have stamped. Revocation compared
    /// against it for one revision and `Engine::is_revoked` explains why it no
    /// longer does.
    fn peek(&self) -> Hlc;
}

/// The production [`HlcClock`]: HLC state over an injected [`Clock`].
///
/// State is not written as it advances — there is no HLC row, and every op
/// this device emits is not also a clock checkpoint. It is instead *restored*
/// at open, by [`Engine::prime_hlc`](crate::engine::Engine::prime_hlc), from
/// the greatest stamp in the op log. The distinction matters because the two
/// have the same effect and very different costs.
///
/// # Why restoring it is not optional
///
/// The comment this replaces said the reset was safe because "the physical
/// component dominates the ordering and only moves forward". That reasoning
/// covered the logical counter alone. `Hlc::default()` zeroes the physical half
/// too, and a device does not generally hold a physical half equal to its own
/// wall clock: `Hlc::receive` takes `max3(local, received, now)` and admits a
/// peer up to [`MAX_DRIFT_MS`](sunrise_cbor::hlc::MAX_DRIFT_MS) — five minutes
/// — ahead, and `Hlc::send` carries that forward. So a device that has absorbed
/// one op from a peer whose clock leads sits above its own wall clock until the
/// clock catches up.
///
/// From a zero start, the first `send` after a restart is roughly the wall
/// clock, which is *below* the stamps that device emitted minutes earlier. That
/// is not the tie the old comment anticipated — a tie is what `seq` breaks. It
/// is an inversion, and `lww_wins` compares `hlc` before it ever looks at
/// `seq`, so the device's newer op loses to its own older one on every replica
/// that merges both. The origin has no LWW gate on its own writes, so it keeps
/// the new value while everyone else keeps the old: silent, permanent
/// divergence, inside a five-minute window after every restart. `engine.rs`'s
/// `a_restart_does_not_make_this_device_emit_beneath_its_own_ops` is that
/// scenario.
///
/// # What is left after priming
///
/// Exactly the case the old comment described: two ops from this device sharing
/// a `(physical_ms, logical)` pair across a restart, when the log's greatest
/// stamp is this device's own last one and the wall clock has not moved since.
/// Those are a tie, not an inversion, and the `seq` term in the LWW tuple
/// breaks them.
///
/// The decision and the options rejected with it are ADR-0036
/// (`docs/11-adr/0036-hlc-restored-at-open.md`), which amends ADR-0016.
#[derive(Debug)]
pub struct MonotonicHlc {
    clock: Arc<dyn Clock>,
    state: parking_lot::Mutex<Hlc>,
}

impl MonotonicHlc {
    /// Build over `clock`, starting from a zero reading.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: parking_lot::Mutex::new(Hlc::default()),
        }
    }
}

impl HlcClock for MonotonicHlc {
    fn send(&self) -> Hlc {
        let now = self.clock.now_ms();
        let mut st = self.state.lock();
        *st = st.send(now);
        *st
    }

    fn observe(&self, received: Hlc) -> Result<(), HlcError> {
        let now = self.clock.now_ms();
        let mut st = self.state.lock();
        *st = st.receive(received, now)?;
        Ok(())
    }

    fn prime(&self, observed: Hlc) {
        let mut st = self.state.lock();
        if observed > *st {
            *st = observed;
        }
    }

    fn peek(&self) -> Hlc {
        *self.state.lock()
    }
}

/// Pluggable randomness source.
pub trait Rng: Send + Sync + std::fmt::Debug {
    /// Fill `dest` with cryptographically random bytes.
    fn fill_bytes(&self, dest: &mut [u8]);
}

/// Production clock backed by `std::time::SystemTime`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // The determinism gate forbids SystemTime::now(). This is the one
        // controlled boundary where the wall clock enters the core: every
        // domain op reads time through `Clock::now_ms`, and this impl is what
        // that resolves to in production. Tests inject a FakeClock and never
        // reach here, so no test outcome depends on the host clock.
        #[allow(clippy::disallowed_methods)]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
    }

    fn timezone(&self) -> String {
        // Only the production clock consults the OS zone; tests inject a fake
        // and get a fixed zone, so no test outcome depends on the host's.
        system_iana_name().unwrap_or_else(|| "UTC".to_string())
    }
}

/// Where every Unix keeps the pointer to the device's zone.
const LOCALTIME: &str = "/etc/localtime";

/// The device's IANA zone name, or `None` when the OS will not name one.
///
/// # Why this is not just `TimeZone::system()`
///
/// It is asked first, and on Linux and Windows it answers. On **macOS it does
/// not**: `/etc/localtime` there points into a versioned tree —
/// `/var/db/timezone/zoneinfo/America/Toronto`, itself a link to
/// `/var/db/timezone/tz/<tzdb-version>/zoneinfo/…` — which is not one of the
/// TZ directories jiff strips a name from. It returns a nameless zone whose
/// offset is **zero**, so `iana_name()` is `None` and the old fallback made
/// every Mac claim to be in UTC.
///
/// That is not cosmetic. The device zone decides which civil day
/// `Query::DayBlocks` covers and which instants a Task's scheduling
/// constraints evaluate against, while the clients render civil time in the
/// zone the OS gives *them*. West of UTC the two disagree for the last hours
/// of every evening, and the calendar grid quietly returns nothing for blocks
/// that are plainly on it.
fn system_iana_name() -> Option<String> {
    if let Some(name) = jiff::tz::TimeZone::system().iana_name() {
        return Some(name.to_string());
    }
    // `read_link` first: it is the shortest spelling, and the one that still
    // names the zone on a macOS whose canonical path buries it under a tzdb
    // version directory. `canonicalize` is the fallback for a chain of links.
    std::fs::read_link(LOCALTIME)
        .ok()
        .and_then(|p| zone_name_in(&p))
        .or_else(|| {
            std::fs::canonicalize(LOCALTIME)
                .ok()
                .and_then(|p| zone_name_in(&p))
        })
}

/// Pull `America/Toronto` out of `…/zoneinfo/America/Toronto`.
///
/// Anchored on the **last** `zoneinfo` component so a versioned or nested tree
/// resolves the same as a flat one, and the `posix/` and `right/` sub-trees
/// some distributions ship are stepped over. The result is only returned when
/// the tzdb actually knows it, so a path this does not understand degrades to
/// UTC rather than naming a zone nothing can load.
fn zone_name_in(path: &std::path::Path) -> Option<String> {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let anchor = parts.iter().rposition(|p| *p == "zoneinfo")?;
    let mut tail = &parts[anchor + 1..];
    if matches!(tail.first(), Some(&"posix" | &"right")) {
        tail = &tail[1..];
    }
    if tail.is_empty() {
        return None;
    }
    let name = tail.join("/");
    jiff::tz::TimeZone::get(&name).is_ok().then_some(name)
}

/// Production CSPRNG backed by `getrandom`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRng;

impl Rng for SystemRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        // OS CSPRNG; production binds via rand's OsRng.
        use rand_core::RngCore;
        rand::rngs::OsRng.fill_bytes(dest);
    }
}

/// Bundle injected when opening a [`crate::Core`].
#[derive(Clone)]
pub struct CoreConfig {
    /// Vault directory.
    pub vault_dir: PathBuf,
    /// Wall-clock source.
    pub clock: Arc<dyn Clock>,
    /// Causal-ordering source. Every emitted op is stamped from this, and
    /// every received op is observed into it.
    pub hlc: Arc<dyn HlcClock>,
    /// Random source.
    pub rng: Arc<dyn Rng>,
    /// `<semver>+<platform>` for log records and Hello frames.
    pub app: String,
    /// Sync configuration. `None` = offline mode (no driver task spawned;
    /// current single-device behavior).
    pub sync: Option<crate::sync_driver::SyncConfig>,
}

impl std::fmt::Debug for CoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreConfig")
            .field("vault_dir", &self.vault_dir)
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

impl CoreConfig {
    /// Construct with the production clock + RNG.
    #[must_use]
    pub fn production(vault_dir: PathBuf, app: impl Into<String>) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Self {
            vault_dir,
            hlc: Arc::new(MonotonicHlc::new(Arc::clone(&clock))),
            clock,
            rng: Arc::new(SystemRng),
            app: app.into(),
            sync: None,
        }
    }

    /// Construct with an injected clock + RNG, deriving the HLC from that
    /// clock. The shape every test wants: skew the [`Clock`] and the HLC skews
    /// with it, without the caller assembling two objects that must agree.
    #[must_use]
    pub fn with_clock(
        vault_dir: PathBuf,
        app: impl Into<String>,
        clock: Arc<dyn Clock>,
        rng: Arc<dyn Rng>,
    ) -> Self {
        Self {
            vault_dir,
            hlc: Arc::new(MonotonicHlc::new(Arc::clone(&clock))),
            clock,
            rng,
            app: app.into(),
            sync: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic test clock.
    #[derive(Debug)]
    struct FakeClock {
        ms: parking_lot::Mutex<u64>,
    }
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.ms.lock()
        }
    }

    // ---- the device zone ----

    /// **The regression test for the calendar's missing blocks.**
    ///
    /// macOS points `/etc/localtime` into a versioned tree jiff does not strip
    /// a name from, so `TimeZone::system().iana_name()` is `None` there and
    /// this used to answer `"UTC"` on every Mac. The core then folded civil
    /// days in UTC while the client rendered them in the real zone, and west
    /// of Greenwich the two named different days for the last hours of every
    /// evening — a day query that returned nothing for blocks visibly on the
    /// grid.
    #[test]
    fn a_versioned_macos_zoneinfo_path_still_names_its_zone() {
        let macos = std::path::Path::new("/var/db/timezone/zoneinfo/America/Toronto");
        assert_eq!(zone_name_in(macos).as_deref(), Some("America/Toronto"));
        let canonical =
            std::path::Path::new("/private/var/db/timezone/tz/2026c.1.0/zoneinfo/America/Toronto");
        assert_eq!(zone_name_in(canonical).as_deref(), Some("America/Toronto"));
    }

    #[test]
    fn the_ordinary_unix_zoneinfo_paths_name_their_zone() {
        for (path, want) in [
            ("/usr/share/zoneinfo/Europe/London", "Europe/London"),
            ("/usr/share/zoneinfo/posix/Europe/London", "Europe/London"),
            ("/usr/share/zoneinfo/right/Europe/London", "Europe/London"),
            ("/usr/share/zoneinfo/UTC", "UTC"),
            (
                "/usr/share/zoneinfo/America/Argentina/Ushuaia",
                "America/Argentina/Ushuaia",
            ),
        ] {
            assert_eq!(
                zone_name_in(std::path::Path::new(path)).as_deref(),
                Some(want),
                "{path}"
            );
        }
    }

    /// A path the tzdb cannot load must produce no name at all. Returning the
    /// text anyway would hand the engine a zone every lookup then silently
    /// resolves to UTC, which is the bug this guards wearing a name tag.
    #[test]
    fn a_path_that_names_no_real_zone_is_refused() {
        for path in [
            "/etc/localtime",
            "/usr/share/zoneinfo",
            "/usr/share/zoneinfo/Mars/Olympus_Mons",
            "/var/db/timezone/tz/2026c.1.0/America/Toronto",
        ] {
            assert_eq!(zone_name_in(std::path::Path::new(path)), None, "{path}");
        }
    }

    /// End to end on whatever host is running this: the production clock must
    /// name the zone `/etc/localtime` points at. Deliberately compared against
    /// the link rather than a literal — the zone of a CI runner is not ours to
    /// pin — and skipped where there is no link to compare against, which is
    /// every Windows host and a container with a copied `/etc/localtime`.
    #[test]
    #[cfg(unix)]
    fn the_production_clock_names_the_zone_this_device_is_in() {
        let Some(expected) = std::fs::read_link(LOCALTIME)
            .ok()
            .and_then(|p| zone_name_in(&p))
        else {
            return;
        };
        assert_eq!(SystemClock.timezone(), expected);
    }

    #[test]
    fn config_holds_injected_clock() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "0.1.0+test",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(123),
            }),
            Arc::new(SystemRng),
        );
        assert_eq!(cfg.clock.now_ms(), 123);
    }

    #[test]
    fn the_hlc_follows_the_injected_clock() {
        let clock = Arc::new(FakeClock {
            ms: parking_lot::Mutex::new(1_000),
        });
        let cfg = CoreConfig::with_clock("/tmp".into(), "t", clock.clone(), Arc::new(SystemRng));
        assert_eq!(cfg.hlc.send(), Hlc::at(1_000));
        *clock.ms.lock() = 2_000;
        assert_eq!(cfg.hlc.send(), Hlc::at(2_000));
    }

    #[test]
    fn successive_sends_within_one_millisecond_still_order() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "t",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(1_000),
            }),
            Arc::new(SystemRng),
        );
        let a = cfg.hlc.send();
        let b = cfg.hlc.send();
        assert!(b > a, "two ops in one millisecond must still be ordered");
    }

    #[test]
    fn observing_a_faster_peer_pulls_this_device_past_it() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "t",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(1_000),
            }),
            Arc::new(SystemRng),
        );
        let peer = Hlc::at(1_200);
        cfg.hlc.observe(peer).unwrap();
        assert!(cfg.hlc.send() > peer);
    }

    #[test]
    fn a_wildly_skewed_peer_is_refused_and_does_not_move_us() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "t",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(1_000),
            }),
            Arc::new(SystemRng),
        );
        let before = cfg.hlc.peek();
        let liar = Hlc::at(1_000 + sunrise_cbor::hlc::MAX_DRIFT_MS + 1);
        assert!(cfg.hlc.observe(liar).is_err());
        assert_eq!(
            cfg.hlc.peek(),
            before,
            "a refused op must not move our clock"
        );
    }
}

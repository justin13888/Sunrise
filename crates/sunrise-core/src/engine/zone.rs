//! The reader's time zone, which every engine read resolves civil time in.

use super::Engine;

impl Engine {
    /// The reader's zone, from the injected clock
    /// (`docs/10-cross-cutting/time.md` §2).
    ///
    /// A name the tzdb cannot resolve is not silently UTC
    /// (`docs/11-adr/0066-times-survive-travel.md` §3): the engine keeps the
    /// last zone it could resolve, and only an engine that has never resolved
    /// one reads in UTC, which [`Engine::zone_unknown`] then reports.
    pub(crate) fn device_zone(&self) -> jiff::tz::TimeZone {
        self.reader_zone().0
    }

    /// Whether the reader's zone is a stand-in: the clock names no zone the
    /// tzdb resolves, and none has resolved before, so reads are in UTC.
    pub(crate) fn zone_unknown(&self) -> bool {
        self.reader_zone().1
    }

    /// [`Engine::device_zone`] and [`Engine::zone_unknown`], from one read
    /// of the clock.
    fn reader_zone(&self) -> (jiff::tz::TimeZone, bool) {
        let mut last = self.last_zone.lock();
        if let Ok(zone) = jiff::tz::TimeZone::get(&self.clock.timezone()) {
            *last = Some(zone.clone());
            return (zone, false);
        }
        last.clone()
            .map_or((jiff::tz::TimeZone::UTC, true), |zone| (zone, false))
    }
}

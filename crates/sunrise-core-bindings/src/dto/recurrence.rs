//! Recurrence: the rule a routine repeats by.

use sunrise_domain::{Frequency, RRule, SunriseTime, Weekday};

use super::{extra_in, extra_out, time::legacy_instant, TimeValue};

/// See [`sunrise_domain::RRule`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct Recurrence {
    /// Base frequency.
    pub freq: Frequency,
    /// Every `interval` periods.
    pub interval: u32,
    /// `BYDAY`.
    pub by_day: Vec<Weekday>,
    /// `BYMONTHDAY`; negative counts back from the end of the month.
    pub by_month_day: Vec<i32>,
    /// `BYMONTH`.
    pub by_month: Vec<u32>,
    /// `BYSETPOS`.
    pub by_set_pos: Vec<i32>,
    /// `COUNT`.
    pub count: Option<u32>,
    /// `UNTIL`, as an instant: [`Self::until_time`] resolved in UTC. Kept
    /// for clients built before #506; read and write `until_time`.
    pub until: Option<jiff::Timestamp>,
    /// Week start.
    pub wkst: Option<Weekday>,
    /// Fields this build does not know, opaque; hand it back unchanged. See
    /// the module docs.
    #[uniffi(default = None)]
    pub extra: Option<Vec<u8>>,
    /// `UNTIL` with its kind (`floating` or `all_day`, compared with
    /// occurrence keys in civil space). When set it wins over [`Self::until`].
    #[uniffi(default = None)]
    pub until_time: Option<TimeValue>,
}

impl From<&RRule> for Recurrence {
    fn from(r: &RRule) -> Self {
        let RRule {
            freq,
            interval,
            by_day,
            by_month_day,
            by_month,
            by_set_pos,
            count,
            until,
            wkst,
            unknown,
        } = r;
        Self {
            freq: freq.clone(),
            interval: *interval,
            by_day: by_day.clone(),
            by_month_day: by_month_day.clone(),
            by_month: by_month.clone(),
            by_set_pos: by_set_pos.clone(),
            count: *count,
            until: until.as_ref().map(|u| legacy_instant(u, "UTC")),
            wkst: wkst.clone(),
            extra: extra_out(unknown),
            until_time: until.as_ref().map(TimeValue::from),
        }
    }
}

impl From<Recurrence> for RRule {
    fn from(r: Recurrence) -> Self {
        Self {
            freq: r.freq,
            interval: r.interval,
            by_day: r.by_day,
            by_month_day: r.by_month_day,
            by_month: r.by_month,
            by_set_pos: r.by_set_pos,
            count: r.count,
            until: r
                .until_time
                .map(SunriseTime::from)
                .or_else(|| r.until.map(SunriseTime::from)),
            wkst: r.wkst,
            unknown: extra_in(r.extra.as_deref()),
        }
    }
}

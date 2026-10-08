//! Private jiff wrapper. **This is the only file in `quota_window` that
//! touches jiff.** Every other file in this module speaks only
//! `time::OffsetDateTime`; conversion happens at this module's boundary.
//!
//! Always looks zones up through [`jiff::tz::TimeZoneDatabase::bundled`],
//! held in a [`OnceLock`]. Never [`jiff::tz::TimeZone::get`] or the
//! global `jiff::tz::db()` — if any future dependency enables jiff's
//! default features, Cargo merges the features and the global db
//! silently starts reading the host's `/usr/share/zoneinfo`, which would
//! break the guarantee that the same window definition evaluates
//! identically on every machine. The tzdb version is pinned by
//! `Cargo.lock`; bumping jiff can legitimately move a future DST
//! boundary if a country changes its rules — see
//! `docs/adr/0015-quota-windows.md`.

use std::sync::OnceLock;

use jiff::civil::{Date, Weekday as JiffWeekday};
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::{Timestamp, Zoned};
use time::OffsetDateTime;

use super::{AlignedPeriod, IanaTimeZone, ResetWeekday, WallClockTime};

fn db() -> &'static TimeZoneDatabase {
    static DB: OnceLock<TimeZoneDatabase> = OnceLock::new();
    DB.get_or_init(TimeZoneDatabase::bundled)
}

/// Whether `name` is a recognized zone in the bundled database. Used by
/// [`IanaTimeZone::new`] — there is never a fallback to UTC for an
/// unrecognized name.
pub(super) fn is_known_zone(name: &str) -> bool {
    db().get(name).is_ok()
}

fn zone(tz: &IanaTimeZone) -> TimeZone {
    // Safe to expect: IanaTimeZone's own constructor already validated
    // this exact name against this exact bundled database.
    db().get(tz.as_str())
        .expect("IanaTimeZone was validated against the bundled db at construction")
}

fn to_jiff_timestamp(value: OffsetDateTime) -> Timestamp {
    // `value` is already normalized to UTC + whole milliseconds by every
    // caller in this module (`normalize_utc_ms`).
    let nanos = (value.unix_timestamp() as i128) * 1_000_000_000 + value.nanosecond() as i128;
    Timestamp::from_nanosecond(nanos)
        .expect("a normalized UTC timestamp is within jiff's representable range")
}

fn from_jiff_timestamp(ts: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(ts.as_nanosecond())
        .expect("a jiff Timestamp is within time::OffsetDateTime's representable range")
}

fn jiff_weekday(weekday: ResetWeekday) -> JiffWeekday {
    match weekday {
        ResetWeekday::Monday => JiffWeekday::Monday,
        ResetWeekday::Tuesday => JiffWeekday::Tuesday,
        ResetWeekday::Wednesday => JiffWeekday::Wednesday,
        ResetWeekday::Thursday => JiffWeekday::Thursday,
        ResetWeekday::Friday => JiffWeekday::Friday,
        ResetWeekday::Saturday => JiffWeekday::Saturday,
        ResetWeekday::Sunday => JiffWeekday::Sunday,
    }
}

/// Resolves `date` at wall-clock time `at` to a concrete instant in
/// `tz`, using jiff's `Compatible` disambiguation (the default for
/// `TimeZone::to_zoned`): a gap moves to the later valid instant, a fold
/// takes the earlier occurrence.
fn candidate_instant(date: Date, at: WallClockTime, tz: &TimeZone) -> Zoned {
    let dt = date.at(at.hour() as i8, at.minute() as i8, 0, 0);
    tz.to_zoned(dt)
        .expect("a validated wall-clock time and any valid Date never fails construction")
}

/// The `[start, end)` instants for the `FixedAligned` period containing
/// `now`, computed entirely by stepping calendar dates/times in `tz` —
/// never by adding a fixed number of seconds.
pub(super) fn fixed_aligned_bounds(
    now: OffsetDateTime,
    period: &AlignedPeriod,
    tz: &IanaTimeZone,
) -> (OffsetDateTime, OffsetDateTime) {
    let z = zone(tz);
    let now_ts = to_jiff_timestamp(now);
    let now_zoned = now_ts.to_zoned(z.clone());

    match period {
        AlignedPeriod::Hour => {
            let start_zoned = now_zoned
                .with()
                .minute(0)
                .second(0)
                .subsec_nanosecond(0)
                .build()
                .expect("truncating to the top of the current hour cannot fail");
            let end_second = start_zoned.timestamp().as_second() + 3600;
            let end_ts = Timestamp::from_second(end_second).expect("a 1-hour step stays in range");
            let start = from_jiff_timestamp(start_zoned.timestamp());
            let end = from_jiff_timestamp(end_ts);
            debug_assert!(end > start, "hourly boundary must be strictly increasing");
            (start, end)
        }
        AlignedPeriod::Day { at } => {
            let mut date = now_zoned.date();
            let mut start_zoned = candidate_instant(date, *at, &z);
            if start_zoned.timestamp() > now_ts {
                date = date
                    .yesterday()
                    .expect("stepping back one calendar day cannot fail for a real Date");
                start_zoned = candidate_instant(date, *at, &z);
            }
            let end_date = date
                .tomorrow()
                .expect("stepping forward one calendar day cannot fail for a real Date");
            let end_zoned = candidate_instant(end_date, *at, &z);
            let start = from_jiff_timestamp(start_zoned.timestamp());
            let end = from_jiff_timestamp(end_zoned.timestamp());
            debug_assert!(end > start, "daily boundary must be strictly increasing");
            (start, end)
        }
        AlignedPeriod::Week { starts_on, at } => {
            let target = jiff_weekday(*starts_on);
            let mut date = now_zoned.date();
            while date.weekday() != target {
                date = date
                    .yesterday()
                    .expect("stepping back one calendar day cannot fail for a real Date");
            }
            let mut start_zoned = candidate_instant(date, *at, &z);
            if start_zoned.timestamp() > now_ts {
                for _ in 0..7 {
                    date = date
                        .yesterday()
                        .expect("stepping back one calendar day cannot fail for a real Date");
                }
                start_zoned = candidate_instant(date, *at, &z);
            }
            let mut end_date = date;
            for _ in 0..7 {
                end_date = end_date
                    .tomorrow()
                    .expect("stepping forward one calendar day cannot fail for a real Date");
            }
            let end_zoned = candidate_instant(end_date, *at, &z);
            let start = from_jiff_timestamp(start_zoned.timestamp());
            let end = from_jiff_timestamp(end_zoned.timestamp());
            debug_assert!(end > start, "weekly boundary must be strictly increasing");
            (start, end)
        }
    }
}

/// A daily working-hours window in one timezone, applied every day
/// (no per-weekday exclusion — see `pacing::PacingPreference::Sustain`'s
/// own docs for why that is an accepted MVP scope limit, not an
/// oversight). `start` must be strictly before `end` on the same
/// calendar day — an overnight-spanning window (e.g. 22:00-06:00) is not
/// supported; validated at construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingHours {
    time_zone: IanaTimeZone,
    start: WallClockTime,
    end: WallClockTime,
}

use serde::{Deserialize, Serialize};

/// Error constructing a [`WorkingHours`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkingHoursError {
    #[error("working-hours start ({start_hour:02}:{start_minute:02}) must be strictly before end ({end_hour:02}:{end_minute:02})")]
    StartNotBeforeEnd {
        start_hour: u8,
        start_minute: u8,
        end_hour: u8,
        end_minute: u8,
    },
}

impl WorkingHours {
    pub fn new(
        time_zone: IanaTimeZone,
        start: WallClockTime,
        end: WallClockTime,
    ) -> Result<Self, WorkingHoursError> {
        if (start.hour(), start.minute()) >= (end.hour(), end.minute()) {
            return Err(WorkingHoursError::StartNotBeforeEnd {
                start_hour: start.hour(),
                start_minute: start.minute(),
                end_hour: end.hour(),
                end_minute: end.minute(),
            });
        }
        Ok(Self {
            time_zone,
            start,
            end,
        })
    }
}

/// The earliest instant `>= now` that falls inside `hours`' daily
/// window — `now` itself when it is already inside, otherwise that same
/// (or next) calendar day's `start`, stepping entirely by calendar
/// date/time in `hours`' own timezone (never by adding a fixed number of
/// seconds — the same DST discipline [`fixed_aligned_bounds`] follows).
pub(crate) fn next_working_instant(now: OffsetDateTime, hours: &WorkingHours) -> OffsetDateTime {
    let z = zone(&hours.time_zone);
    let now_ts = to_jiff_timestamp(now);
    let now_zoned = now_ts.to_zoned(z.clone());
    let mut date = now_zoned.date();

    let today_start = candidate_instant(date, hours.start, &z);
    let today_end = candidate_instant(date, hours.end, &z);
    if now_ts >= today_start.timestamp() && now_ts < today_end.timestamp() {
        return now;
    }
    if now_ts < today_start.timestamp() {
        return from_jiff_timestamp(today_start.timestamp());
    }
    date = date
        .tomorrow()
        .expect("stepping forward one calendar day cannot fail for a real Date");
    let next_start = candidate_instant(date, hours.start, &z);
    from_jiff_timestamp(next_start.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn utc_9_to_17() -> WorkingHours {
        WorkingHours::new(
            IanaTimeZone::new("UTC").unwrap(),
            WallClockTime::new(9, 0).unwrap(),
            WallClockTime::new(17, 0).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn before_start_pushes_to_that_days_start() {
        let now = datetime!(2024-01-01 06:00:00 UTC);
        let result = next_working_instant(now, &utc_9_to_17());
        assert_eq!(result, datetime!(2024-01-01 09:00:00 UTC));
    }

    #[test]
    fn inside_the_window_returns_now_unchanged() {
        let now = datetime!(2024-01-01 12:30:00 UTC);
        let result = next_working_instant(now, &utc_9_to_17());
        assert_eq!(result, now);
    }

    #[test]
    fn at_the_boundary_start_counts_as_inside() {
        let now = datetime!(2024-01-01 09:00:00 UTC);
        let result = next_working_instant(now, &utc_9_to_17());
        assert_eq!(result, now);
    }

    #[test]
    fn at_the_boundary_end_counts_as_outside() {
        let now = datetime!(2024-01-01 17:00:00 UTC);
        let result = next_working_instant(now, &utc_9_to_17());
        assert_eq!(result, datetime!(2024-01-02 09:00:00 UTC));
    }

    #[test]
    fn after_end_pushes_to_the_next_days_start() {
        let now = datetime!(2024-01-01 20:00:00 UTC);
        let result = next_working_instant(now, &utc_9_to_17());
        assert_eq!(result, datetime!(2024-01-02 09:00:00 UTC));
    }

    #[test]
    fn working_hours_construction_rejects_start_not_before_end() {
        let tz = IanaTimeZone::new("UTC").unwrap();
        let err = WorkingHours::new(
            tz,
            WallClockTime::new(17, 0).unwrap(),
            WallClockTime::new(9, 0).unwrap(),
        );
        assert!(err.is_err());
    }
}

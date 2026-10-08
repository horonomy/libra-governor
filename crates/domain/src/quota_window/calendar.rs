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

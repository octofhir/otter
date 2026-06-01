//! JavaScript `Date` value (ECMA-262 §21.4).
//!
//! Date instances are ordinary objects with a `[[DateValue]]`
//! internal slot per §21.4.5. The slot is stored on
//! [`crate::object::ObjectBody::date_data`] — same shape as
//! [[BooleanData]] / [[NumberData]] / [[StringData]] — so Date
//! reuses the standard property machinery (V8 `JSDate` and JSC
//! `DateInstance` follow the same model).
//!
//! UTC operations use self-contained proleptic Gregorian arithmetic in
//! [`broken_down`] / [`make_date`]. Local operations use the same
//! `temporal_rs` host time-zone hook and compiled tzdb that power the
//! engine's Temporal objects.
//!
//! # Contents
//! - [`broken_down`] — convert epoch ms to UTC components.
//! - [`local_broken_down`] — convert epoch ms to host-local components.
//! - [`make_date`] — convert UTC wall-clock components back to ms.
//! - [`make_local_date`] — convert host-local wall-clock components
//!   back to ms.
//! - [`to_iso_string`] — `Date.prototype.toISOString` body.
//! - [`now_ms`] — host clock epoch milliseconds for `Date.now` /
//!   the 0-argument constructor.
//! - [`dispatch`] — `Date(...)` / `Date.<static>(...)` entry.
//! - [`prototype`] — `Date.prototype.<method>` lookup table.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date-objects>

pub mod dispatch;
pub mod prototype;
pub mod well_known;

pub(crate) const WEEKDAY_ABBREVIATIONS: [&str; 7] =
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub(crate) const MONTH_ABBREVIATIONS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub(crate) fn month_abbreviation_index(s: &str) -> Option<u8> {
    MONTH_ABBREVIATIONS
        .iter()
        .position(|name| *name == s)
        .map(|idx| idx as u8)
}

/// Host wall-clock epoch milliseconds. Used by `Date.now` and the
/// 0-argument `new Date()` form. Falls back to `NaN` if the host
/// clock is misbehaving.
#[must_use]
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(f64::NAN)
}

/// Host local UTC offset in minutes (`local time - UTC`) for `time_ms`.
/// Uses the same `temporal_rs` host time-zone hook and compiled tzdb
/// that power `Temporal.Now` / `Temporal.ZonedDateTime`.
#[must_use]
pub fn local_utc_offset_minutes(time_ms: f64) -> f64 {
    if !time_ms.is_finite() {
        return f64::NAN;
    }
    let Ok(time_zone) = temporal_rs::Temporal::local_now().time_zone() else {
        return 0.0;
    };
    let lookup_ms = time_ms
        .trunc()
        .clamp(-DATE_TIME_LIMIT_MS, DATE_TIME_LIMIT_MS) as i64;
    let Ok(instant) = temporal_rs::Instant::from_epoch_milliseconds(lookup_ms) else {
        return 0.0;
    };
    let Ok(zdt) = instant.to_zoned_date_time_iso(time_zone) else {
        return 0.0;
    };
    zdt.offset_nanoseconds() as f64 / 60_000_000_000.0
}

/// ECMA-262 TimeZoneString shape (`GMT+HHMM` / `GMT-HHMM`) for the
/// host local time zone at `time_ms`.
#[must_use]
pub fn local_timezone_string(time_ms: f64) -> String {
    let offset = local_utc_offset_minutes(time_ms).trunc() as i32;
    let sign = if offset >= 0 { '+' } else { '-' };
    let abs = offset.abs();
    format!("GMT{sign}{:02}{:02}", abs / 60, abs % 60)
}

/// Convert an epoch millisecond value to host-local broken-down time.
#[must_use]
pub fn local_broken_down(time_ms: f64) -> Option<BrokenDown> {
    let offset_ms = local_utc_offset_minutes(time_ms) * 60_000.0;
    broken_down(time_ms + offset_ms)
}

/// Broken-down UTC components of a Date's time value. All ranges
/// match ECMA-262 §21.4.1.x getter semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokenDown {
    /// Full Gregorian year (e.g., `2026`).
    pub year: i32,
    /// `0..=11` — January is 0.
    pub month: u8,
    /// `1..=31`.
    pub day: u8,
    /// `0..=6` — Sunday is 0.
    pub weekday: u8,
    /// `0..=23`.
    pub hour: u8,
    /// `0..=59`.
    pub minute: u8,
    /// `0..=59`.
    pub second: u8,
    /// `0..=999`.
    pub millisecond: u16,
}

/// Milliseconds in one second.
const MS_PER_SEC: i64 = 1_000;
/// Milliseconds in one minute.
const MS_PER_MINUTE: i64 = 60 * MS_PER_SEC;
/// Milliseconds in one hour.
const MS_PER_HOUR: i64 = 60 * MS_PER_MINUTE;
/// Milliseconds in one day.
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;
const DATE_TIME_LIMIT_MS: f64 = 8.64e15;

/// Days from the epoch to start of `year` (proleptic Gregorian).
fn days_from_year(year: i32) -> i64 {
    let y = (year - 1) as i64;
    365 * (year as i64 - 1970) + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400) - 477
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const MONTH_DAYS: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn days_in_month(year: i32, month: u8) -> u8 {
    if month == 1 && is_leap(year) {
        29
    } else {
        MONTH_DAYS[month as usize]
    }
}

/// Convert a UTC epoch-millisecond value to broken-down components.
/// Returns `None` for NaN / out-of-range values.
///
/// Self-contained proleptic Gregorian calendar arithmetic — does
/// not lean on temporal_rs's timezone-provider plumbing for the
/// hot path.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-yearfromtime>
#[must_use]
pub fn broken_down(ms: f64) -> Option<BrokenDown> {
    if !ms.is_finite() {
        return None;
    }
    let total_ms = ms as i64;
    let day_offset = total_ms.div_euclid(MS_PER_DAY);
    let day_ms = total_ms.rem_euclid(MS_PER_DAY);

    // Find the year containing this day.
    let mut year = 1970 + (day_offset / 365) as i32;
    while days_from_year(year) > day_offset {
        year -= 1;
    }
    while days_from_year(year + 1) <= day_offset {
        year += 1;
    }
    let day_of_year = (day_offset - days_from_year(year)) as i32;

    // Walk the month table.
    let mut month: u8 = 0;
    let mut remaining = day_of_year;
    while month < 12 {
        let dim = days_in_month(year, month) as i32;
        if remaining < dim {
            break;
        }
        remaining -= dim;
        month += 1;
    }
    let day = remaining as u8 + 1;

    // 1970-01-01 is a Thursday → weekday = 4. Compute via day_offset
    // mod 7 (handle negative epoch values).
    let weekday = ((day_offset % 7 + 11) % 7) as u8;

    let hour = (day_ms / MS_PER_HOUR) as u8;
    let minute = ((day_ms % MS_PER_HOUR) / MS_PER_MINUTE) as u8;
    let second = ((day_ms % MS_PER_MINUTE) / MS_PER_SEC) as u8;
    let millisecond = (day_ms % MS_PER_SEC) as u16;

    Some(BrokenDown {
        year,
        month,
        day,
        weekday,
        hour,
        minute,
        second,
        millisecond,
    })
}

/// Build a UTC epoch-millisecond value from individual components.
/// Used by both `new Date(year, month, ...)` and `Date.UTC(...)`.
/// Returns `NaN` when any argument is non-finite.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-makedate>
#[must_use]
pub fn make_date(
    year: f64,
    month: f64,
    day: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
    ms: f64,
) -> f64 {
    time_clip(make_date_unclipped(
        year, month, day, hours, minutes, seconds, ms,
    ))
}

fn make_date_unclipped(
    year: f64,
    month: f64,
    day: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
    ms: f64,
) -> f64 {
    if [year, month, day, hours, minutes, seconds, ms]
        .iter()
        .any(|v| !v.is_finite())
    {
        return f64::NAN;
    }
    // §21.4.1.12 `MakeDay` operates on the raw year — the
    // 2-digit-year fixup is **not** applied here; the
    // `Date(year, ...)` constructor / `Date.UTC` / Annex B
    // `setYear` callers do that mapping themselves before
    // invoking `make_date`. `setFullYear` / `setUTCFullYear`
    // pass year through unchanged per §21.4.4.21.
    // Normalise month overflow into the year (spec lets month
    // overflow shift the year — `new Date(2024, 13, 1)` ===
    // `new Date(2025, 1, 1)`).
    let year = year.trunc();
    let month = month.trunc();
    let day = day.trunc();
    let hours = hours.trunc();
    let minutes = minutes.trunc();
    let seconds = seconds.trunc();
    let ms = ms.trunc();

    let total_months = year as i64 * 12 + month as i64;
    let final_year = total_months.div_euclid(12) as i32;
    let final_month = total_months.rem_euclid(12) as u8;

    let mut total_days = days_from_year(final_year);
    for m in 0..final_month {
        total_days += days_in_month(final_year, m) as i64;
    }
    total_days += day as i64 - 1;

    (total_days as f64 * MS_PER_DAY as f64)
        + (hours * MS_PER_HOUR as f64)
        + (minutes * MS_PER_MINUTE as f64)
        + (seconds * MS_PER_SEC as f64)
        + ms
}

fn time_clip(ms: f64) -> f64 {
    if !ms.is_finite() || ms.abs() > DATE_TIME_LIMIT_MS {
        return f64::NAN;
    }
    let clipped = ms.trunc();
    if clipped == 0.0 { 0.0 } else { clipped }
}

/// Build an epoch millisecond value by interpreting the provided
/// components in the host local time zone.
#[must_use]
pub fn make_local_date(
    year: f64,
    month: f64,
    day: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
    ms: f64,
) -> f64 {
    let local_ms = make_date_unclipped(year, month, day, hours, minutes, seconds, ms);
    if !local_ms.is_finite() {
        return f64::NAN;
    }
    let mut utc_ms = local_ms;
    for _ in 0..3 {
        let offset_ms = local_utc_offset_minutes(utc_ms) * 60_000.0;
        if !offset_ms.is_finite() {
            return f64::NAN;
        }
        let next = local_ms - offset_ms;
        if (next - utc_ms).abs() < 1.0 {
            utc_ms = next;
            break;
        }
        utc_ms = next;
    }
    time_clip(utc_ms)
}

/// §21.4.4.41 Date.prototype.toISOString format. Renders an
/// Invalid Date's time value as the canonical empty placeholder.
#[must_use]
pub fn to_iso_string(ms: f64) -> Option<String> {
    let bd = broken_down(ms)?;
    let year = if (0..=9999).contains(&bd.year) {
        format!("{:04}", bd.year)
    } else if bd.year < 0 {
        format!("-{:06}", bd.year.abs())
    } else {
        format!("+{:06}", bd.year)
    };
    Some(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        bd.month + 1,
        bd.day,
        bd.hour,
        bd.minute,
        bd.second,
        bd.millisecond
    ))
}

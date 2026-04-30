//! JavaScript `Date` value (ECMA-262 §21.4).
//!
//! A Date carries an `f64` time value (UTC milliseconds since the
//! Unix epoch, with `NaN` representing an invalid date). Cloning
//! shares storage so `setX` / `setY` mutations are observable
//! through every handle, matching spec mutation semantics.
//!
//! All broken-down accessors (year / month / hours / …) lower
//! through self-contained proleptic Gregorian arithmetic in
//! [`broken_down`] / [`make_date`] — temporal_rs's full timezone
//! provider isn't needed for the foundation surface.
//!
//! # Contents
//! - [`JsDate`] — heap-shared handle.
//! - [`broken_down`] — convert epoch ms to UTC components.
//! - [`make_date`] — convert wall-clock components back to ms.
//! - [`to_iso_string`] — `Date.prototype.toISOString` body.
//! - [`dispatch`] — `Date(...)` / `Date.<static>(...)` entry.
//! - [`prototype`] — `Date.prototype.<method>` lookup table.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date-objects>

pub mod dispatch;
pub mod prototype;

use std::cell::Cell;
use std::rc::Rc;

/// Heap-shared `Date` handle. Cloning shares storage; mutation via
/// [`Self::set_time`] is observable through every clone.
#[derive(Debug, Clone)]
pub struct JsDate {
    inner: Rc<Cell<f64>>,
}

impl JsDate {
    /// Allocate a new Date from raw epoch milliseconds.
    #[must_use]
    pub fn from_ms(ms: f64) -> Self {
        // §21.4.1.6 TimeClip — values outside ±8.64e15 ms (≈ ±100 M
        // days) collapse to NaN.
        let clipped = if !ms.is_finite() || ms.abs() > 8.64e15 {
            f64::NAN
        } else {
            // Spec says the time value is an integer; truncate
            // toward zero.
            ms.trunc()
        };
        Self {
            inner: Rc::new(Cell::new(clipped)),
        }
    }

    /// Allocate a Date for "now" (the host's current epoch ms).
    #[must_use]
    pub fn now() -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(f64::NAN);
        Self::from_ms(ms)
    }

    /// Allocate an Invalid Date — the canonical NaN-time form.
    #[must_use]
    pub fn invalid() -> Self {
        Self::from_ms(f64::NAN)
    }

    /// Read the raw time value (NaN for Invalid Date).
    #[must_use]
    pub fn time(&self) -> f64 {
        self.inner.get()
    }

    /// Update the time value. Mutation is observable through every
    /// clone of this handle.
    pub fn set_time(&self, ms: f64) {
        let clipped = if !ms.is_finite() || ms.abs() > 8.64e15 {
            f64::NAN
        } else {
            ms.trunc()
        };
        self.inner.set(clipped);
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl PartialEq for JsDate {
    fn eq(&self, other: &Self) -> bool {
        // Two distinct Dates with the same time value compare equal
        // for `==` purposes when used through `Value::PartialEq`.
        // Identity comparison is exposed via `ptr_eq`.
        self.time() == other.time()
    }
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
    if [year, month, day, hours, minutes, seconds, ms]
        .iter()
        .any(|v| !v.is_finite())
    {
        return f64::NAN;
    }
    // §21.4.1.13 — years in `0..=99` map to `1900 + year`.
    let resolved_year = if (0.0..=99.0).contains(&year) {
        year as i32 + 1900
    } else {
        year as i32
    };
    // Normalise month overflow into the year (spec lets month
    // overflow shift the year — `new Date(2024, 13, 1)` ===
    // `new Date(2025, 1, 1)`).
    let total_months = resolved_year as i64 * 12 + month as i64;
    let final_year = total_months.div_euclid(12) as i32;
    let final_month = total_months.rem_euclid(12) as u8;

    let mut total_days = days_from_year(final_year);
    for m in 0..final_month {
        total_days += days_in_month(final_year, m) as i64;
    }
    total_days += day as i64 - 1;

    let total_ms = total_days * MS_PER_DAY
        + hours as i64 * MS_PER_HOUR
        + minutes as i64 * MS_PER_MINUTE
        + seconds as i64 * MS_PER_SEC
        + ms as i64;
    let result = total_ms as f64;
    if !result.is_finite() || result.abs() > 8.64e15 {
        return f64::NAN;
    }
    result
}

/// §21.4.4.41 Date.prototype.toISOString format. Renders an
/// Invalid Date's time value as the canonical empty placeholder.
#[must_use]
pub fn to_iso_string(ms: f64) -> Option<String> {
    let bd = broken_down(ms)?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        bd.year,
        bd.month + 1,
        bd.day,
        bd.hour,
        bd.minute,
        bd.second,
        bd.millisecond
    ))
}

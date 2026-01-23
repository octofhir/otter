//! Temporal API (Stage 3, shipping in Chrome 144+ / Firefox 139+)
//!
//! Provides the modern date/time API for JavaScript:
//! - `Temporal.Now` - Current time utilities
//! - `Temporal.Instant` - Fixed UTC point (nanosecond precision)
//! - `Temporal.ZonedDateTime` - Date+time+timezone
//! - `Temporal.PlainDate` - Calendar date only
//! - `Temporal.PlainTime` - Time only
//! - `Temporal.PlainDateTime` - Date+time without timezone
//! - `Temporal.PlainYearMonth` - Year+month
//! - `Temporal.PlainMonthDay` - Month+day
//! - `Temporal.Duration` - Time spans

mod duration;
mod instant;
mod now;
mod plain_date;
mod plain_date_time;
mod plain_month_day;
mod plain_time;
mod plain_year_month;
mod zoned_date_time;

use otter_vm_runtime::Op;

/// Get all Temporal ops for extension registration
pub fn ops() -> Vec<Op> {
    let mut all_ops = Vec::new();
    all_ops.extend(now::ops());
    all_ops.extend(instant::ops());
    all_ops.extend(plain_date::ops());
    all_ops.extend(plain_time::ops());
    all_ops.extend(plain_date_time::ops());
    all_ops.extend(plain_year_month::ops());
    all_ops.extend(plain_month_day::ops());
    all_ops.extend(zoned_date_time::ops());
    all_ops.extend(duration::ops());
    all_ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ops_count() {
        let all = ops();
        // Should have a reasonable number of ops
        assert!(!all.is_empty());
        println!("Total Temporal ops: {}", all.len());
    }
}

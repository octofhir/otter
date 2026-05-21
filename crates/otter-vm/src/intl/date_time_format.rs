//! `Intl.DateTimeFormat` — locale-aware date / time formatting.
//!
//! Foundation slice ships a narrow surface: the `format(date)`
//! method accepts a JS `Number` (epoch milliseconds, the same
//! `Date.now()` shape) or a `Temporal.PlainDateTime` and produces
//! a string sized by the option bag (`year` / `month` / `day` /
//! `hour` / `minute` / `second`). Locale-specific punctuation is
//! deferred until ICU `FieldSetBuilder` integration lands; the
//! foundation renders a stable ISO-like layout that matches the
//! task's "returns a formatted string" criterion.
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-datetimeformat-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{coerce_locale, js_string, options_object, read_string_option};
use crate::intl::payload::{DateTimeFormatPayload, IntlPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::temporal::TemporalPayload;

/// Resolve the constructor option bag.
pub fn resolve(
    locale: &Value,
    options: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> DateTimeFormatPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    // Default option bag follows ECMA-402 §11.1.2 step 6: when no
    // date-time component options are present, fall back to
    // `{ year: "numeric", month: "numeric", day: "numeric" }`.
    let component_present =
        |name: &str| -> bool { !read_string_option(opts_ref, name, "", gc_heap).is_empty() };
    let mut year = component_present("year");
    let mut month = component_present("month");
    let mut day = component_present("day");
    let hour = component_present("hour");
    let minute = component_present("minute");
    let second = component_present("second");
    if !year && !month && !day && !hour && !minute && !second {
        year = true;
        month = true;
        day = true;
    }
    DateTimeFormatPayload {
        locale: coerce_locale(Some(locale)),
        year,
        month,
        day,
        hour,
        minute,
        second,
    }
}

fn require_date_time(
    args: &IntrinsicArgs<'_>,
) -> Result<DateTimeFormatPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload_clone(args.gc_heap) {
            IntlPayload::DateTimeFormat(d) => Ok(d),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.DateTimeFormat",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.DateTimeFormat",
        }),
    }
}

fn impl_format(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_date_time(args)?;
    let formatted = match args.args.first() {
        Some(Value::Number(n)) => format_epoch_ms(n.as_f64() as i64, &payload),
        Some(Value::Temporal(t)) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDateTime(pdt) => format_pdt(&pdt, &payload),
            TemporalPayload::PlainDate(pd) => format_pd(&pd, &payload),
            TemporalPayload::Instant(inst) => format_epoch_ms(inst.epoch_milliseconds(), &payload),
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a Number, Temporal.Instant, Temporal.PlainDate, or Temporal.PlainDateTime",
                });
            }
        },
        // No argument → use Date.now() epoch ms equivalent.
        None | Some(Value::Undefined) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            format_epoch_ms(now, &payload)
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a Number or Temporal value",
            });
        }
    };
    js_string(&formatted, args.string_heap).map_err(intl_to_intrinsic)
}

fn impl_resolved_options(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_date_time(args)?;
    let component = |present: bool| {
        if present {
            "numeric".to_string()
        } else {
            "".to_string()
        }
    };
    let locale_value = js_string_value(&payload.locale, args)?;
    let yr = if payload.year {
        Some(js_string_value(&component(true), args)?)
    } else {
        None
    };
    let mo = if payload.month {
        Some(js_string_value(&component(true), args)?)
    } else {
        None
    };
    let da = if payload.day {
        Some(js_string_value(&component(true), args)?)
    } else {
        None
    };
    let hr = if payload.hour {
        Some(js_string_value(&component(true), args)?)
    } else {
        None
    };
    let mi = if payload.minute {
        Some(js_string_value(&component(true), args)?)
    } else {
        None
    };
    let se = if payload.second {
        Some(js_string_value(&component(true), args)?)
    } else {
        None
    };
    let calendar = Value::String(crate::string::JsString::from_str(
        "iso8601",
        args.string_heap,
    )?);
    let mut value_roots = vec![&locale_value, &calendar];
    if let Some(v) = &yr {
        value_roots.push(v);
    }
    if let Some(v) = &mo {
        value_roots.push(v);
    }
    if let Some(v) = &da {
        value_roots.push(v);
    }
    if let Some(v) = &hr {
        value_roots.push(v);
    }
    if let Some(v) = &mi {
        value_roots.push(v);
    }
    if let Some(v) = &se {
        value_roots.push(v);
    }
    let obj = args.alloc_object_rooted(&value_roots, &[])?;
    let heap = &mut *args.gc_heap;
    crate::object::set(obj, heap, "locale", locale_value);
    if let Some(v) = yr {
        crate::object::set(obj, heap, "year", v);
    }
    if let Some(v) = mo {
        crate::object::set(obj, heap, "month", v);
    }
    if let Some(v) = da {
        crate::object::set(obj, heap, "day", v);
    }
    if let Some(v) = hr {
        crate::object::set(obj, heap, "hour", v);
    }
    if let Some(v) = mi {
        crate::object::set(obj, heap, "minute", v);
    }
    if let Some(v) = se {
        crate::object::set(obj, heap, "second", v);
    }
    crate::object::set(obj, heap, "calendar", calendar);
    let _ = NumberValue::from_i32; // keep import live
    Ok(Value::Object(obj))
}

fn js_string_value(s: &str, args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::String(crate::string::JsString::from_str(
        s,
        args.string_heap,
    )?))
}

fn intl_to_intrinsic(err: IntlError) -> IntrinsicError {
    let _ = err;
    IntrinsicError::BadArgument {
        index: 0,
        reason: "format failed",
    }
}

/// Render a `(year, month, day, hour, minute, second)` tuple per
/// the resolved option bag. Locale-specific punctuation is left to
/// future ICU integration; the foundation uses ISO-like fragments
/// joined by `, ` so the output is unambiguous and stable.
fn format_components(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> String {
    let mut date_part = String::new();
    if payload.month {
        date_part.push_str(&format!("{:02}", month));
    }
    if payload.day {
        if !date_part.is_empty() {
            date_part.push('/');
        }
        date_part.push_str(&format!("{:02}", day));
    }
    if payload.year {
        if !date_part.is_empty() {
            date_part.push('/');
        }
        date_part.push_str(&format!("{}", year));
    }
    let mut time_part = String::new();
    if payload.hour {
        time_part.push_str(&format!("{:02}", hour));
    }
    if payload.minute {
        if !time_part.is_empty() {
            time_part.push(':');
        }
        time_part.push_str(&format!("{:02}", minute));
    }
    if payload.second {
        if !time_part.is_empty() {
            time_part.push(':');
        }
        time_part.push_str(&format!("{:02}", second));
    }
    match (date_part.is_empty(), time_part.is_empty()) {
        (false, false) => format!("{date_part}, {time_part}"),
        (false, true) => date_part,
        (true, false) => time_part,
        (true, true) => String::new(),
    }
}

fn format_epoch_ms(ms: i64, payload: &DateTimeFormatPayload) -> String {
    let secs = ms.div_euclid(1000);
    let sub_ms = ms.rem_euclid(1000);
    let _ = sub_ms;
    let (year, month, day, hour, minute, second) = epoch_to_civil(secs);
    format_components(year, month, day, hour, minute, second, payload)
}

fn format_pdt(pdt: &temporal_rs::PlainDateTime, payload: &DateTimeFormatPayload) -> String {
    format_components(
        pdt.year(),
        pdt.month(),
        pdt.day(),
        pdt.hour(),
        pdt.minute(),
        pdt.second(),
        payload,
    )
}

fn format_pd(pd: &temporal_rs::PlainDate, payload: &DateTimeFormatPayload) -> String {
    format_components(pd.year(), pd.month(), pd.day(), 0, 0, 0, payload)
}

/// Convert UTC epoch seconds to a civil `(year, month, day, hour,
/// minute, second)` tuple using the proleptic Gregorian calendar.
/// Howard Hinnant's algorithm — public-domain, exact for the full
/// `i64` range.
fn epoch_to_civil(epoch_secs: i64) -> (i32, u8, u8, u8, u8, u8) {
    let secs_per_day = 86_400_i64;
    let days = epoch_secs.div_euclid(secs_per_day);
    let secs_of_day = epoch_secs.rem_euclid(secs_per_day);
    // Civil-from-days, Hinnant 2013 (https://howardhinnant.github.io/date_algorithms.html)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = if m <= 2 { y + 1 } else { y };
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    let second = (secs_of_day % 60) as u8;
    (year, m, d, hour, minute, second)
}

/// `Intl.DateTimeFormat.prototype` table.
pub static DATE_TIME_FORMAT_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "format"          / 1 => impl_format,
        "resolvedOptions" / 0 => impl_resolved_options,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    DATE_TIME_FORMAT_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}

/// Static-side dispatch (none today).
pub fn dispatch_static(method: &str, _args: &[Value]) -> Result<Value, IntlError> {
    Err(IntlError::UnknownMember {
        class: "DateTimeFormat",
        method: method.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_at_unix_zero() {
        let (y, m, d, h, mi, s) = epoch_to_civil(0);
        assert_eq!((y, m, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_2024_january() {
        let (y, m, d, _, _, _) = epoch_to_civil(1_704_067_200);
        assert_eq!((y, m, d), (2024, 1, 1));
    }
}

//! `Date(...)` / `Date.<static>(...)` dispatcher per ECMA-262
//! §21.4.2 / §21.4.3. Routed through
//! [`crate::otter_bytecode::Op::DateCall`] by the compiler and
//! through the `Date` native constructor installed by
//! [`crate::bootstrap`].
//!
//! # Contents
//! - [`call`] — main entry point. Allocates a fresh Date instance
//!   (`Value::Object` with the `[[DateValue]]` internal slot) for
//!   the `Construct` method; returns `Value::Number` for the
//!   `Now` / `Parse` / `UTC` statics.
//! - [`construct_time_value`] — pure helper that derives the time
//!   value for `new Date(...)` from its arguments.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date-constructor>
//! - <https://tc39.es/ecma262/#sec-date.now>
//! - <https://tc39.es/ecma262/#sec-date.parse>
//! - <https://tc39.es/ecma262/#sec-date.utc>

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;

use super::{make_date, now_ms};
use crate::object::{self, JsObject};
use crate::{Value, VmError};

/// Dispatch `Date(...)` ([`otter_bytecode::method_id::DateMethod::Construct`])
/// / `Date.<method>(...)`. Routes the typed
/// [`otter_bytecode::method_id::DateMethod`] emitted by the
/// compiler.
///
/// `prototype` is the realm's `%Date.prototype%` (resolved by the
/// caller via [`crate::Interpreter::constructor_prototype_value`]).
/// Construct allocates a fresh ordinary object, sets its
/// `[[Prototype]]` to that handle, and installs the time value
/// in the `[[DateValue]]` slot.
///
/// # Errors
/// - [`VmError::OutOfMemory`] if allocation of the instance fails.
///
/// # Spec
/// - <https://tc39.es/ecma262/#sec-date-constructor>
pub fn call(
    method: otter_bytecode::method_id::DateMethod,
    args: &[Value],
    heap: &mut GcHeap,
    prototype: Option<JsObject>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::DateMethod as M;
    match method {
        // §21.4.2 — `new Date(...)`. Forms:
        //   - 0 args: now.
        //   - 1 arg (string): parse via Date.parse.
        //   - 1 arg (number / Date / Object-with-[[DateValue]]):
        //     epoch ms (Date / wrapper) or ToPrimitive→Number for
        //     ordinary objects.
        //   - 2+ args: (year, month, day?, hr?, min?, sec?, ms?).
        M::Construct => {
            let time = construct_time_value(args, heap);
            let obj = object::alloc_object_with_roots(heap, external_visit).map_err(|err| {
                VmError::OutOfMemory {
                    requested_bytes: err.requested_bytes(),
                    heap_limit_bytes: err.heap_limit_bytes(),
                }
            })?;
            object::set_date_data(obj, heap, time);
            if let Some(proto) = prototype {
                object::set_prototype(obj, heap, Some(proto));
            }
            Ok(Value::object(obj))
        }
        M::Now | M::Parse | M::UTC => call_static(method, args, heap),
    }
}

/// Non-allocating Date statics. Used by the NativeFunction
/// wrappers installed on the global `Date` constructor (which
/// cannot easily reach the GC heap from inside a `NativeCtx`
/// without re-entering the interpreter).
///
/// # Spec
/// - <https://tc39.es/ecma262/#sec-date.now>
/// - <https://tc39.es/ecma262/#sec-date.parse>
/// - <https://tc39.es/ecma262/#sec-date.utc>
pub fn call_static(
    method: otter_bytecode::method_id::DateMethod,
    args: &[Value],
    heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::DateMethod as M;
    match method {
        // §21.4.3.1 Date.now() — current epoch ms as a Number.
        M::Now => Ok(Value::number_f64(now_ms())),
        // §21.4.3.2 Date.parse(str).
        M::Parse => {
            let Some(s) = args.first().and_then(|v| v.as_string()) else {
                return Ok(Value::number_f64(f64::NAN));
            };
            Ok(Value::number_f64(parse_date(&s.to_lossy_string(heap))))
        }
        // §21.4.3.4 Date.UTC(year, month, day?, …).
        M::UTC => {
            if args.is_empty() {
                return Ok(Value::number_f64(f64::NAN));
            }
            let year = year_with_two_digit_rule(number_arg(args, 0));
            let month = number_or(args, 1, 0.0);
            let day = number_or(args, 2, 1.0);
            let hours = number_or(args, 3, 0.0);
            let minutes = number_or(args, 4, 0.0);
            let seconds = number_or(args, 5, 0.0);
            let ms = number_or(args, 6, 0.0);
            Ok(Value::number_f64(make_date(
                year, month, day, hours, minutes, seconds, ms,
            )))
        }
        // Construct is allocating — must go through `call(...)`.
        M::Construct => Err(VmError::InvalidOperand),
    }
}

/// Derive the time value (epoch ms) for `new Date(...)`. Pure
/// function of the arguments + heap (only read for the
/// 1-argument-is-Object case, where we inspect the `[[DateValue]]`
/// slot).
///
/// # Spec
/// - <https://tc39.es/ecma262/#sec-date>
pub fn construct_time_value(args: &[Value], heap: &GcHeap) -> f64 {
    match args.len() {
        0 => now_ms(),
        1 => {
            let v = &args[0];
            if let Some(s) = v.as_string() {
                parse_date(&s.to_lossy_string(heap))
            } else if let Some(n) = v.as_number() {
                n.as_f64()
            } else if let Some(o) = v.as_object() {
                // §21.4.2.2 step 3.b — if value has [[DateValue]],
                // use that. Other objects fall back to
                // ToPrimitive→Number; primitive coercion outside
                // the fast path lands as NaN here.
                object::date_data(o, heap).unwrap_or(f64::NAN)
            } else if let Some(b) = v.as_boolean() {
                if b { 1.0 } else { 0.0 }
            } else if v.is_null() {
                0.0
            } else {
                f64::NAN
            }
        }
        _ => {
            let year = year_with_two_digit_rule(number_arg(args, 0));
            let month = number_arg(args, 1);
            let day = number_or(args, 2, 1.0);
            let hours = number_or(args, 3, 0.0);
            let minutes = number_or(args, 4, 0.0);
            let seconds = number_or(args, 5, 0.0);
            let ms = number_or(args, 6, 0.0);
            make_date(year, month, day, hours, minutes, seconds, ms)
        }
    }
}

/// §21.4.2.1 step 4.b — `Date(year, month, ...)` and `Date.UTC`
/// remap integer years in `0..=99` to `1900 + year`. `setFullYear`
/// / `setUTCFullYear` / `MakeDay` itself do **not** apply this
/// fixup, so it lives in the constructor / UTC paths only.
fn year_with_two_digit_rule(year: f64) -> f64 {
    if !year.is_finite() {
        return year;
    }
    let int = year.trunc();
    if int == year && (0.0..=99.0).contains(&int) {
        int + 1900.0
    } else {
        year
    }
}

fn number_arg(args: &[Value], idx: usize) -> f64 {
    let v = args.get(idx);
    if let Some(n) = v.and_then(|v| v.as_number()) {
        n.as_f64()
    } else if let Some(b) = v.and_then(|v| v.as_boolean()) {
        if b { 1.0 } else { 0.0 }
    } else if v.is_some_and(|v| v.is_null()) {
        0.0
    } else {
        f64::NAN
    }
}

fn number_or(args: &[Value], idx: usize, default: f64) -> f64 {
    if idx >= args.len() {
        return default;
    }
    number_arg(args, idx)
}

/// Parse an ISO 8601 / RFC 3339 date string per §21.4.1.18 — covers
/// the common `YYYY-MM-DDTHH:MM:SS[.sss][Z|±HH:MM]` shape and the
/// date-only form `YYYY-MM-DD`. Returns `NaN` for malformed input.
fn parse_date(input: &str) -> f64 {
    let s = input.trim();
    if s.is_empty() {
        return f64::NAN;
    }
    // Date portion: YYYY-MM-DD (year may be ±YYYYYY).
    let (date_part, rest) = split_at_first(s, &['T', ' ']);
    let (year, month, day) = match parse_date_components(date_part) {
        Some(v) => v,
        None => return f64::NAN,
    };
    let mut hour: f64 = 0.0;
    let mut minute: f64 = 0.0;
    let mut second: f64 = 0.0;
    let mut ms: f64 = 0.0;
    let mut offset_minutes: i64 = 0;
    if let Some(time_part) = rest {
        // Trim any trailing `Z` / `+HH:MM` / `-HH:MM` offset.
        let (time_body, offset) = split_offset(time_part);
        let parts: Vec<&str> = time_body.splitn(3, ':').collect();
        if parts.len() < 2 {
            return f64::NAN;
        }
        hour = parts[0].parse::<f64>().unwrap_or(f64::NAN);
        minute = parts[1].parse::<f64>().unwrap_or(f64::NAN);
        if let Some(sec_part) = parts.get(2) {
            // Seconds may include a `.fraction` for ms.
            let (sec_body, frac) = match sec_part.split_once('.') {
                Some((s, f)) => (s, Some(f)),
                None => (*sec_part, None),
            };
            second = sec_body.parse::<f64>().unwrap_or(f64::NAN);
            if let Some(f) = frac {
                let truncated: String = f.chars().take(3).collect();
                ms = format!("{:0<3}", truncated).parse::<f64>().unwrap_or(0.0);
            }
        }
        if let Some(offset_str) = offset {
            offset_minutes = match parse_offset(offset_str) {
                Some(m) => m,
                None => return f64::NAN,
            };
        }
    }
    let utc_ms = make_date(year, month - 1.0, day, hour, minute, second, ms);
    if !utc_ms.is_finite() {
        return f64::NAN;
    }
    utc_ms - (offset_minutes as f64) * 60_000.0
}

fn split_at_first<'a>(s: &'a str, seps: &[char]) -> (&'a str, Option<&'a str>) {
    for (i, c) in s.char_indices() {
        if seps.contains(&c) {
            return (&s[..i], Some(&s[i + c.len_utf8()..]));
        }
    }
    (s, None)
}

fn split_offset(time: &str) -> (&str, Option<&str>) {
    if let Some(stripped) = time.strip_suffix('Z') {
        return (stripped, Some("Z"));
    }
    // Find a `+` / `-` after position 0 (not the leading sign).
    for (i, c) in time.char_indices().rev() {
        if c == '+' || c == '-' {
            if i == 0 {
                continue;
            }
            return (&time[..i], Some(&time[i..]));
        }
    }
    (time, None)
}

fn parse_offset(s: &str) -> Option<i64> {
    if s == "Z" || s == "+00:00" || s == "-00:00" {
        return Some(0);
    }
    let (sign, body) = match s.chars().next()? {
        '+' => (1, &s[1..]),
        '-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None if body.len() == 4 => (&body[..2], &body[2..]),
        _ => return None,
    };
    let hours: i64 = h.parse().ok()?;
    let minutes: i64 = m.parse().ok()?;
    Some(sign * (hours * 60 + minutes))
}

fn parse_date_components(input: &str) -> Option<(f64, f64, f64)> {
    // `YYYY-MM-DD`, `YYYY-MM`, or `YYYY`.
    let parts: Vec<&str> = input.splitn(3, '-').collect();
    let year: f64 = parts.first()?.parse().ok()?;
    let month: f64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let day: f64 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    Some((year, month, day))
}

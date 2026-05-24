//! Shared coercion / extraction helpers for Temporal intrinsic
//! implementations.
//!
//! Each prototype-method file (`instant.rs`, `duration.rs`, …)
//! pulls the receiver- and argument-shaped helpers out of this
//! module so the per-kind logic stays focused on the spec algorithm.
//!
//! # Contents
//! - [`require_string_arg`] — coerce arg N to a Rust string.
//! - [`require_object_arg`] — coerce arg N to a `JsObject` (used
//!   for `{ days: 1 }` shaped Duration partials and `{ unit:
//!   "minutes" }` total-options).
//! - [`from_instant`] / [`from_duration`] / … — extractors that
//!   panic-free downcast a payload to the expected variant.
//! - [`make_temporal`] — construct a [`crate::Value::Temporal`]
//!   from a payload.

use crate::intrinsics::{IntrinsicArgs, IntrinsicError};
use crate::object::JsObject;
use crate::string::JsString;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// Coerce arg `index` to a Rust string. Returns
/// [`IntrinsicError::BadArgument`] when the slot is missing or the
/// value is not a string. Foundation does not yet thread ToString
/// through every primitive — strings flow through directly.
pub fn require_string_arg(args: &IntrinsicArgs<'_>, index: u16) -> Result<String, IntrinsicError> {
    let v = args
        .args
        .get(index as usize)
        .ok_or(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        })?;
    v.as_string(args.gc_heap)
        .map(|s| s.to_lossy_string(args.gc_heap))
        .ok_or(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        })
}

/// Coerce arg `index` to a [`JsObject`] handle. Used for
/// option / partial-record arguments.
pub fn require_object_arg(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<JsObject, IntrinsicError> {
    args.args
        .get(index as usize)
        .and_then(|v| v.as_object())
        .ok_or(IntrinsicError::BadArgument {
            index,
            reason: "must be an object",
        })
}

/// Optional object arg — returns [`None`] when missing/`undefined`.
pub fn optional_object_arg(args: &IntrinsicArgs<'_>, index: u16) -> Option<JsObject> {
    args.args.get(index as usize).and_then(|v| v.as_object())
}

/// Read a numeric field from a partial-record object. Returns the
/// default when the property is missing or `undefined`. Coerces
/// `Value::Number` only; non-numeric values fail.
pub fn read_i64_field(
    obj: JsObject,
    name: &str,
    default: i64,
    gc_heap: &otter_gc::GcHeap,
) -> Result<i64, IntrinsicError> {
    let Some(v) = crate::object::get(obj, gc_heap, name) else {
        return Ok(default);
    };
    if v.is_undefined() {
        return Ok(default);
    }
    if let Some(n) = v.as_number() {
        match n.as_smi() {
            Some(v) => Ok(v as i64),
            None => Ok(n.as_f64() as i64),
        }
    } else {
        Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "partial-record fields must be numbers",
        })
    }
}

/// Read an optional string field (`{ unit: "minutes" }`).
pub fn read_string_field(obj: JsObject, name: &str, gc_heap: &otter_gc::GcHeap) -> Option<String> {
    let v = crate::object::get(obj, gc_heap, name)?;
    v.as_string(gc_heap).map(|s| s.to_lossy_string(gc_heap))
}

/// Build a `Value::Temporal` from a payload, allocating the backing
/// GC body via [`IntrinsicArgs::gc_heap`].
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] via [`IntrinsicError::OutOfMemory`].
pub fn make_temporal(
    args: &mut IntrinsicArgs<'_>,
    payload: TemporalPayload,
) -> Result<Value, IntrinsicError> {
    let handle = JsTemporal::new(args.gc_heap, payload)?;
    Ok(Value::temporal(handle))
}

/// `make_temporal` variant for the static-dispatch path
/// ([`crate::temporal::dispatch::call`]) which surfaces
/// [`crate::temporal::dispatch::TemporalError`] rather than
/// [`IntrinsicError`].
///
/// # Errors
///
/// Maps [`otter_gc::OutOfMemory`] onto
/// [`crate::temporal::dispatch::TemporalError::OutOfMemory`].
pub fn alloc_temporal_value(
    heap: &mut otter_gc::GcHeap,
    payload: TemporalPayload,
) -> Result<Value, crate::temporal::dispatch::TemporalError> {
    let handle = JsTemporal::new(heap, payload).map_err(|e| {
        crate::temporal::dispatch::TemporalError::OutOfMemory {
            requested_bytes: e.requested_bytes(),
            heap_limit_bytes: e.heap_limit_bytes(),
        }
    })?;
    Ok(Value::temporal(handle))
}

/// Extract a [`temporal_rs::Instant`] from the receiver, or raise
/// [`IntrinsicError::BadReceiver`] for the wrong kind.
pub fn require_instant(args: &IntrinsicArgs<'_>) -> Result<temporal_rs::Instant, IntrinsicError> {
    if let Some(t) = args.receiver.as_temporal(args.gc_heap) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::Instant(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.Instant",
            }),
        }
    } else {
        Err(IntrinsicError::BadReceiver {
            expected: "Temporal.Instant",
        })
    }
}

/// Extract a [`temporal_rs::Duration`] from the receiver.
pub fn require_duration(args: &IntrinsicArgs<'_>) -> Result<temporal_rs::Duration, IntrinsicError> {
    if let Some(t) = args.receiver.as_temporal(args.gc_heap) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::Duration(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.Duration",
            }),
        }
    } else {
        Err(IntrinsicError::BadReceiver {
            expected: "Temporal.Duration",
        })
    }
}

/// Extract a [`temporal_rs::PlainDate`] from the receiver.
pub fn require_plain_date(
    args: &IntrinsicArgs<'_>,
) -> Result<temporal_rs::PlainDate, IntrinsicError> {
    if let Some(t) = args.receiver.as_temporal(args.gc_heap) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDate(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.PlainDate",
            }),
        }
    } else {
        Err(IntrinsicError::BadReceiver {
            expected: "Temporal.PlainDate",
        })
    }
}

/// Extract a [`temporal_rs::PlainTime`] from the receiver.
pub fn require_plain_time(
    args: &IntrinsicArgs<'_>,
) -> Result<temporal_rs::PlainTime, IntrinsicError> {
    if let Some(t) = args.receiver.as_temporal(args.gc_heap) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainTime(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.PlainTime",
            }),
        }
    } else {
        Err(IntrinsicError::BadReceiver {
            expected: "Temporal.PlainTime",
        })
    }
}

/// Extract a [`temporal_rs::PlainDateTime`] from the receiver.
pub fn require_plain_date_time(
    args: &IntrinsicArgs<'_>,
) -> Result<temporal_rs::PlainDateTime, IntrinsicError> {
    if let Some(t) = args.receiver.as_temporal(args.gc_heap) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.PlainDateTime",
            }),
        }
    } else {
        Err(IntrinsicError::BadReceiver {
            expected: "Temporal.PlainDateTime",
        })
    }
}

/// Convert a `temporal_rs` error into the foundation
/// [`IntrinsicError`] hierarchy, honouring the spec error class:
/// `Range` → [`IntrinsicError::OutOfRange`], `Type` / `Generic` /
/// `Syntax` / `Assert` → [`IntrinsicError::BadArgument`].
pub fn temporal_err(err: temporal_rs::TemporalError) -> IntrinsicError {
    use temporal_rs::error::ErrorKind;
    match err.kind() {
        ErrorKind::Range => IntrinsicError::OutOfRange {
            index: 0,
            reason: "Temporal operation produced an out-of-range value",
        },
        _ => IntrinsicError::BadArgument {
            index: 0,
            reason: "Temporal operation failed",
        },
    }
}

/// Build a `Value::String` from a Rust string via the active heap.
pub fn js_string_value(
    value: String,
    args: &mut IntrinsicArgs<'_>,
) -> Result<Value, IntrinsicError> {
    Ok(Value::string(JsString::from_str(&value, args.gc_heap)?))
}

// ── Constructor helpers ──────────────────────────────────────────
//
// Shared between each `Temporal.<Class>` `[[Construct]]` body
// (`instant::construct`, `duration::construct`, …). The naming
// mirrors the ECMA-262 / proposal-temporal abstract operations.

/// Reject a `Foo(...)` call form on a class constructor that must
/// be invoked as `new Foo(...)`. Per ECMA-262, each Temporal class
/// constructor throws `TypeError` if `NewTarget` is undefined.
pub fn require_construct(ctx: &NativeCtx<'_>, class: &'static str) -> Result<(), NativeError> {
    if ctx.is_construct_call() {
        Ok(())
    } else {
        Err(NativeError::TypeError {
            name: class,
            reason: format!("{class} constructor must be invoked with `new`"),
        })
    }
}

/// §7.1.6 `ToIntegerWithTruncation(value)` — `ToNumber`, then reject
/// `NaN`/`±∞` with `RangeError`, then truncate toward zero. `Symbol`
/// / `BigInt` inputs surface as `TypeError` per §7.1.4.
pub fn to_integer_with_truncation(
    value: &Value,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    if value.is_symbol() {
        return Err(NativeError::TypeError {
            name: class,
            reason: format!("{field}: cannot convert a Symbol to a Number"),
        });
    }
    if value.is_big_int() {
        return Err(NativeError::TypeError {
            name: class,
            reason: format!("{field}: cannot convert a BigInt to a Number"),
        });
    }
    let n = crate::number::parse::to_number_value(value, heap);
    if n.is_nan() || n.is_infinite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be a finite integer"),
        });
    }
    Ok(n.trunc())
}

/// Temporal-specific `ToIntegerIfIntegral(value)` — like
/// [`to_integer_with_truncation`] but also rejects non-integral
/// finite numbers with `RangeError`.
pub fn to_integer_if_integral(
    value: &Value,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let n = to_integer_with_truncation(value, heap, class, field)?;
    let raw = crate::number::parse::to_number_value(value, heap);
    if (raw - n).abs() > 0.0 {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be an integer"),
        });
    }
    Ok(n)
}

/// Read positional arg `index`, defaulting to `undefined`.
#[must_use]
pub fn arg_or_undef(args: &[Value], index: usize) -> Value {
    args.get(index).copied().unwrap_or(Value::undefined())
}

/// Coerce an optional numeric arg (defaulting to `0` when missing
/// or `undefined`) via [`to_integer_with_truncation`].
pub fn opt_integer_with_truncation(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(0.0);
    }
    to_integer_with_truncation(&v, heap, class, field)
}

/// Coerce an optional numeric arg (defaulting to `0` when missing
/// or `undefined`) via [`to_integer_if_integral`].
pub fn opt_integer_if_integral(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(0.0);
    }
    to_integer_if_integral(&v, heap, class, field)
}

/// Resolve a calendar argument to [`temporal_rs::Calendar`]. Missing
/// or `undefined` defaults to ISO 8601. Strings are parsed; any other
/// value surfaces as `TypeError`.
pub fn arg_to_calendar(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::Calendar, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(temporal_rs::Calendar::default());
    }
    let Some(js) = v.as_string(heap) else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "calendar argument must be a string".to_string(),
        });
    };
    let s = js.to_lossy_string(heap);
    temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(|e| NativeError::RangeError {
        name: class,
        reason: format!("invalid calendar identifier: {e}"),
    })
}

/// Clamp a coerced numeric component to `u8`, surfacing
/// `RangeError` when the value falls outside `0..=255`. Per-field
/// bounds (`isoMonth ∈ 1..=12`, etc.) are then enforced by
/// `temporal_rs`.
pub fn clamp_to_u8(n: f64, class: &'static str, field: &str) -> Result<u8, NativeError> {
    if !(0.0..=255.0).contains(&n) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field} out of range"),
        });
    }
    Ok(n as u8)
}

/// `u16` companion to [`clamp_to_u8`].
pub fn clamp_to_u16(n: f64, class: &'static str, field: &str) -> Result<u16, NativeError> {
    if !(0.0..=65_535.0).contains(&n) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field} out of range"),
        });
    }
    Ok(n as u16)
}

/// Parse a `{ largestUnit, smallestUnit, roundingMode,
/// roundingIncrement }` options bag into [`DifferenceSettings`].
/// Returns the default when the argument is missing or `undefined`.
/// Any unrecognised unit / rounding mode surfaces as
/// `IntrinsicError::BadArgument`.
pub fn parse_difference_settings(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::options::DifferenceSettings, IntrinsicError> {
    use core::str::FromStr;
    let mut settings = temporal_rs::options::DifferenceSettings::default();
    let v = args.args.get(index as usize).copied().unwrap_or_default();
    if v.is_undefined() {
        return Ok(settings);
    }
    let Some(obj) = v.as_object() else {
        return Err(IntrinsicError::BadArgument {
            index,
            reason: "options must be an object",
        });
    };
    let heap = &*args.gc_heap;
    if let Some(name) = read_string_field(obj, "largestUnit", heap)
        && !name.is_empty()
        && !name.eq_ignore_ascii_case("auto")
    {
        let unit = temporal_rs::options::Unit::from_str(&name).map_err(|_| {
            IntrinsicError::OutOfRange {
                index,
                reason: "invalid `largestUnit`",
            }
        })?;
        settings.largest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "smallestUnit", heap) {
        let unit = temporal_rs::options::Unit::from_str(&name).map_err(|_| {
            IntrinsicError::OutOfRange {
                index,
                reason: "invalid `smallestUnit`",
            }
        })?;
        settings.smallest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "roundingMode", heap) {
        let mode = temporal_rs::options::RoundingMode::from_str(&name).map_err(|_| {
            IntrinsicError::OutOfRange {
                index,
                reason: "invalid `roundingMode`",
            }
        })?;
        settings.rounding_mode = Some(mode);
    }
    if let Some(n) = crate::object::get(obj, heap, "roundingIncrement")
        && !n.is_undefined()
        && let Some(num) = n.as_number()
    {
        let raw = num.as_f64();
        if raw.is_finite() && raw >= 1.0 {
            if let Ok(incr) = temporal_rs::options::RoundingIncrement::try_from(raw.trunc()) {
                settings.increment = Some(incr);
            } else {
                return Err(IntrinsicError::OutOfRange {
                    index,
                    reason: "invalid `roundingIncrement`",
                });
            }
        }
    }
    Ok(settings)
}

/// Parse a `{ largestUnit, smallestUnit, roundingMode,
/// roundingIncrement }` options bag into [`RoundingOptions`]. Mirror
/// of [`parse_difference_settings`] for `prototype.round`.
pub fn parse_rounding_options(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::options::RoundingOptions, IntrinsicError> {
    use core::str::FromStr;
    let mut options = temporal_rs::options::RoundingOptions::default();
    let v = args.args.get(index as usize).copied().unwrap_or_default();
    if let Some(s) = v.as_string(args.gc_heap) {
        // `round("hour")` is shorthand for `round({ smallestUnit: "hour" })`.
        let name = s.to_lossy_string(args.gc_heap);
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| IntrinsicError::OutOfRange {
                index,
                reason: "invalid smallest-unit shorthand",
            })?;
        options.smallest_unit = Some(unit);
        return Ok(options);
    }
    if v.is_undefined() {
        return Ok(options);
    }
    let Some(obj) = v.as_object() else {
        return Err(IntrinsicError::BadArgument {
            index,
            reason: "round() requires an options object or smallest-unit string",
        });
    };
    let heap = &*args.gc_heap;
    if let Some(name) = read_string_field(obj, "largestUnit", heap) {
        let unit = temporal_rs::options::Unit::from_str(&name).map_err(|_| {
            IntrinsicError::OutOfRange {
                index,
                reason: "invalid `largestUnit`",
            }
        })?;
        options.largest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "smallestUnit", heap) {
        let unit = temporal_rs::options::Unit::from_str(&name).map_err(|_| {
            IntrinsicError::OutOfRange {
                index,
                reason: "invalid `smallestUnit`",
            }
        })?;
        options.smallest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "roundingMode", heap) {
        let mode = temporal_rs::options::RoundingMode::from_str(&name).map_err(|_| {
            IntrinsicError::OutOfRange {
                index,
                reason: "invalid `roundingMode`",
            }
        })?;
        options.rounding_mode = Some(mode);
    }
    if let Some(n) = crate::object::get(obj, heap, "roundingIncrement")
        && !n.is_undefined()
        && let Some(num) = n.as_number()
    {
        let raw = num.as_f64();
        if raw.is_finite() && raw >= 1.0 {
            if let Ok(incr) = temporal_rs::options::RoundingIncrement::try_from(raw.trunc()) {
                options.increment = Some(incr);
            } else {
                return Err(IntrinsicError::OutOfRange {
                    index,
                    reason: "invalid `roundingIncrement`",
                });
            }
        }
    }
    Ok(options)
}

/// Read an `Option<integer>` field from a `PartialTime` /
/// `CalendarFields` / `DateTimeFields` partial-record object.
/// Returns `Ok(None)` when the property is missing or `undefined`.
/// Non-numeric values surface as `IntrinsicError::BadArgument`.
fn read_partial_integer(
    obj: JsObject,
    name: &str,
    heap: &otter_gc::GcHeap,
) -> Result<Option<i64>, IntrinsicError> {
    let Some(v) = crate::object::get(obj, heap, name) else {
        return Ok(None);
    };
    if v.is_undefined() {
        return Ok(None);
    }
    let Some(n) = v.as_number() else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "partial-record fields must be numbers",
        });
    };
    let raw = n.as_f64();
    if !raw.is_finite() {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "partial-record field must be finite",
        });
    }
    if (raw - raw.trunc()).abs() > 0.0 {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "partial-record field must be an integer",
        });
    }
    Ok(Some(raw.trunc() as i64))
}

/// Parse a `{ hour, minute, second, millisecond, microsecond,
/// nanosecond }` partial-record into [`temporal_rs::PartialTime`].
pub fn parse_partial_time(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::partial::PartialTime, IntrinsicError> {
    let mut t = temporal_rs::partial::PartialTime::default();
    if let Some(v) = read_partial_integer(obj, "hour", heap)? {
        t.hour = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "minute", heap)? {
        t.minute = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "second", heap)? {
        t.second = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "millisecond", heap)? {
        t.millisecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(obj, "microsecond", heap)? {
        t.microsecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(obj, "nanosecond", heap)? {
        t.nanosecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    Ok(t)
}

/// Parse a `{ year, month, day }` partial-record into
/// [`temporal_rs::fields::CalendarFields`]. Bare-bones —
/// covers the foundation slice (no era / month_code surface yet).
pub fn parse_calendar_fields(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::fields::CalendarFields, IntrinsicError> {
    let mut f = temporal_rs::fields::CalendarFields::default();
    if let Some(v) = read_partial_integer(obj, "year", heap)? {
        f.year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    if let Some(v) = read_partial_integer(obj, "month", heap)? {
        f.month = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "day", heap)? {
        f.day = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    Ok(f)
}

/// Parse a `{ year, month, day, hour, minute, second, ms, us, ns }`
/// partial-record into [`temporal_rs::fields::DateTimeFields`].
pub fn parse_date_time_fields(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::fields::DateTimeFields, IntrinsicError> {
    Ok(temporal_rs::fields::DateTimeFields {
        calendar_fields: parse_calendar_fields(obj, heap)?,
        time: parse_partial_time(obj, heap)?,
    })
}

/// Convert the dispatch-level `TemporalError` into a `NativeError`
/// — used by `[[Construct]]` bodies to surface OOM and any
/// future pass-through cases via the native-fn boundary.
pub fn temporal_dispatch_err(err: TemporalError) -> NativeError {
    match err {
        TemporalError::OutOfMemory { .. } => NativeError::TypeError {
            name: "Temporal",
            reason: "out of memory".to_string(),
        },
        other => NativeError::TypeError {
            name: "Temporal",
            reason: other.to_string(),
        },
    }
}

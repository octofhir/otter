//! Shared coercion / extraction helpers for `Temporal.*` native
//! function bodies.
//!
//! Every helper takes a [`NativeCtx`] / `&[Value]` pair so the
//! per-class `couch!` blocks can call algorithms with the same
//! signature the macro itself uses (no bridge layer).

#![allow(missing_docs)]

use crate::object::JsObject;
use crate::string::JsString;
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

#[must_use]
pub fn arg_or_undef(args: &[Value], index: usize) -> Value {
    args.get(index).copied().unwrap_or(Value::undefined())
}

pub fn make_temporal(
    ctx: &mut NativeCtx<'_>,
    payload: TemporalPayload,
) -> Result<Value, NativeError> {
    let handle = JsTemporal::new(ctx.heap_mut(), payload).map_err(|_| NativeError::TypeError {
        name: "Temporal",
        reason: "out of memory".to_string(),
    })?;
    Ok(Value::temporal(handle))
}

pub fn js_string_value(value: String, ctx: &mut NativeCtx<'_>) -> Result<Value, NativeError> {
    let s = JsString::from_str(&value, ctx.heap_mut()).map_err(|_| NativeError::TypeError {
        name: "Temporal",
        reason: "out of memory".to_string(),
    })?;
    Ok(Value::string(s))
}

/// Build a string [`Value`] from `s` for `load_property` getters,
/// falling back to `undefined` if the GC string allocation fails.
pub fn str_or_undef(s: &str, heap: &mut otter_gc::GcHeap) -> Value {
    match JsString::from_str(s, heap) {
        Ok(js) => Value::string(js),
        Err(_) => Value::undefined(),
    }
}

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

/// §7.1.6 `ToIntegerWithTruncation` — `ToNumber`, reject `NaN`/`±∞`
/// with `RangeError`, truncate toward zero.
/// §7.1.4 ToNumber on a Temporal field, running ToPrimitive(number)
/// so a user `valueOf` / `@@toPrimitive` fires observably (a Symbol /
/// BigInt operand is a TypeError). Heap-only paths that cannot reach
/// user code fall back to the non-observing reader.
pub fn to_number_field(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
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
    if value.is_object_type() {
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: class,
                reason: "missing execution context".to_string(),
            })?;
        let n = ctx
            .cx
            .interp
            .number_for_number_ctor(&exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, class))?;
        return Ok(n.as_f64());
    }
    Ok(crate::number::parse::to_number_value(value, ctx.heap()))
}

pub fn to_integer_with_truncation(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let n = to_number_field(ctx, value, class, field)?;
    if n.is_nan() || n.is_infinite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be a finite integer"),
        });
    }
    Ok(n.trunc())
}

pub fn to_integer_if_integral(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let raw = to_number_field(ctx, value, class, field)?;
    if raw.is_nan() || raw.is_infinite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be a finite integer"),
        });
    }
    if (raw - raw.trunc()).abs() > 0.0 {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be an integer"),
        });
    }
    Ok(raw.trunc())
}

pub fn opt_integer_with_truncation(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(0.0);
    }
    to_integer_with_truncation(ctx, &v, class, field)
}

/// §GetOptionsObject — `undefined` yields `None` (use defaults); any
/// object (including a callable / array) is valid and its plain
/// object-bag, when present, is returned for field reads; a non-object
/// primitive is a TypeError. Callables expose no plain bag, so option
/// fields read as absent (defaults), matching the spec for the
/// no-fields case.
/// §GetOption(options, "overflow", "string", « "constrain", "reject" »,
/// "constrain") — read and validate the `overflow` option. The value
/// is coerced with ToString (observable; a Symbol throws TypeError),
/// then matched against the enum (an out-of-list value is a
/// RangeError). Absent options / overflow → `None` (temporal_rs
/// defaults to constrain).
/// §GetOption(options, name, "string", …) — read an option field and
/// coerce it with ToString (observable; a Symbol throws TypeError),
/// returning `None` for an absent / undefined field. The caller then
/// matches the string against the option's allowed values (an
/// out-of-list value is the caller's RangeError).
/// §GetRoundingIncrementOption — read `roundingIncrement`, ToNumber
/// the value observably, and require a finite integer in
/// `[1, 1e9]`. Absent / undefined → `None` (default 1). A non-integer
/// / NaN / Infinity / out-of-range value is a RangeError; a Symbol
/// (via ToNumber) is a TypeError.
pub fn read_rounding_increment(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    class: &'static str,
) -> Result<Option<temporal_rs::options::RoundingIncrement>, NativeError> {
    let field = get_option_value(ctx, target, "roundingIncrement", class)?;
    if field.is_undefined() {
        return Ok(None);
    }
    let raw = to_number_field(ctx, &field, class, "roundingIncrement")?;
    // §ToTemporalRoundingIncrement steps 2-4 — a non-finite value is
    // a RangeError; the integer increment is truncate(value) and must
    // land in [1, 1e9].
    if !raw.is_finite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: "roundingIncrement must be finite".to_string(),
        });
    }
    let integer = raw.trunc();
    if !(1.0..=1_000_000_000.0).contains(&integer) {
        return Err(NativeError::RangeError {
            name: class,
            reason: "roundingIncrement out of range [1, 1e9]".to_string(),
        });
    }
    temporal_rs::options::RoundingIncrement::try_from(integer)
        .map(Some)
        .map_err(|_| NativeError::RangeError {
            name: class,
            reason: "invalid roundingIncrement".to_string(),
        })
}

pub fn read_option_string(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    name: &str,
    class: &'static str,
) -> Result<Option<String>, NativeError> {
    let field = get_option_value(ctx, target, name, class)?;
    if field.is_undefined() {
        return Ok(None);
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: class,
            reason: "missing execution context".to_string(),
        })?;
    ctx.cx
        .interp
        .coerce_to_string(&exec, &field)
        .map(Some)
        .map_err(|e| crate::native_function::vm_to_native_error(e, class))
}

/// Read a `monthCode` field. §CalendarFieldsToISO requires a String
/// value: an object is coerced with ToPrimitive(string) (firing
/// `toString`) and the result must itself be a String, otherwise a
/// TypeError; any other non-string primitive is a TypeError. A
/// well-formed-but-invalid string is a RangeError.
fn read_month_code(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    class: &'static str,
) -> Result<Option<temporal_rs::MonthCode>, NativeError> {
    let field = get_option_value(ctx, target, "monthCode", class)?;
    if field.is_undefined() {
        return Ok(None);
    }
    let prim = if field.is_object_type() {
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: class,
                reason: "missing execution context".to_string(),
            })?;
        ctx.cx
            .interp
            .to_primitive_string_hint_sync(&exec, field)
            .map_err(|e| crate::native_function::vm_to_native_error(e, class))?
    } else {
        field
    };
    let Some(s) = prim.as_string(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "monthCode must be a string".to_string(),
        });
    };
    let code = temporal_rs::MonthCode::try_from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
        .map_err(|_| NativeError::RangeError {
            name: class,
            reason: "monthCode is not well-formed".to_string(),
        })?;
    Ok(Some(code))
}

pub fn parse_overflow(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
) -> Result<Option<temporal_rs::options::Overflow>, NativeError> {
    use core::str::FromStr;
    let v = arg_or_undef(args, index);
    // GetOptionsObject: undefined → no options; a non-object is a
    // TypeError. We check the type directly (not `options_object`,
    // whose `as_object()` collapse drops Proxy option bags) so an
    // observable Proxy options object still routes through the
    // getter-firing [[Get]] in `get_option_value`.
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: "Temporal",
            reason: "options must be an object or undefined".to_string(),
        });
    }
    let field = get_option_value(ctx, v, "overflow", "Temporal")?;
    if field.is_undefined() {
        return Ok(None);
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Temporal",
            reason: "missing execution context".to_string(),
        })?;
    let s = ctx
        .cx
        .interp
        .coerce_to_string(&exec, &field)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "Temporal"))?;
    temporal_rs::options::Overflow::from_str(&s)
        .map(Some)
        .map_err(|_| NativeError::RangeError {
            name: "Temporal",
            reason: format!("invalid overflow option: {s:?}"),
        })
}

pub fn options_object(v: &Value, class: &'static str) -> Result<Option<JsObject>, NativeError> {
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    Ok(v.as_object())
}

/// Perform a spec [[Get]] (`options.<name>`) that walks the prototype
/// chain and **fires an accessor getter** with `options` as the
/// receiver. The raw `object::get` used elsewhere returns `undefined`
/// for an accessor slot without invoking the getter, so option bags
/// exposing observable getters (the `propertyBagObserver` Test262
/// pattern) were silently read as absent. `options` must be the
/// options object value itself.
pub fn get_option_value(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
    class: &'static str,
) -> Result<Value, NativeError> {
    use crate::native_function::vm_to_native_error;
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: class,
            reason: "missing execution context".to_string(),
        })?;
    let key = crate::VmPropertyKey::String(name);
    let outcome = ctx
        .cx
        .interp
        .ordinary_get_value(&exec, options, options, &key, 0)
        .map_err(|e| vm_to_native_error(e, class))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => ctx
            .cx
            .interp
            .run_callable_sync(&exec, &getter, options, smallvec::SmallVec::new())
            .map_err(|e| vm_to_native_error(e, class)),
    }
}

pub fn opt_integer_if_integral(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(0.0);
    }
    to_integer_if_integral(ctx, &v, class, field)
}

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
    // The constructor's calendar argument is a bare calendar identifier
    // (not ParseTemporalCalendarString) — an ISO date string is invalid
    // here, so `try_from_utf8` rather than `FromStr`.
    temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(|e| NativeError::RangeError {
        name: class,
        reason: format!("invalid calendar identifier: {e}"),
    })
}

pub fn clamp_to_u8(n: f64, class: &'static str, field: &str) -> Result<u8, NativeError> {
    if !(0.0..=255.0).contains(&n) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field} out of range"),
        });
    }
    Ok(n as u8)
}

pub fn clamp_to_u16(n: f64, class: &'static str, field: &str) -> Result<u16, NativeError> {
    if !(0.0..=65_535.0).contains(&n) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field} out of range"),
        });
    }
    Ok(n as u16)
}

/// Convert a [`temporal_rs::TemporalError`] into a [`NativeError`]
/// honouring the spec error class: `Range` → `RangeError`, `Type` →
/// `TypeError`, `Syntax` → `SyntaxError`. Other engine variants fall
/// through as `TypeError`.
pub fn temporal_err(err: temporal_rs::TemporalError, class: &'static str) -> NativeError {
    use temporal_rs::error::ErrorKind;
    let reason = err.to_string();
    match err.kind() {
        ErrorKind::Range => NativeError::RangeError {
            name: class,
            reason,
        },
        ErrorKind::Type => NativeError::TypeError {
            name: class,
            reason,
        },
        ErrorKind::Syntax => NativeError::SyntaxError {
            name: class,
            reason,
        },
        _ => NativeError::TypeError {
            name: class,
            reason,
        },
    }
}

// ── Receiver extractors ──────────────────────────────────────────

fn require_payload<F, T>(
    ctx: &NativeCtx<'_>,
    expected: &'static str,
    extract: F,
) -> Result<T, NativeError>
where
    F: FnOnce(TemporalPayload) -> Option<T>,
{
    let recv = *ctx.this_value();
    let t = recv
        .as_temporal(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: expected,
            reason: format!("receiver must be a {expected}"),
        })?;
    extract(t.payload_clone(ctx.heap())).ok_or_else(|| NativeError::TypeError {
        name: expected,
        reason: format!("receiver must be a {expected}"),
    })
}

pub fn require_instant(ctx: &NativeCtx<'_>) -> Result<temporal_rs::Instant, NativeError> {
    require_payload(ctx, "Temporal.Instant", |p| match p {
        TemporalPayload::Instant(v) => Some(v),
        _ => None,
    })
}

pub fn require_duration(ctx: &NativeCtx<'_>) -> Result<temporal_rs::Duration, NativeError> {
    require_payload(ctx, "Temporal.Duration", |p| match p {
        TemporalPayload::Duration(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_date(ctx: &NativeCtx<'_>) -> Result<temporal_rs::PlainDate, NativeError> {
    require_payload(ctx, "Temporal.PlainDate", |p| match p {
        TemporalPayload::PlainDate(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_time(ctx: &NativeCtx<'_>) -> Result<temporal_rs::PlainTime, NativeError> {
    require_payload(ctx, "Temporal.PlainTime", |p| match p {
        TemporalPayload::PlainTime(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_date_time(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::PlainDateTime, NativeError> {
    require_payload(ctx, "Temporal.PlainDateTime", |p| match p {
        TemporalPayload::PlainDateTime(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_year_month(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::PlainYearMonth, NativeError> {
    require_payload(ctx, "Temporal.PlainYearMonth", |p| match p {
        TemporalPayload::PlainYearMonth(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_month_day(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::PlainMonthDay, NativeError> {
    require_payload(ctx, "Temporal.PlainMonthDay", |p| match p {
        TemporalPayload::PlainMonthDay(v) => Some(v),
        _ => None,
    })
}

pub fn require_zoned_date_time(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::ZonedDateTime, NativeError> {
    require_payload(ctx, "Temporal.ZonedDateTime", |p| match p {
        TemporalPayload::ZonedDateTime(v) => Some(v),
        _ => None,
    })
}

// ── Options bag parsers ──────────────────────────────────────────

/// Parse the `disambiguation` option (`"compatible"`/`"earlier"`/
/// `"later"`/`"reject"`) from an options argument, defaulting to
/// `Compatible`.
pub fn parse_disambiguation(
    args: &[Value],
    index: usize,
    ctx: &mut NativeCtx<'_>,
    class: &'static str,
) -> Result<temporal_rs::options::Disambiguation, NativeError> {
    use core::str::FromStr;
    let v = arg_or_undef(args, index);
    let Some(obj) = options_object(&v, class)? else {
        return Ok(temporal_rs::options::Disambiguation::Compatible);
    };
    match read_option_string(ctx, Value::object(obj), "disambiguation", class)? {
        Some(name) => temporal_rs::options::Disambiguation::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `disambiguation`".to_string(),
            }
        }),
        None => Ok(temporal_rs::options::Disambiguation::Compatible),
    }
}

/// Resolve a time-zone argument: a string identifier (e.g.
/// `"UTC"`, `"+05:00"`, `"America/New_York"`) or a
/// `Temporal.ZonedDateTime` whose own time zone is reused.
pub fn parse_time_zone(
    v: &Value,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::TimeZone, NativeError> {
    if let Some(t) = v.as_temporal(heap)
        && let TemporalPayload::ZonedDateTime(zdt) = t.payload_clone(heap)
    {
        return Ok(*zdt.time_zone());
    }
    if let Some(s) = v.as_string(heap) {
        return temporal_rs::TimeZone::try_from_str(&s.to_lossy_string(heap))
            .map_err(|e| temporal_err(e, class));
    }
    Err(NativeError::TypeError {
        name: class,
        reason: "time zone must be a string identifier or a Temporal.ZonedDateTime".to_string(),
    })
}

/// Parse the rounding options (`smallestUnit`, `roundingMode`,
/// `fractionalSecondDigits`) from a time-bearing Temporal `toString`
/// options argument into a
/// [`temporal_rs::options::ToStringRoundingOptions`]. Absent options
/// keep `Precision::Auto` / no unit / default mode.
pub fn parse_to_string_rounding_options(
    args: &[Value],
    index: usize,
    ctx: &mut NativeCtx<'_>,
    class: &'static str,
) -> Result<temporal_rs::options::ToStringRoundingOptions, NativeError> {
    use core::str::FromStr;
    let mut opts = temporal_rs::options::ToStringRoundingOptions::default();
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(opts);
    }
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    // §ToSecondsStringPrecisionRecord / GetRoundingModeOption read the
    // keys in the order fractionalSecondDigits, roundingMode,
    // smallestUnit through getter/Proxy-aware [[Get]]s.
    let frac = get_option_value(ctx, v, "fractionalSecondDigits", class)?;
    if !frac.is_undefined() {
        // GetTemporalFractionalSecondDigitsOption: "auto" or an integer
        // 0-9 (a non-"auto" value is coerced via ToNumber, firing
        // valueOf, then floored and range-checked).
        if let Some(s) = frac.as_string(ctx.heap()) {
            if s.to_lossy_string(ctx.heap()) == "auto" {
                opts.precision = temporal_rs::parsers::Precision::Auto;
            } else {
                return Err(NativeError::RangeError {
                    name: class,
                    reason: "`fractionalSecondDigits` must be \"auto\" or an integer 0-9"
                        .to_string(),
                });
            }
        } else {
            let raw = to_number_field(ctx, &frac, class, "fractionalSecondDigits")?;
            if raw.is_nan() {
                return Err(NativeError::RangeError {
                    name: class,
                    reason: "`fractionalSecondDigits` must be \"auto\" or an integer 0-9"
                        .to_string(),
                });
            }
            let d = raw.floor();
            if !(0.0..=9.0).contains(&d) {
                return Err(NativeError::RangeError {
                    name: class,
                    reason: "`fractionalSecondDigits` must be an integer 0-9".to_string(),
                });
            }
            opts.precision = temporal_rs::parsers::Precision::Digit(d as u8);
        }
    }
    if let Some(name) = read_option_string(ctx, v, "roundingMode", class)? {
        opts.rounding_mode = Some(temporal_rs::options::RoundingMode::from_str(&name).map_err(
            |_| NativeError::RangeError {
                name: class,
                reason: "invalid `roundingMode`".to_string(),
            },
        )?);
    }
    if let Some(name) = read_option_string(ctx, v, "smallestUnit", class)? {
        opts.smallest_unit = Some(temporal_rs::options::Unit::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `smallestUnit`".to_string(),
            }
        })?);
    }
    Ok(opts)
}

/// Parse the `calendarName` option (`"auto"`/`"always"`/`"never"`/
/// `"critical"`) from a Temporal `toString` options argument into a
/// [`temporal_rs::options::DisplayCalendar`]. Absent options or an
/// absent `calendarName` default to `Auto`.
pub fn parse_display_calendar(
    args: &[Value],
    index: usize,
    ctx: &mut NativeCtx<'_>,
    class: &'static str,
) -> Result<temporal_rs::options::DisplayCalendar, NativeError> {
    use core::str::FromStr;
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(temporal_rs::options::DisplayCalendar::Auto);
    }
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    match read_option_string(ctx, v, "calendarName", class)? {
        Some(name) => temporal_rs::options::DisplayCalendar::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `calendarName`".to_string(),
            }
        }),
        None => Ok(temporal_rs::options::DisplayCalendar::Auto),
    }
}

fn read_partial_integer(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    name: &str,
    class: &'static str,
) -> Result<Option<i64>, NativeError> {
    // Read through a spec [[Get]] so accessor getters, Proxy traps, and
    // a Temporal instance's own virtual field accessors all fire — a
    // raw `object::get` would miss every one of them.
    let v = get_option_value(ctx, target, name, class)?;
    if v.is_undefined() {
        return Ok(None);
    }
    // §ToIntegerWithTruncation over the field, observing valueOf: a
    // non-finite value is a RangeError, otherwise the value is
    // truncated toward zero (a fractional field is NOT rejected).
    let raw = to_number_field(ctx, &v, class, name)?;
    if !raw.is_finite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{name}: partial-record field must be finite"),
        });
    }
    Ok(Some(raw.trunc() as i64))
}

pub fn parse_difference_settings(
    args: &[Value],
    index: usize,
    ctx: &mut NativeCtx<'_>,
    class: &'static str,
) -> Result<temporal_rs::options::DifferenceSettings, NativeError> {
    use core::str::FromStr;
    let mut settings = temporal_rs::options::DifferenceSettings::default();
    let v = arg_or_undef(args, index);
    // §GetOptionsObject: undefined → defaults; a non-object is a
    // TypeError; a Proxy options bag is accepted (is_object_type) so its
    // observable getters fire through `get_option_value`.
    if v.is_undefined() {
        return Ok(settings);
    }
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    // §GetDifferenceSettings reads the options in the fixed order
    // largestUnit, roundingIncrement, roundingMode, smallestUnit; the
    // cross-option (smallestUnit > largestUnit) validation is performed
    // by temporal_rs afterwards.
    if let Some(name) = read_option_string(ctx, v, "largestUnit", class)?
        && !name.is_empty()
        && !name.eq_ignore_ascii_case("auto")
    {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `largestUnit`".to_string(),
            })?;
        settings.largest_unit = Some(unit);
    }
    if let Some(incr) = read_rounding_increment(ctx, v, class)? {
        settings.increment = Some(incr);
    }
    if let Some(name) = read_option_string(ctx, v, "roundingMode", class)? {
        let mode = temporal_rs::options::RoundingMode::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `roundingMode`".to_string(),
            }
        })?;
        settings.rounding_mode = Some(mode);
    }
    if let Some(name) = read_option_string(ctx, v, "smallestUnit", class)? {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `smallestUnit`".to_string(),
            })?;
        settings.smallest_unit = Some(unit);
    }
    Ok(settings)
}

pub fn parse_rounding_options(
    args: &[Value],
    index: usize,
    ctx: &mut NativeCtx<'_>,
    class: &'static str,
) -> Result<temporal_rs::options::RoundingOptions, NativeError> {
    use core::str::FromStr;
    let mut options = temporal_rs::options::RoundingOptions::default();
    let v = arg_or_undef(args, index);
    if let Some(s) = v.as_string(ctx.heap()) {
        let name = s.to_lossy_string(ctx.heap());
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid smallest-unit shorthand".to_string(),
            })?;
        options.smallest_unit = Some(unit);
        return Ok(options);
    }
    // round() requires the `roundTo`/options argument — `undefined` (a
    // missing argument) is a TypeError, not a defaulted options object.
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "round() requires an options object or smallest-unit string".to_string(),
        });
    }
    // Fixed option read order (largestUnit, roundingIncrement,
    // roundingMode, smallestUnit); cross-option validation runs in
    // temporal_rs afterwards. Reads fire through `get_option_value` so a
    // Proxy options bag is observed.
    if let Some(name) = read_option_string(ctx, v, "largestUnit", class)? {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `largestUnit`".to_string(),
            })?;
        options.largest_unit = Some(unit);
    }
    if let Some(incr) = read_rounding_increment(ctx, v, class)? {
        options.increment = Some(incr);
    }
    if let Some(name) = read_option_string(ctx, v, "roundingMode", class)? {
        let mode = temporal_rs::options::RoundingMode::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `roundingMode`".to_string(),
            }
        })?;
        options.rounding_mode = Some(mode);
    }
    if let Some(name) = read_option_string(ctx, v, "smallestUnit", class)? {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `smallestUnit`".to_string(),
            })?;
        options.smallest_unit = Some(unit);
    }
    Ok(options)
}

pub fn parse_partial_time(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    class: &'static str,
) -> Result<temporal_rs::partial::PartialTime, NativeError> {
    let mut t = temporal_rs::partial::PartialTime::default();
    if let Some(v) = read_partial_integer(ctx, target, "hour", class)? {
        t.hour = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(ctx, target, "microsecond", class)? {
        t.microsecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(ctx, target, "millisecond", class)? {
        t.millisecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(ctx, target, "minute", class)? {
        t.minute = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(ctx, target, "nanosecond", class)? {
        t.nanosecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(ctx, target, "second", class)? {
        t.second = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    Ok(t)
}

/// §ToTemporalCalendarIdentifier — read the `calendar` property of a
/// fields object and validate it through temporal_rs. Absent /
/// undefined → ISO8601 default; a string → validated `Calendar`
/// (bad / empty / mixed-case-non-ASCII identifiers throw RangeError);
/// any non-string, non-undefined value (null, number, …) is a
/// TypeError.
pub fn read_calendar_field(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    class: &'static str,
) -> Result<temporal_rs::Calendar, NativeError> {
    // Read `calendar` through a spec [[Get]] so accessor getters and
    // Proxy traps fire on a property bag.
    let v = get_option_value(ctx, target, "calendar", class)?;
    if v.is_undefined() {
        return Ok(temporal_rs::Calendar::default());
    }
    let heap = ctx.heap();
    if let Some(t) = v.as_temporal(heap) {
        // A Temporal instance with a [[Calendar]] slot contributes it
        // directly. A calendar-less Temporal type (Duration, Instant)
        // is not a valid calendar value — §ToTemporalCalendarSlotValue
        // throws a TypeError rather than falling back to ISO8601.
        return match t.payload_clone(heap) {
            TemporalPayload::PlainDate(d) => Ok(d.calendar().clone()),
            TemporalPayload::PlainDateTime(d) => Ok(d.calendar().clone()),
            TemporalPayload::PlainYearMonth(d) => Ok(d.calendar().clone()),
            TemporalPayload::PlainMonthDay(d) => Ok(d.calendar().clone()),
            TemporalPayload::ZonedDateTime(d) => Ok(d.calendar().clone()),
            _ => Err(NativeError::TypeError {
                name: class,
                reason: "calendar-less Temporal object is not a valid calendar".to_string(),
            }),
        };
    }
    let Some(s) = v.as_string(heap) else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "calendar must be a string or a calendar-bearing Temporal object".to_string(),
        });
    };
    // §13.34 ParseTemporalCalendarString — a bare identifier or an
    // ISO date/time string carrying a `[u-ca=...]` annotation (an
    // un-annotated ISO string yields the ISO8601 calendar).
    let id = s.to_lossy_string(heap);
    use core::str::FromStr;
    temporal_rs::Calendar::from_str(&id).map_err(|_| NativeError::RangeError {
        name: class,
        reason: format!("invalid calendar identifier: {id:?}"),
    })
}

pub fn parse_calendar_fields(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    calendar: &temporal_rs::Calendar,
    class: &'static str,
) -> Result<temporal_rs::fields::CalendarFields, NativeError> {
    // §PrepareCalendarFields reads the calendar's field keys in
    // alphabetical order, each through a spec [[Get]] so accessor
    // getters / Proxy traps / Temporal-instance accessors all fire. The
    // ISO calendar has no `era`/`eraYear` keys, so they are read only
    // for era-bearing calendars.
    let has_eras = !calendar.is_iso();
    let mut f = temporal_rs::fields::CalendarFields::default();
    if let Some(v) = read_partial_integer(ctx, target, "day", class)? {
        if v < 1 {
            return Err(NativeError::RangeError {
                name: class,
                reason: "day must be a positive integer".to_string(),
            });
        }
        f.day = Some(v.min(u8::MAX as i64) as u8);
    }
    if has_eras {
        if let Some(s) = read_option_string(ctx, target, "era", class)? {
            let era = temporal_rs::TinyAsciiStr::<19>::try_from_str(&s).map_err(|_| {
                NativeError::RangeError {
                    name: class,
                    reason: "invalid era".to_string(),
                }
            })?;
            f.era = Some(era);
        }
        if let Some(v) = read_partial_integer(ctx, target, "eraYear", class)? {
            f.era_year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        }
    }
    if let Some(v) = read_partial_integer(ctx, target, "month", class)? {
        if v < 1 {
            return Err(NativeError::RangeError {
                name: class,
                reason: "month must be a positive integer".to_string(),
            });
        }
        f.month = Some(v.min(u8::MAX as i64) as u8);
    }
    if let Some(code) = read_month_code(ctx, target, class)? {
        f.month_code = Some(code);
    }
    if let Some(v) = read_partial_integer(ctx, target, "year", class)? {
        f.year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    Ok(f)
}

pub fn parse_date_time_fields(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    calendar: &temporal_rs::Calendar,
    class: &'static str,
) -> Result<temporal_rs::fields::DateTimeFields, NativeError> {
    // §PrepareCalendarFields for a date-time bag reads the calendar AND
    // time keys in a single alphabetical pass (day, era, eraYear, hour,
    // microsecond, millisecond, minute, month, monthCode, nanosecond,
    // second, year), each through a getter/Proxy-aware [[Get]]. The ISO
    // calendar has no era keys.
    let has_eras = !calendar.is_iso();
    let mut cf = temporal_rs::fields::CalendarFields::default();
    let mut time = temporal_rs::partial::PartialTime::default();
    if let Some(v) = read_partial_integer(ctx, target, "day", class)? {
        if v < 1 {
            return Err(NativeError::RangeError {
                name: class,
                reason: "day must be a positive integer".to_string(),
            });
        }
        cf.day = Some(v.min(u8::MAX as i64) as u8);
    }
    if has_eras {
        if let Some(s) = read_option_string(ctx, target, "era", class)? {
            let era = temporal_rs::TinyAsciiStr::<19>::try_from_str(&s).map_err(|_| {
                NativeError::RangeError {
                    name: class,
                    reason: "invalid era".to_string(),
                }
            })?;
            cf.era = Some(era);
        }
        if let Some(v) = read_partial_integer(ctx, target, "eraYear", class)? {
            cf.era_year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        }
    }
    if let Some(v) = read_partial_integer(ctx, target, "hour", class)? {
        time.hour = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(ctx, target, "microsecond", class)? {
        time.microsecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(ctx, target, "millisecond", class)? {
        time.millisecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(ctx, target, "minute", class)? {
        time.minute = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(ctx, target, "month", class)? {
        if v < 1 {
            return Err(NativeError::RangeError {
                name: class,
                reason: "month must be a positive integer".to_string(),
            });
        }
        cf.month = Some(v.min(u8::MAX as i64) as u8);
    }
    if let Some(code) = read_month_code(ctx, target, class)? {
        cf.month_code = Some(code);
    }
    if let Some(v) = read_partial_integer(ctx, target, "nanosecond", class)? {
        time.nanosecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(ctx, target, "second", class)? {
        time.second = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(ctx, target, "year", class)? {
        cf.year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    Ok(temporal_rs::fields::DateTimeFields {
        calendar_fields: cf,
        time,
    })
}

pub fn parse_year_month_fields(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    calendar: &temporal_rs::Calendar,
    class: &'static str,
) -> Result<temporal_rs::fields::YearMonthCalendarFields, NativeError> {
    // Alphabetical spec field order, each read through a getter/Proxy-
    // aware [[Get]]. ISO has no era keys, so they are read only for
    // era-bearing calendars.
    let has_eras = !calendar.is_iso();
    let mut f = temporal_rs::fields::YearMonthCalendarFields::default();
    if has_eras {
        if let Some(s) = read_option_string(ctx, target, "era", class)? {
            let era = temporal_rs::TinyAsciiStr::<19>::try_from_str(&s).map_err(|_| {
                NativeError::RangeError {
                    name: class,
                    reason: "invalid era".to_string(),
                }
            })?;
            f.era = Some(era);
        }
        if let Some(v) = read_partial_integer(ctx, target, "eraYear", class)? {
            f.era_year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        }
    }
    if let Some(v) = read_partial_integer(ctx, target, "month", class)? {
        if v < 1 {
            return Err(NativeError::RangeError {
                name: class,
                reason: "month must be a positive integer".to_string(),
            });
        }
        f.month = Some(v.min(u8::MAX as i64) as u8);
    }
    if let Some(code) = read_month_code(ctx, target, class)? {
        f.month_code = Some(code);
    }
    if let Some(v) = read_partial_integer(ctx, target, "year", class)? {
        f.year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    Ok(f)
}

//! `Temporal.Duration` — calendar / time difference value.
//!
//! Backed by [`temporal_rs::Duration`]. The foundation slice ships
//! the parts used in real applications: construction (string parse,
//! partial-record), `total({ unit })`, `add` / `subtract`, and
//! component accessors.
//!
//! # Contents
//! - [`dispatch_static`] — `Temporal.Duration.from(...)` /
//!   `Duration.compare(...)`.
//! - [`load_property`] — accessor reads (`years`, `months`, `days`,
//!   `hours`, `minutes`, `seconds`, `milliseconds`, `microseconds`,
//!   `nanoseconds`, `sign`, `blank`).
//! - [`partial_from_object`] — coerce a `{ days: 1 }` shaped object
//!   to a [`temporal_rs::Duration`]. Reused by the `Instant` /
//!   `PlainDate` / `PlainTime` arithmetic helpers.
//! - [`DURATION_PROTOTYPE_TABLE`] — synchronous prototype methods
//!   (`add`, `subtract`, `negated`, `abs`, `total`, `toString`).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-duration-objects>

use std::str::FromStr;
use std::sync::LazyLock;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::object::JsObject;
use crate::string::StringHeap;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::{
    js_string_value, make_temporal, optional_object_arg, read_i64_field, read_string_field,
    require_duration, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

/// Dispatch `Temporal.Duration.<method>(args...)`.
pub fn dispatch_static(
    string_heap: &StringHeap,
    method: &str,
    args: &[Value],
) -> Result<Value, TemporalError> {
    let _ = string_heap;
    match method {
        "from" => from(args),
        "compare" => compare(args),
        other => Err(TemporalError::UnknownMember {
            class: "Duration".to_string(),
            method: other.to_string(),
        }),
    }
}

/// Spec §7.2.1 `Temporal.Duration.from`.
fn from(args: &[Value]) -> Result<Value, TemporalError> {
    let dur = match args.first() {
        Some(Value::String(s)) => temporal_rs::Duration::from_utf8(s.to_lossy_string().as_bytes())
            .map_err(|e| TemporalError::Engine {
                class: "Duration",
                method: "from",
                message: e.to_string(),
            })?,
        Some(Value::Object(obj)) => {
            partial_from_object(obj).map_err(|e| TemporalError::Engine {
                class: "Duration",
                method: "from",
                message: e.to_string(),
            })?
        }
        Some(Value::Temporal(t)) => match t.payload() {
            TemporalPayload::Duration(d) => *d,
            _ => {
                return Err(TemporalError::BadArgument {
                    class: "Duration",
                    method: "from",
                    index: 0,
                    reason: "must be a Temporal.Duration, partial-record, or ISO string",
                });
            }
        },
        _ => {
            return Err(TemporalError::BadArgument {
                class: "Duration",
                method: "from",
                index: 0,
                reason: "must be a Temporal.Duration, partial-record, or ISO string",
            });
        }
    };
    Ok(make_temporal(TemporalPayload::Duration(dur)))
}

/// Spec §7.2.2 `Temporal.Duration.compare(a, b, options?)`. The
/// foundation skips the `relativeTo` option (only date-only or
/// time-only durations compare without it).
fn compare(args: &[Value]) -> Result<Value, TemporalError> {
    let a = expect_duration(args, 0)?;
    let b = expect_duration(args, 1)?;
    let cmp = a.compare(&b, None).map_err(|e| TemporalError::Engine {
        class: "Duration",
        method: "compare",
        message: e.to_string(),
    })?;
    let n = match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::Number(NumberValue::from_i32(n)))
}

fn expect_duration(args: &[Value], index: u16) -> Result<temporal_rs::Duration, TemporalError> {
    match args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload() {
            TemporalPayload::Duration(d) => Ok(*d),
            _ => Err(TemporalError::BadArgument {
                class: "Duration",
                method: "compare",
                index,
                reason: "must be a Temporal.Duration",
            }),
        },
        Some(Value::String(s)) => temporal_rs::Duration::from_utf8(s.to_lossy_string().as_bytes())
            .map_err(|e| TemporalError::Engine {
                class: "Duration",
                method: "compare",
                message: e.to_string(),
            }),
        Some(Value::Object(obj)) => partial_from_object(obj).map_err(|e| TemporalError::Engine {
            class: "Duration",
            method: "compare",
            message: e.to_string(),
        }),
        _ => Err(TemporalError::BadArgument {
            class: "Duration",
            method: "compare",
            index,
            reason: "must be a Temporal.Duration or partial-record",
        }),
    }
}

/// Coerce a `{ days: 1, hours: 2, … }` shaped JS object into a
/// [`temporal_rs::Duration`]. Used by `Duration.from(partial)` and
/// by `Instant`/`PlainDate`/`PlainTime` arithmetic when the
/// argument is a plain object.
pub fn partial_from_object(
    obj: &JsObject,
) -> Result<temporal_rs::Duration, temporal_rs::TemporalError> {
    let mut partial = temporal_rs::partial::PartialDuration::empty();
    if let Some(v) = optional_field(obj, "years")? {
        partial = partial.with_years(v);
    }
    if let Some(v) = optional_field(obj, "months")? {
        partial = partial.with_months(v);
    }
    if let Some(v) = optional_field(obj, "weeks")? {
        partial = partial.with_weeks(v);
    }
    if let Some(v) = optional_field(obj, "days")? {
        partial = partial.with_days(v);
    }
    if let Some(v) = optional_field(obj, "hours")? {
        partial = partial.with_hours(v);
    }
    if let Some(v) = optional_field(obj, "minutes")? {
        partial = partial.with_minutes(v);
    }
    if let Some(v) = optional_field(obj, "seconds")? {
        partial = partial.with_seconds(v);
    }
    if let Some(v) = optional_field(obj, "milliseconds")? {
        partial = partial.with_milliseconds(v);
    }
    if let Some(v) = optional_field(obj, "microseconds")? {
        partial = partial.with_microseconds(v as i128);
    }
    if let Some(v) = optional_field(obj, "nanoseconds")? {
        partial = partial.with_nanoseconds(v as i128);
    }
    temporal_rs::Duration::from_partial_duration(partial)
}

fn optional_field(obj: &JsObject, name: &str) -> Result<Option<i64>, temporal_rs::TemporalError> {
    match obj.get(name) {
        None | Some(Value::Undefined) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(match n.as_smi() {
            Some(v) => v as i64,
            None => n.as_f64() as i64,
        })),
        Some(_) => Err(temporal_rs::TemporalError::range()
            .with_message("Duration partial fields must be numbers")),
    }
}

/// Property reads on a `Temporal.Duration` receiver.
#[must_use]
pub fn load_property(temporal: &JsTemporal, name: &str) -> Value {
    let TemporalPayload::Duration(d) = temporal.payload() else {
        return Value::Undefined;
    };
    match name {
        "years" => Value::Number(NumberValue::from_i32(d.years() as i32)),
        "months" => Value::Number(NumberValue::from_i32(d.months() as i32)),
        "weeks" => Value::Number(NumberValue::from_i32(d.weeks() as i32)),
        "days" => Value::Number(NumberValue::from_i32(d.days() as i32)),
        "hours" => Value::Number(NumberValue::from_i32(d.hours() as i32)),
        "minutes" => Value::Number(NumberValue::from_i32(d.minutes() as i32)),
        "seconds" => Value::Number(NumberValue::from_i32(d.seconds() as i32)),
        "milliseconds" => Value::Number(NumberValue::from_i32(d.milliseconds() as i32)),
        "microseconds" => Value::Number(NumberValue::from_f64(d.microseconds() as f64)),
        "nanoseconds" => Value::Number(NumberValue::from_f64(d.nanoseconds() as f64)),
        "sign" => Value::Number(NumberValue::from_i32(d.sign() as i32)),
        "blank" => Value::Boolean(d.is_zero()),
        _ => Value::Undefined,
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    let s = dur
        .as_temporal_string(temporal_rs::options::ToStringRoundingOptions::default())
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let lhs = require_duration(args)?;
    let rhs = duration_arg(args, 0)?;
    let result = lhs.add(&rhs).map_err(temporal_err)?;
    Ok(make_temporal(TemporalPayload::Duration(result)))
}

fn impl_subtract(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let lhs = require_duration(args)?;
    let rhs = duration_arg(args, 0)?;
    let result = lhs.subtract(&rhs).map_err(temporal_err)?;
    Ok(make_temporal(TemporalPayload::Duration(result)))
}

fn impl_negated(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    Ok(make_temporal(TemporalPayload::Duration(dur.negated())))
}

fn impl_abs(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    Ok(make_temporal(TemporalPayload::Duration(dur.abs())))
}

fn impl_total(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    let opts = optional_object_arg(args, 0).ok_or(IntrinsicError::BadArgument {
        index: 0,
        reason: "must be { unit: '<unit>' } options",
    })?;
    let _ = read_i64_field;
    let unit_name = read_string_field(opts, "unit").ok_or(IntrinsicError::BadArgument {
        index: 0,
        reason: "options must include a `unit` string",
    })?;
    let unit = temporal_rs::options::Unit::from_str(&unit_name).map_err(|_| {
        IntrinsicError::BadArgument {
            index: 0,
            reason: "unknown duration unit",
        }
    })?;
    let total = dur.total(unit, None).map_err(temporal_err)?;
    Ok(Value::Number(NumberValue::from_f64(total.as_inner())))
}

/// Coerce arg `index` to a `temporal_rs::Duration`. Accepts a real
/// `Temporal.Duration` value or a partial-record object.
fn duration_arg(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::Duration, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload() {
            TemporalPayload::Duration(d) => Ok(*d),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.Duration",
            }),
        },
        Some(Value::Object(obj)) => {
            partial_from_object(obj).map_err(|_| IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.Duration partial",
            })
        }
        _ => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.Duration",
        }),
    }
}

/// `Temporal.Duration.prototype` table.
pub static DURATION_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString" / 0 => impl_to_string,
        "add"      / 1 => impl_add,
        "subtract" / 1 => impl_subtract,
        "negated"  / 0 => impl_negated,
        "abs"      / 0 => impl_abs,
        "total"    / 1 => impl_total,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    DURATION_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

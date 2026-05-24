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

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::object::JsObject;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::{
    alloc_temporal_value, js_string_value, make_temporal, opt_integer_if_integral,
    optional_object_arg, read_i64_field, read_string_field, require_construct, require_duration,
    temporal_dispatch_err, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// §7.1.1 `Temporal.Duration(years, months, weeks, days, hours,
/// minutes, seconds, milliseconds, microseconds, nanoseconds)`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const CLASS: &str = "Temporal.Duration";
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let years = opt_integer_if_integral(args, 0, heap, CLASS, "years")? as i64;
    let months = opt_integer_if_integral(args, 1, heap, CLASS, "months")? as i64;
    let weeks = opt_integer_if_integral(args, 2, heap, CLASS, "weeks")? as i64;
    let days = opt_integer_if_integral(args, 3, heap, CLASS, "days")? as i64;
    let hours = opt_integer_if_integral(args, 4, heap, CLASS, "hours")? as i64;
    let minutes = opt_integer_if_integral(args, 5, heap, CLASS, "minutes")? as i64;
    let seconds = opt_integer_if_integral(args, 6, heap, CLASS, "seconds")? as i64;
    let ms = opt_integer_if_integral(args, 7, heap, CLASS, "milliseconds")? as i64;
    let us = opt_integer_if_integral(args, 8, heap, CLASS, "microseconds")? as i128;
    let ns = opt_integer_if_integral(args, 9, heap, CLASS, "nanoseconds")? as i128;
    let dur = temporal_rs::Duration::new(
        years, months, weeks, days, hours, minutes, seconds, ms, us, ns,
    )
    .map_err(|e| NativeError::RangeError {
        name: CLASS,
        reason: e.to_string(),
    })?;
    let heap = ctx.heap_mut();
    alloc_temporal_value(heap, TemporalPayload::Duration(dur)).map_err(temporal_dispatch_err)
}

/// Dispatch `Temporal.Duration.<method>(args...)` via the typed
/// [`TemporalMethod`].
pub fn dispatch_static(
    gc_heap: &mut otter_gc::GcHeap,
    method: otter_bytecode::method_id::TemporalMethod,
    args: &[Value],
) -> Result<Value, TemporalError> {
    use otter_bytecode::method_id::TemporalMethod as M;
    match method {
        M::From => from(args, gc_heap),
        M::Compare => compare(args, gc_heap),
        other => Err(TemporalError::UnknownMember {
            class: "Duration".to_string(),
            method: other.name().to_string(),
        }),
    }
}

/// Spec §7.2.1 `Temporal.Duration.from`.
fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let first = args.first();
    let dur = if let Some(s) = first.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::Duration::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
            TemporalError::Engine {
                class: "Duration",
                method: "from",
                message: e.to_string(),
            kind: e.kind(),
            }
        })?
    } else if let Some(obj) = first.and_then(|v| v.as_object()) {
        partial_from_object(&obj, gc_heap).map_err(|e| TemporalError::Engine {
            class: "Duration",
            method: "from",
            message: e.to_string(),
            kind: e.kind(),
        })?
    } else if let Some(t) = first.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::Duration(d) => d,
            _ => {
                return Err(TemporalError::BadArgument {
                    class: "Duration",
                    method: "from",
                    index: 0,
                    reason: "must be a Temporal.Duration, partial-record, or ISO string",
                });
            }
        }
    } else {
        return Err(TemporalError::BadArgument {
            class: "Duration",
            method: "from",
            index: 0,
            reason: "must be a Temporal.Duration, partial-record, or ISO string",
        });
    };
    alloc_temporal_value(gc_heap, TemporalPayload::Duration(dur))
}

/// Spec §7.2.2 `Temporal.Duration.compare(a, b, options?)`. The
/// foundation skips the `relativeTo` option (only date-only or
/// time-only durations compare without it).
fn compare(args: &[Value], gc_heap: &otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let a = expect_duration(args, 0, gc_heap)?;
    let b = expect_duration(args, 1, gc_heap)?;
    let cmp = a.compare(&b, None).map_err(|e| TemporalError::Engine {
        class: "Duration",
        method: "compare",
        message: e.to_string(),
            kind: e.kind(),
    })?;
    let n = match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn expect_duration(
    args: &[Value],
    index: u16,
    gc_heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::Duration, TemporalError> {
    let arg = args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(TemporalError::BadArgument {
                class: "Duration",
                method: "compare",
                index,
                reason: "must be a Temporal.Duration",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::Duration::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
            TemporalError::Engine {
                class: "Duration",
                method: "compare",
                message: e.to_string(),
            kind: e.kind(),
            }
        })
    } else if let Some(obj) = arg.and_then(|v| v.as_object()) {
        partial_from_object(&obj, gc_heap).map_err(|e| TemporalError::Engine {
            class: "Duration",
            method: "compare",
            message: e.to_string(),
            kind: e.kind(),
        })
    } else {
        Err(TemporalError::BadArgument {
            class: "Duration",
            method: "compare",
            index,
            reason: "must be a Temporal.Duration or partial-record",
        })
    }
}

/// Coerce a `{ days: 1, hours: 2, … }` shaped JS object into a
/// [`temporal_rs::Duration`]. Used by `Duration.from(partial)` and
/// by `Instant`/`PlainDate`/`PlainTime` arithmetic when the
/// argument is a plain object.
pub fn partial_from_object(
    obj: &JsObject,
    gc_heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::Duration, temporal_rs::TemporalError> {
    let mut partial = temporal_rs::partial::PartialDuration::empty();
    if let Some(v) = optional_field(obj, "years", gc_heap)? {
        partial = partial.with_years(v);
    }
    if let Some(v) = optional_field(obj, "months", gc_heap)? {
        partial = partial.with_months(v);
    }
    if let Some(v) = optional_field(obj, "weeks", gc_heap)? {
        partial = partial.with_weeks(v);
    }
    if let Some(v) = optional_field(obj, "days", gc_heap)? {
        partial = partial.with_days(v);
    }
    if let Some(v) = optional_field(obj, "hours", gc_heap)? {
        partial = partial.with_hours(v);
    }
    if let Some(v) = optional_field(obj, "minutes", gc_heap)? {
        partial = partial.with_minutes(v);
    }
    if let Some(v) = optional_field(obj, "seconds", gc_heap)? {
        partial = partial.with_seconds(v);
    }
    if let Some(v) = optional_field(obj, "milliseconds", gc_heap)? {
        partial = partial.with_milliseconds(v);
    }
    if let Some(v) = optional_field(obj, "microseconds", gc_heap)? {
        partial = partial.with_microseconds(v as i128);
    }
    if let Some(v) = optional_field(obj, "nanoseconds", gc_heap)? {
        partial = partial.with_nanoseconds(v as i128);
    }
    temporal_rs::Duration::from_partial_duration(partial)
}

fn optional_field(
    obj: &JsObject,
    name: &str,
    gc_heap: &otter_gc::GcHeap,
) -> Result<Option<i64>, temporal_rs::TemporalError> {
    let v = crate::object::get(*obj, gc_heap, name);
    let Some(v) = v else {
        return Ok(None);
    };
    if v.is_undefined() {
        return Ok(None);
    }
    if let Some(n) = v.as_number() {
        return Ok(Some(match n.as_smi() {
            Some(v) => v as i64,
            None => n.as_f64() as i64,
        }));
    }
    Err(temporal_rs::TemporalError::range().with_message("Duration partial fields must be numbers"))
}

/// Property reads on a `Temporal.Duration` receiver.
#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let d = match temporal.payload_clone(gc_heap) {
        TemporalPayload::Duration(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "years" => Value::number_i32(d.years() as i32),
        "months" => Value::number_i32(d.months() as i32),
        "weeks" => Value::number_i32(d.weeks() as i32),
        "days" => Value::number_i32(d.days() as i32),
        "hours" => Value::number_i32(d.hours() as i32),
        "minutes" => Value::number_i32(d.minutes() as i32),
        "seconds" => Value::number_i32(d.seconds() as i32),
        "milliseconds" => Value::number_i32(d.milliseconds() as i32),
        "microseconds" => Value::number_f64(d.microseconds() as f64),
        "nanoseconds" => Value::number_f64(d.nanoseconds() as f64),
        "sign" => Value::number_i32(d.sign() as i32),
        "blank" => Value::boolean(d.is_zero()),
        _ => Value::undefined(),
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    let s = dur
        .as_temporal_string(temporal_rs::options::ToStringRoundingOptions::default())
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let lhs = require_duration(args)?;
    let rhs = duration_arg(args, 0)?;
    let result = lhs.add(&rhs).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_subtract(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let lhs = require_duration(args)?;
    let rhs = duration_arg(args, 0)?;
    let result = lhs.subtract(&rhs).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_negated(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    let negated = dur.negated();
    make_temporal(args, TemporalPayload::Duration(negated))
}

fn impl_abs(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    let abs = dur.abs();
    make_temporal(args, TemporalPayload::Duration(abs))
}

fn impl_total(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let dur = require_duration(args)?;
    let opts = optional_object_arg(args, 0).ok_or(IntrinsicError::BadArgument {
        index: 0,
        reason: "must be { unit: '<unit>' } options",
    })?;
    let _ = read_i64_field;
    let unit_name = {
        let heap = &*args.gc_heap;
        read_string_field(opts, "unit", heap)
    }
    .ok_or(IntrinsicError::BadArgument {
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
    Ok(Value::number_f64(total.as_inner()))
}

/// Coerce arg `index` to a `temporal_rs::Duration`. Accepts a real
/// `Temporal.Duration` value or a partial-record object.
fn duration_arg(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::Duration, IntrinsicError> {
    let bad = || IntrinsicError::BadArgument {
        index,
        reason: "must be a Temporal.Duration",
    };
    let arg = args.args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(bad()),
        }
    } else if let Some(obj) = arg.and_then(|v| v.as_object()) {
        let heap = &*args.gc_heap;
        partial_from_object(&obj, heap).map_err(|_| IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.Duration partial",
        })
    } else {
        Err(bad())
    }
}

fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_to_string(args)
}

fn impl_value_of(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Err(IntrinsicError::BadReceiver {
        expected: "Temporal.Duration has no `.valueOf` — use `compare`",
    })
}

/// `Temporal.Duration.prototype` table.
pub static DURATION_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString" / 0 => impl_to_string,
        "toJSON"   / 0 => impl_to_json,
        "valueOf"  / 0 => impl_value_of,
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

crate::temporal::proto_bridge::temporal_proto_methods! {
    class = "Duration",
    slice = DURATION_PROTOTYPE_METHODS,
    methods = [
        "toString" / 0 => impl_to_string as native_duration_to_string,
        "toJSON"   / 0 => impl_to_json   as native_duration_to_json,
        "valueOf"  / 0 => impl_value_of  as native_duration_value_of,
        "add"      / 1 => impl_add       as native_duration_add,
        "subtract" / 1 => impl_subtract  as native_duration_subtract,
        "negated"  / 0 => impl_negated   as native_duration_negated,
        "abs"      / 0 => impl_abs       as native_duration_abs,
        "total"    / 1 => impl_total     as native_duration_total,
    ]
}

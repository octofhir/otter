//! `Temporal.PlainDateTime` — combined wall-clock date + time
//! without a zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindatetime-objects>

use std::sync::LazyLock;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, arg_or_undef, arg_to_calendar, clamp_to_u16, clamp_to_u8,
    js_string_value, make_temporal, opt_integer_with_truncation, parse_date_time_fields,
    parse_difference_settings, parse_partial_time, parse_rounding_options, require_construct,
    require_plain_date_time, temporal_dispatch_err, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// §5.1.1 `Temporal.PlainDateTime(isoYear, isoMonth, isoDay [, hour
/// [, minute [, second [, ms [, us [, ns [, calendar]]]]]]])`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const CLASS: &str = "Temporal.PlainDateTime";
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let year = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoYear")? as i32;
    let month = clamp_to_u8(
        to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoMonth")?,
        CLASS,
        "isoMonth",
    )?;
    let day = clamp_to_u8(
        to_integer_with_truncation(&arg_or_undef(args, 2), heap, CLASS, "isoDay")?,
        CLASS,
        "isoDay",
    )?;
    let hour = clamp_to_u8(
        opt_integer_with_truncation(args, 3, heap, CLASS, "hour")?,
        CLASS,
        "hour",
    )?;
    let minute = clamp_to_u8(
        opt_integer_with_truncation(args, 4, heap, CLASS, "minute")?,
        CLASS,
        "minute",
    )?;
    let second = clamp_to_u8(
        opt_integer_with_truncation(args, 5, heap, CLASS, "second")?,
        CLASS,
        "second",
    )?;
    let millisecond = clamp_to_u16(
        opt_integer_with_truncation(args, 6, heap, CLASS, "millisecond")?,
        CLASS,
        "millisecond",
    )?;
    let microsecond = clamp_to_u16(
        opt_integer_with_truncation(args, 7, heap, CLASS, "microsecond")?,
        CLASS,
        "microsecond",
    )?;
    let nanosecond = clamp_to_u16(
        opt_integer_with_truncation(args, 8, heap, CLASS, "nanosecond")?,
        CLASS,
        "nanosecond",
    )?;
    let calendar = arg_to_calendar(args, 9, heap, CLASS)?;
    let pdt = temporal_rs::PlainDateTime::try_new(
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
        calendar,
    )
    .map_err(|e| NativeError::RangeError {
        name: CLASS,
        reason: e.to_string(),
    })?;
    let heap = ctx.heap_mut();
    alloc_temporal_value(heap, TemporalPayload::PlainDateTime(pdt)).map_err(temporal_dispatch_err)
}

/// Dispatch `Temporal.PlainDateTime.<method>(args...)` via the
/// typed [`TemporalMethod`].
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
            class: "PlainDateTime".to_string(),
            method: other.name().to_string(),
        }),
    }
}

fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pdt = parse_arg(args, gc_heap, 0, "from")?;
    alloc_temporal_value(gc_heap, TemporalPayload::PlainDateTime(pdt))
}

fn compare(args: &[Value], gc_heap: &otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let a = parse_arg(args, gc_heap, 0, "compare")?;
    let b = parse_arg(args, gc_heap, 1, "compare")?;
    let cmp = temporal_rs::PlainDateTime::compare_iso(&a, &b);
    let n = match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_arg(
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
    index: u16,
    method: &'static str,
) -> Result<temporal_rs::PlainDateTime, TemporalError> {
    let arg = args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            _ => Err(TemporalError::BadArgument {
                class: "PlainDateTime",
                method,
                index,
                reason: "must be a Temporal.PlainDateTime",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::PlainDateTime::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
            TemporalError::Engine {
                class: "PlainDateTime",
                method,
                message: e.to_string(),
            kind: e.kind(),
            }
        })
    } else {
        Err(TemporalError::BadArgument {
            class: "PlainDateTime",
            method,
            index,
            reason: "must be a Temporal.PlainDateTime or ISO string",
        })
    }
}

/// Property reads on a `Temporal.PlainDateTime` receiver.
#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pdt = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainDateTime(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(pdt.year()),
        "month" => Value::number_i32(pdt.month() as i32),
        "day" => Value::number_i32(pdt.day() as i32),
        "hour" => Value::number_i32(pdt.hour() as i32),
        "minute" => Value::number_i32(pdt.minute() as i32),
        "second" => Value::number_i32(pdt.second() as i32),
        "millisecond" => Value::number_i32(pdt.millisecond() as i32),
        "microsecond" => Value::number_i32(pdt.microsecond() as i32),
        "nanosecond" => Value::number_i32(pdt.nanosecond() as i32),
        _ => Value::undefined(),
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let s = pdt
        .to_ixdtf_string(
            temporal_rs::options::ToStringRoundingOptions::default(),
            temporal_rs::options::DisplayCalendar::Auto,
        )
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pdt.add(&dur, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDateTime(result))
}

fn impl_subtract(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pdt.subtract(&dur, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDateTime(result))
}

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

fn impl_until(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let other = arg_as_plain_date_time(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = pdt.until(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_since(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let other = arg_as_plain_date_time(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = pdt.since(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_round(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let options = parse_rounding_options(args, 0)?;
    let result = pdt.round(options).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDateTime(result))
}

fn impl_equals(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let other = arg_as_plain_date_time(args, 0)?;
    Ok(Value::boolean(pdt == other))
}

fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_to_string(args)
}

fn impl_value_of(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Err(IntrinsicError::BadReceiver {
        expected: "Temporal.PlainDateTime has no `.valueOf` — use `compare` or `equals`",
    })
}

fn arg_as_plain_date_time(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::PlainDateTime, IntrinsicError> {
    let arg = args.args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.PlainDateTime",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(args.gc_heap)) {
        temporal_rs::PlainDateTime::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
            .map_err(temporal_err)
    } else {
        Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.PlainDateTime or ISO string",
        })
    }
}

fn impl_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let Some(obj) = args.args.first().and_then(|v| v.as_object()) else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "first argument must be an object",
        });
    };
    let fields = parse_date_time_fields(obj, args.gc_heap)?;
    let result = pdt.with(fields, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDateTime(result))
}

fn impl_with_calendar(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let cal_value = args.args.first().copied().unwrap_or_default();
    let Some(js) = cal_value.as_string(args.gc_heap) else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "calendar identifier must be a string",
        });
    };
    let s = js.to_lossy_string(args.gc_heap);
    let calendar = temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(temporal_err)?;
    let result = pdt.with_calendar(calendar);
    make_temporal(args, TemporalPayload::PlainDateTime(result))
}

fn impl_with_plain_time(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    let arg = args.args.first().copied().unwrap_or_default();
    let time = if arg.is_undefined() {
        temporal_rs::PlainTime::default()
    } else if let Some(t) = arg.as_temporal(args.gc_heap) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainTime(pt) => pt,
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a Temporal.PlainTime or partial-time object",
                });
            }
        }
    } else if let Some(obj) = arg.as_object() {
        let partial = parse_partial_time(obj, args.gc_heap)?;
        temporal_rs::PlainTime::default()
            .with(partial, None)
            .map_err(temporal_err)?
    } else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a Temporal.PlainTime, partial-time object, or undefined",
        });
    };
    // `PlainDateTime.with({ time })` shape: rebuild from existing
    // ISO date + the new time fields. `temporal_rs` exposes this as
    // `to_plain_date().to_plain_date_time(Some(time))`.
    let pd = pdt.to_plain_date();
    let result = pd.to_plain_date_time(Some(time)).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDateTime(result))
}

fn impl_to_plain_date(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    make_temporal(args, TemporalPayload::PlainDate(pdt.to_plain_date()))
}

fn impl_to_plain_time(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pdt = require_plain_date_time(args)?;
    make_temporal(args, TemporalPayload::PlainTime(pdt.to_plain_time()))
}

/// `Temporal.PlainDateTime.prototype` table.
pub static PLAIN_DATE_TIME_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString"       / 0 => impl_to_string,
        "toJSON"         / 0 => impl_to_json,
        "valueOf"        / 0 => impl_value_of,
        "add"            / 1 => impl_add,
        "subtract"       / 1 => impl_subtract,
        "equals"         / 1 => impl_equals,
        "until"          / 1 => impl_until,
        "since"          / 1 => impl_since,
        "round"          / 1 => impl_round,
        "with"           / 1 => impl_with,
        "withCalendar"   / 1 => impl_with_calendar,
        "withPlainTime"  / 0 => impl_with_plain_time,
        "toPlainDate"    / 0 => impl_to_plain_date,
        "toPlainTime"    / 0 => impl_to_plain_time,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    PLAIN_DATE_TIME_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

crate::temporal::proto_bridge::temporal_proto_methods! {
    class = "PlainDateTime",
    slice = PLAIN_DATE_TIME_PROTOTYPE_METHODS,
    methods = [
        "toString"       / 0 => impl_to_string        as native_pdt_to_string,
        "toJSON"         / 0 => impl_to_json          as native_pdt_to_json,
        "valueOf"        / 0 => impl_value_of         as native_pdt_value_of,
        "add"            / 1 => impl_add              as native_pdt_add,
        "subtract"       / 1 => impl_subtract         as native_pdt_subtract,
        "equals"         / 1 => impl_equals           as native_pdt_equals,
        "until"          / 1 => impl_until            as native_pdt_until,
        "since"          / 1 => impl_since            as native_pdt_since,
        "round"          / 1 => impl_round            as native_pdt_round,
        "with"           / 1 => impl_with             as native_pdt_with,
        "withCalendar"   / 1 => impl_with_calendar    as native_pdt_with_calendar,
        "withPlainTime"  / 0 => impl_with_plain_time  as native_pdt_with_plain_time,
        "toPlainDate"    / 0 => impl_to_plain_date    as native_pdt_to_plain_date,
        "toPlainTime"    / 0 => impl_to_plain_time    as native_pdt_to_plain_time,
    ]
}

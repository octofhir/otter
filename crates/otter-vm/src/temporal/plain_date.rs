//! `Temporal.PlainDate` — calendar date `YYYY-MM-DD`.
//!
//! Backed by [`temporal_rs::PlainDate`]. ISO calendar only in the
//! foundation slice (non-ISO calendars filed as a follow-up task).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindate-objects>

use std::sync::LazyLock;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, arg_or_undef, arg_to_calendar, clamp_to_u8, js_string_value,
    make_temporal, parse_calendar_fields, parse_difference_settings, require_construct,
    require_plain_date, temporal_dispatch_err, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// §3.1.1 `Temporal.PlainDate(isoYear, isoMonth, isoDay [, calendar])`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const CLASS: &str = "Temporal.PlainDate";
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let year = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoYear")? as i32;
    let month_f = to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoMonth")?;
    let day_f = to_integer_with_truncation(&arg_or_undef(args, 2), heap, CLASS, "isoDay")?;
    let calendar = arg_to_calendar(args, 3, heap, CLASS)?;
    let month = clamp_to_u8(month_f, CLASS, "isoMonth")?;
    let day = clamp_to_u8(day_f, CLASS, "isoDay")?;
    let pd =
        temporal_rs::PlainDate::try_new(year, month, day, calendar).map_err(|e| {
            NativeError::RangeError {
                name: CLASS,
                reason: e.to_string(),
            }
        })?;
    let heap = ctx.heap_mut();
    alloc_temporal_value(heap, TemporalPayload::PlainDate(pd)).map_err(temporal_dispatch_err)
}

/// Dispatch `Temporal.PlainDate.<method>(args...)` via the typed
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
            class: "PlainDate".to_string(),
            method: other.name().to_string(),
        }),
    }
}

fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pd = parse_arg(args, gc_heap, 0, "from")?;
    alloc_temporal_value(gc_heap, TemporalPayload::PlainDate(pd))
}

fn compare(args: &[Value], gc_heap: &otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let a = parse_arg(args, gc_heap, 0, "compare")?;
    let b = parse_arg(args, gc_heap, 1, "compare")?;
    let cmp = a.compare_iso(&b);
    let n = match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number(NumberValue::from_i32(n)))
}

fn parse_arg(
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
    index: u16,
    method: &'static str,
) -> Result<temporal_rs::PlainDate, TemporalError> {
    let arg = args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::PlainDate(v) => Ok(v),
            _ => Err(TemporalError::BadArgument {
                class: "PlainDate",
                method,
                index,
                reason: "must be a Temporal.PlainDate",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::PlainDate::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
            TemporalError::Engine {
                class: "PlainDate",
                method,
                message: e.to_string(),
            kind: e.kind(),
            }
        })
    } else {
        Err(TemporalError::BadArgument {
            class: "PlainDate",
            method,
            index,
            reason: "must be a Temporal.PlainDate or ISO string",
        })
    }
}

/// Property reads on a `Temporal.PlainDate` receiver.
#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pd = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainDate(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(pd.year()),
        "month" => Value::number_i32(pd.month() as i32),
        "day" => Value::number_i32(pd.day() as i32),
        "dayOfWeek" => Value::number_i32(pd.day_of_week() as i32),
        "dayOfYear" => Value::number_i32(pd.day_of_year() as i32),
        "daysInMonth" => Value::number_i32(pd.days_in_month() as i32),
        "daysInYear" => Value::number_i32(pd.days_in_year() as i32),
        "monthsInYear" => Value::number_i32(pd.months_in_year() as i32),
        "inLeapYear" => Value::boolean(pd.in_leap_year()),
        _ => Value::undefined(),
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let s = pd.to_ixdtf_string(temporal_rs::options::DisplayCalendar::Auto);
    js_string_value(s, args)
}

fn impl_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pd.add(&dur, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDate(result))
}

fn impl_subtract(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pd.subtract(&dur, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDate(result))
}

fn impl_equals(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let first = args.args.first();
    let other = if let Some(t) = first.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDate(v) => v,
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a Temporal.PlainDate",
                });
            }
        }
    } else if let Some(s) = first.and_then(|v| v.as_string(args.gc_heap)) {
        temporal_rs::PlainDate::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
            .map_err(temporal_err)?
    } else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a Temporal.PlainDate or ISO string",
        });
    };
    Ok(Value::boolean(
        pd.compare_iso(&other) == std::cmp::Ordering::Equal,
    ))
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
    let pd = require_plain_date(args)?;
    let other = arg_as_plain_date(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = pd.until(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_since(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let other = arg_as_plain_date(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = pd.since(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_to_string(args)
}

fn impl_value_of(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Err(IntrinsicError::BadReceiver {
        expected: "Temporal.PlainDate has no `.valueOf` — use `compare` or `equals`",
    })
}

fn arg_as_plain_date(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::PlainDate, IntrinsicError> {
    let arg = args.args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDate(v) => Ok(v),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.PlainDate",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(args.gc_heap)) {
        temporal_rs::PlainDate::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
            .map_err(temporal_err)
    } else {
        Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.PlainDate or ISO string",
        })
    }
}

fn impl_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let Some(obj) = args.args.first().and_then(|v| v.as_object()) else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "first argument must be an object",
        });
    };
    let fields = parse_calendar_fields(obj, args.gc_heap)?;
    let result = pd.with(fields, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDate(result))
}

fn impl_with_calendar(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let cal_value = args.args.first().copied().unwrap_or_default();
    let Some(js) = cal_value.as_string(args.gc_heap) else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "calendar identifier must be a string",
        });
    };
    let s = js.to_lossy_string(args.gc_heap);
    let calendar = temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(temporal_err)?;
    let result = pd.with_calendar(calendar);
    make_temporal(args, TemporalPayload::PlainDate(result))
}

fn impl_to_plain_date_time(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pd = require_plain_date(args)?;
    let time = if let Some(v) = args.args.first().copied()
        && !v.is_undefined()
    {
        let Some(obj) = v.as_object() else {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "first argument must be an object or undefined",
            });
        };
        // Accept either a Temporal.PlainTime body or a partial-time
        // object.
        if let Some(t) = v.as_temporal(args.gc_heap) {
            match t.payload_clone(args.gc_heap) {
                TemporalPayload::PlainTime(pt) => Some(pt),
                _ => {
                    return Err(IntrinsicError::BadArgument {
                        index: 0,
                        reason: "must be a Temporal.PlainTime or partial-time object",
                    });
                }
            }
        } else {
            let partial = crate::temporal::helpers::parse_partial_time(obj, args.gc_heap)?;
            let pt = temporal_rs::PlainTime::default()
                .with(partial, None)
                .map_err(temporal_err)?;
            Some(pt)
        }
    } else {
        None
    };
    let pdt = pd.to_plain_date_time(time).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDateTime(pdt))
}

/// `Temporal.PlainDate.prototype` table.
pub static PLAIN_DATE_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString"          / 0 => impl_to_string,
        "toJSON"            / 0 => impl_to_json,
        "valueOf"           / 0 => impl_value_of,
        "add"               / 1 => impl_add,
        "subtract"          / 1 => impl_subtract,
        "equals"            / 1 => impl_equals,
        "until"             / 1 => impl_until,
        "since"             / 1 => impl_since,
        "with"              / 1 => impl_with,
        "withCalendar"      / 1 => impl_with_calendar,
        "toPlainDateTime"   / 0 => impl_to_plain_date_time,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    PLAIN_DATE_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

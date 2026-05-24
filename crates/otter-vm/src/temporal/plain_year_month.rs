//! `Temporal.PlainYearMonth` — calendar year+month (`YYYY-MM`).
//!
//! Backed by [`temporal_rs::PlainYearMonth`]. ISO calendar only in
//! the foundation slice (non-ISO calendars filed as a follow-up
//! task).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plainyearmonth-objects>

use std::sync::LazyLock;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, arg_or_undef, arg_to_calendar, clamp_to_u8, js_string_value,
    make_temporal, parse_difference_settings, parse_year_month_fields, require_construct,
    require_plain_year_month, temporal_dispatch_err, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// §9.1.1 `Temporal.PlainYearMonth(isoYear, isoMonth [, calendar [, referenceISODay]])`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const CLASS: &str = "Temporal.PlainYearMonth";
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let year = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoYear")? as i32;
    let month_f = to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoMonth")?;
    let calendar = arg_to_calendar(args, 2, heap, CLASS)?;
    let ref_day_v = arg_or_undef(args, 3);
    let ref_day = if ref_day_v.is_undefined() {
        None
    } else {
        let n = to_integer_with_truncation(&ref_day_v, heap, CLASS, "referenceISODay")?;
        Some(clamp_to_u8(n, CLASS, "referenceISODay")?)
    };
    let month = clamp_to_u8(month_f, CLASS, "isoMonth")?;
    let pym = temporal_rs::PlainYearMonth::try_new(year, month, ref_day, calendar).map_err(
        |e| NativeError::RangeError {
            name: CLASS,
            reason: e.to_string(),
        },
    )?;
    let heap = ctx.heap_mut();
    alloc_temporal_value(heap, TemporalPayload::PlainYearMonth(pym)).map_err(temporal_dispatch_err)
}

/// Dispatch `Temporal.PlainYearMonth.<method>(args...)`.
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
            class: "PlainYearMonth".to_string(),
            method: other.name().to_string(),
        }),
    }
}

fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pym = parse_arg(args, gc_heap, 0, "from")?;
    alloc_temporal_value(gc_heap, TemporalPayload::PlainYearMonth(pym))
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
) -> Result<temporal_rs::PlainYearMonth, TemporalError> {
    let arg = args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::PlainYearMonth(v) => Ok(v),
            _ => Err(TemporalError::BadArgument {
                class: "PlainYearMonth",
                method,
                index,
                reason: "must be a Temporal.PlainYearMonth",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::PlainYearMonth::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(
            |e| TemporalError::Engine {
                class: "PlainYearMonth",
                method,
                message: e.to_string(),
                kind: e.kind(),
            },
        )
    } else if let Some(obj) = arg.and_then(|v| v.as_object()) {
        let fields = match crate::temporal::helpers::parse_year_month_fields(obj, gc_heap) {
            Ok(f) => f,
            Err(_) => {
                return Err(TemporalError::BadArgument {
                    class: "PlainYearMonth",
                    method,
                    index,
                    reason: "must be a year-month-like object",
                });
            }
        };
        let partial = temporal_rs::partial::PartialYearMonth {
            calendar_fields: fields,
            calendar: temporal_rs::Calendar::default(),
        };
        temporal_rs::PlainYearMonth::from_partial(partial, None).map_err(|e| {
            TemporalError::Engine {
                class: "PlainYearMonth",
                method,
                message: e.to_string(),
                kind: e.kind(),
            }
        })
    } else {
        Err(TemporalError::BadArgument {
            class: "PlainYearMonth",
            method,
            index,
            reason: "must be a Temporal.PlainYearMonth, ISO string, or year-month-like object",
        })
    }
}

/// Property reads on a `Temporal.PlainYearMonth` receiver.
#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pym = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainYearMonth(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(pym.year()),
        "month" => Value::number_i32(pym.month() as i32),
        "daysInMonth" => Value::number_i32(pym.days_in_month() as i32),
        "daysInYear" => Value::number_i32(pym.days_in_year() as i32),
        "monthsInYear" => Value::number_i32(pym.months_in_year() as i32),
        "inLeapYear" => Value::boolean(pym.in_leap_year()),
        _ => Value::undefined(),
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let s = pym.to_ixdtf_string(temporal_rs::options::DisplayCalendar::Auto);
    js_string_value(s, args)
}

fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_to_string(args)
}

fn impl_value_of(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Err(IntrinsicError::BadReceiver {
        expected: "Temporal.PlainYearMonth has no `.valueOf` — use `compare` or `equals`",
    })
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

fn impl_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pym
        .add(&dur, temporal_rs::options::Overflow::Constrain)
        .map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainYearMonth(result))
}

fn impl_subtract(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pym
        .subtract(&dur, temporal_rs::options::Overflow::Constrain)
        .map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainYearMonth(result))
}

fn arg_as_plain_year_month(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::PlainYearMonth, IntrinsicError> {
    let arg = args.args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainYearMonth(v) => Ok(v),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.PlainYearMonth",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(args.gc_heap)) {
        temporal_rs::PlainYearMonth::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
            .map_err(temporal_err)
    } else if let Some(obj) = arg.and_then(|v| v.as_object()) {
        let fields = parse_year_month_fields(obj, args.gc_heap)?;
        let partial = temporal_rs::partial::PartialYearMonth {
            calendar_fields: fields,
            calendar: temporal_rs::Calendar::default(),
        };
        temporal_rs::PlainYearMonth::from_partial(partial, None).map_err(temporal_err)
    } else {
        Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.PlainYearMonth, ISO string, or year-month-like object",
        })
    }
}

fn impl_equals(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let other = arg_as_plain_year_month(args, 0)?;
    Ok(Value::boolean(
        pym.compare_iso(&other) == std::cmp::Ordering::Equal,
    ))
}

fn impl_until(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let other = arg_as_plain_year_month(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = pym.until(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_since(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let other = arg_as_plain_year_month(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = pym.since(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let Some(obj) = args.args.first().and_then(|v| v.as_object()) else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "first argument must be an object",
        });
    };
    let fields = parse_year_month_fields(obj, args.gc_heap)?;
    let result = pym.with(fields, None).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainYearMonth(result))
}

fn impl_to_plain_date(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pym = require_plain_year_month(args)?;
    let Some(obj) = args.args.first().and_then(|v| v.as_object()) else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "first argument must be an object with a `day` field",
        });
    };
    let day_fields = crate::temporal::helpers::parse_calendar_fields(obj, args.gc_heap)?;
    let result = pym.to_plain_date(Some(day_fields)).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainDate(result))
}

/// `Temporal.PlainYearMonth.prototype` table.
pub static PLAIN_YEAR_MONTH_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString"      / 0 => impl_to_string,
        "toJSON"        / 0 => impl_to_json,
        "valueOf"       / 0 => impl_value_of,
        "add"           / 1 => impl_add,
        "subtract"      / 1 => impl_subtract,
        "equals"        / 1 => impl_equals,
        "until"         / 1 => impl_until,
        "since"         / 1 => impl_since,
        "with"          / 1 => impl_with,
        "toPlainDate"   / 1 => impl_to_plain_date,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    PLAIN_YEAR_MONTH_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

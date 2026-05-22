//! `Temporal.PlainDate` — calendar date `YYYY-MM-DD`.
//!
//! Backed by [`temporal_rs::PlainDate`]. ISO calendar only in the
//! foundation slice (non-ISO calendars filed as a follow-up task).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindate-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, js_string_value, make_temporal, require_plain_date, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

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
    match args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload_clone(gc_heap) {
            TemporalPayload::PlainDate(v) => Ok(v),
            _ => Err(TemporalError::BadArgument {
                class: "PlainDate",
                method,
                index,
                reason: "must be a Temporal.PlainDate",
            }),
        },
        Some(Value::String(s)) => {
            temporal_rs::PlainDate::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
                TemporalError::Engine {
                    class: "PlainDate",
                    method,
                    message: e.to_string(),
                }
            })
        }
        _ => Err(TemporalError::BadArgument {
            class: "PlainDate",
            method,
            index,
            reason: "must be a Temporal.PlainDate or ISO string",
        }),
    }
}

/// Property reads on a `Temporal.PlainDate` receiver.
#[must_use]
pub fn load_property(temporal: &JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pd = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainDate(v) => v,
        _ => return Value::Undefined,
    };
    match name {
        "year" => Value::number(NumberValue::from_i32(pd.year())),
        "month" => Value::number(NumberValue::from_i32(pd.month() as i32)),
        "day" => Value::number(NumberValue::from_i32(pd.day() as i32)),
        "dayOfWeek" => Value::number(NumberValue::from_i32(pd.day_of_week() as i32)),
        "dayOfYear" => Value::number(NumberValue::from_i32(pd.day_of_year() as i32)),
        "daysInMonth" => Value::number(NumberValue::from_i32(pd.days_in_month() as i32)),
        "daysInYear" => Value::number(NumberValue::from_i32(pd.days_in_year() as i32)),
        "monthsInYear" => Value::number(NumberValue::from_i32(pd.months_in_year() as i32)),
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
    let other = match args.args.first() {
        Some(Value::Temporal(t)) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDate(v) => v,
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a Temporal.PlainDate",
                });
            }
        },
        Some(Value::String(s)) => {
            temporal_rs::PlainDate::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
                .map_err(temporal_err)?
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a Temporal.PlainDate or ISO string",
            });
        }
    };
    Ok(Value::Boolean(
        pd.compare_iso(&other) == std::cmp::Ordering::Equal,
    ))
}

fn duration_arg(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::Duration, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.Duration",
            }),
        },
        Some(Value::Object(obj)) => {
            let heap = &*args.gc_heap;
            partial_from_object(obj, heap).map_err(|_| IntrinsicError::BadArgument {
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

/// `Temporal.PlainDate.prototype` table.
pub static PLAIN_DATE_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString" / 0 => impl_to_string,
        "add"      / 1 => impl_add,
        "subtract" / 1 => impl_subtract,
        "equals"   / 1 => impl_equals,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    PLAIN_DATE_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

//! `Temporal.PlainDateTime` — combined wall-clock date + time
//! without a zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindatetime-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, js_string_value, make_temporal, require_plain_date_time, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

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
    Ok(Value::Number(NumberValue::from_i32(n)))
}

fn parse_arg(
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
    index: u16,
    method: &'static str,
) -> Result<temporal_rs::PlainDateTime, TemporalError> {
    match args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload_clone(gc_heap) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            _ => Err(TemporalError::BadArgument {
                class: "PlainDateTime",
                method,
                index,
                reason: "must be a Temporal.PlainDateTime",
            }),
        },
        Some(Value::String(s)) => temporal_rs::PlainDateTime::from_utf8(
            s.to_lossy_string(gc_heap).as_bytes(),
        )
        .map_err(|e| TemporalError::Engine {
            class: "PlainDateTime",
            method,
            message: e.to_string(),
        }),
        _ => Err(TemporalError::BadArgument {
            class: "PlainDateTime",
            method,
            index,
            reason: "must be a Temporal.PlainDateTime or ISO string",
        }),
    }
}

/// Property reads on a `Temporal.PlainDateTime` receiver.
#[must_use]
pub fn load_property(temporal: &JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pdt = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainDateTime(v) => v,
        _ => return Value::Undefined,
    };
    match name {
        "year" => Value::number(NumberValue::from_i32(pdt.year())),
        "month" => Value::number(NumberValue::from_i32(pdt.month() as i32)),
        "day" => Value::number(NumberValue::from_i32(pdt.day() as i32)),
        "hour" => Value::number(NumberValue::from_i32(pdt.hour() as i32)),
        "minute" => Value::number(NumberValue::from_i32(pdt.minute() as i32)),
        "second" => Value::number(NumberValue::from_i32(pdt.second() as i32)),
        "millisecond" => Value::number(NumberValue::from_i32(pdt.millisecond() as i32)),
        "microsecond" => Value::number(NumberValue::from_i32(pdt.microsecond() as i32)),
        "nanosecond" => Value::number(NumberValue::from_i32(pdt.nanosecond() as i32)),
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

/// `Temporal.PlainDateTime.prototype` table.
pub static PLAIN_DATE_TIME_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString" / 0 => impl_to_string,
        "add"      / 1 => impl_add,
        "subtract" / 1 => impl_subtract,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    PLAIN_DATE_TIME_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

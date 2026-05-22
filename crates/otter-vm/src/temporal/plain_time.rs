//! `Temporal.PlainTime` — wall-clock time without a date or zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaintime-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, js_string_value, make_temporal, require_plain_time, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

/// Dispatch `Temporal.PlainTime.<method>(args...)` via the typed
/// [`TemporalMethod`].
pub fn dispatch_static(
    gc_heap: &mut otter_gc::GcHeap,
    method: otter_bytecode::method_id::TemporalMethod,
    args: &[Value],
) -> Result<Value, TemporalError> {
    use otter_bytecode::method_id::TemporalMethod as M;
    match method {
        M::From => from(args, gc_heap),
        other => Err(TemporalError::UnknownMember {
            class: "PlainTime".to_string(),
            method: other.name().to_string(),
        }),
    }
}

fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pt = match args.first() {
        Some(Value::Temporal(t)) => match t.payload_clone(gc_heap) {
            TemporalPayload::PlainTime(v) => v,
            _ => {
                return Err(TemporalError::BadArgument {
                    class: "PlainTime",
                    method: "from",
                    index: 0,
                    reason: "must be a Temporal.PlainTime or ISO string",
                });
            }
        },
        Some(Value::String(s)) => temporal_rs::PlainTime::from_utf8(
            s.to_lossy_string(gc_heap).as_bytes(),
        )
        .map_err(|e| TemporalError::Engine {
            class: "PlainTime",
            method: "from",
            message: e.to_string(),
        })?,
        _ => {
            return Err(TemporalError::BadArgument {
                class: "PlainTime",
                method: "from",
                index: 0,
                reason: "must be a Temporal.PlainTime or ISO string",
            });
        }
    };
    alloc_temporal_value(gc_heap, TemporalPayload::PlainTime(pt))
}

/// Property reads on a `Temporal.PlainTime` receiver.
#[must_use]
pub fn load_property(temporal: &JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pt = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainTime(v) => v,
        _ => return Value::Undefined,
    };
    match name {
        "hour" => Value::Number(NumberValue::from_i32(pt.hour() as i32)),
        "minute" => Value::Number(NumberValue::from_i32(pt.minute() as i32)),
        "second" => Value::Number(NumberValue::from_i32(pt.second() as i32)),
        "millisecond" => Value::Number(NumberValue::from_i32(pt.millisecond() as i32)),
        "microsecond" => Value::Number(NumberValue::from_i32(pt.microsecond() as i32)),
        "nanosecond" => Value::Number(NumberValue::from_i32(pt.nanosecond() as i32)),
        _ => Value::undefined(),
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pt = require_plain_time(args)?;
    let s = pt
        .to_ixdtf_string(temporal_rs::options::ToStringRoundingOptions::default())
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pt = require_plain_time(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pt.add(&dur).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainTime(result))
}

fn impl_subtract(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pt = require_plain_time(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pt.subtract(&dur).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::PlainTime(result))
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

/// `Temporal.PlainTime.prototype` table.
pub static PLAIN_TIME_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
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
    PLAIN_TIME_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

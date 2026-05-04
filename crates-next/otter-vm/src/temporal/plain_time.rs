//! `Temporal.PlainTime` — wall-clock time without a date or zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaintime-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::string::StringHeap;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{js_string_value, make_temporal, require_plain_time, temporal_err};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

/// Dispatch `Temporal.PlainTime.<method>(args...)`.
pub fn dispatch_static(
    string_heap: &StringHeap,
    method: &str,
    args: &[Value],
) -> Result<Value, TemporalError> {
    let _ = string_heap;
    match method {
        "from" => from(args),
        other => Err(TemporalError::UnknownMember {
            class: "PlainTime".to_string(),
            method: other.to_string(),
        }),
    }
}

fn from(args: &[Value]) -> Result<Value, TemporalError> {
    let pt = match args.first() {
        Some(Value::Temporal(t)) => match t.payload() {
            TemporalPayload::PlainTime(v) => *v,
            _ => {
                return Err(TemporalError::BadArgument {
                    class: "PlainTime",
                    method: "from",
                    index: 0,
                    reason: "must be a Temporal.PlainTime or ISO string",
                });
            }
        },
        Some(Value::String(s)) => temporal_rs::PlainTime::from_utf8(s.to_lossy_string().as_bytes())
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
    Ok(make_temporal(TemporalPayload::PlainTime(pt)))
}

/// Property reads on a `Temporal.PlainTime` receiver.
#[must_use]
pub fn load_property(temporal: &JsTemporal, name: &str) -> Value {
    let TemporalPayload::PlainTime(pt) = temporal.payload() else {
        return Value::Undefined;
    };
    match name {
        "hour" => Value::Number(NumberValue::from_i32(pt.hour() as i32)),
        "minute" => Value::Number(NumberValue::from_i32(pt.minute() as i32)),
        "second" => Value::Number(NumberValue::from_i32(pt.second() as i32)),
        "millisecond" => Value::Number(NumberValue::from_i32(pt.millisecond() as i32)),
        "microsecond" => Value::Number(NumberValue::from_i32(pt.microsecond() as i32)),
        "nanosecond" => Value::Number(NumberValue::from_i32(pt.nanosecond() as i32)),
        _ => Value::Undefined,
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pt = require_plain_time(args)?;
    let s = pt
        .to_ixdtf_string(temporal_rs::options::ToStringRoundingOptions::default())
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pt = require_plain_time(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pt.add(&dur).map_err(temporal_err)?;
    Ok(make_temporal(TemporalPayload::PlainTime(result)))
}

fn impl_subtract(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let pt = require_plain_time(args)?;
    let dur = duration_arg(args, 0)?;
    let result = pt.subtract(&dur).map_err(temporal_err)?;
    Ok(make_temporal(TemporalPayload::PlainTime(result)))
}

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
            let heap = args.gc_heap.borrow();
            partial_from_object(obj, &heap).map_err(|_| IntrinsicError::BadArgument {
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

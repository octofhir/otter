//! `Temporal.Instant` — point on the UTC timeline.
//!
//! Backed entirely by [`temporal_rs::Instant`]; this module is the
//! thin glue that routes JS-visible static / prototype calls through
//! the spec algorithms.
//!
//! # Contents
//! - [`dispatch_static`] — `Temporal.Instant.from(...)` /
//!   `Instant.compare(...)` / `Instant.fromEpochMilliseconds(ms)`.
//! - [`load_property`] — accessor reads (`epochMilliseconds`,
//!   `epochNanoseconds`).
//! - [`INSTANT_PROTOTYPE_TABLE`] — synchronous prototype methods
//!   (`add`, `subtract`, `equals`, `toString`).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-instant-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::string::StringHeap;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::{js_string_value, make_temporal, require_instant, temporal_err};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

/// Dispatch `Temporal.Instant.<method>(args...)`.
pub fn dispatch_static(
    string_heap: &StringHeap,
    method: &str,
    args: &[Value],
) -> Result<Value, TemporalError> {
    let _ = string_heap;
    match method {
        "from" => from(args),
        "fromEpochMilliseconds" => from_epoch_milliseconds(args),
        "compare" => compare(args),
        other => Err(TemporalError::UnknownMember {
            class: "Instant".to_string(),
            method: other.to_string(),
        }),
    }
}

/// Spec §8.2.1 `Temporal.Instant.from`.
fn from(args: &[Value]) -> Result<Value, TemporalError> {
    let inst = parse_instant_arg(args, 0, "from")?;
    Ok(make_temporal(TemporalPayload::Instant(inst)))
}

/// Spec §8.2.3 `Temporal.Instant.fromEpochMilliseconds(ms)`.
fn from_epoch_milliseconds(args: &[Value]) -> Result<Value, TemporalError> {
    let ms = match args.first() {
        Some(Value::Number(n)) => n.as_f64() as i64,
        _ => {
            return Err(TemporalError::BadArgument {
                class: "Instant",
                method: "fromEpochMilliseconds",
                index: 0,
                reason: "must be a number",
            });
        }
    };
    let inst =
        temporal_rs::Instant::from_epoch_milliseconds(ms).map_err(|e| TemporalError::Engine {
            class: "Instant",
            method: "fromEpochMilliseconds",
            message: e.to_string(),
        })?;
    Ok(make_temporal(TemporalPayload::Instant(inst)))
}

/// Spec §8.2.4 `Temporal.Instant.compare(a, b)`.
fn compare(args: &[Value]) -> Result<Value, TemporalError> {
    let a = parse_instant_arg(args, 0, "compare")?;
    let b = parse_instant_arg(args, 1, "compare")?;
    let cmp = a.as_i128().cmp(&b.as_i128());
    let n = match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::Number(NumberValue::from_i32(n)))
}

fn parse_instant_arg(
    args: &[Value],
    index: u16,
    method: &'static str,
) -> Result<temporal_rs::Instant, TemporalError> {
    match args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload() {
            TemporalPayload::Instant(v) => Ok(*v),
            _ => Err(TemporalError::BadArgument {
                class: "Instant",
                method,
                index,
                reason: "must be a Temporal.Instant",
            }),
        },
        Some(Value::String(s)) => temporal_rs::Instant::from_utf8(s.to_lossy_string().as_bytes())
            .map_err(|e| TemporalError::Engine {
                class: "Instant",
                method,
                message: e.to_string(),
            }),
        _ => Err(TemporalError::BadArgument {
            class: "Instant",
            method,
            index,
            reason: "must be a Temporal.Instant or ISO string",
        }),
    }
}

/// Property reads on a `Temporal.Instant` receiver.
#[must_use]
pub fn load_property(temporal: &JsTemporal, name: &str) -> Value {
    let TemporalPayload::Instant(inst) = temporal.payload() else {
        return Value::Undefined;
    };
    match name {
        "epochMilliseconds" => {
            Value::Number(NumberValue::from_f64(inst.epoch_milliseconds() as f64))
        }
        "epochNanoseconds" => {
            // Per spec returns a BigInt.
            Value::BigInt(crate::bigint::BigIntValue::from_i128(inst.as_i128()))
        }
        _ => Value::Undefined,
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let s = inst
        .to_ixdtf_string(
            None,
            temporal_rs::options::ToStringRoundingOptions::default(),
        )
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let dur = arg_as_duration(args, 0)?;
    let result = inst.add(&dur).map_err(temporal_err)?;
    Ok(make_temporal(TemporalPayload::Instant(result)))
}

fn impl_subtract(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let dur = arg_as_duration(args, 0)?;
    let result = inst.subtract(&dur).map_err(temporal_err)?;
    Ok(make_temporal(TemporalPayload::Instant(result)))
}

fn impl_equals(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let other = match args.args.first() {
        Some(Value::Temporal(t)) => match t.payload() {
            TemporalPayload::Instant(v) => *v,
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a Temporal.Instant",
                });
            }
        },
        Some(Value::String(s)) => {
            temporal_rs::Instant::from_utf8(s.to_lossy_string().as_bytes()).map_err(temporal_err)?
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a Temporal.Instant or ISO string",
            });
        }
    };
    Ok(Value::Boolean(inst.as_i128() == other.as_i128()))
}

/// Coerce the argument at `index` to a [`temporal_rs::Duration`].
fn arg_as_duration(
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
            crate::temporal::duration::partial_from_object(obj).map_err(|_| {
                IntrinsicError::BadArgument {
                    index,
                    reason: "must be a Temporal.Duration partial",
                }
            })
        }
        _ => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.Duration",
        }),
    }
}

/// `Temporal.Instant.prototype` table.
pub static INSTANT_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
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
    INSTANT_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

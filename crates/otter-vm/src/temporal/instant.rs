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
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::{
    alloc_temporal_value, js_string_value, make_temporal, require_instant, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};

/// Dispatch `Temporal.Instant.<method>(args...)` via the typed
/// [`TemporalMethod`].
pub fn dispatch_static(
    gc_heap: &mut otter_gc::GcHeap,
    method: otter_bytecode::method_id::TemporalMethod,
    args: &[Value],
) -> Result<Value, TemporalError> {
    use otter_bytecode::method_id::TemporalMethod as M;
    match method {
        M::From => from(args, gc_heap),
        M::FromEpochMilliseconds => from_epoch_milliseconds(args, gc_heap),
        M::Compare => compare(args, gc_heap),
        other => Err(TemporalError::UnknownMember {
            class: "Instant".to_string(),
            method: other.name().to_string(),
        }),
    }
}

/// Spec §8.2.1 `Temporal.Instant.from`.
fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let inst = parse_instant_arg(args, gc_heap, 0, "from")?;
    alloc_temporal_value(gc_heap, TemporalPayload::Instant(inst))
}

/// Spec §8.2.3 `Temporal.Instant.fromEpochMilliseconds(ms)`.
fn from_epoch_milliseconds(
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, TemporalError> {
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
    alloc_temporal_value(gc_heap, TemporalPayload::Instant(inst))
}

/// Spec §8.2.4 `Temporal.Instant.compare(a, b)`.
fn compare(args: &[Value], gc_heap: &otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let a = parse_instant_arg(args, gc_heap, 0, "compare")?;
    let b = parse_instant_arg(args, gc_heap, 1, "compare")?;
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
    gc_heap: &otter_gc::GcHeap,
    index: u16,
    method: &'static str,
) -> Result<temporal_rs::Instant, TemporalError> {
    match args.get(index as usize) {
        Some(Value::Temporal(t)) => match t.payload_clone(gc_heap) {
            TemporalPayload::Instant(v) => Ok(v),
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

/// Property reads on a `Temporal.Instant` receiver. The
/// `epochNanoseconds` accessor allocates a BigInt body and so takes
/// `&mut GcHeap`; the `epochMilliseconds` arm is heap-free but
/// shares the signature for uniform dispatch.
pub fn load_property(temporal: &JsTemporal, gc_heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let inst = match temporal.payload_clone(gc_heap) {
        TemporalPayload::Instant(v) => v,
        _ => return Value::Undefined,
    };
    match name {
        "epochMilliseconds" => {
            Value::Number(NumberValue::from_f64(inst.epoch_milliseconds() as f64))
        }
        "epochNanoseconds" => {
            // Per spec returns a BigInt. On allocation failure the
            // accessor returns `undefined` — the caller has no error
            // channel; this matches the spec's "throws abrupt" only
            // when the GC cap is hit, which `RangeError` would
            // normally surface but the property-load path can't
            // propagate that here without a wider API change.
            match crate::bigint::BigIntValue::from_i128(gc_heap, inst.as_i128()) {
                Ok(handle) => Value::BigInt(handle),
                Err(_) => Value::Undefined,
            }
        }
        _ => Value::Undefined,
    }
}

// ── Prototype table ──────────────────────────────────────────────

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let s = inst
        .to_ixdtf_string(
            None,
            temporal_rs::options::ToStringRoundingOptions::default(),
        )
        .map_err(temporal_err)?;
    js_string_value(s, args)
}

fn impl_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let dur = arg_as_duration(args, 0)?;
    let result = inst.add(&dur).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Instant(result))
}

fn impl_subtract(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let dur = arg_as_duration(args, 0)?;
    let result = inst.subtract(&dur).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Instant(result))
}

fn impl_equals(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let other = match args.args.first() {
        Some(Value::Temporal(t)) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::Instant(v) => v,
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
        Some(Value::Temporal(t)) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.Duration",
            }),
        },
        Some(Value::Object(obj)) => {
            let heap = &*args.gc_heap;
            crate::temporal::duration::partial_from_object(obj, heap).map_err(|_| {
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

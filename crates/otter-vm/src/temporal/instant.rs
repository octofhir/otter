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

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::{
    alloc_temporal_value, arg_or_undef, js_string_value, make_temporal,
    parse_difference_settings, parse_rounding_options, require_construct, require_instant,
    temporal_dispatch_err, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// §8.1.1 `Temporal.Instant(epochNanoseconds)` — `[[Construct]]`
/// body. Coerces `epochNanoseconds` to a `BigInt`, validates the
/// epoch range, and allocates a `Value::Temporal`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const CLASS: &str = "Temporal.Instant";
    require_construct(ctx, CLASS)?;
    let raw = arg_or_undef(args, 0);
    let ns = if let Some(b) = raw.as_big_int() {
        b.with_inner(ctx.heap(), |bi| {
            use num_traits::ToPrimitive;
            bi.to_i128()
        })
    } else if let Some(s) = raw.as_string(ctx.heap()) {
        let text = s.to_lossy_string(ctx.heap());
        let parsed =
            crate::abstract_ops::string_to_big_int(&text).ok_or(NativeError::SyntaxError {
                name: CLASS,
                reason: format!("cannot convert {text:?} to a BigInt"),
            })?;
        use num_traits::ToPrimitive;
        parsed.to_i128()
    } else if let Some(b) = raw.as_boolean() {
        Some(i128::from(b))
    } else if raw.is_number() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds: cannot convert a Number to a BigInt".to_string(),
        });
    } else if raw.is_symbol() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds: cannot convert a Symbol to a BigInt".to_string(),
        });
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds must be a BigInt".to_string(),
        });
    };
    let Some(ns) = ns else {
        return Err(NativeError::RangeError {
            name: CLASS,
            reason: "epochNanoseconds out of i128 range".to_string(),
        });
    };
    let inst = temporal_rs::Instant::try_new(ns).map_err(|e| NativeError::RangeError {
        name: CLASS,
        reason: e.to_string(),
    })?;
    let heap = ctx.heap_mut();
    alloc_temporal_value(heap, TemporalPayload::Instant(inst)).map_err(temporal_dispatch_err)
}

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
    let Some(ms) = args
        .first()
        .and_then(|v| v.as_number())
        .map(|n| n.as_f64() as i64)
    else {
        return Err(TemporalError::BadArgument {
            class: "Instant",
            method: "fromEpochMilliseconds",
            index: 0,
            reason: "must be a number",
        });
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
    Ok(Value::number_i32(n))
}

fn parse_instant_arg(
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
    index: u16,
    method: &'static str,
) -> Result<temporal_rs::Instant, TemporalError> {
    let arg = args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::Instant(v) => Ok(v),
            _ => Err(TemporalError::BadArgument {
                class: "Instant",
                method,
                index,
                reason: "must be a Temporal.Instant",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::Instant::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
            TemporalError::Engine {
                class: "Instant",
                method,
                message: e.to_string(),
            }
        })
    } else {
        Err(TemporalError::BadArgument {
            class: "Instant",
            method,
            index,
            reason: "must be a Temporal.Instant or ISO string",
        })
    }
}

/// Property reads on a `Temporal.Instant` receiver. The
/// `epochNanoseconds` accessor allocates a BigInt body and so takes
/// `&mut GcHeap`; the `epochMilliseconds` arm is heap-free but
/// shares the signature for uniform dispatch.
pub fn load_property(temporal: JsTemporal, gc_heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let inst = match temporal.payload_clone(gc_heap) {
        TemporalPayload::Instant(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "epochMilliseconds" => Value::number_f64(inst.epoch_milliseconds() as f64),
        "epochNanoseconds" => {
            // Per spec returns a BigInt. On allocation failure the
            // accessor returns `undefined` — the caller has no error
            // channel; this matches the spec's "throws abrupt" only
            // when the GC cap is hit, which `RangeError` would
            // normally surface but the property-load path can't
            // propagate that here without a wider API change.
            match crate::bigint::BigIntValue::from_i128(gc_heap, inst.as_i128()) {
                Ok(handle) => Value::big_int(handle),
                Err(_) => Value::undefined(),
            }
        }
        _ => Value::undefined(),
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
    let first = args.args.first();
    let other = if let Some(t) = first.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::Instant(v) => v,
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a Temporal.Instant",
                });
            }
        }
    } else if let Some(s) = first.and_then(|v| v.as_string(args.gc_heap)) {
        temporal_rs::Instant::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
            .map_err(temporal_err)?
    } else {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a Temporal.Instant or ISO string",
        });
    };
    Ok(Value::boolean(inst.as_i128() == other.as_i128()))
}

/// Coerce the argument at `index` to a [`temporal_rs::Duration`].
fn arg_as_duration(
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
        crate::temporal::duration::partial_from_object(&obj, heap).map_err(|_| {
            IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.Duration partial",
            }
        })
    } else {
        Err(bad())
    }
}

fn impl_until(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let other = arg_as_instant(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = inst.until(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_since(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let other = arg_as_instant(args, 0)?;
    let settings = parse_difference_settings(args, 1)?;
    let result = inst.since(&other, settings).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Duration(result))
}

fn impl_round(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let inst = require_instant(args)?;
    let options = parse_rounding_options(args, 0)?;
    let result = inst.round(options).map_err(temporal_err)?;
    make_temporal(args, TemporalPayload::Instant(result))
}

/// `toJSON` returns the same ISO string as `toString` per
/// §8.3.10 — used by `JSON.stringify`.
fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_to_string(args)
}

/// `valueOf` always throws `TypeError` per §8.3.13 to block ordering
/// comparisons (`<`, `>=`) on Temporal values.
fn impl_value_of(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Err(IntrinsicError::BadReceiver {
        expected: "Temporal.Instant has no `.valueOf` — use `compare` or `equals`",
    })
}

/// Coerce the argument at `index` to a [`temporal_rs::Instant`].
fn arg_as_instant(
    args: &IntrinsicArgs<'_>,
    index: u16,
) -> Result<temporal_rs::Instant, IntrinsicError> {
    let arg = args.args.get(index as usize);
    if let Some(t) = arg.and_then(|v| v.as_temporal(args.gc_heap)) {
        match t.payload_clone(args.gc_heap) {
            TemporalPayload::Instant(v) => Ok(v),
            _ => Err(IntrinsicError::BadArgument {
                index,
                reason: "must be a Temporal.Instant",
            }),
        }
    } else if let Some(s) = arg.and_then(|v| v.as_string(args.gc_heap)) {
        temporal_rs::Instant::from_utf8(s.to_lossy_string(args.gc_heap).as_bytes())
            .map_err(temporal_err)
    } else {
        Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a Temporal.Instant or ISO string",
        })
    }
}

/// `Temporal.Instant.prototype` table.
pub static INSTANT_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Temporal,
        "toString" / 0 => impl_to_string,
        "toJSON"   / 0 => impl_to_json,
        "valueOf"  / 0 => impl_value_of,
        "add"      / 1 => impl_add,
        "subtract" / 1 => impl_subtract,
        "equals"   / 1 => impl_equals,
        "until"    / 1 => impl_until,
        "since"    / 1 => impl_since,
        "round"    / 1 => impl_round,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    INSTANT_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Temporal, name)
}

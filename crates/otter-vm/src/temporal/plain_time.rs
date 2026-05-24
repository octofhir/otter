//! `Temporal.PlainTime` — wall-clock time without a date or zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaintime-objects>

use std::sync::LazyLock;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::temporal::dispatch::TemporalError;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    alloc_temporal_value, clamp_to_u16, clamp_to_u8, js_string_value, make_temporal,
    opt_integer_with_truncation, require_construct, require_plain_time, temporal_dispatch_err,
    temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

/// §4.1.1 `Temporal.PlainTime([hour [, minute [, second [, ms [, us [, ns]]]]]])`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const CLASS: &str = "Temporal.PlainTime";
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let hour = clamp_to_u8(
        opt_integer_with_truncation(args, 0, heap, CLASS, "hour")?,
        CLASS,
        "hour",
    )?;
    let minute = clamp_to_u8(
        opt_integer_with_truncation(args, 1, heap, CLASS, "minute")?,
        CLASS,
        "minute",
    )?;
    let second = clamp_to_u8(
        opt_integer_with_truncation(args, 2, heap, CLASS, "second")?,
        CLASS,
        "second",
    )?;
    let millisecond = clamp_to_u16(
        opt_integer_with_truncation(args, 3, heap, CLASS, "millisecond")?,
        CLASS,
        "millisecond",
    )?;
    let microsecond = clamp_to_u16(
        opt_integer_with_truncation(args, 4, heap, CLASS, "microsecond")?,
        CLASS,
        "microsecond",
    )?;
    let nanosecond = clamp_to_u16(
        opt_integer_with_truncation(args, 5, heap, CLASS, "nanosecond")?,
        CLASS,
        "nanosecond",
    )?;
    let pt = temporal_rs::PlainTime::try_new(
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
    )
    .map_err(|e| NativeError::RangeError {
        name: CLASS,
        reason: e.to_string(),
    })?;
    let heap = ctx.heap_mut();
    alloc_temporal_value(heap, TemporalPayload::PlainTime(pt)).map_err(temporal_dispatch_err)
}

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
    let bad = || TemporalError::BadArgument {
        class: "PlainTime",
        method: "from",
        index: 0,
        reason: "must be a Temporal.PlainTime or ISO string",
    };
    let first = args.first();
    let pt = if let Some(t) = first.and_then(|v| v.as_temporal(gc_heap)) {
        match t.payload_clone(gc_heap) {
            TemporalPayload::PlainTime(v) => v,
            _ => return Err(bad()),
        }
    } else if let Some(s) = first.and_then(|v| v.as_string(gc_heap)) {
        temporal_rs::PlainTime::from_utf8(s.to_lossy_string(gc_heap).as_bytes()).map_err(|e| {
            TemporalError::Engine {
                class: "PlainTime",
                method: "from",
                message: e.to_string(),
            }
        })?
    } else {
        return Err(bad());
    };
    alloc_temporal_value(gc_heap, TemporalPayload::PlainTime(pt))
}

/// Property reads on a `Temporal.PlainTime` receiver.
#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pt = match temporal.payload_clone(gc_heap) {
        TemporalPayload::PlainTime(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "hour" => Value::number_i32(pt.hour() as i32),
        "minute" => Value::number_i32(pt.minute() as i32),
        "second" => Value::number_i32(pt.second() as i32),
        "millisecond" => Value::number_i32(pt.millisecond() as i32),
        "microsecond" => Value::number_i32(pt.microsecond() as i32),
        "nanosecond" => Value::number_i32(pt.nanosecond() as i32),
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

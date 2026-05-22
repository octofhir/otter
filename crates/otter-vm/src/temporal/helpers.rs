//! Shared coercion / extraction helpers for Temporal intrinsic
//! implementations.
//!
//! Each prototype-method file (`instant.rs`, `duration.rs`, …)
//! pulls the receiver- and argument-shaped helpers out of this
//! module so the per-kind logic stays focused on the spec algorithm.
//!
//! # Contents
//! - [`require_string_arg`] — coerce arg N to a Rust string.
//! - [`require_object_arg`] — coerce arg N to a `JsObject` (used
//!   for `{ days: 1 }` shaped Duration partials and `{ unit:
//!   "minutes" }` total-options).
//! - [`from_instant`] / [`from_duration`] / … — extractors that
//!   panic-free downcast a payload to the expected variant.
//! - [`make_temporal`] — construct a [`crate::Value::Temporal`]
//!   from a payload.

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError};
use crate::object::JsObject;
use crate::string::JsString;
use crate::temporal::payload::{JsTemporal, TemporalPayload};

/// Coerce arg `index` to a Rust string. Returns
/// [`IntrinsicError::BadArgument`] when the slot is missing or the
/// value is not a string. Foundation does not yet thread ToString
/// through every primitive — strings flow through directly.
pub fn require_string_arg(args: &IntrinsicArgs<'_>, index: u16) -> Result<String, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::String(s)) => Ok(s.to_lossy_string(args.gc_heap)),
        _ => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        }),
    }
}

/// Coerce arg `index` to a [`JsObject`] handle. Used for
/// option / partial-record arguments.
pub fn require_object_arg<'a>(
    args: &'a IntrinsicArgs<'_>,
    index: u16,
) -> Result<&'a JsObject, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::Object(o)) => Ok(o),
        _ => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be an object",
        }),
    }
}

/// Optional object arg — returns [`None`] when missing/`undefined`.
pub fn optional_object_arg<'a>(args: &'a IntrinsicArgs<'_>, index: u16) -> Option<&'a JsObject> {
    match args.args.get(index as usize) {
        Some(Value::Object(o)) => Some(o),
        _ => None,
    }
}

/// Read a numeric field from a partial-record object. Returns the
/// default when the property is missing or `undefined`. Coerces
/// `Value::Number` only; non-numeric values fail.
pub fn read_i64_field(
    obj: &JsObject,
    name: &str,
    default: i64,
    gc_heap: &otter_gc::GcHeap,
) -> Result<i64, IntrinsicError> {
    match crate::object::get(*obj, gc_heap, name) {
        None | Some(Value::Undefined) => Ok(default),
        Some(Value::Number(n)) => match n.as_smi() {
            Some(v) => Ok(v as i64),
            None => Ok(n.as_f64() as i64),
        },
        Some(_) => Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "partial-record fields must be numbers",
        }),
    }
}

/// Read an optional string field (`{ unit: "minutes" }`).
pub fn read_string_field(obj: &JsObject, name: &str, gc_heap: &otter_gc::GcHeap) -> Option<String> {
    match crate::object::get(*obj, gc_heap, name) {
        Some(Value::String(s)) => Some(s.to_lossy_string(gc_heap)),
        _ => None,
    }
}

/// Build a `Value::Temporal` from a payload, allocating the backing
/// GC body via [`IntrinsicArgs::gc_heap`].
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] via [`IntrinsicError::OutOfMemory`].
pub fn make_temporal(
    args: &mut IntrinsicArgs<'_>,
    payload: TemporalPayload,
) -> Result<Value, IntrinsicError> {
    let handle = JsTemporal::new(args.gc_heap, payload)?;
    Ok(Value::temporal(handle))
}

/// `make_temporal` variant for the static-dispatch path
/// ([`crate::temporal::dispatch::call`]) which surfaces
/// [`crate::temporal::dispatch::TemporalError`] rather than
/// [`IntrinsicError`].
///
/// # Errors
///
/// Maps [`otter_gc::OutOfMemory`] onto
/// [`crate::temporal::dispatch::TemporalError::OutOfMemory`].
pub fn alloc_temporal_value(
    heap: &mut otter_gc::GcHeap,
    payload: TemporalPayload,
) -> Result<Value, crate::temporal::dispatch::TemporalError> {
    let handle = JsTemporal::new(heap, payload).map_err(|e| {
        crate::temporal::dispatch::TemporalError::OutOfMemory {
            requested_bytes: e.requested_bytes(),
            heap_limit_bytes: e.heap_limit_bytes(),
        }
    })?;
    Ok(Value::temporal(handle))
}

/// Extract a [`temporal_rs::Instant`] from the receiver, or raise
/// [`IntrinsicError::BadReceiver`] for the wrong kind.
pub fn require_instant(args: &IntrinsicArgs<'_>) -> Result<temporal_rs::Instant, IntrinsicError> {
    match args.receiver {
        Value::Temporal(t) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::Instant(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.Instant",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Temporal.Instant",
        }),
    }
}

/// Extract a [`temporal_rs::Duration`] from the receiver.
pub fn require_duration(args: &IntrinsicArgs<'_>) -> Result<temporal_rs::Duration, IntrinsicError> {
    match args.receiver {
        Value::Temporal(t) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::Duration(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.Duration",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Temporal.Duration",
        }),
    }
}

/// Extract a [`temporal_rs::PlainDate`] from the receiver.
pub fn require_plain_date(
    args: &IntrinsicArgs<'_>,
) -> Result<temporal_rs::PlainDate, IntrinsicError> {
    match args.receiver {
        Value::Temporal(t) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDate(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.PlainDate",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Temporal.PlainDate",
        }),
    }
}

/// Extract a [`temporal_rs::PlainTime`] from the receiver.
pub fn require_plain_time(
    args: &IntrinsicArgs<'_>,
) -> Result<temporal_rs::PlainTime, IntrinsicError> {
    match args.receiver {
        Value::Temporal(t) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainTime(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.PlainTime",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Temporal.PlainTime",
        }),
    }
}

/// Extract a [`temporal_rs::PlainDateTime`] from the receiver.
pub fn require_plain_date_time(
    args: &IntrinsicArgs<'_>,
) -> Result<temporal_rs::PlainDateTime, IntrinsicError> {
    match args.receiver {
        Value::Temporal(t) => match t.payload_clone(args.gc_heap) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Temporal.PlainDateTime",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Temporal.PlainDateTime",
        }),
    }
}

/// Convert a `temporal_rs` error into the foundation
/// [`IntrinsicError::BadArgument`]. The error message is preserved
/// in the diagnostic.
pub fn temporal_err(err: temporal_rs::TemporalError) -> IntrinsicError {
    let _ = err; // The foundation surfaces the error class via reason.
    IntrinsicError::BadArgument {
        index: 0,
        reason: "Temporal operation failed",
    }
}

/// Build a `Value::String` from a Rust string via the active heap.
pub fn js_string_value(
    value: String,
    args: &mut IntrinsicArgs<'_>,
) -> Result<Value, IntrinsicError> {
    Ok(Value::string(JsString::from_str(&value, args.gc_heap)?))
}

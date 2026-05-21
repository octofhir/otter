//! `Temporal.Now` — read-only views of the host clock.
//!
//! Each member here goes through `temporal_rs::Temporal::local_now()`
//! (host-system clock + host-system time zone). Fixtures that target
//! Now methods just verify a stable shape (instance kind, presence
//! of components) since the value moves with the wall clock.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-now-object>

use crate::Value;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::alloc_temporal_value;
use crate::temporal::payload::TemporalPayload;

/// Dispatch `Temporal.Now.<method>(args...)` via the typed
/// [`TemporalMethod`].
pub fn dispatch(
    gc_heap: &mut otter_gc::GcHeap,
    method: otter_bytecode::method_id::TemporalMethod,
    args: &[Value],
) -> Result<Value, TemporalError> {
    use otter_bytecode::method_id::TemporalMethod as M;
    let _ = args;
    match method {
        M::NowInstant => instant(gc_heap),
        M::NowPlainDateTimeISO => plain_date_time_iso(gc_heap),
        M::NowPlainDateISO => plain_date_iso(gc_heap),
        M::NowPlainTimeISO => plain_time_iso(gc_heap),
        other => Err(TemporalError::UnknownMember {
            class: "Now".to_string(),
            method: other.name().to_string(),
        }),
    }
}

fn instant(gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let inst = temporal_rs::sys::Temporal::local_now()
        .instant()
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "instant",
            message: e.to_string(),
        })?;
    alloc_temporal_value(gc_heap, TemporalPayload::Instant(inst))
}

fn plain_date_time_iso(gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pdt = temporal_rs::sys::Temporal::local_now()
        .plain_date_time_iso(None)
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "plainDateTimeISO",
            message: e.to_string(),
        })?;
    alloc_temporal_value(gc_heap, TemporalPayload::PlainDateTime(pdt))
}

fn plain_date_iso(gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pd = temporal_rs::sys::Temporal::local_now()
        .plain_date_iso(None)
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "plainDateISO",
            message: e.to_string(),
        })?;
    alloc_temporal_value(gc_heap, TemporalPayload::PlainDate(pd))
}

fn plain_time_iso(gc_heap: &mut otter_gc::GcHeap) -> Result<Value, TemporalError> {
    let pt = temporal_rs::sys::Temporal::local_now()
        .plain_time_iso(None)
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "plainTimeISO",
            message: e.to_string(),
        })?;
    alloc_temporal_value(gc_heap, TemporalPayload::PlainTime(pt))
}

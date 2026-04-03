//! Native payload wrapper for Temporal objects.
//!
//! Each Temporal type stores its `temporal_rs` value inside a `TemporalPayload`
//! enum, which is registered in the VM's `NativePayloadRegistry`. The payload
//! holds no VM references (no `ObjectHandle` / `RegisterValue`), so `VmTrace`
//! is a no-op.
//!
//! Spec: <https://tc39.es/proposal-temporal/>

use crate::object::ObjectHandle;
use crate::payload::{NativePayloadError, VmTrace, VmValueTracer};
use crate::value::RegisterValue;

/// Native payload for all eight Temporal value types.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TemporalPayload {
    Instant(temporal_rs::Instant),
    Duration(temporal_rs::Duration),
    PlainDate(temporal_rs::PlainDate),
    PlainTime(temporal_rs::PlainTime),
    PlainDateTime(temporal_rs::PlainDateTime),
    PlainYearMonth(temporal_rs::PlainYearMonth),
    PlainMonthDay(temporal_rs::PlainMonthDay),
    ZonedDateTime(temporal_rs::ZonedDateTime),
}

impl VmTrace for TemporalPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {
        // No VM references — temporal_rs types hold only Rust data.
    }
}

// ── Extraction helpers ──────────────────────────────────────────────

#[allow(dead_code)]
impl TemporalPayload {
    pub fn as_instant(&self) -> Option<&temporal_rs::Instant> {
        match self {
            Self::Instant(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_duration(&self) -> Option<&temporal_rs::Duration> {
        match self {
            Self::Duration(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_plain_date(&self) -> Option<&temporal_rs::PlainDate> {
        match self {
            Self::PlainDate(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_plain_time(&self) -> Option<&temporal_rs::PlainTime> {
        match self {
            Self::PlainTime(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_plain_date_time(&self) -> Option<&temporal_rs::PlainDateTime> {
        match self {
            Self::PlainDateTime(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_plain_year_month(&self) -> Option<&temporal_rs::PlainYearMonth> {
        match self {
            Self::PlainYearMonth(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_plain_month_day(&self) -> Option<&temporal_rs::PlainMonthDay> {
        match self {
            Self::PlainMonthDay(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_zoned_date_time(&self) -> Option<&temporal_rs::ZonedDateTime> {
        match self {
            Self::ZonedDateTime(v) => Some(v),
            _ => None,
        }
    }
}

// ── RuntimeState helpers ────────────────────────────────────────────

/// Extracts a `TemporalPayload` reference from a `this` value.
pub fn require_temporal_payload<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a TemporalPayload, NativePayloadError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or(NativePayloadError::ExpectedObjectValue)?;
    runtime.native_payload::<TemporalPayload>(handle)
}

/// Constructs a Temporal object: allocates a native object with the given
/// prototype and TemporalPayload.
pub fn construct_temporal(
    payload: TemporalPayload,
    prototype: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> ObjectHandle {
    runtime.alloc_native_object_with_prototype(Some(prototype), payload)
}

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
use crate::string::StringHeap;
use crate::temporal::dispatch::TemporalError;
use crate::temporal::helpers::make_temporal;
use crate::temporal::payload::TemporalPayload;

/// Dispatch `Temporal.Now.<method>(args...)`.
pub fn dispatch(
    string_heap: &StringHeap,
    method: &str,
    args: &[Value],
) -> Result<Value, TemporalError> {
    let _ = string_heap;
    let _ = args;
    match method {
        "instant" => instant(),
        "plainDateTimeISO" => plain_date_time_iso(),
        "plainDateISO" => plain_date_iso(),
        "plainTimeISO" => plain_time_iso(),
        other => Err(TemporalError::UnknownMember {
            class: "Now".to_string(),
            method: other.to_string(),
        }),
    }
}

fn instant() -> Result<Value, TemporalError> {
    let inst = temporal_rs::sys::Temporal::local_now()
        .instant()
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "instant",
            message: e.to_string(),
        })?;
    Ok(make_temporal(TemporalPayload::Instant(inst)))
}

fn plain_date_time_iso() -> Result<Value, TemporalError> {
    let pdt = temporal_rs::sys::Temporal::local_now()
        .plain_date_time_iso(None)
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "plainDateTimeISO",
            message: e.to_string(),
        })?;
    Ok(make_temporal(TemporalPayload::PlainDateTime(pdt)))
}

fn plain_date_iso() -> Result<Value, TemporalError> {
    let pd = temporal_rs::sys::Temporal::local_now()
        .plain_date_iso(None)
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "plainDateISO",
            message: e.to_string(),
        })?;
    Ok(make_temporal(TemporalPayload::PlainDate(pd)))
}

fn plain_time_iso() -> Result<Value, TemporalError> {
    let pt = temporal_rs::sys::Temporal::local_now()
        .plain_time_iso(None)
        .map_err(|e| TemporalError::Engine {
            class: "Now",
            method: "plainTimeISO",
            message: e.to_string(),
        })?;
    Ok(make_temporal(TemporalPayload::PlainTime(pt)))
}

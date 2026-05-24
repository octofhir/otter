//! `Temporal.*` namespace — modern date / time API.
//!
//! Foundation slice (task 39) ships five core types from the
//! ECMA-262 Temporal proposal: `Instant`, `Duration`, `PlainDate`,
//! `PlainTime`, `PlainDateTime`, plus the read-only `Temporal.Now`
//! views. Calendars beyond ISO and time-zones beyond UTC are
//! follow-up tasks; the surface that exists today routes through
//! `temporal_rs` (octoshikari-pinned) for every algorithm.
//!
//! # Contents
//! - [`payload`] — heap payload + `JsTemporal` handle + kind tag.
//! - [`dispatch`] — `Op::TemporalCall` / `Op::TemporalLoad` backend.
//!   Handles `Temporal.<Type>.from(...)` / `compare(...)` / `Now.*`.
//! - [`prototype`] — receiver-method dispatch for each Temporal
//!   kind. Split into one submodule per kind so the file fan-out
//!   stays readable.
//!
//! # Invariants
//! - Every Temporal method that fails (parse error, range overflow,
//!   …) surfaces as a [`crate::VmError::Uncaught`]-shaped runtime
//!   diagnostic so the JS-side `try/catch` sees a string the user
//!   can act on. Once `RangeError` / `TypeError` constructors land,
//!   the mapper here will produce real Error objects.
//! - JS-side methods that return new Temporal values clone via
//!   `temporal_rs`'s value-type semantics (no shared mutability).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

pub mod dispatch;
pub mod duration;
pub mod helpers;
pub mod instant;
pub mod intrinsic;
pub mod now;
pub mod payload;
pub mod plain_date;
pub mod plain_date_time;
pub mod plain_time;
pub mod plain_year_month;

pub use dispatch::{TemporalError, call as call_static, load_static};
pub use payload::{
    JsTemporal, TEMPORAL_BODY_TYPE_TAG, TemporalBody, TemporalHandle, TemporalKind,
    TemporalPayload, alloc_temporal,
};

use crate::Value;
use crate::intrinsics::IntrinsicEntry;

/// Resolve `<receiver-kind>.prototype.<name>` to the matching
/// intrinsic entry.
///
/// # Algorithm
/// 1. Inspect the receiver's [`TemporalKind`].
/// 2. Look up `name` in the kind's prototype table.
/// 3. Return [`None`] when the method is unknown — the dispatcher
///    raises `VmError::UnknownIntrinsic` from there.
#[must_use]
pub fn lookup_prototype(
    receiver: &Value,
    gc_heap: &otter_gc::GcHeap,
    name: &str,
) -> Option<&'static IntrinsicEntry> {
    let temporal = receiver.as_temporal(gc_heap)?;
    match temporal.kind() {
        TemporalKind::Instant => instant::lookup(name),
        TemporalKind::Duration => duration::lookup(name),
        TemporalKind::PlainDate => plain_date::lookup(name),
        TemporalKind::PlainTime => plain_time::lookup(name),
        TemporalKind::PlainDateTime => plain_date_time::lookup(name),
        TemporalKind::PlainYearMonth => plain_year_month::lookup(name),
    }
}

/// Read a non-method property off a Temporal value (component
/// accessors like `.year`, `.epochMilliseconds`, …). Returns
/// [`Value::Undefined`] for unknown names; the dispatcher uses this
/// from `Op::LoadProperty`.
#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    match temporal.kind() {
        TemporalKind::Instant => instant::load_property(temporal, gc_heap, name),
        TemporalKind::Duration => duration::load_property(temporal, gc_heap, name),
        TemporalKind::PlainDate => plain_date::load_property(temporal, gc_heap, name),
        TemporalKind::PlainTime => plain_time::load_property(temporal, gc_heap, name),
        TemporalKind::PlainDateTime => plain_date_time::load_property(temporal, gc_heap, name),
        TemporalKind::PlainYearMonth => plain_year_month::load_property(temporal, gc_heap, name),
    }
}

//! `Temporal.*` namespace.
//!
//! Each per-class module (`instant`, `duration`, `plain_date`, …)
//! owns its constructor body, its `&[MethodSpec]` prototype slice,
//! its property accessors, and its `couch!` installer; the
//! `intrinsic` module is the bootstrap driver that allocates the
//! `Temporal` namespace and calls each per-class installer.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

#![allow(missing_docs)]

pub mod duration;
pub mod helpers;
pub mod instant;
pub mod intrinsic;
pub mod native_dispatch;
pub mod now;
pub mod payload;
pub mod plain_date;
pub mod plain_date_time;
pub mod plain_month_day;
pub mod plain_time;
pub mod plain_year_month;
pub mod zoned_date_time;

pub use payload::{
    JsTemporal, TEMPORAL_BODY_TYPE_TAG, TemporalBody, TemporalHandle, TemporalKind,
    TemporalPayload, alloc_temporal,
};

use crate::Value;

#[must_use]
pub fn load_property(temporal: JsTemporal, gc_heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    match temporal.kind() {
        TemporalKind::Instant => instant::load_property(temporal, gc_heap, name),
        TemporalKind::Duration => duration::load_property(temporal, gc_heap, name),
        TemporalKind::PlainDate => plain_date::load_property(temporal, gc_heap, name),
        TemporalKind::PlainTime => plain_time::load_property(temporal, gc_heap, name),
        TemporalKind::PlainDateTime => plain_date_time::load_property(temporal, gc_heap, name),
        TemporalKind::PlainYearMonth => plain_year_month::load_property(temporal, gc_heap, name),
        TemporalKind::PlainMonthDay => plain_month_day::load_property(temporal, gc_heap, name),
        TemporalKind::ZonedDateTime => zoned_date_time::load_property(temporal, gc_heap, name),
    }
}

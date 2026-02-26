//! Native storage for Temporal types.
//!
//! Instead of storing temporal data as string properties on JsObject,
//! we store `temporal_rs` types directly in GC-managed objects.
//! This matches the V8/Deno approach for efficient access.

/// A GC-managed wrapper for `temporal_rs` types.
///
/// Stored as a single `__temporal_inner__` slot on the JsObject,
/// replacing the previous pattern of multiple string slots
/// (`__temporal_iso_year__`, `__temporal_iso_month__`, etc.).
pub enum TemporalValue {
    /// Temporal.PlainDate
    PlainDate(temporal_rs::PlainDate),
    /// Temporal.PlainTime
    PlainTime(temporal_rs::PlainTime),
    /// Temporal.PlainDateTime
    PlainDateTime(temporal_rs::PlainDateTime),
    /// Temporal.PlainYearMonth
    PlainYearMonth(temporal_rs::PlainYearMonth),
    /// Temporal.PlainMonthDay
    PlainMonthDay(temporal_rs::PlainMonthDay),
    /// Temporal.Instant
    Instant(temporal_rs::Instant),
    /// Temporal.ZonedDateTime
    ZonedDateTime(temporal_rs::ZonedDateTime),
    /// Temporal.Duration
    Duration(temporal_rs::Duration),
}

impl std::fmt::Debug for TemporalValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlainDate(d) => write!(f, "TemporalValue::PlainDate({:?})", d),
            Self::PlainTime(t) => write!(f, "TemporalValue::PlainTime({:?})", t),
            Self::PlainDateTime(dt) => write!(f, "TemporalValue::PlainDateTime({:?})", dt),
            Self::PlainYearMonth(ym) => write!(f, "TemporalValue::PlainYearMonth({:?})", ym),
            Self::PlainMonthDay(md) => write!(f, "TemporalValue::PlainMonthDay({:?})", md),
            Self::Instant(i) => write!(f, "TemporalValue::Instant({:?})", i),
            Self::ZonedDateTime(zdt) => write!(f, "TemporalValue::ZonedDateTime({:?})", zdt),
            Self::Duration(d) => write!(f, "TemporalValue::Duration({:?})", d),
        }
    }
}

// TemporalValue contains no GC references — all fields are plain Rust types.
impl otter_vm_gc::GcTraceable for TemporalValue {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::NONE;

    fn trace(&self, _tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // No GC references to trace
    }
}

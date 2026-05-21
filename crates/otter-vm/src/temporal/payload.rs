//! Heap payload for `Value::Temporal`.
//!
//! All `Temporal.*` values share one [`Value`](crate::Value) variant
//! that wraps an `Rc<TemporalPayload>`. Each variant of the payload
//! corresponds to one ECMA-262 / Temporal proposal type.
//!
//! The payload is immutable from JS's perspective — every method
//! that produces a new value (e.g. `add`, `subtract`, `with`) returns
//! a fresh [`JsTemporal`] handle. Cloning a handle is `Rc::clone`.
//!
//! # Contents
//! - [`TemporalPayload`] — sum type over the seven shipped Temporal
//!   value kinds.
//! - [`TemporalKind`] — light tag used by the dispatcher to route
//!   prototype lookups without inspecting the payload bytes.
//! - [`JsTemporal`] — heap handle.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

use std::rc::Rc;

use otter_gc::raw::SlotVisitor;

/// One Temporal value, parameterised over the [`temporal_rs`] type.
///
/// Foundation slice ships every variant the task acceptance criteria
/// require (`Instant`, `Duration`, `PlainDate`, `PlainTime`,
/// `PlainDateTime`); `PlainYearMonth`, `PlainMonthDay`,
/// `ZonedDateTime` are filed as follow-up tasks but the variants
/// exist here so the dispatcher does not need to grow.
#[derive(Debug, Clone)]
pub enum TemporalPayload {
    /// `Temporal.Instant` — point on the UTC timeline.
    Instant(temporal_rs::Instant),
    /// `Temporal.Duration` — calendar / time difference.
    Duration(temporal_rs::Duration),
    /// `Temporal.PlainDate` — `YYYY-MM-DD` calendar date.
    PlainDate(temporal_rs::PlainDate),
    /// `Temporal.PlainTime` — wall-clock time without a date.
    PlainTime(temporal_rs::PlainTime),
    /// `Temporal.PlainDateTime` — combined wall-clock date + time.
    PlainDateTime(temporal_rs::PlainDateTime),
}

impl TemporalPayload {
    /// Tag for routing prototype dispatch.
    #[must_use]
    pub fn kind(&self) -> TemporalKind {
        match self {
            TemporalPayload::Instant(_) => TemporalKind::Instant,
            TemporalPayload::Duration(_) => TemporalKind::Duration,
            TemporalPayload::PlainDate(_) => TemporalKind::PlainDate,
            TemporalPayload::PlainTime(_) => TemporalKind::PlainTime,
            TemporalPayload::PlainDateTime(_) => TemporalKind::PlainDateTime,
        }
    }
}

/// Light tag for [`TemporalPayload`] variants. Used by the
/// dispatcher to pick the right prototype table without re-matching
/// the payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TemporalKind {
    /// `Temporal.Instant` instance.
    Instant,
    /// `Temporal.Duration` instance.
    Duration,
    /// `Temporal.PlainDate` instance.
    PlainDate,
    /// `Temporal.PlainTime` instance.
    PlainTime,
    /// `Temporal.PlainDateTime` instance.
    PlainDateTime,
}

impl TemporalKind {
    /// JS-visible class name (`"Instant"` / `"Duration"` / …).
    #[must_use]
    pub const fn class_name(self) -> &'static str {
        match self {
            TemporalKind::Instant => "Instant",
            TemporalKind::Duration => "Duration",
            TemporalKind::PlainDate => "PlainDate",
            TemporalKind::PlainTime => "PlainTime",
            TemporalKind::PlainDateTime => "PlainDateTime",
        }
    }

    /// Resolve a `Temporal.<Type>` member name to its kind tag.
    #[must_use]
    pub fn from_class_name(name: &str) -> Option<Self> {
        Some(match name {
            "Instant" => TemporalKind::Instant,
            "Duration" => TemporalKind::Duration,
            "PlainDate" => TemporalKind::PlainDate,
            "PlainTime" => TemporalKind::PlainTime,
            "PlainDateTime" => TemporalKind::PlainDateTime,
            _ => return None,
        })
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`TemporalBody`].
pub const TEMPORAL_BODY_TYPE_TAG: u8 = 0x27;

/// GC-managed body for [`crate::Value::Temporal`] — migration target
/// for the legacy `JsTemporal { inner: Rc<TemporalPayload> }` wrapper.
#[derive(Debug, Clone)]
pub struct TemporalBody {
    /// The variant-typed Temporal payload (Instant / Duration /
    /// PlainDate / PlainTime / PlainDateTime).
    pub payload: TemporalPayload,
}

impl otter_gc::SafeTraceable for TemporalBody {
    const TYPE_TAG: u8 = TEMPORAL_BODY_TYPE_TAG;

    /// No outgoing GC slots — every Temporal variant wraps
    /// `temporal_rs::*` plain numeric data with no GC references.
    fn trace_slots_safe(&self, _visitor: &mut SlotVisitor<'_>) {}
}

/// 4-byte compressed GC handle to a [`TemporalBody`]. `Copy`.
pub type TemporalHandle = otter_gc::Gc<TemporalBody>;

/// Allocate a Temporal body on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_temporal(
    heap: &mut otter_gc::GcHeap,
    payload: TemporalPayload,
) -> Result<TemporalHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(TemporalBody { payload })
}

/// Heap-shared handle for [`crate::Value::Temporal`].
#[derive(Debug, Clone)]
pub struct JsTemporal {
    inner: Rc<TemporalPayload>,
}

impl JsTemporal {
    /// Wrap a payload in a fresh handle.
    #[must_use]
    pub fn new(payload: TemporalPayload) -> Self {
        Self {
            inner: Rc::new(payload),
        }
    }

    /// Borrow the payload.
    #[must_use]
    pub fn payload(&self) -> &TemporalPayload {
        &self.inner
    }

    /// Tag for prototype routing.
    #[must_use]
    pub fn kind(&self) -> TemporalKind {
        self.inner.kind()
    }

    /// Identity comparison via `Rc::ptr_eq`.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

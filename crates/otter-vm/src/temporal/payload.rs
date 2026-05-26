//! Heap payload for `Value::Temporal`.
//!
//! All `Temporal.*` values share one [`Value`](crate::Value) variant
//! that wraps a [`TemporalHandle`] (compressed `Gc<TemporalBody>`).
//! Each variant of the payload corresponds to one ECMA-262 / Temporal
//! proposal type.
//!
//! The payload is immutable from JS's perspective — every method that
//! produces a new value (e.g. `add`, `subtract`, `with`) allocates a
//! fresh [`JsTemporal`] handle.
//!
//! # Contents
//! - [`TemporalPayload`] — sum type over the shipped Temporal value
//!   kinds.
//! - [`TemporalKind`] — light tag used by the dispatcher to route
//!   prototype lookups without inspecting the payload bytes.
//! - [`JsTemporal`] — heap handle.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

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
    /// `Temporal.PlainYearMonth` — `YYYY-MM` calendar year+month.
    PlainYearMonth(temporal_rs::PlainYearMonth),
    /// `Temporal.PlainMonthDay` — `MM-DD` calendar month+day.
    PlainMonthDay(temporal_rs::PlainMonthDay),
    /// `Temporal.ZonedDateTime` — instant + IANA time zone + calendar.
    ZonedDateTime(temporal_rs::ZonedDateTime),
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
            TemporalPayload::PlainYearMonth(_) => TemporalKind::PlainYearMonth,
            TemporalPayload::PlainMonthDay(_) => TemporalKind::PlainMonthDay,
            TemporalPayload::ZonedDateTime(_) => TemporalKind::ZonedDateTime,
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
    /// `Temporal.PlainYearMonth` instance.
    PlainYearMonth,
    /// `Temporal.PlainMonthDay` instance.
    PlainMonthDay,
    /// `Temporal.ZonedDateTime` instance.
    ZonedDateTime,
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
            TemporalKind::PlainYearMonth => "PlainYearMonth",
            TemporalKind::PlainMonthDay => "PlainMonthDay",
            TemporalKind::ZonedDateTime => "ZonedDateTime",
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
            "PlainYearMonth" => TemporalKind::PlainYearMonth,
            "PlainMonthDay" => TemporalKind::PlainMonthDay,
            "ZonedDateTime" => TemporalKind::ZonedDateTime,
            _ => return None,
        })
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`TemporalBody`].
pub const TEMPORAL_BODY_TYPE_TAG: u8 = 0x27;

/// GC body holding a Temporal value's [`TemporalPayload`].
///
/// `temporal_rs::*` records hold no GC references; the derive emits
/// an empty `trace_slots_safe` body.
#[derive(Debug, Clone, otter_macros::Pelt)]
#[pelt(tag = TEMPORAL_BODY_TYPE_TAG)]
pub struct TemporalBody {
    /// Variant-typed Temporal payload.
    ///
    /// Boxed so the body stays 8-byte aligned. `temporal_rs::Instant`
    /// embeds an `i128` (epoch nanoseconds), giving [`TemporalPayload`]
    /// a 16-byte alignment the GC cage cannot satisfy inline: the
    /// allocator only guarantees [`otter_gc::OBJECT_ALIGNMENT`]-aligned
    /// (8-byte) cells and the payload sits one [`otter_gc::GcHeader`]
    /// (8 bytes) past the cell start, so an inline 16-aligned field
    /// would land on an 8-aligned address and trip the misaligned-read
    /// check. The `Box` moves the over-aligned record to a side
    /// allocation, matching the V8-style rule that managed cells never
    /// embed data needing more than pointer alignment.
    #[pelt(skip)]
    pub payload: Box<TemporalPayload>,
}

/// 4-byte compressed GC handle to a [`TemporalBody`]. `Copy`. Packs
/// into [`crate::Value`] under `TAG_PTR_OBJECT`.
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
    heap.alloc_old(TemporalBody {
        payload: Box::new(payload),
    })
}

/// Heap handle for [`crate::Value::Temporal`].
///
/// Backed by a GC body ([`TemporalBody`]). The wrapper caches the
/// lightweight [`TemporalKind`] discriminator so prototype routing and
/// `typeof`-style display avoid a heap touch; the full payload lives
/// in the GC body.
#[derive(Debug, Clone, Copy)]
pub struct JsTemporal {
    inner: TemporalHandle,
    kind: TemporalKind,
}

impl JsTemporal {
    /// Allocate a fresh handle wrapping `payload` on the GC heap.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        payload: TemporalPayload,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let kind = payload.kind();
        Ok(Self {
            inner: alloc_temporal(heap, payload)?,
            kind,
        })
    }

    /// Run `f` against the payload borrowed from the GC body.
    ///
    /// The closure receives `&TemporalPayload`; the borrow does not
    /// escape, so the call is sound under the single-mutator otter-gc
    /// contract.
    #[inline]
    #[must_use]
    pub fn with_payload<F, R>(self, heap: &otter_gc::GcHeap, f: F) -> R
    where
        F: FnOnce(&TemporalPayload) -> R,
    {
        heap.read_payload(self.inner, |body| f(&body.payload))
    }

    /// Clone the payload out of the GC body. Used by helpers that
    /// need to return the payload across a borrow boundary
    /// (`require_instant`, `require_duration`, …). `temporal_rs`
    /// value types are small `Clone` records — `Instant` / `Duration`
    /// / `PlainTime` are `Copy`; `PlainDate` / `PlainDateTime` clone
    /// a calendar tag.
    #[inline]
    #[must_use]
    pub fn payload_clone(self, heap: &otter_gc::GcHeap) -> TemporalPayload {
        heap.read_payload(self.inner, |body| (*body.payload).clone())
    }

    /// Tag for prototype routing. Read from the wrapper-side cache
    /// without a heap touch.
    #[inline]
    #[must_use]
    pub fn kind(self) -> TemporalKind {
        self.kind
    }

    /// Raw GC handle — used by tracing and write barriers.
    #[doc(hidden)]
    #[inline]
    #[must_use]
    pub fn handle(self) -> TemporalHandle {
        self.inner
    }

    /// Rebuild a [`JsTemporal`] from a pre-existing [`TemporalHandle`].
    /// Reads the body once to recover the cached
    /// [`TemporalKind`] discriminator.
    #[inline]
    #[must_use]
    pub fn from_handle(heap: &otter_gc::GcHeap, handle: TemporalHandle) -> Self {
        let kind = heap.read_payload(handle, |body| body.payload.kind());
        Self {
            inner: handle,
            kind,
        }
    }

    /// Identity comparison — `===` follows compressed-offset equality.
    #[inline]
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.inner == other.inner
    }
}

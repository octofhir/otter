//! Heap payload for `Value::Intl`.
//!
//! Each `Intl.*` instance wraps an [`IntlPayload`] whose variant
//! identifies the constructor that produced it. The payload is
//! immutable after construction; method calls (`.format(x)` /
//! `.compare(a, b)`) read the resolved option set and pass it to
//! the underlying ICU formatter / collator on demand.
//!
//! # Contents
//! - [`IntlPayload`] — sum type over the three shipped constructors.
//! - [`IntlKind`] — light tag used by the dispatcher.
//! - [`JsIntl`] — heap handle.
//!
//! # See also
//! - <https://tc39.es/ecma402/>

use icu_collator::options::{CaseLevel, Strength};

/// Resolved option bag for `Intl.Collator`.
#[derive(Debug, Clone)]
pub struct CollatorPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `usage` option (`"sort"` / `"search"`). Foundation defaults to
    /// `"sort"`.
    pub usage: String,
    /// `sensitivity` option (`"base"` / `"accent"` / `"case"` /
    /// `"variant"`).
    pub sensitivity: String,
    /// `ignorePunctuation` option.
    pub ignore_punctuation: bool,
    /// `numeric` option.
    pub numeric: bool,
    /// `caseFirst` option (`"upper"` / `"lower"` / `"false"`).
    pub case_first: String,
}

impl CollatorPayload {
    /// Map the spec-`sensitivity` string to the ICU strength / case
    /// level pair.
    #[must_use]
    pub fn icu_strength(&self) -> (Option<Strength>, Option<CaseLevel>) {
        match self.sensitivity.as_str() {
            "base" => (Some(Strength::Primary), None),
            "accent" => (Some(Strength::Secondary), None),
            "case" => (Some(Strength::Primary), Some(CaseLevel::On)),
            // "variant" or unknown — default to Tertiary.
            _ => (Some(Strength::Tertiary), None),
        }
    }
}

/// Resolved option bag for `Intl.NumberFormat`.
#[derive(Debug, Clone)]
pub struct NumberFormatPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `style` option (`"decimal"` / `"currency"` / `"percent"`).
    pub style: String,
    /// `currency` option (ISO-4217 code) — set only when
    /// `style == "currency"`.
    pub currency: Option<String>,
    /// `minimumFractionDigits` (default depends on style).
    pub minimum_fraction_digits: u8,
    /// `maximumFractionDigits` (default depends on style).
    pub maximum_fraction_digits: u8,
    /// `useGrouping` option.
    pub use_grouping: bool,
}

/// Resolved option bag for `Intl.DateTimeFormat`.
#[derive(Debug, Clone)]
pub struct DateTimeFormatPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// Whether the `year` field is present.
    pub year: bool,
    /// Whether the `month` field is present (rendered as numeric).
    pub month: bool,
    /// Whether the `day` field is present.
    pub day: bool,
    /// Whether the `hour` field is present.
    pub hour: bool,
    /// Whether the `minute` field is present.
    pub minute: bool,
    /// Whether the `second` field is present.
    pub second: bool,
}

/// Resolved option bag for `Intl.PluralRules`.
#[derive(Debug, Clone)]
pub struct PluralRulesPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `type` option (`"cardinal"` / `"ordinal"`).
    pub kind: String,
    /// `minimumIntegerDigits`.
    pub minimum_integer_digits: u8,
    /// `minimumFractionDigits`.
    pub minimum_fraction_digits: u8,
    /// `maximumFractionDigits`.
    pub maximum_fraction_digits: u8,
}

/// Resolved option bag for `Intl.RelativeTimeFormat`.
#[derive(Debug, Clone)]
pub struct RelativeTimeFormatPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `style` option (`"long"` / `"short"` / `"narrow"`).
    pub style: String,
    /// `numeric` option (`"always"` / `"auto"`).
    pub numeric: String,
}

/// Resolved option bag for `Intl.ListFormat`.
#[derive(Debug, Clone)]
pub struct ListFormatPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `type` option (`"conjunction"` / `"disjunction"` / `"unit"`).
    pub kind: String,
    /// `style` option (`"long"` / `"short"` / `"narrow"`).
    pub style: String,
}

/// Resolved option bag for `Intl.DisplayNames`.
#[derive(Debug, Clone)]
pub struct DisplayNamesPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `type` option (`"language"` / `"region"` / `"script"` /
    /// `"currency"` / `"calendar"` / `"dateTimeField"`).
    pub kind: String,
    /// `style` option (`"long"` / `"short"` / `"narrow"`).
    pub style: String,
    /// `fallback` option (`"code"` / `"none"`).
    pub fallback: String,
}

/// Resolved option bag for `Intl.Segmenter`.
#[derive(Debug, Clone)]
pub struct SegmenterPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `granularity` option (`"grapheme"` / `"word"` / `"sentence"`).
    pub granularity: String,
}

/// One [`crate::Value::Intl`] instance.
#[derive(Debug, Clone)]
pub enum IntlPayload {
    /// `new Intl.Collator(...)` instance.
    Collator(CollatorPayload),
    /// `new Intl.NumberFormat(...)` instance.
    NumberFormat(NumberFormatPayload),
    /// `new Intl.DateTimeFormat(...)` instance.
    DateTimeFormat(DateTimeFormatPayload),
    /// `new Intl.PluralRules(...)` instance.
    PluralRules(PluralRulesPayload),
    /// `new Intl.RelativeTimeFormat(...)` instance.
    RelativeTimeFormat(RelativeTimeFormatPayload),
    /// `new Intl.ListFormat(...)` instance.
    ListFormat(ListFormatPayload),
    /// `new Intl.DisplayNames(...)` instance.
    DisplayNames(DisplayNamesPayload),
    /// `new Intl.Segmenter(...)` instance.
    Segmenter(SegmenterPayload),
}

impl IntlPayload {
    /// Tag for prototype routing.
    #[must_use]
    pub fn kind(&self) -> IntlKind {
        match self {
            IntlPayload::Collator(_) => IntlKind::Collator,
            IntlPayload::NumberFormat(_) => IntlKind::NumberFormat,
            IntlPayload::DateTimeFormat(_) => IntlKind::DateTimeFormat,
            IntlPayload::PluralRules(_) => IntlKind::PluralRules,
            IntlPayload::RelativeTimeFormat(_) => IntlKind::RelativeTimeFormat,
            IntlPayload::ListFormat(_) => IntlKind::ListFormat,
            IntlPayload::DisplayNames(_) => IntlKind::DisplayNames,
            IntlPayload::Segmenter(_) => IntlKind::Segmenter,
        }
    }
}

/// Light tag for prototype-routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntlKind {
    /// `Intl.Collator` instance.
    Collator,
    /// `Intl.NumberFormat` instance.
    NumberFormat,
    /// `Intl.DateTimeFormat` instance.
    DateTimeFormat,
    /// `Intl.PluralRules` instance.
    PluralRules,
    /// `Intl.RelativeTimeFormat` instance.
    RelativeTimeFormat,
    /// `Intl.ListFormat` instance.
    ListFormat,
    /// `Intl.DisplayNames` instance.
    DisplayNames,
    /// `Intl.Segmenter` instance.
    Segmenter,
}

impl IntlKind {
    /// JS-visible class name.
    #[must_use]
    pub const fn class_name(self) -> &'static str {
        match self {
            IntlKind::Collator => "Collator",
            IntlKind::NumberFormat => "NumberFormat",
            IntlKind::DateTimeFormat => "DateTimeFormat",
            IntlKind::PluralRules => "PluralRules",
            IntlKind::RelativeTimeFormat => "RelativeTimeFormat",
            IntlKind::ListFormat => "ListFormat",
            IntlKind::DisplayNames => "DisplayNames",
            IntlKind::Segmenter => "Segmenter",
        }
    }

    /// Resolve `Intl.<Class>` to its kind tag.
    #[must_use]
    pub fn from_class_name(name: &str) -> Option<Self> {
        Some(match name {
            "Collator" => IntlKind::Collator,
            "NumberFormat" => IntlKind::NumberFormat,
            "DateTimeFormat" => IntlKind::DateTimeFormat,
            "PluralRules" => IntlKind::PluralRules,
            "RelativeTimeFormat" => IntlKind::RelativeTimeFormat,
            "ListFormat" => IntlKind::ListFormat,
            "DisplayNames" => IntlKind::DisplayNames,
            "Segmenter" => IntlKind::Segmenter,
            _ => return None,
        })
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`IntlBody`].
pub const INTL_BODY_TYPE_TAG: u8 = 0x28;

/// GC-managed body for [`crate::Value::Intl`] — migration target for
/// the legacy `JsIntl { inner: Rc<IntlPayload> }` wrapper.
#[derive(Debug)]
pub struct IntlBody {
    /// Variant-typed Intl payload (Collator / NumberFormat /
    /// DateTimeFormat / …).
    pub payload: IntlPayload,
}

impl otter_gc::SafeTraceable for IntlBody {
    const TYPE_TAG: u8 = INTL_BODY_TYPE_TAG;

    /// No outgoing GC slots — Intl payloads wrap ICU library state
    /// (collators, formatters) which holds no GC references.
    fn trace_slots_safe(&self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

/// 4-byte compressed GC handle to an [`IntlBody`]. `Copy`.
pub type IntlHandle = otter_gc::Gc<IntlBody>;

/// Allocate an Intl body on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_intl(
    heap: &mut otter_gc::GcHeap,
    payload: IntlPayload,
) -> Result<IntlHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(IntlBody { payload })
}

/// Heap handle for [`crate::Value::Intl`].
///
/// Backed by a GC body ([`IntlBody`]) — the legacy `Rc<IntlPayload>`
/// storage has been retired. The wrapper carries a cached
/// [`IntlKind`] discriminator so prototype routing and `typeof`
/// display can avoid a heap read; the full payload still lives in
/// the GC body.
#[derive(Debug, Clone, Copy)]
pub struct JsIntl {
    inner: IntlHandle,
    kind: IntlKind,
}

impl JsIntl {
    /// Allocate a fresh handle wrapping `payload` on the GC heap.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        payload: IntlPayload,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let kind = payload.kind();
        Ok(Self {
            inner: alloc_intl(heap, payload)?,
            kind,
        })
    }

    /// Run `f` against the payload borrowed from the GC body.
    ///
    /// The closure receives `&IntlPayload`; the borrow does not
    /// escape so the call is sound against the single-mutator
    /// otter-gc contract.
    #[inline]
    #[must_use]
    pub fn with_payload<F, R>(self, heap: &otter_gc::GcHeap, f: F) -> R
    where
        F: FnOnce(&IntlPayload) -> R,
    {
        heap.read_payload(self.inner, |body| f(&body.payload))
    }

    /// Clone the payload out of the GC body. Used by call sites that
    /// need to return the payload across a borrow boundary (e.g. the
    /// per-variant `require_X` helpers). `IntlPayload` variants hold
    /// small Clone-able settings + ICU objects; cost is acceptable
    /// for non-hot Intl call sites.
    #[inline]
    #[must_use]
    pub fn payload_clone(self, heap: &otter_gc::GcHeap) -> IntlPayload {
        heap.read_payload(self.inner, |body| body.payload.clone())
    }

    /// Tag for prototype routing. Read from the wrapper-side cache
    /// without a heap touch.
    #[inline]
    #[must_use]
    pub fn kind(self) -> IntlKind {
        self.kind
    }

    /// Raw GC handle — used by tracing and write barriers.
    #[doc(hidden)]
    #[inline]
    #[must_use]
    pub fn handle(self) -> IntlHandle {
        self.inner
    }

    /// Identity comparison — `===` follows compressed-offset equality.
    #[inline]
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.inner == other.inner
    }
}

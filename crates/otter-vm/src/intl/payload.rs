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
    /// `signDisplay` option (`"auto"` / `"always"` / `"never"` /
    /// `"exceptZero"` / `"negative"`) — controls when a plus/minus sign
    /// is shown.
    pub sign_display: String,
    /// `notation` option (`"standard"` / `"scientific"` /
    /// `"engineering"` / `"compact"`).
    pub notation: String,
}

/// Text-component width (`weekday`, `era`, `dayPeriod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtTextWidth {
    /// `"narrow"`.
    Narrow,
    /// `"short"`.
    Short,
    /// `"long"`.
    Long,
}

/// Numeric-component width (`year`, `day`, `hour`, `minute`, `second`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtNumWidth {
    /// `"numeric"`.
    Numeric,
    /// `"2-digit"`.
    TwoDigit,
}

/// `month` width — numeric or textual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtMonthWidth {
    /// `"numeric"`.
    Numeric,
    /// `"2-digit"`.
    TwoDigit,
    /// `"narrow"`.
    Narrow,
    /// `"short"`.
    Short,
    /// `"long"`.
    Long,
}

/// `timeZoneName` style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtZoneName {
    /// `"long"`.
    Long,
    /// `"short"`.
    Short,
    /// `"shortOffset"`.
    ShortOffset,
    /// `"longOffset"`.
    LongOffset,
    /// `"shortGeneric"`.
    ShortGeneric,
    /// `"longGeneric"`.
    LongGeneric,
}

/// `dateStyle` / `timeStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtStyle {
    /// `"full"`.
    Full,
    /// `"long"`.
    Long,
    /// `"medium"`.
    Medium,
    /// `"short"`.
    Short,
}

/// Resolved hour cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtHourCycle {
    /// `"h11"`.
    H11,
    /// `"h12"`.
    H12,
    /// `"h23"`.
    H23,
    /// `"h24"`.
    H24,
}

/// Resolved option bag for `Intl.DateTimeFormat`. Each component carries
/// its requested width (`None` = absent), mirroring ECMA-402
/// §11.1.2 `CreateDateTimeFormat`.
#[derive(Debug, Clone)]
pub struct DateTimeFormatPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// `weekday` width.
    pub weekday: Option<DtTextWidth>,
    /// `era` width.
    pub era: Option<DtTextWidth>,
    /// `year` width.
    pub year: Option<DtNumWidth>,
    /// `month` width.
    pub month: Option<DtMonthWidth>,
    /// `day` width.
    pub day: Option<DtNumWidth>,
    /// `dayPeriod` width.
    pub day_period: Option<DtTextWidth>,
    /// `hour` width.
    pub hour: Option<DtNumWidth>,
    /// `minute` width.
    pub minute: Option<DtNumWidth>,
    /// `second` width.
    pub second: Option<DtNumWidth>,
    /// `fractionalSecondDigits` (1..=3).
    pub fractional_second_digits: Option<u8>,
    /// `timeZoneName` style.
    pub time_zone_name: Option<DtZoneName>,
    /// Resolved `hourCycle`.
    pub hour_cycle: Option<DtHourCycle>,
    /// `hour12` request.
    pub hour12: Option<bool>,
    /// `dateStyle`.
    pub date_style: Option<DtStyle>,
    /// `timeStyle`.
    pub time_style: Option<DtStyle>,
    /// `timeZone` identifier.
    pub time_zone: Option<String>,
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

/// Resolved option bag for `Intl.DurationFormat`.
#[derive(Debug, Clone)]
pub struct DurationFormatPayload {
    /// Spec-resolved BCP-47 locale tag.
    pub locale: String,
    /// Resolved `numberingSystem`.
    pub numbering_system: String,
    /// `style` option (`"long"` / `"short"` / `"narrow"` / `"digital"`).
    pub style: String,
    /// Per-unit `(style, display)` for the ten duration units in spec
    /// order: years, months, weeks, days, hours, minutes, seconds,
    /// milliseconds, microseconds, nanoseconds.
    pub units: Vec<(String, String)>,
    /// `fractionalDigits` option (absent → `None`).
    pub fractional_digits: Option<u8>,
}

/// Resolved data for `Intl.Locale`.
///
/// Stores the canonical `[[Locale]]` BCP-47 string. Every getter
/// (`language`, `script`, `calendar`, …) re-parses this string with
/// `icu_locale` on demand — Locale instances are cold relative to
/// formatters, so the re-parse cost is acceptable and avoids holding
/// a non-`Copy` ICU `Locale` in the GC body.
#[derive(Debug, Clone)]
pub struct LocalePayload {
    /// Canonical `[[Locale]]` string (language id + sorted Unicode
    /// extension keywords).
    pub locale: String,
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
    /// `new Intl.Locale(...)` instance.
    Locale(LocalePayload),
    /// `new Intl.DurationFormat(...)` instance.
    DurationFormat(DurationFormatPayload),
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
            IntlPayload::Locale(_) => IntlKind::Locale,
            IntlPayload::DurationFormat(_) => IntlKind::DurationFormat,
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
    /// `Intl.Locale` instance.
    Locale,
    /// `Intl.DurationFormat` instance.
    DurationFormat,
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
            IntlKind::Locale => "Locale",
            IntlKind::DurationFormat => "DurationFormat",
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
            "Locale" => IntlKind::Locale,
            "DurationFormat" => IntlKind::DurationFormat,
            _ => return None,
        })
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`IntlBody`].
pub const INTL_BODY_TYPE_TAG: u8 = 0x28;

/// GC body holding an Intl value's [`IntlPayload`].
///
/// ICU formatter / collator state holds no GC references; the derive
/// emits an empty `trace_slots_safe` body.
#[derive(Debug, otter_macros::Pelt)]
#[pelt(tag = INTL_BODY_TYPE_TAG)]
pub struct IntlBody {
    /// Variant-typed Intl payload.
    #[pelt(skip)]
    pub payload: IntlPayload,
}

/// 4-byte compressed GC handle to an [`IntlBody`]. `Copy`. Packs
/// into [`crate::Value`] under `TAG_PTR_OBJECT`.
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
/// Backed by a GC body ([`IntlBody`]); the wrapper carries a cached
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

    /// Rebuild a [`JsIntl`] from a pre-existing [`IntlHandle`]. Reads
    /// the body once to recover the cached [`IntlKind`] discriminator.
    #[inline]
    #[must_use]
    pub fn from_handle(heap: &otter_gc::GcHeap, handle: IntlHandle) -> Self {
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

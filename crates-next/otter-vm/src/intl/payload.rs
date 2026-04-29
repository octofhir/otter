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

use std::rc::Rc;

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

/// One [`crate::Value::Intl`] instance.
#[derive(Debug, Clone)]
pub enum IntlPayload {
    /// `new Intl.Collator(...)` instance.
    Collator(CollatorPayload),
    /// `new Intl.NumberFormat(...)` instance.
    NumberFormat(NumberFormatPayload),
    /// `new Intl.DateTimeFormat(...)` instance.
    DateTimeFormat(DateTimeFormatPayload),
}

impl IntlPayload {
    /// Tag for prototype routing.
    #[must_use]
    pub fn kind(&self) -> IntlKind {
        match self {
            IntlPayload::Collator(_) => IntlKind::Collator,
            IntlPayload::NumberFormat(_) => IntlKind::NumberFormat,
            IntlPayload::DateTimeFormat(_) => IntlKind::DateTimeFormat,
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
}

impl IntlKind {
    /// JS-visible class name.
    #[must_use]
    pub const fn class_name(self) -> &'static str {
        match self {
            IntlKind::Collator => "Collator",
            IntlKind::NumberFormat => "NumberFormat",
            IntlKind::DateTimeFormat => "DateTimeFormat",
        }
    }

    /// Resolve `Intl.<Class>` to its kind tag.
    #[must_use]
    pub fn from_class_name(name: &str) -> Option<Self> {
        Some(match name {
            "Collator" => IntlKind::Collator,
            "NumberFormat" => IntlKind::NumberFormat,
            "DateTimeFormat" => IntlKind::DateTimeFormat,
            _ => return None,
        })
    }
}

/// Heap handle for [`crate::Value::Intl`].
#[derive(Debug, Clone)]
pub struct JsIntl {
    inner: Rc<IntlPayload>,
}

impl JsIntl {
    /// Wrap a payload in a fresh handle.
    #[must_use]
    pub fn new(payload: IntlPayload) -> Self {
        Self {
            inner: Rc::new(payload),
        }
    }

    /// Borrow the payload.
    #[must_use]
    pub fn payload(&self) -> &IntlPayload {
        &self.inner
    }

    /// Tag for prototype routing.
    #[must_use]
    pub fn kind(&self) -> IntlKind {
        self.inner.kind()
    }

    /// Identity comparison via `Rc::ptr_eq`.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

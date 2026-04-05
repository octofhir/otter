//! Native payload wrapper for Intl objects.
//!
//! Each Intl type stores its options/state inside an `IntlPayload` enum,
//! registered in the VM's `NativePayloadRegistry`. The payload holds no VM
//! references (`ObjectHandle` / `RegisterValue`), so `VmTrace` is a no-op.
//!
//! Spec: <https://tc39.es/ecma402/>

use crate::object::ObjectHandle;
use crate::payload::{NativePayloadError, VmTrace, VmValueTracer};
use crate::value::RegisterValue;

// ── Resolved option structs ────────────────────────────────────────

/// §11.1.3 Intl.Collator internal slots.
/// Spec: <https://tc39.es/ecma402/#sec-intl-collator-internal-slots>
#[derive(Debug, Clone)]
pub struct CollatorData {
    pub locale: String,
    pub usage: CollatorUsage,
    pub sensitivity: CollatorSensitivity,
    pub ignore_punctuation: bool,
    pub collation: String,
    pub numeric: bool,
    pub case_first: CollatorCaseFirst,
}

/// §11.1 Collator usage option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollatorUsage {
    Sort,
    Search,
}

/// §11.1 Collator sensitivity option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollatorSensitivity {
    Base,
    Accent,
    Case,
    Variant,
}

/// §11.1 Collator caseFirst option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollatorCaseFirst {
    Upper,
    Lower,
    False,
}

/// §15.1.3 Intl.NumberFormat internal slots.
/// Spec: <https://tc39.es/ecma402/#sec-intl-numberformat-internal-slots>
#[derive(Debug, Clone)]
pub struct NumberFormatData {
    pub locale: String,
    pub style: NumberFormatStyle,
    pub currency: Option<String>,
    pub currency_display: CurrencyDisplay,
    pub currency_sign: CurrencySign,
    pub unit: Option<String>,
    pub unit_display: UnitDisplay,
    pub notation: Notation,
    pub compact_display: CompactDisplay,
    pub use_grouping: UseGrouping,
    pub sign_display: SignDisplay,
    pub minimum_integer_digits: u32,
    pub minimum_fraction_digits: Option<u32>,
    pub maximum_fraction_digits: Option<u32>,
    pub minimum_significant_digits: Option<u32>,
    pub maximum_significant_digits: Option<u32>,
    pub rounding_increment: u32,
    pub rounding_mode: RoundingMode,
    pub rounding_priority: RoundingPriority,
    pub trailing_zero_display: TrailingZeroDisplay,
    pub numbering_system: String,
}

/// §15.1 NumberFormat style option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberFormatStyle {
    Decimal,
    Currency,
    Percent,
    Unit,
}

/// §15.1 NumberFormat currencyDisplay option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrencyDisplay {
    Code,
    Symbol,
    NarrowSymbol,
    Name,
}

/// §15.1 NumberFormat currencySign option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrencySign {
    Standard,
    Accounting,
}

/// §15.1 NumberFormat unit display option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitDisplay {
    Short,
    Narrow,
    Long,
}

/// §15.1 NumberFormat notation option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notation {
    Standard,
    Scientific,
    Engineering,
    Compact,
}

/// §15.1 NumberFormat compactDisplay option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactDisplay {
    Short,
    Long,
}

/// §15.1 NumberFormat useGrouping option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseGrouping {
    Always,
    Auto,
    Min2,
    False,
}

/// §15.1 NumberFormat signDisplay option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignDisplay {
    Auto,
    Never,
    Always,
    ExceptZero,
    Negative,
}

/// §15.1 NumberFormat roundingMode option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundingMode {
    Ceil,
    Floor,
    Expand,
    Trunc,
    HalfCeil,
    HalfFloor,
    HalfExpand,
    HalfTrunc,
    HalfEven,
}

/// §15.1 NumberFormat roundingPriority option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundingPriority {
    Auto,
    MorePrecision,
    LessPrecision,
}

/// §15.1 NumberFormat trailingZeroDisplay option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailingZeroDisplay {
    Auto,
    StripIfInteger,
}

/// §13.1.3 Intl.PluralRules internal slots.
/// Spec: <https://tc39.es/ecma402/#sec-intl-pluralrules-internal-slots>
#[derive(Debug, Clone)]
pub struct PluralRulesData {
    pub locale: String,
    pub plural_type: PluralRulesType,
    pub minimum_integer_digits: u32,
    pub minimum_fraction_digits: Option<u32>,
    pub maximum_fraction_digits: Option<u32>,
    pub minimum_significant_digits: Option<u32>,
    pub maximum_significant_digits: Option<u32>,
}

/// §13.1 PluralRules type option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralRulesType {
    Cardinal,
    Ordinal,
}

/// §14.1.3 Intl.Locale internal slots (simplified).
/// Spec: <https://tc39.es/ecma402/#sec-intl-locale-internal-slots>
#[derive(Debug, Clone)]
pub struct LocaleData {
    pub locale: String,
    pub calendar: Option<String>,
    pub collation: Option<String>,
    pub numbering_system: Option<String>,
}

/// §12.1.3 Intl.DateTimeFormat internal slots.
/// Spec: <https://tc39.es/ecma402/#sec-intl-datetimeformat-internal-slots>
#[derive(Debug, Clone)]
pub struct DateTimeFormatData {
    pub locale: String,
    pub calendar: String,
    pub numbering_system: String,
    pub time_zone: String,
    pub date_style: Option<DateTimeStyle>,
    pub time_style: Option<DateTimeStyle>,
    // Component options (set when dateStyle/timeStyle are not used).
    pub weekday: Option<String>,
    pub era: Option<String>,
    pub year: Option<String>,
    pub month: Option<String>,
    pub day: Option<String>,
    pub day_period: Option<String>,
    pub hour: Option<String>,
    pub minute: Option<String>,
    pub second: Option<String>,
    pub fractional_second_digits: Option<u8>,
    pub time_zone_name: Option<String>,
}

/// §12.1 DateTimeFormat dateStyle / timeStyle option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeStyle {
    Full,
    Long,
    Medium,
    Short,
}

// ── Payload enum ───────────────────────────────────────────────────

/// Native payload for all Intl type instances.
#[derive(Debug, Clone)]
pub enum IntlPayload {
    Collator(CollatorData),
    NumberFormat(NumberFormatData),
    PluralRules(PluralRulesData),
    Locale(LocaleData),
    DateTimeFormat(DateTimeFormatData),
}

impl VmTrace for IntlPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {
        // No VM references — Intl payloads hold only Rust data.
    }
}

// ── Extraction helpers ─────────────────────────────────────────────

impl IntlPayload {
    pub fn as_collator(&self) -> Option<&CollatorData> {
        match self {
            Self::Collator(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_number_format(&self) -> Option<&NumberFormatData> {
        match self {
            Self::NumberFormat(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_plural_rules(&self) -> Option<&PluralRulesData> {
        match self {
            Self::PluralRules(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_locale(&self) -> Option<&LocaleData> {
        match self {
            Self::Locale(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_date_time_format(&self) -> Option<&DateTimeFormatData> {
        match self {
            Self::DateTimeFormat(v) => Some(v),
            _ => None,
        }
    }
}

// ── RuntimeState helpers ───────────────────────────────────────────

/// Extracts an `IntlPayload` reference from a `this` value.
pub fn require_intl_payload<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a IntlPayload, NativePayloadError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or(NativePayloadError::ExpectedObjectValue)?;
    runtime.native_payload::<IntlPayload>(handle)
}

/// Constructs an Intl object: allocates a native object with the given
/// prototype and `IntlPayload`.
pub fn construct_intl(
    payload: IntlPayload,
    prototype: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> ObjectHandle {
    runtime.alloc_native_object_with_prototype(Some(prototype), payload)
}

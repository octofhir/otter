//! `Intl.*` namespace — locale-aware string / number / date
//! formatting backed by ICU 4X.
//!
//! Foundation slice (task 40) ships three constructors:
//! `Intl.Collator`, `Intl.NumberFormat`, `Intl.DateTimeFormat`. The
//! remaining constructors (`PluralRules`, `RelativeTimeFormat`,
//! `ListFormat`, `DisplayNames`, `Segmenter`) are filed as
//! follow-up tasks. Locale resolution falls back to `"en-US"` when
//! the requested tag is unknown, matching the spec's lookup-only
//! algorithm without the full BestFitMatcher.
//!
//! # Contents
//! - [`payload`] — `IntlPayload` enum + `JsIntl` handle + per-class
//!   resolved option bags.
//! - [`helpers`] — locale / option-bag coercion utilities shared
//!   across the per-class modules, including the spec `GetOption`
//!   ladder that fires JS option getters in observation order.
//! - [`collator`] — `Intl.Collator` static + prototype.
//! - [`number_format`] — `Intl.NumberFormat` static + prototype.
//! - [`date_time_format`] — `Intl.DateTimeFormat` static + prototype.
//!
//! # Invariants
//! - Construction is eager: every option that affects formatting is
//!   resolved at `new Intl.X(...)` time and stashed on the
//!   payload. Method calls re-instantiate the underlying ICU
//!   formatter / collator on demand because the ICU types are
//!   borrow-locked to a specific `Locale`.
//! - Every class constructs through its own `NativeCtx`-based
//!   constructor (`resolve_ctx`), so option getters fire in spec
//!   order with proper coercion and `RangeError` validation.
//!
//! # Binary-size note
//! Pulling in `icu_collator` / `icu_decimal` / `icu_datetime` with
//! `compiled_data` features adds ~3 MiB to a release `otter` binary.
//! That cost is justified by the spec-coverage win — every
//! production JS engine ships ICU.
//!
//! # See also
//! - <https://tc39.es/ecma402/>

pub mod bootstrap;
pub mod collator;
pub mod date_time_format;
pub mod display_names;
pub mod duration_format;
pub mod helpers;
pub mod list_format;
pub mod locale;
pub mod namespace;
pub mod number_format;
pub mod payload;
pub mod plural_rules;
pub mod relative_time_format;
pub mod segmenter;
pub mod supported;

pub use payload::{
    CollatorPayload, DateTimeFormatPayload, INTL_BODY_TYPE_TAG, IntlBody, IntlHandle, IntlKind,
    IntlPayload, JsIntl, NumberFormatPayload, alloc_intl,
};

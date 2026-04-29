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
//! - [`dispatch`] — central `Op::NewIntl` constructor router.
//! - [`helpers`] — locale / option-bag coercion utilities shared
//!   across the three per-class modules.
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
//! - All public failure modes flow through [`dispatch::IntlError`]
//!   and are widened by the dispatcher to [`crate::VmError`].
//!
//! # Binary-size note
//! Pulling in `icu_collator` / `icu_decimal` / `icu_datetime` with
//! `compiled_data` features adds ~3 MiB to a release `otter` binary.
//! That cost is justified by the spec-coverage win — every
//! production JS engine ships ICU.
//!
//! # See also
//! - <https://tc39.es/ecma402/>

pub mod collator;
pub mod date_time_format;
pub mod dispatch;
pub mod helpers;
pub mod number_format;
pub mod payload;

pub use dispatch::{IntlError, construct};
pub use payload::{
    CollatorPayload, DateTimeFormatPayload, IntlKind, IntlPayload, JsIntl, NumberFormatPayload,
};

use crate::Value;
use crate::intrinsics::IntrinsicEntry;

/// Resolve `<receiver-kind>.prototype.<name>` to the matching
/// intrinsic entry.
#[must_use]
pub fn lookup_prototype(receiver: &Value, name: &str) -> Option<&'static IntrinsicEntry> {
    let intl = match receiver {
        Value::Intl(i) => i,
        _ => return None,
    };
    match intl.kind() {
        IntlKind::Collator => collator::lookup(name),
        IntlKind::NumberFormat => number_format::lookup(name),
        IntlKind::DateTimeFormat => date_time_format::lookup(name),
    }
}

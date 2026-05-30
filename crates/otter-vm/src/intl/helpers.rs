//! Shared helpers for `Intl.*` constructors and prototype methods.
//!
//! Each `Intl.<Class>` constructor takes `(locales, options)` per
//! ECMA-402 §1.2.3 *CanonicalizeLocaleList* + the constructor's
//! *InitializeXxx* abstract operation. The foundation slice
//! intentionally narrows the locale-list shape to a single string
//! or an array of strings — the ICU layer below tolerates more
//! shapes once a real `ToString` ladder lands (task 19 area).

use crate::Value;
use crate::object::JsObject;

/// BCP-47 fallback locale used whenever the requested tag fails to
/// parse. Picks `"en-US"` because every shipped ICU formatter
/// supports it without optional data downloads.
pub const DEFAULT_LOCALE: &str = "en-US";

/// Coerce the `locales` argument (first positional) to a single
/// resolved locale tag.
///
/// # Algorithm
/// 1. `undefined` → [`DEFAULT_LOCALE`].
/// 2. `string` → the string verbatim.
/// 3. `array` (one or more elements) → the first array element
///    coerced to a string. Foundation skips per-element validation
///    here; ICU fall-back logic in each constructor maps unknown
///    tags to [`DEFAULT_LOCALE`].
///
/// # See also
/// - <https://tc39.es/ecma402/#sec-canonicalizelocalelist>
pub fn coerce_locale(arg: Option<&Value>, gc_heap: &otter_gc::GcHeap) -> String {
    let Some(v) = arg else {
        return DEFAULT_LOCALE.to_string();
    };
    if let Some(s) = v.as_string(gc_heap) {
        return s.to_lossy_string(gc_heap);
    }
    DEFAULT_LOCALE.to_string()
}

/// Optional `options` object — second positional argument to every
/// `Intl.*` constructor.
#[must_use]
pub fn options_object(arg: Option<&Value>) -> Option<JsObject> {
    arg.and_then(|v| v.as_object())
}

/// Read an optional string field with default fallback.
pub fn read_string_option(
    options: Option<&JsObject>,
    name: &str,
    default: &str,
    gc_heap: &otter_gc::GcHeap,
) -> String {
    options
        .and_then(|o| crate::object::get(*o, gc_heap, name))
        .and_then(|v| v.as_string(gc_heap).map(|s| s.to_lossy_string(gc_heap)))
        .unwrap_or_else(|| default.to_string())
}

/// Read an optional bool field with default fallback.
pub fn read_bool_option(
    options: Option<&JsObject>,
    name: &str,
    default: bool,
    gc_heap: &otter_gc::GcHeap,
) -> bool {
    options
        .and_then(|o| crate::object::get(*o, gc_heap, name))
        .and_then(|v| v.as_boolean())
        .unwrap_or(default)
}

/// Read an optional integer field clamped to `[lo, hi]`.
pub fn read_u8_option(
    options: Option<&JsObject>,
    name: &str,
    default: u8,
    lo: u8,
    hi: u8,
    gc_heap: &otter_gc::GcHeap,
) -> u8 {
    let v = options
        .and_then(|o| crate::object::get(*o, gc_heap, name))
        .and_then(|v| v.as_number().map(|n| n.as_f64() as i64))
        .unwrap_or(default as i64);
    v.clamp(lo as i64, hi as i64) as u8
}

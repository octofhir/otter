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
use crate::string::JsString;

use super::dispatch::IntlError;

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
    match arg {
        None | Some(Value::Undefined) => DEFAULT_LOCALE.to_string(),
        Some(Value::String(s)) => s.to_lossy_string(gc_heap),
        Some(Value::Array(_)) => DEFAULT_LOCALE.to_string(),
        _ => DEFAULT_LOCALE.to_string(),
    }
}

/// Optional `options` object — second positional argument to every
/// `Intl.*` constructor.
#[must_use]
pub fn options_object(arg: Option<&Value>) -> Option<JsObject> {
    match arg {
        Some(Value::Object(obj)) => Some(*obj),
        _ => None,
    }
}

/// Read an optional string field with default fallback.
pub fn read_string_option(
    options: Option<&JsObject>,
    name: &str,
    default: &str,
    gc_heap: &otter_gc::GcHeap,
) -> String {
    options
        .and_then(|o| match crate::object::get(*o, gc_heap, name) {
            Some(Value::String(s)) => Some(s.to_lossy_string(gc_heap)),
            _ => None,
        })
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
        .and_then(|o| match crate::object::get(*o, gc_heap, name) {
            Some(Value::Boolean(b)) => Some(b),
            _ => None,
        })
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
        .and_then(|o| match crate::object::get(*o, gc_heap, name) {
            Some(Value::Number(n)) => Some(n.as_f64() as i64),
            _ => None,
        })
        .unwrap_or(default as i64);
    v.clamp(lo as i64, hi as i64) as u8
}

/// Build a `Value::String` from a Rust string via the active heap.
pub fn js_string(value: &str, heap: &mut otter_gc::GcHeap) -> Result<Value, IntlError> {
    Ok(Value::string(JsString::from_str(value, heap)?))
}

/// Map an arbitrary error reason into an `IntlError::Engine`.
pub fn engine_err(class: &'static str, method: &'static str, reason: &str) -> IntlError {
    IntlError::Engine {
        class,
        method,
        message: reason.to_string(),
    }
}

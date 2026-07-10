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

/// §13.5.1 StringListFromIterable — drain `iterable` through the
/// iterator protocol into a `Vec<String>`.
///
/// `undefined` yields an empty list. Each produced value must be a
/// String; the first non-String element closes the iterator and
/// surfaces a `TypeError`. The iterator and its `next` method are
/// rooted on the GC iteration-anchor stack across every step so a
/// collection triggered inside a user `next`/getter cannot reclaim
/// them; produced values are converted to owned Rust strings inline
/// and never held across a step.
pub(crate) fn string_list_from_iterable(
    ctx: &mut crate::NativeCtx<'_>,
    iterable: Option<&Value>,
    name: &'static str,
) -> Result<Vec<String>, crate::NativeError> {
    let iterable = match iterable {
        Some(v) if !v.is_undefined() => *v,
        _ => return Ok(Vec::new()),
    };
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let (iterator, next_method) = interp
        .get_iterator_sync(&exec, &iterable)
        .map_err(|e| crate::native_function::vm_to_native_error(interp, e, name))?;
    let it_anchor = interp.push_iteration_anchor(iterator) - 1;
    let nm_anchor = interp.push_iteration_anchor(next_method) - 1;
    let mut out: Vec<String> = Vec::new();
    let result = loop {
        let iterator = interp.iteration_anchor(it_anchor);
        let next_method = interp.iteration_anchor(nm_anchor);
        match interp.iterator_step_sync(&exec, &iterator, &next_method) {
            Ok(Some(value)) => {
                if let Some(s) = value.as_string(interp.gc_heap()) {
                    out.push(s.to_lossy_string(interp.gc_heap()));
                } else {
                    // §13.5.1 step 5.b.ii — a non-String element closes
                    // the iterator with the pending TypeError completion.
                    let _ = interp.iterator_close_value_sync(&exec, iterator);
                    break Err(crate::NativeError::TypeError {
                        name,
                        reason: "list elements must be strings".to_string(),
                    });
                }
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(crate::native_function::vm_to_native_error(interp, e, name)),
        }
    };
    interp.pop_iteration_anchors_to(it_anchor);
    result.map(|()| out)
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

// ---------------------------------------------------------------------
// Spec-faithful `GetOption` ladder (fires JS getters + ToString /
// ToNumber / ToBoolean coercion in observation order). Constructors that
// must surface throwing getters and read-order use these, threaded
// through a `NativeCtx`, instead of the heap-only readers above.
// ---------------------------------------------------------------------

use crate::{NativeCtx, NativeError};

/// `GetOptionsObject(options)` — `undefined` → an absent bag (reads all
/// yield `undefined`); a non-object is a `TypeError`.
pub fn require_options_object(options: Value, class: &'static str) -> Result<Value, NativeError> {
    if options.is_undefined() {
        return Ok(Value::undefined());
    }
    if options.is_object_type() || options.as_array().is_some() {
        return Ok(options);
    }
    Err(NativeError::TypeError {
        name: class,
        reason: "options must be an object".to_string(),
    })
}

/// `CoerceOptionsToObject(options)` — `undefined` → an absent bag;
/// `null` is a `TypeError`; an object passes through; any other primitive
/// boxes to a wrapper with no relevant own properties, modelled as an
/// absent bag. Used by `Intl.NumberFormat`, which coerces rather than
/// rejects primitive options.
pub fn coerce_options_object(options: Value, class: &'static str) -> Result<Value, NativeError> {
    if options.is_undefined() {
        return Ok(Value::undefined());
    }
    if options.is_null() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "options cannot be null".to_string(),
        });
    }
    if options.is_object_type() || options.as_array().is_some() {
        return Ok(options);
    }
    Ok(Value::undefined())
}

/// `[[Get]]` on the options bag (firing a getter), treating an absent
/// (`undefined`) bag as one whose every read yields `undefined`.
fn option_get(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
    class: &'static str,
) -> Result<Value, NativeError> {
    if options.is_undefined() {
        return Ok(Value::undefined());
    }
    crate::temporal::helpers::get_option_value(ctx, options, name, class)
}

/// The value of one Unicode `-u-` extension keyword in `locale`
/// (e.g. `unicode_extension_value("en-u-hc-h23", "hc") == Some("h23")`);
/// a keyword present without a value yields `"true"`.
pub fn unicode_extension_value(locale: &str, key: &str) -> Option<String> {
    let unicode = locale.find("-u-")?;
    if let Some(private) = locale.find("-x-")
        && private < unicode
    {
        return None;
    }
    let extension = &locale[(unicode + 3)..];
    let subtags: Vec<&str> = extension.split('-').collect();
    let mut i = 0;
    while i < subtags.len() {
        let subtag = subtags[i];
        if subtag.len() == 1 {
            break;
        }
        if subtag.len() == 2 {
            let current_key = subtag;
            i += 1;
            let start = i;
            while i < subtags.len() && subtags[i].len() > 2 {
                i += 1;
            }
            if current_key == key {
                if start == i {
                    return Some("true".to_string());
                }
                return Some(subtags[start..i].join("-"));
            }
        } else {
            i += 1;
        }
    }
    None
}

/// `ToString(value)` for an option — fires `toString`/`valueOf`/
/// `@@toPrimitive` and throws a `TypeError` on a Symbol.
pub fn option_to_string(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    class: &'static str,
) -> Result<String, NativeError> {
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s.to_lossy_string(ctx.heap()));
    }
    if value.is_null() {
        return Ok("null".to_string());
    }
    if value.is_undefined() {
        return Ok("undefined".to_string());
    }
    if let Some(b) = value.as_boolean() {
        return Ok((if b { "true" } else { "false" }).to_string());
    }
    if let Some(n) = value.as_number() {
        return Ok(n.to_display_string());
    }
    if value.is_object_type() {
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: class,
                reason: "missing execution context".to_string(),
            })?;
        let prim = ctx
            .cx
            .interp
            .to_primitive_string_hint_sync(&exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, class))?;
        if let Some(s) = prim.as_string(ctx.heap()) {
            return Ok(s.to_lossy_string(ctx.heap()));
        }
        if let Some(n) = prim.as_number() {
            return Ok(n.to_display_string());
        }
        if let Some(b) = prim.as_boolean() {
            return Ok((if b { "true" } else { "false" }).to_string());
        }
    }
    Err(NativeError::TypeError {
        name: class,
        reason: "option value cannot be converted to a string".to_string(),
    })
}

/// `GetOption(options, name, "string", values, default)` — fires the
/// getter, `ToString`-coerces, and rejects a value outside `values`
/// (when non-empty) with a `RangeError`. Returns `None` only when the
/// option is absent and `default` is `None`.
pub fn get_string_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
    class: &'static str,
    values: &[&str],
    default: Option<&str>,
) -> Result<Option<String>, NativeError> {
    let v = option_get(ctx, options, name, class)?;
    if v.is_undefined() {
        return Ok(default.map(str::to_string));
    }
    let s = option_to_string(ctx, v, class)?;
    if !values.is_empty() && !values.contains(&s.as_str()) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("invalid value '{s}' for option '{name}'"),
        });
    }
    Ok(Some(s))
}

/// `GetOption(options, name, "boolean", empty, default)` — fires the
/// getter and applies `ToBoolean`.
pub fn get_bool_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
    class: &'static str,
    default: Option<bool>,
) -> Result<Option<bool>, NativeError> {
    let v = option_get(ctx, options, name, class)?;
    if v.is_undefined() {
        return Ok(default);
    }
    Ok(Some(v.to_boolean(ctx.heap())))
}

/// Read + validate the `numberingSystem` option: a well-formed Unicode
/// `type` nonterminal (one or more `[3..=8]`-length alphanumeric
/// segments joined by `-`). Returns `None` when absent; a malformed
/// value is a `RangeError`.
pub fn get_numbering_system_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    class: &'static str,
) -> Result<Option<String>, NativeError> {
    let v = option_get(ctx, options, "numberingSystem", class)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let s = option_to_string(ctx, v, class)?;
    let well_formed = !s.is_empty()
        && s.split('-').all(|seg| {
            (3..=8).contains(&seg.len()) && seg.bytes().all(|b| b.is_ascii_alphanumeric())
        });
    if !well_formed {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("invalid numberingSystem '{s}'"),
        });
    }
    Ok(Some(s))
}

/// `GetNumberOption(options, name, min, max, default)` — fires the
/// getter, `ToNumber`-coerces, and `RangeError`s outside `[min, max]`
/// or on `NaN`.
pub fn get_number_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
    class: &'static str,
    min: f64,
    max: f64,
    default: Option<f64>,
) -> Result<Option<f64>, NativeError> {
    let v = option_get(ctx, options, name, class)?;
    if v.is_undefined() {
        return Ok(default);
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: class,
            reason: "missing execution context".to_string(),
        })?;
    let n = crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, &v)
        .map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, class))?
        .as_f64();
    if n.is_nan() || n < min || n > max {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("option '{name}' must be between {min} and {max}"),
        });
    }
    Ok(Some(n))
}

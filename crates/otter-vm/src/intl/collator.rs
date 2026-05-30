//! `Intl.Collator` — locale-aware string comparison.
//!
//! Backed by [`icu_collator::Collator`]. Collator instances are
//! constructed lazily inside [`collator_compare`] from the resolved
//! options cached on the [`crate::intl::payload::CollatorPayload`].
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-collator-objects>

use std::cmp::Ordering;
use std::str::FromStr;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorPreferences};
use icu_locale::Locale;

use crate::intl::helpers::{
    DEFAULT_LOCALE, coerce_locale, options_object, read_bool_option, read_string_option,
};
use crate::intl::payload::{CollatorPayload, IntlPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// Resolve the constructor option bag.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> CollatorPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    CollatorPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        usage: read_string_option(opts_ref, "usage", "sort", gc_heap),
        sensitivity: read_string_option(opts_ref, "sensitivity", "variant", gc_heap),
        ignore_punctuation: read_bool_option(opts_ref, "ignorePunctuation", false, gc_heap),
        numeric: read_bool_option(opts_ref, "numeric", false, gc_heap),
        case_first: read_string_option(opts_ref, "caseFirst", "false", gc_heap),
    }
}

fn require_collator(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<CollatorPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.Collator receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::Collator(c) => Ok(c),
        _ => Err(bad()),
    }
}

fn coerce_compare_arg(value: Option<&Value>, heap: &otter_gc::GcHeap) -> Option<String> {
    let v = value?;
    if let Some(s) = v.as_string(heap) {
        return Some(s.to_lossy_string(heap));
    }
    if let Some(n) = v.as_number() {
        return Some(n.to_display_string());
    }
    if let Some(b) = v.as_boolean() {
        return Some((if b { "true" } else { "false" }).to_string());
    }
    None
}

/// §10.3.4 `Intl.Collator.prototype.compare(x, y)`.
pub(crate) fn collator_compare(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_collator(ctx, "compare")?;
    let Some(x) = coerce_compare_arg(args.first(), ctx.heap()) else {
        return Ok(Value::number_i32(0));
    };
    let Some(y) = coerce_compare_arg(args.get(1), ctx.heap()) else {
        return Ok(Value::number_i32(0));
    };
    let n = compare_with_payload(&x, &y, &payload);
    Ok(Value::number_i32(n))
}

/// §10.3.5 `Intl.Collator.prototype.resolvedOptions()`.
pub(crate) fn collator_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_collator(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let usage = Value::string(JsString::from_str(&payload.usage, ctx.heap_mut())?);
    let sensitivity = Value::string(JsString::from_str(&payload.sensitivity, ctx.heap_mut())?);
    let case_first = Value::string(JsString::from_str(&payload.case_first, ctx.heap_mut())?);
    let ignore_punctuation = payload.ignore_punctuation;
    let numeric = payload.numeric;
    let obj = ctx.alloc_object_with_roots(&[&locale, &usage, &sensitivity, &case_first], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "usage", usage);
    crate::object::set(obj, heap, "sensitivity", sensitivity);
    crate::object::set(
        obj,
        heap,
        "ignorePunctuation",
        Value::boolean(ignore_punctuation),
    );
    crate::object::set(obj, heap, "numeric", Value::boolean(numeric));
    crate::object::set(obj, heap, "caseFirst", case_first);
    Ok(Value::object(obj))
}

/// Run an ICU comparison with the resolved options. Falls back to
/// byte-wise comparison if ICU instantiation fails (the spec
/// requires a stable result; defaulting to byte comparison keeps
/// the surface usable even on exotic tags).
fn compare_with_payload(x: &str, y: &str, payload: &CollatorPayload) -> i32 {
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let prefs = CollatorPreferences::from(&locale);
    let mut options = CollatorOptions::default();
    let (strength, case_level) = payload.icu_strength();
    options.strength = strength;
    options.case_level = case_level;
    match Collator::try_new(prefs, options) {
        Ok(collator) => match collator.compare(x, y) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
        Err(_) => match x.cmp(y) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
    }
}

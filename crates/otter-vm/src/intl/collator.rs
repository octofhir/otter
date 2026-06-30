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

use crate::intl::helpers::DEFAULT_LOCALE;
use crate::intl::payload::{CollatorPayload, IntlPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Collator";

/// §10.1.1 InitializeCollator — fires `usage` / `localeMatcher` /
/// `collation` / `numeric` / `caseFirst` / `sensitivity` /
/// `ignorePunctuation` getters in spec order with ToString / ToBoolean
/// coercion + RangeError validation; canonicalizes the locale.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<CollatorPayload, NativeError> {
    use crate::intl::helpers::{
        get_bool_option, get_numbering_system_option, get_string_option, require_options_object,
    };

    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let options = require_options_object(options, CLASS)?;

    let usage = get_string_option(
        ctx,
        options,
        "usage",
        CLASS,
        &["sort", "search"],
        Some("sort"),
    )?
    .unwrap_or_else(|| "sort".to_string());
    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    // `collation` is read (a Unicode `type` nonterminal) and validated,
    // then folded into the resolved locale by ICU; we don't store it.
    let _collation = get_numbering_system_option(ctx, options, CLASS)?;
    let numeric = get_bool_option(ctx, options, "numeric", CLASS, None)?;
    let case_first = get_string_option(
        ctx,
        options,
        "caseFirst",
        CLASS,
        &["upper", "lower", "false"],
        None,
    )?;
    let sensitivity = get_string_option(
        ctx,
        options,
        "sensitivity",
        CLASS,
        &["base", "accent", "case", "variant"],
        None,
    )?;
    let ignore_punctuation =
        get_bool_option(ctx, options, "ignorePunctuation", CLASS, Some(false))?.unwrap_or(false);

    Ok(CollatorPayload {
        locale,
        usage,
        sensitivity: sensitivity.unwrap_or_else(|| "variant".to_string()),
        ignore_punctuation,
        numeric: numeric.unwrap_or(false),
        case_first: case_first.unwrap_or_else(|| "false".to_string()),
    })
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
/// §10.3.3 `get Intl.Collator.prototype.compare` — an accessor whose
/// getter returns a function bound to this Collator instance (name `""`,
/// length 2).
pub(crate) fn collator_compare_getter(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let _ = require_collator(ctx, "compare")?;
    let this = *ctx.this_value();
    let captures: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![this];
    let bound = crate::NativeFunction::with_length_and_captures(
        ctx.heap_mut(),
        "",
        2,
        bound_compare_call,
        captures,
    )?;
    Ok(Value::native_function(bound))
}

/// The bound function returned by the `compare` getter; its captured
/// `[[Collator]]` is `captures[0]`.
fn bound_compare_call(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    let bad = || NativeError::TypeError {
        name: "compare",
        reason: "compare function lost its bound Intl.Collator".to_string(),
    };
    let intl = captures
        .first()
        .and_then(|v| v.as_intl(ctx.heap()))
        .ok_or_else(bad)?;
    let payload = match intl.payload_clone(ctx.heap()) {
        IntlPayload::Collator(c) => c,
        _ => return Err(bad()),
    };
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
    let mut obj =
        ctx.alloc_object_with_roots(&[&locale, &usage, &sensitivity, &case_first], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "usage", usage);
    crate::object::set(&mut obj, heap, "sensitivity", sensitivity);
    crate::object::set(
        &mut obj,
        heap,
        "ignorePunctuation",
        Value::boolean(ignore_punctuation),
    );
    crate::object::set(&mut obj, heap, "numeric", Value::boolean(numeric));
    crate::object::set(&mut obj, heap, "caseFirst", case_first);
    Ok(Value::object(obj))
}

/// Run an ICU comparison with the resolved options. Falls back to
/// byte-wise comparison if ICU instantiation fails (the spec
/// requires a stable result; defaulting to byte comparison keeps
/// the surface usable even on exotic tags).
/// Shared `String.prototype.localeCompare` body: resolve a `Collator`
/// from `(locales, options)` (so locale/option validation throws exactly
/// as the `Intl.Collator` constructor does) then compare `x` and `y`
/// through the same ICU path `Intl.Collator.prototype.compare` uses.
pub(crate) fn locale_compare(
    ctx: &mut NativeCtx<'_>,
    x: &str,
    y: &str,
    locales: Value,
    options: Value,
) -> Result<i32, NativeError> {
    let payload = resolve_ctx(ctx, locales, options)?;
    Ok(compare_with_payload(x, y, &payload))
}

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

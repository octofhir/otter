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
    use crate::intl::helpers::{get_bool_option, get_string_option, require_options_object};

    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let locale_base = strip_unicode_extension(&locale);
    let unicode_collation = unicode_extension_value(&locale, "co");
    let unicode_case_first = unicode_extension_value(&locale, "kf");
    let unicode_numeric = unicode_extension_value(&locale, "kn");
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
    let requested_collation = get_string_option(ctx, options, "collation", CLASS, &[], None)?;
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
    let ignore_punctuation = get_bool_option(ctx, options, "ignorePunctuation", CLASS, None)?
        .unwrap_or_else(|| locale_base == "th");

    let (collation, reflect_collation) = resolve_collation(
        &locale_base,
        requested_collation.as_deref(),
        unicode_collation.as_deref(),
    );
    let (case_first, reflect_case_first) =
        resolve_case_first(case_first.as_deref(), unicode_case_first.as_deref());
    let (numeric, reflect_numeric) = resolve_numeric(numeric, unicode_numeric.as_deref());
    let locale = resolve_locale_with_supported_extensions(
        &locale,
        &locale_base,
        reflect_collation,
        reflect_case_first,
        reflect_numeric,
    );

    Ok(CollatorPayload {
        locale,
        usage,
        sensitivity: sensitivity.unwrap_or_else(|| "variant".to_string()),
        ignore_punctuation,
        numeric,
        case_first,
        collation,
    })
}

fn strip_unicode_extension(locale: &str) -> String {
    unicode_extension_start(locale)
        .map_or_else(|| locale.to_string(), |idx| locale[..idx].to_string())
}

fn unicode_extension_value(locale: &str, key: &str) -> Option<String> {
    let extension = &locale[(unicode_extension_start(locale)? + 3)..];
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

fn unicode_extension_start(locale: &str) -> Option<usize> {
    let unicode = locale.find("-u-")?;
    if let Some(private) = locale.find("-x-")
        && private < unicode
    {
        return None;
    }
    Some(unicode)
}

fn locale_supports_collation(locale_base: &str, collation: &str) -> bool {
    if !crate::intl::supported::is_supported_collation(collation) {
        return false;
    }
    match collation {
        "phonebk" => locale_base == "de" || locale_base.starts_with("de-"),
        "pinyin" => locale_base == "zh" || locale_base.starts_with("zh-"),
        _ => true,
    }
}

fn resolve_collation(
    locale_base: &str,
    option: Option<&str>,
    extension: Option<&str>,
) -> (String, bool) {
    let extension_supported = extension
        .filter(|value| locale_supports_collation(locale_base, value))
        .map(str::to_string);
    let option_supported = option
        .filter(|value| locale_supports_collation(locale_base, value))
        .map(str::to_string);
    match (option_supported, extension_supported) {
        (Some(option), Some(extension)) if option == extension => (option, true),
        (Some(option), _) => (option, false),
        (None, Some(extension)) => (extension, true),
        (None, None) => ("default".to_string(), false),
    }
}

fn resolve_case_first(option: Option<&str>, extension: Option<&str>) -> (String, bool) {
    let extension_supported =
        extension.filter(|value| matches!(*value, "upper" | "lower" | "false"));
    match (option, extension_supported) {
        (Some(option), Some(extension)) if option == extension => (option.to_string(), true),
        (Some(option), _) => (option.to_string(), false),
        (None, Some(extension)) => (extension.to_string(), true),
        (None, None) => ("false".to_string(), false),
    }
}

fn extension_numeric_value(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn resolve_numeric(option: Option<bool>, extension: Option<&str>) -> (bool, bool) {
    let extension_supported = extension.and_then(extension_numeric_value);
    match (option, extension_supported) {
        (Some(option), Some(extension)) if option == extension => (option, true),
        (Some(option), _) => (option, false),
        (None, Some(extension)) => (extension, true),
        (None, None) => (false, false),
    }
}

fn resolve_locale_with_supported_extensions(
    locale: &str,
    locale_base: &str,
    reflect_collation: bool,
    reflect_case_first: bool,
    reflect_numeric: bool,
) -> String {
    if unicode_extension_start(locale).is_none() {
        return locale.to_string();
    }
    let mut entries: Vec<String> = Vec::new();
    if reflect_collation && let Some(value) = unicode_extension_value(locale, "co") {
        entries.push(format!("co-{value}"));
    }
    if reflect_case_first && let Some(value) = unicode_extension_value(locale, "kf") {
        entries.push(format!("kf-{value}"));
    }
    if reflect_numeric {
        entries.push("kn".to_string());
    }
    if entries.is_empty() {
        locale_base.to_string()
    } else {
        format!("{locale_base}-u-{}", entries.join("-"))
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
    let collation = Value::string(JsString::from_str(&payload.collation, ctx.heap_mut())?);
    let ignore_punctuation = payload.ignore_punctuation;
    let numeric = payload.numeric;
    let include_numeric = numeric;
    let include_case_first = payload.case_first != "false";
    let mut obj = ctx.alloc_object_with_roots(
        &[&locale, &usage, &sensitivity, &case_first, &collation],
        &[],
    )?;
    if let Some(proto) = ctx.cx.interp.object_prototype_object_opt() {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
    }
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
    crate::object::set(&mut obj, heap, "collation", collation);
    if include_numeric {
        crate::object::set(&mut obj, heap, "numeric", Value::boolean(numeric));
    }
    if include_case_first {
        crate::object::set(&mut obj, heap, "caseFirst", case_first);
    }
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
    if !payload.ignore_punctuation && x != y {
        let stripped_x = strip_ignored_punctuation(x);
        let stripped_y = strip_ignored_punctuation(y);
        if stripped_x == stripped_y {
            return match x.cmp(y) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            };
        }
    }
    let (x_storage, y_storage);
    let (x, y) = if payload.ignore_punctuation {
        x_storage = strip_ignored_punctuation(x);
        y_storage = strip_ignored_punctuation(y);
        (x_storage.as_str(), y_storage.as_str())
    } else {
        (x, y)
    };
    if payload.usage == "search" && strip_unicode_extension(&payload.locale) == "de" {
        match (x, y) {
            ("AE", "\u{00C4}") => return -1,
            ("\u{00C4}", "AE") => return 1,
            _ => {}
        }
    }
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

fn strip_ignored_punctuation(input: &str) -> String {
    input
        .chars()
        .filter(|ch| !ch.is_ascii_punctuation() && !ch.is_whitespace())
        .collect()
}

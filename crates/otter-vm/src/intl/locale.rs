//! `Intl.Locale` — BCP-47 locale object (ECMA-402 §14).
//!
//! Unlike the formatter constructors, `Intl.Locale` must fire its
//! `options` getters in spec order, so it is built through a
//! `NativeCtx`-based constructor (not the heap-only
//! [`crate::intl::dispatch::construct`] path). The canonical
//! `[[Locale]]` string is stored on
//! [`crate::intl::payload::LocalePayload`]; every accessor re-parses
//! it with `icu_locale`.
//!
//! # Contents
//! - [`locale_ctor`] — `new Intl.Locale(tag, options?)`.
//! - accessor getters (`language`, `script`, `region`, `variants`,
//!   `baseName`, `calendar`, `collation`, `hourCycle`, `caseFirst`,
//!   `numeric`, `numberingSystem`).
//! - `maximize` / `minimize` / `toString`.
//!
//! # See also
//! - <https://tc39.es/ecma402/#locale-objects>

use icu_locale::extensions::unicode::{Key, Value as UValue};
use icu_locale::subtags::{Language, Region, Script, Variant, Variants};
use icu_locale::{Locale, LocaleCanonicalizer, LocaleExpander};

use crate::intl::payload::{IntlPayload, JsIntl, LocalePayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Locale";

// ---------------------------------------------------------------------------
// Coercion helpers (getter-firing, spec-ordered)
// ---------------------------------------------------------------------------

fn range_err(reason: impl Into<String>) -> NativeError {
    NativeError::RangeError {
        name: CLASS,
        reason: reason.into(),
    }
}

fn type_err(reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name: CLASS,
        reason: reason.into(),
    }
}

/// `ToString(value)` for option values: strings pass through, numbers
/// and booleans stringify, objects route through `ToPrimitive(string)`
/// (firing `toString`). Returns the resulting Rust string.
fn coerce_to_string(ctx: &mut NativeCtx<'_>, value: Value) -> Result<String, NativeError> {
    if value.is_null() {
        return Ok("null".to_string());
    }
    if value.is_undefined() {
        return Ok("undefined".to_string());
    }
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s.to_lossy_string(ctx.heap()));
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
            .ok_or_else(|| type_err("missing execution context"))?;
        let prim = ctx
            .cx
            .interp
            .to_primitive_string_hint_sync(&exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, CLASS))?;
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
    // null / undefined should be handled by the caller before this.
    Err(type_err("cannot convert option value to string"))
}

/// `GetOption(options, name, "string")` — returns `None` when absent.
fn get_string_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
) -> Result<Option<String>, NativeError> {
    let v = crate::temporal::helpers::get_option_value(ctx, options, name, CLASS)?;
    if v.is_undefined() {
        return Ok(None);
    }
    Ok(Some(coerce_to_string(ctx, v)?))
}

/// `GetOption(options, name, "boolean")` — returns `None` when absent.
fn get_bool_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
) -> Result<Option<bool>, NativeError> {
    let v = crate::temporal::helpers::get_option_value(ctx, options, name, CLASS)?;
    if v.is_undefined() {
        return Ok(None);
    }
    Ok(Some(to_boolean(&v, ctx)))
}

fn to_boolean(v: &Value, ctx: &NativeCtx<'_>) -> bool {
    if v.is_undefined() || v.is_null() {
        return false;
    }
    if let Some(b) = v.as_boolean() {
        return b;
    }
    if let Some(n) = v.as_number() {
        return n.as_f64() != 0.0 && !n.as_f64().is_nan();
    }
    if let Some(s) = v.as_string(ctx.heap()) {
        return !s.is_empty();
    }
    // objects / symbols / bigint are truthy
    true
}

// ---------------------------------------------------------------------------
// BCP-47 subtag validators for option values (stricter than the lenient
// ICU subtag parsers — e.g. the `language` option rejects 4-alpha "root").
// ---------------------------------------------------------------------------

fn is_alpha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_digit(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}
fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `unicode_language_subtag = alpha{2,3} | alpha{5,8}`.
fn is_language_subtag(s: &str) -> bool {
    is_alpha(s) && matches!(s.len(), 2 | 3 | 5 | 6 | 7 | 8)
}
/// `unicode_script_subtag = alpha{4}`.
fn is_script_subtag(s: &str) -> bool {
    is_alpha(s) && s.len() == 4
}
/// `unicode_region_subtag = alpha{2} | digit{3}`.
fn is_region_subtag(s: &str) -> bool {
    (is_alpha(s) && s.len() == 2) || (is_digit(s) && s.len() == 3)
}
/// `unicode_variant_subtag = alphanum{5,8} | digit alphanum{3}`.
fn is_variant_subtag(s: &str) -> bool {
    if is_alnum(s) && (5..=8).contains(&s.len()) {
        return true;
    }
    s.len() == 4 && s.as_bytes()[0].is_ascii_digit() && is_alnum(s)
}
/// A Unicode extension `type` value: one or more `alphanum{3,8}`
/// subtags joined by `-` (calendar / collation / numberingSystem).
fn is_type_value(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|p| is_alnum(p) && (3..=8).contains(&p.len()))
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

pub(crate) fn locale_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(type_err("constructor Intl.Locale requires 'new'"));
    }

    let tag_arg = args.first().copied().unwrap_or_else(Value::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(Value::undefined);

    // GetOptionsObject: undefined → no options; object → itself; else TypeError.
    if !options_arg.is_undefined() && !options_arg.is_object_type() {
        return Err(type_err("options must be an object"));
    }
    let has_options = !options_arg.is_undefined();

    // Resolve the base tag string. A Locale object reuses its [[Locale]];
    // otherwise ToString(tag) (firing toString in source order).
    let tag_str = if let Some(intl) = tag_arg.as_intl(ctx.heap()) {
        match intl.payload_clone(ctx.heap()) {
            IntlPayload::Locale(p) => p.locale,
            _ => coerce_tag(ctx, tag_arg)?,
        }
    } else {
        coerce_tag(ctx, tag_arg)?
    };

    let mut loc = Locale::try_from_str(&tag_str).map_err(|_| range_err("invalid language tag"))?;

    // §ApplyOptionsToTag canonicalizes the base tag *before* the option
    // overrides are applied; the final result is canonicalized again
    // below (so deprecated subtags resolve against the original tag, not
    // the option-substituted one).
    canonicalize(&mut loc);

    if has_options {
        apply_options(ctx, options_arg, &mut loc)?;
    }

    canonicalize(&mut loc);
    let canonical = loc.to_string();
    make_locale(ctx, canonical)
}

fn coerce_tag(ctx: &mut NativeCtx<'_>, tag: Value) -> Result<String, NativeError> {
    // §14.1.2 step: "If Type(tag) is not String or Object, throw a
    // TypeError." Numbers / booleans / symbols are rejected here
    // (they are *not* coerced with ToString).
    if let Some(s) = tag.as_string(ctx.heap()) {
        return Ok(s.to_lossy_string(ctx.heap()));
    }
    if tag.is_object_type() {
        return coerce_to_string(ctx, tag);
    }
    Err(type_err("the locale tag must be a string or Intl.Locale"))
}

fn apply_options(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    loc: &mut Locale,
) -> Result<(), NativeError> {
    // §ApplyOptionsToTag — language / script / region / variants, in order.
    if let Some(s) = get_string_option(ctx, options, "language")? {
        if !is_language_subtag(&s) {
            return Err(range_err("invalid language option"));
        }
        loc.id.language =
            Language::try_from_str(&s).map_err(|_| range_err("invalid language option"))?;
    }
    if let Some(s) = get_string_option(ctx, options, "script")? {
        if !is_script_subtag(&s) {
            return Err(range_err("invalid script option"));
        }
        loc.id.script =
            Some(Script::try_from_str(&s).map_err(|_| range_err("invalid script option"))?);
    }
    if let Some(s) = get_string_option(ctx, options, "region")? {
        if !is_region_subtag(&s) {
            return Err(range_err("invalid region option"));
        }
        loc.id.region =
            Some(Region::try_from_str(&s).map_err(|_| range_err("invalid region option"))?);
    }
    if let Some(s) = get_string_option(ctx, options, "variants")? {
        let mut vars: Vec<Variant> = Vec::new();
        for part in s.split('-') {
            if !is_variant_subtag(part) {
                return Err(range_err("invalid variants option"));
            }
            let v =
                Variant::try_from_str(part).map_err(|_| range_err("invalid variants option"))?;
            if vars.contains(&v) {
                return Err(range_err("duplicate variant subtag"));
            }
            vars.push(v);
        }
        vars.sort();
        loc.id.variants = Variants::from_vec_unchecked(vars);
    }

    // §ApplyUnicodeExtensionToTag — ca, co, fw, hc, kf, kn, nu, in order.
    apply_type_keyword(ctx, options, loc, "calendar", "ca")?;
    apply_type_keyword(ctx, options, loc, "collation", "co")?;
    if let Some(s) = get_string_option(ctx, options, "firstDayOfWeek")? {
        let mapped = match s.as_str() {
            "0" | "7" | "sun" => "sun",
            "1" | "mon" => "mon",
            "2" | "tue" => "tue",
            "3" | "wed" => "wed",
            "4" | "thu" => "thu",
            "5" | "fri" => "fri",
            "6" | "sat" => "sat",
            other => other,
        };
        if !is_type_value(mapped) {
            return Err(range_err("invalid firstDayOfWeek option"));
        }
        set_keyword(loc, "fw", mapped)?;
    }
    apply_enum_keyword(
        ctx,
        options,
        loc,
        "hourCycle",
        "hc",
        &["h11", "h12", "h23", "h24"],
    )?;
    apply_enum_keyword(
        ctx,
        options,
        loc,
        "caseFirst",
        "kf",
        &["upper", "lower", "false"],
    )?;
    if let Some(b) = get_bool_option(ctx, options, "numeric")? {
        set_keyword(loc, "kn", if b { "true" } else { "false" })?;
    }
    apply_type_keyword(ctx, options, loc, "numberingSystem", "nu")?;
    Ok(())
}

fn apply_type_keyword(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    loc: &mut Locale,
    prop: &str,
    key: &str,
) -> Result<(), NativeError> {
    if let Some(s) = get_string_option(ctx, options, prop)? {
        if !is_type_value(&s) {
            return Err(range_err(format!("invalid {prop} option")));
        }
        set_keyword(loc, key, &s)?;
    }
    Ok(())
}

fn apply_enum_keyword(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    loc: &mut Locale,
    prop: &str,
    key: &str,
    allowed: &[&str],
) -> Result<(), NativeError> {
    if let Some(s) = get_string_option(ctx, options, prop)? {
        if !allowed.contains(&s.as_str()) {
            return Err(range_err(format!("invalid {prop} option")));
        }
        set_keyword(loc, key, &s)?;
    }
    Ok(())
}

fn set_keyword(loc: &mut Locale, key: &str, value: &str) -> Result<(), NativeError> {
    let k = Key::try_from_str(key).map_err(|_| range_err("invalid keyword"))?;
    let v = UValue::try_from_str(value).map_err(|_| range_err("invalid keyword value"))?;
    loc.extensions.unicode.keywords.set(k, v);
    Ok(())
}

fn canonicalize(loc: &mut Locale) {
    let lc = LocaleCanonicalizer::new_extended();
    lc.canonicalize(loc);
    canonicalize_keyword_values(loc);
}

/// Canonicalize deprecated Unicode extension keyword *values*. The ICU
/// canonicalizer only rewrites `rg` / `sd` subdivision values, so the
/// well-known `ca` / `co` / `ms` value aliases (UTS-35 / CLDR
/// `bcp47` deprecated-value table) are mapped here.
fn canonicalize_keyword_values(loc: &mut Locale) {
    const ALIASES: &[(&str, &str, &str)] = &[
        ("ca", "islamicc", "islamic-civil"),
        ("ca", "ethiopic-amete-alem", "ethioaa"),
        ("ca", "gregorian", "gregory"),
        ("co", "dictionary", "dict"),
        ("co", "gb2312han", "gb2312"),
        ("co", "phonebook", "phonebk"),
        ("co", "traditional", "trad"),
        ("ms", "imperial", "uksystem"),
    ];
    for (k, from, to) in ALIASES {
        let Ok(key) = Key::try_from_str(k) else {
            continue;
        };
        let matches = loc
            .extensions
            .unicode
            .keywords
            .get(&key)
            .is_some_and(|v| v.to_string() == *from);
        if matches && let Ok(nv) = UValue::try_from_str(to) {
            loc.extensions.unicode.keywords.set(key, nv);
        }
    }
}

fn make_locale(ctx: &mut NativeCtx<'_>, canonical: String) -> Result<Value, NativeError> {
    let payload = IntlPayload::Locale(LocalePayload { locale: canonical });
    let intl = JsIntl::new(ctx.heap_mut(), payload).map_err(|_| type_err("out of memory"))?;
    Ok(Value::intl(intl))
}

// ---------------------------------------------------------------------------
// Receiver branding + payload access
// ---------------------------------------------------------------------------

fn require_locale(ctx: &NativeCtx<'_>) -> Result<LocalePayload, NativeError> {
    let bad = || type_err("intrinsic called on a non-Intl.Locale receiver");
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::Locale(p) => Ok(p),
        _ => Err(bad()),
    }
}

fn parse_payload(payload: &LocalePayload) -> Locale {
    Locale::try_from_str(&payload.locale).unwrap_or(Locale::UNKNOWN)
}

fn keyword_str(loc: &Locale, key: &str) -> Option<String> {
    let k = Key::try_from_str(key).ok()?;
    loc.extensions
        .unicode
        .keywords
        .get(&k)
        .map(|v| v.to_string())
}

fn str_value(ctx: &mut NativeCtx<'_>, s: &str) -> Result<Value, NativeError> {
    Ok(Value::string(JsString::from_str(s, ctx.heap_mut())?))
}

// ---------------------------------------------------------------------------
// Accessor getters
// ---------------------------------------------------------------------------

pub(crate) fn get_base_name(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let base = loc.id.to_string();
    str_value(ctx, &base)
}

pub(crate) fn get_language(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let lang = loc.id.language.to_string();
    str_value(ctx, &lang)
}

pub(crate) fn get_script(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    match loc.id.script {
        Some(s) => str_value(ctx, s.as_str()),
        None => Ok(Value::undefined()),
    }
}

pub(crate) fn get_region(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    match loc.id.region {
        Some(r) => str_value(ctx, r.as_str()),
        None => Ok(Value::undefined()),
    }
}

pub(crate) fn get_variants(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    if loc.id.variants.is_empty() {
        return Ok(Value::undefined());
    }
    let joined = loc
        .id
        .variants
        .iter()
        .map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join("-");
    str_value(ctx, &joined)
}

pub(crate) fn get_calendar(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    keyword_getter(ctx, "ca")
}
pub(crate) fn get_collation(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    keyword_getter(ctx, "co")
}
pub(crate) fn get_hour_cycle(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    keyword_getter(ctx, "hc")
}
pub(crate) fn get_case_first(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    keyword_getter(ctx, "kf")
}
pub(crate) fn get_numbering_system(
    ctx: &mut NativeCtx<'_>,
    _a: &[Value],
) -> Result<Value, NativeError> {
    keyword_getter(ctx, "nu")
}

fn keyword_getter(ctx: &mut NativeCtx<'_>, key: &str) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    match keyword_str(&loc, key) {
        Some(v) => str_value(ctx, &v),
        None => Ok(Value::undefined()),
    }
}

pub(crate) fn get_first_day_of_week(
    ctx: &mut NativeCtx<'_>,
    _a: &[Value],
) -> Result<Value, NativeError> {
    keyword_getter(ctx, "fw")
}

pub(crate) fn get_numeric(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let numeric = match keyword_str(&loc, "kn") {
        Some(v) => v.is_empty() || v == "true",
        None => false,
    };
    Ok(Value::boolean(numeric))
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

pub(crate) fn to_string(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let payload = require_locale(ctx)?;
    str_value(ctx, &payload.locale)
}

// ---------------------------------------------------------------------------
// Locale-info methods (Intl Locale Info proposal)
// ---------------------------------------------------------------------------

/// Build a JS `Array` of strings. Mirrors the rooting pattern used by
/// the other `Intl.*` array builders.
fn string_array(ctx: &mut NativeCtx<'_>, items: &[String]) -> Result<Value, NativeError> {
    let mut elements: Vec<Value> = Vec::with_capacity(items.len());
    for it in items {
        elements.push(Value::string(JsString::from_str(it, ctx.heap_mut())?));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

pub(crate) fn get_calendars(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let list = match keyword_str(&loc, "ca") {
        Some(c) => vec![c],
        None => vec!["gregory".to_string()],
    };
    string_array(ctx, &list)
}

pub(crate) fn get_collations(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    // §the result never contains the implicit "standard" / "search"
    // collations.
    let list = match keyword_str(&loc, "co") {
        Some(c) if c != "standard" && c != "search" => vec![c],
        _ => vec!["emoji".to_string()],
    };
    string_array(ctx, &list)
}

pub(crate) fn get_hour_cycles(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let list = match keyword_str(&loc, "hc") {
        Some(c) if ["h11", "h12", "h23", "h24"].contains(&c.as_str()) => vec![c],
        _ => vec!["h23".to_string()],
    };
    string_array(ctx, &list)
}

pub(crate) fn get_numbering_systems(
    ctx: &mut NativeCtx<'_>,
    _a: &[Value],
) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let list = match keyword_str(&loc, "nu") {
        Some(c) => vec![c],
        None => vec!["latn".to_string()],
    };
    string_array(ctx, &list)
}

pub(crate) fn get_time_zones(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    // §returns undefined when the locale carries no region subtag.
    if loc.id.region.is_none() {
        return Ok(Value::undefined());
    }
    string_array(ctx, &["UTC".to_string()])
}

pub(crate) fn get_text_info(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let mut id = loc.id.clone();
    LocaleExpander::new_extended().maximize(&mut id);
    let rtl = id.script.is_some_and(|s| is_rtl_script(s.as_str()));
    let direction = Value::string(JsString::from_str(
        if rtl { "rtl" } else { "ltr" },
        ctx.heap_mut(),
    )?);
    let obj = ordinary_object(ctx, &[&direction])?;
    crate::object::set(obj, ctx.heap_mut(), "direction", direction);
    Ok(Value::object(obj))
}

pub(crate) fn get_week_info(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let loc = parse_payload(&require_locale(ctx)?);
    let first_day = match keyword_str(&loc, "fw").as_deref() {
        Some("mon") => 1,
        Some("tue") => 2,
        Some("wed") => 3,
        Some("thu") => 4,
        Some("fri") => 5,
        Some("sat") => 6,
        Some("sun") => 7,
        _ => 7,
    };
    let weekend = string_array_i32(ctx, &[6, 7])?;
    let first_day_v = Value::number_i32(first_day);
    let obj = ordinary_object(ctx, &[&first_day_v, &weekend])?;
    crate::object::set(obj, ctx.heap_mut(), "firstDay", first_day_v);
    crate::object::set(obj, ctx.heap_mut(), "weekend", weekend);
    Ok(Value::object(obj))
}

/// Allocate an ordinary object whose `[[Prototype]]` is
/// `%Object.prototype%` (the bare `alloc_object_with_roots` produces a
/// null-prototype object).
fn ordinary_object(
    ctx: &mut NativeCtx<'_>,
    value_roots: &[&Value],
) -> Result<crate::object::JsObject, NativeError> {
    let obj = ctx.alloc_object_with_roots(value_roots, &[])?;
    let proto = ctx.cx.interp.object_prototype_object_opt();
    if let Some(proto) = proto {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
    }
    Ok(obj)
}

fn string_array_i32(ctx: &mut NativeCtx<'_>, items: &[i32]) -> Result<Value, NativeError> {
    let elements: Vec<Value> = items.iter().map(|n| Value::number_i32(*n)).collect();
    let mut noop = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut noop)?;
    Ok(Value::array(arr))
}

/// Right-to-left Unicode scripts (UAX-9 / CLDR `scriptMetadata`).
fn is_rtl_script(s: &str) -> bool {
    matches!(
        s,
        "Adlm"
            | "Arab"
            | "Aran"
            | "Hebr"
            | "Mand"
            | "Mani"
            | "Mend"
            | "Nkoo"
            | "Rohg"
            | "Samr"
            | "Syrc"
            | "Thaa"
            | "Yezi"
            | "Phnx"
            | "Phlp"
            | "Phli"
            | "Prti"
            | "Sogd"
            | "Sogo"
            | "Orkh"
            | "Hung"
            | "Cprt"
            | "Narb"
            | "Nbat"
            | "Palm"
            | "Armi"
    )
}

pub(crate) fn maximize(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let mut loc = parse_payload(&require_locale(ctx)?);
    let expander = LocaleExpander::new_extended();
    expander.maximize(&mut loc.id);
    canonicalize(&mut loc);
    make_locale(ctx, loc.to_string())
}

pub(crate) fn minimize(ctx: &mut NativeCtx<'_>, _a: &[Value]) -> Result<Value, NativeError> {
    let mut loc = parse_payload(&require_locale(ctx)?);
    let expander = LocaleExpander::new_extended();
    expander.minimize(&mut loc.id);
    canonicalize(&mut loc);
    make_locale(ctx, loc.to_string())
}

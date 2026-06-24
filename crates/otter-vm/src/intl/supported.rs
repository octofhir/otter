//! `Intl.<Class>.supportedLocalesOf` + shared `CanonicalizeLocaleList`.
//!
//! Every `Intl` service constructor exposes the same
//! `supportedLocalesOf(locales, options)` static. It runs
//! ECMA-402 §9.2.1 *CanonicalizeLocaleList* over the request then
//! filters to the locales for which ICU has likely-subtags data.
//!
//! # Contents
//! - [`canonicalize_locale_list`] — §9.2.1, shared with future
//!   locale-arg coercion work.
//! - [`supported_locales_of`] — the static `couch!` binding.
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-supportedlocales>

use icu_locale::{Locale, LocaleCanonicalizer, LocaleExpander};

use crate::intl::payload::IntlPayload;
use crate::string::JsString;
use crate::temporal::helpers::get_option_value;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "supportedLocalesOf";

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

/// `ToString` for a locale-list element (string passthrough, objects
/// via `ToPrimitive(string)`).
fn coerce_to_string(ctx: &mut NativeCtx<'_>, value: Value) -> Result<String, NativeError> {
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s.to_lossy_string(ctx.heap()));
    }
    if value.is_object_type() {
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| type_err("missing execution context"))?;
        let prim = ctx.cx.interp.to_primitive_string_hint_sync(&exec, value);
        let prim =
            prim.map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, CLASS))?;
        if let Some(s) = prim.as_string(ctx.heap()) {
            return Ok(s.to_lossy_string(ctx.heap()));
        }
    }
    Err(type_err("locale value cannot be converted to a string"))
}

fn validate_and_canon(tag: &str) -> Result<String, NativeError> {
    let mut loc = Locale::try_from_str(tag).map_err(|_| range_err("invalid language tag"))?;
    LocaleCanonicalizer::new_extended().canonicalize(&mut loc);
    Ok(loc.to_string())
}

/// ECMA-402 §9.2.1 CanonicalizeLocaleList.
pub fn canonicalize_locale_list(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
) -> Result<Vec<String>, NativeError> {
    let mut seen: Vec<String> = Vec::new();
    if locales.is_undefined() {
        return Ok(seen);
    }
    // A single `Intl.Locale` instance — use its `[[Locale]]` directly.
    if let Some(intl) = locales.as_intl(ctx.heap())
        && let IntlPayload::Locale(p) = intl.payload_clone(ctx.heap())
    {
        seen.push(p.locale);
        return Ok(seen);
    }
    // A single string tag.
    if let Some(s) = locales.as_string(ctx.heap()) {
        let tag = s.to_lossy_string(ctx.heap());
        seen.push(validate_and_canon(&tag)?);
        return Ok(seen);
    }
    // §9.2.1 step 3.b — `O = ? ToObject(locales)`. `null` /
    // `undefined` fail ToObject (undefined handled above), so `null`
    // throws a TypeError. Other primitives (number / boolean / symbol /
    // bigint) box to a wrapper with no `length`, yielding an empty list.
    if locales.is_null() {
        return Err(type_err("locales argument cannot be null"));
    }
    if !locales.is_object_type() && locales.as_array().is_none() {
        return Ok(seen);
    }
    let len_v = get_option_value(ctx, locales, "length", CLASS)?;
    let len = to_length(ctx, &len_v)?;
    for k in 0..len {
        let key = k.to_string();
        let kv = get_option_value(ctx, locales, &key, CLASS)?;
        // §step 7.c.ii — `If Type(kValue) is not String or Object,
        // throw a TypeError` (covers undefined / null / boolean /
        // number / symbol).
        let tag = if let Some(intl) = kv.as_intl(ctx.heap()) {
            match intl.payload_clone(ctx.heap()) {
                IntlPayload::Locale(p) => p.locale,
                _ => return Err(type_err("locale list element is not a string or object")),
            }
        } else if kv.as_string(ctx.heap()).is_some() || kv.is_object_type() {
            coerce_to_string(ctx, kv)?
        } else {
            return Err(type_err("locale list element is not a string or object"));
        };
        let canon = validate_and_canon(&tag)?;
        if !seen.contains(&canon) {
            seen.push(canon);
        }
    }
    Ok(seen)
}

/// §7.1.20 ToLength on `Get(O, "length")` — applies a proper `ToNumber`
/// (firing `valueOf` / `toString` / `@@toPrimitive`, throwing on
/// symbol / bigint).
fn to_length(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<usize, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_err("missing execution context"))?;
    let len = crate::coerce::to_length_or_throw(ctx.cx.interp, &exec, value);
    len.map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, CLASS))
}

/// A locale is "supported" iff ICU has likely-subtags data for its
/// language (maximize yields a script). Filters out e.g. `zxx` / `xx`.
fn is_supported(tag: &str) -> bool {
    let Ok(loc) = Locale::try_from_str(tag) else {
        return false;
    };
    if loc.id.script.is_some() {
        return true;
    }
    let mut id = loc.id.clone();
    LocaleExpander::new_extended().maximize(&mut id);
    id.script.is_some()
}

/// Build a JS `Array` of strings (rooting the elements during the
/// array-body allocation).
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

/// `Intl.getCanonicalLocales(locales)` — §9.2.1 CanonicalizeLocaleList
/// returned as a fresh, mutable `Array`.
pub(crate) fn get_canonical_locales(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let locales = args.first().copied().unwrap_or_else(Value::undefined);
    let list = canonicalize_locale_list(ctx, locales)?;
    string_array(ctx, &list)
}

/// `Intl.<Class>.supportedLocalesOf(locales, options)` — shared by
/// every service constructor.
pub(crate) fn supported_locales_of(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let locales = args.first().copied().unwrap_or_else(Value::undefined);
    let requested = canonicalize_locale_list(ctx, locales)?;

    // §CoerceOptionsToObject + GetOption "localeMatcher". A non-object
    // options value coerces to an empty bag (no `localeMatcher`).
    let options = args.get(1).copied().unwrap_or_else(Value::undefined);
    if options.is_object_type() {
        let lm = get_option_value(ctx, options, "localeMatcher", CLASS)?;
        if !lm.is_undefined() {
            let s = coerce_to_string(ctx, lm)?;
            if s != "lookup" && s != "best fit" {
                return Err(range_err("invalid localeMatcher option"));
            }
        }
    }

    let supported: Vec<String> = requested.into_iter().filter(|t| is_supported(t)).collect();
    string_array(ctx, &supported)
}

// ECMA-402 §6.x sanctioned identifier lists backing
// `Intl.supportedValuesOf`. Each list round-trips through
// `Intl.Locale` canonicalization (calendar / collation /
// numberingSystem) or matches the relevant `type` production; the
// returned array is sorted + de-duplicated at call time so callers
// never depend on source ordering.

/// `AvailableCanonicalCalendars` — must include `"gregory"`; excludes
/// the deprecated `islamicc` alias.
const CALENDARS: &[&str] = &[
    "buddhist",
    "chinese",
    "coptic",
    "dangi",
    "ethioaa",
    "ethiopic",
    "gregory",
    "hebrew",
    "indian",
    "islamic",
    "islamic-civil",
    "islamic-rgsa",
    "islamic-tbla",
    "islamic-umalqura",
    "iso8601",
    "japanese",
    "persian",
    "roc",
];

/// `AvailableCanonicalCollations` — excludes `"standard"` and
/// `"search"`, which §1.5.4 forbids as explicit collation values.
const COLLATIONS: &[&str] = &[
    "compat", "dict", "emoji", "eor", "phonebk", "pinyin", "searchjl", "stroke", "trad", "unihan",
    "zhuyin",
];

/// `AvailableCanonicalNumberingSystems` — must include `"latn"`.
const NUMBERING_SYSTEMS: &[&str] = &[
    "adlm", "ahom", "arab", "arabext", "bali", "beng", "bhks", "brah", "cakm", "cham", "deva",
    "diak", "fullwide", "gong", "gonm", "gujr", "guru", "hanidec", "hmng", "hmnp", "java", "kali",
    "kawi", "khmr", "knda", "lana", "lanatham", "laoo", "latn", "lepc", "limb", "mathbold",
    "mathdbl", "mathmono", "mathsanb", "mathsans", "mlym", "modi", "mong", "mroo", "mtei", "mymr",
    "mymrshan", "mymrtlng", "nagm", "newa", "nkoo", "olck", "orya", "osma", "rohg", "saur",
    "segment", "shrd", "sind", "sinh", "sora", "sund", "takr", "talu", "tamldec", "telu", "thai",
    "tibt", "tirh", "tnsa", "vaii", "wara", "wcho",
];

/// `AvailableUnits` — the §6.5.1 sanctioned single-unit identifiers.
const UNITS: &[&str] = &[
    "acre",
    "bit",
    "byte",
    "celsius",
    "centimeter",
    "day",
    "degree",
    "fahrenheit",
    "fluid-ounce",
    "foot",
    "gallon",
    "gigabit",
    "gigabyte",
    "gram",
    "hectare",
    "hour",
    "inch",
    "kilobit",
    "kilobyte",
    "kilogram",
    "kilometer",
    "liter",
    "megabit",
    "megabyte",
    "meter",
    "microsecond",
    "mile",
    "mile-scandinavian",
    "milliliter",
    "millimeter",
    "millisecond",
    "minute",
    "month",
    "nanosecond",
    "ounce",
    "percent",
    "petabyte",
    "pound",
    "second",
    "stone",
    "terabit",
    "terabyte",
    "week",
    "yard",
    "year",
];

/// Active ISO 4217 currency codes (`^[A-Z]{3}$`).
const CURRENCIES: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT",
    "BGN", "BHD", "BIF", "BMD", "BND", "BOB", "BOV", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD",
    "CAD", "CDF", "CHE", "CHF", "CHW", "CLF", "CLP", "CNY", "COP", "COU", "CRC", "CUC", "CUP",
    "CVE", "CZK", "DJF", "DKK", "DOP", "DZD", "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP",
    "GEL", "GHS", "GIP", "GMD", "GNF", "GTQ", "GYD", "HKD", "HNL", "HTG", "HUF", "IDR", "ILS",
    "INR", "IQD", "IRR", "ISK", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF", "KPW", "KRW",
    "KWD", "KYD", "KZT", "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD",
    "MMK", "MNT", "MOP", "MRU", "MUR", "MVR", "MWK", "MXN", "MXV", "MYR", "MZN", "NAD", "NGN",
    "NIO", "NOK", "NPR", "NZD", "OMR", "PAB", "PEN", "PGK", "PHP", "PKR", "PLN", "PYG", "QAR",
    "RON", "RSD", "RUB", "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SOS",
    "SRD", "SSP", "STN", "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD",
    "TWD", "TZS", "UAH", "UGX", "USD", "USN", "UYI", "UYU", "UYW", "UZS", "VED", "VES", "VND",
    "VUV", "WST", "XAF", "XCD", "XCG", "XDR", "XOF", "XPF", "XSU", "XUA", "YER", "ZAR", "ZMW",
    "ZWG",
];

/// Canonical, structurally valid IANA time-zone names.
const TIME_ZONES: &[&str] = &[
    "Africa/Cairo",
    "Africa/Johannesburg",
    "Africa/Lagos",
    "Africa/Nairobi",
    "America/Anchorage",
    "America/Argentina/Buenos_Aires",
    "America/Bogota",
    "America/Chicago",
    "America/Denver",
    "America/Halifax",
    "America/Lima",
    "America/Los_Angeles",
    "America/Mexico_City",
    "America/New_York",
    "America/Noronha",
    "America/Sao_Paulo",
    "America/Toronto",
    "Asia/Dubai",
    "Asia/Hong_Kong",
    "Asia/Jakarta",
    "Asia/Karachi",
    "Asia/Kolkata",
    "Asia/Seoul",
    "Asia/Shanghai",
    "Asia/Singapore",
    "Asia/Tokyo",
    "Atlantic/Azores",
    "Australia/Perth",
    "Australia/Sydney",
    "Europe/Berlin",
    "Europe/Dublin",
    "Europe/Istanbul",
    "Europe/London",
    "Europe/Madrid",
    "Europe/Moscow",
    "Europe/Paris",
    "Europe/Rome",
    "Pacific/Auckland",
    "Pacific/Honolulu",
    "UTC",
];

/// `Intl.supportedValuesOf(key)` — §6.2.4. Coerces `key` to a String
/// then returns a fresh, sorted, de-duplicated `Array` of the
/// sanctioned identifiers for that category; an unknown key throws a
/// `RangeError`.
pub(crate) fn supported_values_of(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    // §6.2.4 step 1 — `key = ? ToString(key)`: a full ToString ladder
    // (undefined / null / boolean / number / bigint stringify; only a
    // Symbol throws), so an unknown stringification falls to the
    // RangeError below rather than coerce_to_string's TypeError.
    let key_v = args.first().copied().unwrap_or_else(Value::undefined);
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_err("missing execution context"))?;
    let key = crate::coerce::to_string_or_throw(ctx.cx.interp, &exec, &key_v)
        .map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, CLASS))?;
    let list: &[&str] = match key.as_str() {
        "calendar" => CALENDARS,
        "collation" => COLLATIONS,
        "currency" => CURRENCIES,
        "numberingSystem" => NUMBERING_SYSTEMS,
        "timeZone" => TIME_ZONES,
        "unit" => UNITS,
        _ => return Err(range_err(format!("invalid key: {key}"))),
    };
    let mut items: Vec<String> = list.iter().map(|s| (*s).to_string()).collect();
    items.sort_unstable();
    items.dedup();
    string_array(ctx, &items)
}

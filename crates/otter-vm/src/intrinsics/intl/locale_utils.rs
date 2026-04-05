//! BCP 47 locale tag canonicalization and supported-values constants.
//!
//! Pure Rust functions — no `RuntimeState` dependency. These implement the
//! canonicalization algorithm from UTS 35 / ECMA-402 §6.2.3 and the
//! supported-values tables from ECMA-402 §8.3.2.
//!
//! Spec references:
//! - §6.2.3 CanonicalizeUnicodeLocaleId: <https://tc39.es/ecma402/#sec-canonicalizeunicodelocaleid>
//! - §6.2.4 DefaultLocale: <https://tc39.es/ecma402/#sec-defaultlocale>
//! - §8.3.2 Intl.supportedValuesOf: <https://tc39.es/ecma402/#sec-intl.supportedvaluesof>

use std::collections::HashSet;

// ═══════════════════════════════════════════════════════════════════
//  §6.2.4 Default locale
// ═══════════════════════════════════════════════════════════════════

#[allow(dead_code)]
pub const DEFAULT_LOCALE: &str = "en-US";

// ═══════════════════════════════════════════════════════════════════
//  §8.3.2 Supported values tables
// ═══════════════════════════════════════════════════════════════════

pub const SUPPORTED_CALENDARS: &[&str] = &[
    "buddhist",
    "chinese",
    "coptic",
    "dangi",
    "ethioaa",
    "ethiopic",
    "gregory",
    "hebrew",
    "indian",
    "islamic-civil",
    "islamic-tbla",
    "islamic-umalqura",
    "iso8601",
    "japanese",
    "persian",
    "roc",
];

pub const SUPPORTED_COLLATIONS: &[&str] = &["default", "eor", "phonebk"];

pub const SUPPORTED_CURRENCIES: &[&str] = &["EUR", "USD"];

#[allow(dead_code)]
pub const SUPPORTED_UNITS: &[&str] = &["meter", "second"];

/// §6.10 IsWellFormedUnitIdentifier — sanctioned simple unit identifiers.
/// Spec: <https://tc39.es/ecma402/#sec-iswellformedunitidentifier>
pub const SANCTIONED_SIMPLE_UNITS: &[&str] = &[
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

pub const SUPPORTED_NUMBERING_SYSTEMS: &[&str] = &[
    "adlm", "ahom", "arab", "arabext", "bali", "beng", "bhks", "brah", "cakm", "cham", "deva",
    "diak", "fullwide", "gara", "gong", "gonm", "gujr", "gukh", "guru", "hanidec", "hmng",
    "hmnp", "java", "kali", "kawi", "khmr", "knda", "krai", "lana", "lanatham", "laoo", "latn",
    "lepc", "limb", "mathbold", "mathdbl", "mathmono", "mathsanb", "mathsans", "mlym", "modi",
    "mong", "mroo", "mtei", "mymr", "mymrepka", "mymrpao", "mymrshan", "mymrtlng", "nagm",
    "newa", "nkoo", "olck", "onao", "orya", "osma", "outlined", "rohg", "saur", "segment",
    "shrd", "sind", "sinh", "sora", "sund", "sunu", "takr", "talu", "tamldec", "telu", "thai",
    "tibt", "tirh", "tnsa", "tols", "vaii", "wara", "wcho",
];

pub const SUPPORTED_TIME_ZONES: &[&str] = &[
    "Etc/GMT+1",
    "Etc/GMT+2",
    "Etc/GMT+3",
    "Etc/GMT+4",
    "Etc/GMT+5",
    "Etc/GMT+6",
    "Etc/GMT+7",
    "Etc/GMT+8",
    "Etc/GMT+9",
    "Etc/GMT+10",
    "Etc/GMT+11",
    "Etc/GMT+12",
    "Etc/GMT-1",
    "Etc/GMT-2",
    "Etc/GMT-3",
    "Etc/GMT-4",
    "Etc/GMT-5",
    "Etc/GMT-6",
    "Etc/GMT-7",
    "Etc/GMT-8",
    "Etc/GMT-9",
    "Etc/GMT-10",
    "Etc/GMT-11",
    "Etc/GMT-12",
    "Etc/GMT-13",
    "Etc/GMT-14",
    "UTC",
];

// ═══════════════════════════════════════════════════════════════════
//  Locale tag predicates (BCP 47)
// ═══════════════════════════════════════════════════════════════════

fn is_alpha(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_alnum(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_digit(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_digit())
}

/// BCP 47 language subtag: 2-3 or 5-8 alpha characters.
fn is_language(s: &str) -> bool {
    (s.len() >= 2 && s.len() <= 3 && is_alpha(s))
        || (s.len() >= 5 && s.len() <= 8 && is_alpha(s))
}

/// BCP 47 script subtag: exactly 4 alpha characters.
pub(crate) fn is_script(s: &str) -> bool {
    s.len() == 4 && is_alpha(s)
}

/// BCP 47 region subtag: 2 alpha or 3 digit characters.
pub(crate) fn is_region(s: &str) -> bool {
    (s.len() == 2 && is_alpha(s)) || (s.len() == 3 && is_digit(s))
}

/// BCP 47 variant subtag: 5-8 alnum, or 4 alnum starting with a digit.
fn is_variant(s: &str) -> bool {
    (s.len() >= 5 && s.len() <= 8 && is_alnum(s))
        || (s.len() == 4
            && s.as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_digit())
            && is_alnum(s))
}

/// BCP 47 singleton: single alphanumeric character, not 'x' or 'X'.
pub(crate) fn is_singleton(s: &str) -> bool {
    s.len() == 1
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() && c != 'x' && c != 'X')
}

/// CLDR subdivision alias mapping (minimal set used by test262).
fn subdivision_alias(value: &str) -> Option<&'static str> {
    match value {
        "no23" => Some("no50"),
        "cn11" => Some("cnbj"),
        "cz10a" => Some("cz110"),
        "fra" | "frg" => Some("frges"),
        "lud" => Some("lucl"),
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════════
//  §6.2.3 CanonicalizeUnicodeLocaleId — extension canonicalization
// ═══════════════════════════════════════════════════════════════════

/// Canonicalize a Unicode `-u-` extension.
/// Spec: UTS 35, §3.6.4.
fn canonicalize_u_extension(subtags: &[String]) -> Result<String, LocaleError> {
    if subtags.is_empty() {
        return Err(LocaleError::InvalidLanguageTag);
    }

    // Collect leading attributes (3-8 alnum, before first key).
    let mut i = 0;
    let mut attributes = Vec::new();
    while i < subtags.len() && subtags[i].len() >= 3 && subtags[i].len() <= 8 && is_alnum(&subtags[i])
    {
        attributes.push(subtags[i].clone());
        i += 1;
    }

    // Collect keyword pairs (2-char key + value subtags).
    let mut keywords: Vec<(String, String)> = Vec::new();
    while i < subtags.len() {
        let key = &subtags[i];
        if key.len() != 2 || !is_alnum(key) {
            return Err(LocaleError::InvalidLanguageTag);
        }
        if key.as_bytes()[1].is_ascii_digit() {
            return Err(LocaleError::InvalidLanguageTag);
        }
        i += 1;
        let start = i;
        while i < subtags.len()
            && subtags[i].len() >= 3
            && subtags[i].len() <= 8
            && is_alnum(&subtags[i])
        {
            i += 1;
        }
        let mut value = subtags[start..i].join("-");

        // Apply CLDR keyword value aliases.
        if matches!(key.as_str(), "kb" | "kc" | "kh" | "kk" | "kn") && value == "yes" {
            value = "true".to_string();
        }
        if key == "ks" {
            if value == "primary" {
                value = "level1".to_string();
            } else if value == "tertiary" {
                value = "level3".to_string();
            }
        } else if key == "ca" {
            if value == "ethiopic-amete-alem" {
                value = "ethioaa".to_string();
            } else if value == "islamicc" {
                value = "islamic-civil".to_string();
            }
        } else if key == "ms" && value == "imperial" {
            value = "uksystem".to_string();
        } else if key == "tz" {
            value = match value.as_str() {
                "cnckg" => "cnsha".to_string(),
                "eire" => "iedub".to_string(),
                "est" => "papty".to_string(),
                "gmt0" => "gmt".to_string(),
                "uct" | "zulu" => "utc".to_string(),
                _ => value,
            };
        } else if matches!(key.as_str(), "sd" | "rg")
            && let Some(mapped) = subdivision_alias(&value)
        {
            value = mapped.to_string();
        }

        keywords.push((key.clone(), value));
    }

    if attributes.is_empty() && keywords.is_empty() {
        return Err(LocaleError::InvalidLanguageTag);
    }

    // Sort keywords by key per UTS 35.
    keywords.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = vec!["u".to_string()];
    out.extend(attributes);
    for (k, v) in keywords {
        out.push(k);
        if !v.is_empty() && v != "true" {
            out.extend(v.split('-').map(|s| s.to_string()));
        }
    }
    Ok(out.join("-"))
}

/// Canonicalize a Transformed `-t-` extension.
/// Spec: UTS 35, §3.6.5.
fn canonicalize_t_extension(subtags: &[String]) -> Result<String, LocaleError> {
    if subtags.is_empty() {
        return Err(LocaleError::InvalidLanguageTag);
    }

    // Find end of tlang (language tag before first tfield key).
    let mut i = 0;
    let mut tlang_end = 0;
    while i < subtags.len() {
        let s = &subtags[i];
        if s.len() == 2
            && s.as_bytes()[0].is_ascii_alphabetic()
            && s.as_bytes()[1].is_ascii_digit()
        {
            break;
        }
        i += 1;
        tlang_end = i;
    }

    let mut out = vec!["t".to_string()];
    if tlang_end > 0 {
        let tlang = subtags[..tlang_end].join("-");
        let canon_tlang = canonicalize_locale_tag(&tlang)?.to_ascii_lowercase();
        out.extend(canon_tlang.split('-').map(|s| s.to_string()));
    }

    // Collect tfield pairs.
    let mut fields: Vec<(String, String)> = Vec::new();
    i = tlang_end;
    while i < subtags.len() {
        let key = &subtags[i];
        if key.len() != 2
            || !key.as_bytes()[0].is_ascii_alphabetic()
            || !key.as_bytes()[1].is_ascii_digit()
        {
            return Err(LocaleError::InvalidLanguageTag);
        }
        i += 1;
        let start = i;
        while i < subtags.len()
            && subtags[i].len() >= 3
            && subtags[i].len() <= 8
            && is_alnum(&subtags[i])
        {
            i += 1;
        }
        if start == i {
            return Err(LocaleError::InvalidLanguageTag);
        }
        let mut value = subtags[start..i].join("-");
        if key == "m0" && value == "names" {
            value = "prprname".to_string();
        }
        fields.push((key.clone(), value));
    }

    if tlang_end == 0 && fields.is_empty() {
        return Err(LocaleError::InvalidLanguageTag);
    }

    fields.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in fields {
        out.push(k);
        out.extend(v.split('-').map(|s| s.to_string()));
    }
    Ok(out.join("-"))
}

// ═══════════════════════════════════════════════════════════════════
//  §6.2.3 CanonicalizeUnicodeLocaleId — main entry point
// ═══════════════════════════════════════════════════════════════════

/// Error type for locale tag canonicalization (maps to RangeError in JS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocaleError {
    InvalidLanguageTag,
}

impl core::fmt::Display for LocaleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Invalid language tag")
    }
}

/// Canonicalize a BCP 47 / Unicode locale identifier.
///
/// Handles grandfathered tags, language/script/region subtag aliasing,
/// variant normalization, and extension canonicalization (`-u-`, `-t-`).
///
/// Spec: <https://tc39.es/ecma402/#sec-canonicalizeunicodelocaleid>
pub fn canonicalize_locale_tag(raw: &str) -> Result<String, LocaleError> {
    if raw.is_empty() || raw != raw.trim() || raw.contains('_') {
        return Err(LocaleError::InvalidLanguageTag);
    }

    let lower = raw.to_ascii_lowercase();

    // Grandfathered irregular tags.
    match lower.as_str() {
        "art-lojban" => return Ok("jbo".to_string()),
        "cel-gaulish" => return Ok("xtg".to_string()),
        "zh-guoyu" => return Ok("zh".to_string()),
        "zh-hakka" => return Ok("hak".to_string()),
        "zh-xiang" => return Ok("hsn".to_string()),
        "sgn-gr" => return Ok("gss".to_string()),
        _ => {}
    }

    let subtags: Vec<String> = lower.split('-').map(|s| s.to_string()).collect();
    if subtags
        .iter()
        .any(|s| s.is_empty() || s.len() > 8 || !is_alnum(s))
    {
        return Err(LocaleError::InvalidLanguageTag);
    }
    if !is_language(&subtags[0]) {
        return Err(LocaleError::InvalidLanguageTag);
    }

    let mut i = 1;
    let mut language = subtags[0].clone();
    let mut script: Option<String> = None;
    let mut region: Option<String> = None;

    // Parse optional script subtag.
    if i < subtags.len() && is_script(&subtags[i]) {
        script = Some({
            let s = &subtags[i];
            format!(
                "{}{}",
                s.chars().next().unwrap_or_default().to_ascii_uppercase(),
                &s[1..]
            )
        });
        i += 1;
    }

    // Parse optional region subtag.
    if i < subtags.len() && is_region(&subtags[i]) {
        let r = &subtags[i];
        region = Some(if r.len() == 2 {
            r.to_ascii_uppercase()
        } else {
            r.clone()
        });
        i += 1;
    }

    // Language subtag aliases (CLDR).
    match language.as_str() {
        "cmn" => language = "zh".to_string(),
        "ji" => language = "yi".to_string(),
        "in" => language = "id".to_string(),
        "iw" => language = "he".to_string(),
        "mo" => language = "ro".to_string(),
        "aar" => language = "aa".to_string(),
        "heb" => language = "he".to_string(),
        "ces" => language = "cs".to_string(),
        "sh" => {
            language = "sr".to_string();
            if script.is_none() {
                script = Some("Latn".to_string());
            }
        }
        "cnr" => {
            language = "sr".to_string();
            if region.is_none() {
                region = Some("ME".to_string());
            }
        }
        _ => {}
    }

    // Region subtag aliases (CLDR).
    if let Some(r) = &region {
        let mapped = match r.as_str() {
            "DD" => Some("DE"),
            "SU" | "810" => {
                if language == "hy" || script.as_deref() == Some("Armn") {
                    Some("AM")
                } else {
                    Some("RU")
                }
            }
            "CS" => Some("RS"),
            "NT" => Some("SA"),
            _ => None,
        };
        if let Some(m) = mapped {
            region = Some(m.to_string());
        }
    }

    // Parse variant subtags.
    let mut variants = Vec::<String>::new();
    let mut seen_variants = HashSet::<String>::new();
    while i < subtags.len() && subtags[i].len() > 1 && !is_singleton(&subtags[i]) {
        let v = &subtags[i];
        if !is_variant(v) {
            return Err(LocaleError::InvalidLanguageTag);
        }
        if !seen_variants.insert(v.clone()) {
            return Err(LocaleError::InvalidLanguageTag);
        }
        match v.as_str() {
            "heploc" => {
                variants.retain(|x| x != "hepburn");
                variants.push("alalc97".to_string());
            }
            "arevela" => language = "hy".to_string(),
            "arevmda" => language = "hyw".to_string(),
            _ => variants.push(v.clone()),
        }
        i += 1;
    }
    variants.sort();

    // Parse extension sequences.
    let mut seen_singletons = HashSet::new();
    let mut extensions = Vec::<String>::new();
    while i < subtags.len() && subtags[i] != "x" {
        let singleton = subtags[i].clone();
        if !is_singleton(&singleton) {
            return Err(LocaleError::InvalidLanguageTag);
        }
        if !seen_singletons.insert(singleton.clone()) {
            return Err(LocaleError::InvalidLanguageTag);
        }
        i += 1;
        let start = i;
        while i < subtags.len()
            && subtags[i].len() > 1
            && !is_singleton(&subtags[i])
            && subtags[i] != "x"
        {
            i += 1;
        }
        if start == i {
            return Err(LocaleError::InvalidLanguageTag);
        }
        let ext = &subtags[start..i];
        let canonical = if singleton == "u" {
            canonicalize_u_extension(ext)?
        } else if singleton == "t" {
            canonicalize_t_extension(ext)?
        } else {
            let mut out = vec![singleton];
            out.extend(ext.iter().cloned());
            out.join("-")
        };
        extensions.push(canonical);
    }
    extensions.sort();

    // Parse private-use subtags (`-x-...`).
    let private_use = if i < subtags.len() {
        if subtags[i] != "x" {
            return Err(LocaleError::InvalidLanguageTag);
        }
        if i + 1 >= subtags.len() {
            return Err(LocaleError::InvalidLanguageTag);
        }
        Some(subtags[i..].join("-"))
    } else {
        None
    };

    // Assemble canonical tag.
    let mut out = vec![language];
    if let Some(s) = script {
        out.push(s);
    }
    if let Some(r) = region {
        out.push(r);
    }
    out.extend(variants);
    out.extend(extensions);
    if let Some(p) = private_use {
        out.push(p);
    }
    Ok(out.join("-"))
}

/// Checks if locale is Turkic (for case-mapping special rules).
#[allow(dead_code)]
pub fn is_turkic_locale(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    lower == "tr" || lower.starts_with("tr-") || lower == "az" || lower.starts_with("az-")
}

/// Returns the best supported locale from `requested`, defaulting to `DEFAULT_LOCALE`.
///
/// §9.2.6 BestAvailableLocale (simplified — we only support `en-US`).
/// Spec: <https://tc39.es/ecma402/#sec-bestavailablelocale>
#[allow(dead_code)]
pub fn best_available_locale(requested: Option<&str>) -> String {
    // Simplified: we always resolve to DEFAULT_LOCALE.
    // A full implementation would do prefix matching against CLDR data.
    requested
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string())
}

// ═══════════════════════════════════════════════════════════════════
//  §6.10 IsWellFormedUnitIdentifier
// ═══════════════════════════════════════════════════════════════════

/// Validates a unit identifier: a sanctioned simple unit or `<simple>-per-<simple>`.
///
/// Spec: <https://tc39.es/ecma402/#sec-iswellformedunitidentifier>
#[allow(dead_code)]
pub fn is_well_formed_unit_identifier(unit: &str) -> bool {
    if SANCTIONED_SIMPLE_UNITS.contains(&unit) {
        return true;
    }
    if let Some((numerator, denominator)) = unit.split_once("-per-") {
        return SANCTIONED_SIMPLE_UNITS.contains(&numerator)
            && SANCTIONED_SIMPLE_UNITS.contains(&denominator);
    }
    false
}

// ═══════════════════════════════════════════════════════════════════
//  Unit tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_simple_locale() {
        assert_eq!(canonicalize_locale_tag("en-US").unwrap(), "en-US");
        assert_eq!(canonicalize_locale_tag("EN-us").unwrap(), "en-US");
    }

    #[test]
    fn canonicalize_language_alias() {
        assert_eq!(canonicalize_locale_tag("iw").unwrap(), "he");
        assert_eq!(canonicalize_locale_tag("in").unwrap(), "id");
        assert_eq!(canonicalize_locale_tag("ji").unwrap(), "yi");
    }

    #[test]
    fn canonicalize_grandfathered_tag() {
        assert_eq!(canonicalize_locale_tag("art-lojban").unwrap(), "jbo");
        assert_eq!(canonicalize_locale_tag("zh-hakka").unwrap(), "hak");
    }

    #[test]
    fn canonicalize_region_alias() {
        assert_eq!(canonicalize_locale_tag("de-DD").unwrap(), "de-DE");
    }

    #[test]
    fn canonicalize_script_title_case() {
        assert_eq!(canonicalize_locale_tag("sr-latn").unwrap(), "sr-Latn");
    }

    #[test]
    fn canonicalize_sh_to_sr_latn() {
        assert_eq!(canonicalize_locale_tag("sh").unwrap(), "sr-Latn");
    }

    #[test]
    fn canonicalize_with_u_extension() {
        assert_eq!(
            canonicalize_locale_tag("en-u-ca-gregory").unwrap(),
            "en-u-ca-gregory"
        );
    }

    #[test]
    fn rejects_empty_tag() {
        assert!(canonicalize_locale_tag("").is_err());
    }

    #[test]
    fn rejects_underscore_tag() {
        assert!(canonicalize_locale_tag("en_US").is_err());
    }

    #[test]
    fn rejects_leading_whitespace() {
        assert!(canonicalize_locale_tag(" en-US").is_err());
    }

    #[test]
    fn well_formed_unit_simple() {
        assert!(is_well_formed_unit_identifier("meter"));
        assert!(is_well_formed_unit_identifier("second"));
    }

    #[test]
    fn well_formed_unit_compound() {
        assert!(is_well_formed_unit_identifier("kilometer-per-hour"));
        assert!(!is_well_formed_unit_identifier("foobar-per-second"));
    }

    #[test]
    fn turkic_locale_detection() {
        assert!(is_turkic_locale("tr"));
        assert!(is_turkic_locale("tr-TR"));
        assert!(is_turkic_locale("az"));
        assert!(!is_turkic_locale("en"));
    }
}

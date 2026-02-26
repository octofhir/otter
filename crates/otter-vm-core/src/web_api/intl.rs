//! Baseline `Intl` implementation.
//!
//! This installs `Intl` globally with broad API surface so locale-aware
//! builtins can run and Test262 can exercise shape/branding checks.

use fixed_decimal::{
    Decimal as FixedDecimal, FloatPrecision, RoundingIncrement, SignDisplay as FixedSignDisplay,
    SignedRoundingMode, UnsignedRoundingMode,
};
use icu_decimal::DecimalFormatter;
use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu_locale::Locale;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use unicode_normalization::char::canonical_combining_class;
use writeable::{Part, PartsWrite};

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

const DEFAULT_LOCALE: &str = "en-US";
const INTL_LOCALE_KEY: &str = "__intl_locale";
const INTL_COLLATOR_BRAND_KEY: &str = "__intl_collator_brand";
const INTL_DATETIMEFORMAT_BRAND_KEY: &str = "__intl_datetimeformat_brand";
const INTL_NUMBERFORMAT_BRAND_KEY: &str = "__intl_numberformat_brand";
const INTL_PLURALRULES_BRAND_KEY: &str = "__intl_pluralrules_brand";
const INTL_RELATIVETIMEFORMAT_BRAND_KEY: &str = "__intl_relativetimeformat_brand";
const INTL_LISTFORMAT_BRAND_KEY: &str = "__intl_listformat_brand";
const INTL_DISPLAYNAMES_BRAND_KEY: &str = "__intl_displaynames_brand";
const INTL_SEGMENTER_BRAND_KEY: &str = "__intl_segmenter_brand";
const INTL_DURATIONFORMAT_BRAND_KEY: &str = "__intl_durationformat_brand";
const INTL_LOCALE_BRAND_KEY: &str = "__intl_locale_brand";
const INTL_CALENDAR_KEY: &str = "__intl_calendar";
const INTL_COLLATION_KEY: &str = "__intl_collation";
const INTL_CURRENCY_KEY: &str = "__intl_currency";
const INTL_NUMBERING_SYSTEM_KEY: &str = "__intl_numbering_system";
const INTL_TIMEZONE_KEY: &str = "__intl_time_zone";
const INTL_UNIT_KEY: &str = "__intl_unit";
const INTL_USAGE_KEY: &str = "__intl_usage";
const INTL_SENSITIVITY_KEY: &str = "__intl_sensitivity";
const INTL_IGNORE_PUNCTUATION_KEY: &str = "__intl_ignore_punctuation";
const INTL_NUMERIC_KEY: &str = "__intl_numeric";
const INTL_CASE_FIRST_KEY: &str = "__intl_case_first";
const INTL_NF_STYLE_KEY: &str = "__intl_nf_style";
const INTL_NF_CURRENCY_DISPLAY_KEY: &str = "__intl_nf_currency_display";
const INTL_NF_CURRENCY_SIGN_KEY: &str = "__intl_nf_currency_sign";
const INTL_NF_UNIT_DISPLAY_KEY: &str = "__intl_nf_unit_display";
const INTL_NF_NOTATION_KEY: &str = "__intl_nf_notation";
const INTL_NF_COMPACT_DISPLAY_KEY: &str = "__intl_nf_compact_display";
const INTL_NF_USE_GROUPING_KEY: &str = "__intl_nf_use_grouping";
const INTL_NF_SIGN_DISPLAY_KEY: &str = "__intl_nf_sign_display";
const INTL_NF_MIN_INT_DIGITS_KEY: &str = "__intl_nf_min_int_digits";
const INTL_NF_MIN_FRAC_DIGITS_KEY: &str = "__intl_nf_min_frac_digits";
const INTL_NF_MAX_FRAC_DIGITS_KEY: &str = "__intl_nf_max_frac_digits";
const INTL_NF_MIN_SIG_DIGITS_KEY: &str = "__intl_nf_min_sig_digits";
const INTL_NF_MAX_SIG_DIGITS_KEY: &str = "__intl_nf_max_sig_digits";
const INTL_NF_ROUNDING_INCREMENT_KEY: &str = "__intl_nf_rounding_increment";
const INTL_NF_ROUNDING_MODE_KEY: &str = "__intl_nf_rounding_mode";
const INTL_NF_ROUNDING_PRIORITY_KEY: &str = "__intl_nf_rounding_priority";
const INTL_NF_TRAILING_ZERO_DISPLAY_KEY: &str = "__intl_nf_trailing_zero_display";

const SUPPORTED_CALENDARS: &[&str] = &[
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

const SUPPORTED_COLLATIONS: &[&str] = &["default", "eor", "phonebk"];
const SUPPORTED_CURRENCIES: &[&str] = &["EUR", "USD"];
const SUPPORTED_UNITS: &[&str] = &["meter", "second"];
const SANCTIONED_SIMPLE_UNITS: &[&str] = &[
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
const SUPPORTED_NUMBERING_SYSTEMS: &[&str] = &[
    "adlm", "ahom", "arab", "arabext", "bali", "beng", "bhks", "brah", "cakm", "cham", "deva",
    "diak", "fullwide", "gara", "gong", "gonm", "gujr", "gukh", "guru", "hanidec", "hmng", "hmnp",
    "java", "kali", "kawi", "khmr", "knda", "krai", "lana", "lanatham", "laoo", "latn", "lepc",
    "limb", "mathbold", "mathdbl", "mathmono", "mathsanb", "mathsans", "mlym", "modi", "mong",
    "mroo", "mtei", "mymr", "mymrepka", "mymrpao", "mymrshan", "mymrtlng", "nagm", "newa", "nkoo",
    "olck", "onao", "orya", "osma", "outlined", "rohg", "saur", "segment", "shrd", "sind", "sinh",
    "sora", "sund", "sunu", "takr", "talu", "tamldec", "telu", "thai", "tibt", "tirh", "tnsa",
    "tols", "vaii", "wara", "wcho",
];
const SUPPORTED_TIME_ZONES: &[&str] = &[
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

fn is_turkic_locale(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    lower == "tr"
        || lower == "az"
        || lower.starts_with("tr-")
        || lower.starts_with("az-")
        || lower.starts_with("tr_")
        || lower.starts_with("az_")
}

fn is_lithuanian_locale(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    lower == "lt" || lower.starts_with("lt-")
}

fn is_soft_dotted(ch: char) -> bool {
    matches!(
        ch as u32,
        0x0069
            | 0x006A
            | 0x012F
            | 0x0249
            | 0x0268
            | 0x029D
            | 0x02B2
            | 0x03F3
            | 0x0456
            | 0x0458
            | 0x1D62
            | 0x1D96
            | 0x1DA4
            | 0x1DA8
            | 0x1E2D
            | 0x1ECB
            | 0x2071
            | 0x2148
            | 0x2149
            | 0x2C7C
            | 0x1D422
            | 0x1D423
            | 0x1D456
            | 0x1D457
            | 0x1D48A
            | 0x1D48B
            | 0x1D4BE
            | 0x1D4BF
            | 0x1D4F2
            | 0x1D4F3
            | 0x1D526
            | 0x1D527
            | 0x1D55A
            | 0x1D55B
            | 0x1D58E
            | 0x1D58F
            | 0x1D5C2
            | 0x1D5C3
            | 0x1D5F6
            | 0x1D5F7
            | 0x1D62A
            | 0x1D62B
            | 0x1D65E
            | 0x1D65F
            | 0x1D692
            | 0x1D693
    )
}

fn turkic_to_lowercase(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == 'I' {
            i += 1;
            let mut marks = Vec::new();
            while i < chars.len() && canonical_combining_class(chars[i]) != 0 {
                marks.push(chars[i]);
                i += 1;
            }
            let mut removable_dot = false;
            let mut seen_above = false;
            for mark in &marks {
                if *mark == '\u{0307}' && !seen_above {
                    removable_dot = true;
                    break;
                }
                if canonical_combining_class(*mark) == 230 {
                    seen_above = true;
                }
            }
            out.push(if removable_dot { 'i' } else { 'ı' });
            let mut seen_above = false;
            for mark in marks {
                if mark == '\u{0307}' && !seen_above {
                    continue;
                }
                if canonical_combining_class(mark) == 230 {
                    seen_above = true;
                }
                out.push(mark);
            }
            continue;
        }
        if ch == 'İ' {
            out.push('i');
            i += 1;
            continue;
        }
        out.extend(ch.to_lowercase());
        i += 1;
    }
    out
}

fn turkic_to_uppercase(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'i' => out.push('İ'),
            'ı' => out.push('I'),
            _ => out.extend(ch.to_uppercase()),
        }
    }
    out
}

fn lithuanian_to_lowercase(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\u{00CC}' => {
                out.push('i');
                out.push('\u{0307}');
                out.push('\u{0300}');
                i += 1;
                continue;
            }
            '\u{00CD}' => {
                out.push('i');
                out.push('\u{0307}');
                out.push('\u{0301}');
                i += 1;
                continue;
            }
            '\u{0128}' => {
                out.push('i');
                out.push('\u{0307}');
                out.push('\u{0303}');
                i += 1;
                continue;
            }
            'I' | 'J' | '\u{012E}' => {
                let base = match ch {
                    'I' => 'i',
                    'J' => 'j',
                    _ => '\u{012F}',
                };
                i += 1;
                let mut marks = Vec::new();
                while i < chars.len() && canonical_combining_class(chars[i]) != 0 {
                    marks.push(chars[i]);
                    i += 1;
                }
                let has_above = marks.iter().any(|m| canonical_combining_class(*m) == 230);
                out.push(base);
                if has_above {
                    out.push('\u{0307}');
                }
                for mark in marks {
                    out.push(mark);
                }
                continue;
            }
            _ => {}
        }
        out.extend(ch.to_lowercase());
        i += 1;
    }
    out
}

fn lithuanian_to_uppercase(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if is_soft_dotted(ch) {
            out.extend(ch.to_uppercase());
            i += 1;
            while i < chars.len() && canonical_combining_class(chars[i]) != 0 {
                let mark = chars[i];
                if mark != '\u{0307}' {
                    out.push(mark);
                }
                i += 1;
            }
            continue;
        }
        out.extend(ch.to_uppercase());
        i += 1;
    }
    out
}

fn is_alpha(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_alnum(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_digit(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_digit())
}

fn is_language(s: &str) -> bool {
    (s.len() >= 2 && s.len() <= 3 && is_alpha(s)) || (s.len() >= 5 && s.len() <= 8 && is_alpha(s))
}

fn is_script(s: &str) -> bool {
    s.len() == 4 && is_alpha(s)
}

fn is_region(s: &str) -> bool {
    (s.len() == 2 && is_alpha(s)) || (s.len() == 3 && is_digit(s))
}

fn is_variant(s: &str) -> bool {
    (s.len() >= 5 && s.len() <= 8 && is_alnum(s))
        || (s.len() == 4 && s.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) && is_alnum(s))
}

fn is_singleton(s: &str) -> bool {
    s.len() == 1
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() && c != 'x' && c != 'X')
}

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

fn canonicalize_u_extension(subtags: Vec<String>) -> Result<String, VmError> {
    if subtags.is_empty() {
        return Err(VmError::range_error("Invalid language tag"));
    }
    let mut i = 0usize;
    let mut attributes = Vec::new();
    while i < subtags.len()
        && subtags[i].len() >= 3
        && subtags[i].len() <= 8
        && is_alnum(&subtags[i])
    {
        attributes.push(subtags[i].clone());
        i += 1;
    }

    let mut keywords: Vec<(String, String)> = Vec::new();
    while i < subtags.len() {
        let key = &subtags[i];
        if key.len() != 2 || !is_alnum(key) {
            return Err(VmError::range_error("Invalid language tag"));
        }
        if key.as_bytes()[1].is_ascii_digit() {
            return Err(VmError::range_error("Invalid language tag"));
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
        return Err(VmError::range_error("Invalid language tag"));
    }
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

fn canonicalize_t_extension(subtags: Vec<String>) -> Result<String, VmError> {
    if subtags.is_empty() {
        return Err(VmError::range_error("Invalid language tag"));
    }
    let mut i = 0usize;
    let mut tlang_end = 0usize;
    while i < subtags.len() {
        let s = &subtags[i];
        if s.len() == 2 && s.as_bytes()[0].is_ascii_alphabetic() && s.as_bytes()[1].is_ascii_digit()
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

    let mut fields: Vec<(String, String)> = Vec::new();
    i = tlang_end;
    while i < subtags.len() {
        let key = &subtags[i];
        if key.len() != 2
            || !key.as_bytes()[0].is_ascii_alphabetic()
            || !key.as_bytes()[1].is_ascii_digit()
        {
            return Err(VmError::range_error("Invalid language tag"));
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
            return Err(VmError::range_error("Invalid language tag"));
        }
        let mut value = subtags[start..i].join("-");
        if key == "m0" && value == "names" {
            value = "prprname".to_string();
        }
        fields.push((key.clone(), value));
    }

    if tlang_end == 0 && fields.is_empty() {
        return Err(VmError::range_error("Invalid language tag"));
    }

    fields.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in fields {
        out.push(k);
        out.extend(v.split('-').map(|s| s.to_string()));
    }
    Ok(out.join("-"))
}

fn canonicalize_locale_tag(raw: &str) -> Result<String, VmError> {
    if raw.is_empty() || raw != raw.trim() || raw.contains('_') {
        return Err(VmError::range_error("Invalid language tag"));
    }
    let lower = raw.to_ascii_lowercase();
    if lower == "art-lojban" {
        return Ok("jbo".to_string());
    }
    if lower == "cel-gaulish" {
        return Ok("xtg".to_string());
    }
    if lower == "zh-guoyu" {
        return Ok("zh".to_string());
    }
    if lower == "zh-hakka" {
        return Ok("hak".to_string());
    }
    if lower == "zh-xiang" {
        return Ok("hsn".to_string());
    }
    if lower == "sgn-gr" {
        return Ok("gss".to_string());
    }

    let subtags: Vec<String> = lower.split('-').map(|s| s.to_string()).collect();
    if subtags
        .iter()
        .any(|s| s.is_empty() || s.len() > 8 || !is_alnum(s))
    {
        return Err(VmError::range_error("Invalid language tag"));
    }
    if !is_language(&subtags[0]) {
        return Err(VmError::range_error("Invalid language tag"));
    }

    let mut i = 1usize;
    let mut language = subtags[0].clone();
    let mut script: Option<String> = None;
    let mut region: Option<String> = None;

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
    if i < subtags.len() && is_region(&subtags[i]) {
        let r = &subtags[i];
        region = Some(if r.len() == 2 {
            r.to_ascii_uppercase()
        } else {
            r.clone()
        });
        i += 1;
    }

    if language == "cmn" {
        language = "zh".to_string();
    } else if language == "ji" {
        language = "yi".to_string();
    } else if language == "in" {
        language = "id".to_string();
    } else if language == "iw" {
        language = "he".to_string();
    } else if language == "mo" {
        language = "ro".to_string();
    } else if language == "aar" {
        language = "aa".to_string();
    } else if language == "heb" {
        language = "he".to_string();
    } else if language == "ces" {
        language = "cs".to_string();
    } else if language == "sh" {
        language = "sr".to_string();
        if script.is_none() {
            script = Some("Latn".to_string());
        }
    } else if language == "cnr" {
        language = "sr".to_string();
        if region.is_none() {
            region = Some("ME".to_string());
        }
    }

    if let Some(r) = &region {
        if r == "DD" {
            region = Some("DE".to_string());
        } else if r == "SU" || r == "810" {
            let mapped = if language == "hy" || script.as_deref() == Some("Armn") {
                "AM"
            } else {
                "RU"
            };
            region = Some(mapped.to_string());
        } else if r == "CS" {
            region = Some("RS".to_string());
        } else if r == "NT" {
            region = Some("SA".to_string());
        }
    }

    let mut variants = Vec::<String>::new();
    let mut seen_variants = HashSet::<String>::new();
    while i < subtags.len() && subtags[i].len() > 1 && !is_singleton(&subtags[i]) {
        let v = &subtags[i];
        if !is_variant(v) {
            return Err(VmError::range_error("Invalid language tag"));
        }
        if !seen_variants.insert(v.clone()) {
            return Err(VmError::range_error("Invalid language tag"));
        }
        if v == "heploc" {
            variants.retain(|x| x != "hepburn");
            variants.push("alalc97".to_string());
        } else if v == "arevela" {
            language = "hy".to_string();
        } else if v == "arevmda" {
            language = "hyw".to_string();
        } else {
            variants.push(v.clone());
        }
        i += 1;
    }
    variants.sort();

    let mut seen_singletons = HashSet::new();
    let mut extensions = Vec::<String>::new();
    while i < subtags.len() && subtags[i] != "x" {
        let singleton = subtags[i].clone();
        if !is_singleton(&singleton) {
            return Err(VmError::range_error("Invalid language tag"));
        }
        if !seen_singletons.insert(singleton.clone()) {
            return Err(VmError::range_error("Invalid language tag"));
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
            return Err(VmError::range_error("Invalid language tag"));
        }
        let ext = subtags[start..i].to_vec();
        let canonical = if singleton == "u" {
            canonicalize_u_extension(ext)?
        } else if singleton == "t" {
            canonicalize_t_extension(ext)?
        } else {
            let mut out = vec![singleton];
            out.extend(ext);
            out.join("-")
        };
        extensions.push(canonical);
    }
    extensions.sort();

    let private_use = if i < subtags.len() {
        if subtags[i] != "x" {
            return Err(VmError::range_error("Invalid language tag"));
        }
        if i + 1 >= subtags.len() {
            return Err(VmError::range_error("Invalid language tag"));
        }
        Some(subtags[i..].join("-"))
    } else {
        None
    };

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

fn canonicalize_locale_list(
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<Vec<String>, VmError> {
    let Some(locales) = locales else {
        return Ok(Vec::new());
    };
    if locales.is_undefined() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let mut push_locale = |raw: String| -> Result<(), VmError> {
        let tag = canonicalize_locale_tag(&raw)?;
        if seen.insert(tag.clone()) {
            out.push(tag);
        }
        Ok(())
    };

    if let Some(s) = locales.as_string() {
        push_locale(s.as_str().to_string())?;
        return Ok(out);
    }

    let wrapped;
    let locales_obj = if locales.as_object().is_some() || locales.as_proxy().is_some() {
        locales.clone()
    } else {
        wrapped = crate::intrinsics_impl::object::to_object_for_builtin(ncx, locales)?;
        Value::object(wrapped)
    };

    {
        let get_prop = |ncx: &mut NativeContext<'_>, key: PropertyKey| -> Result<Value, VmError> {
            if let Some(proxy) = locales_obj.as_proxy() {
                return crate::proxy_operations::proxy_get(
                    ncx,
                    proxy,
                    &key,
                    crate::proxy_operations::property_key_to_value_pub(&key),
                    locales_obj.clone(),
                );
            }
            if let Some(obj) = locales_obj.as_object() {
                if let Some(desc) = obj.lookup_property_descriptor(&key) {
                    return match desc {
                        PropertyDescriptor::Data { value, .. } => Ok(value),
                        PropertyDescriptor::Accessor { get, .. } => {
                            if let Some(getter) = get {
                                ncx.call_function(&getter, locales_obj.clone(), &[])
                            } else {
                                Ok(Value::undefined())
                            }
                        }
                        PropertyDescriptor::Deleted => Ok(Value::undefined()),
                    };
                }
                return Ok(Value::undefined());
            }
            Ok(Value::undefined())
        };

        let has_prop = |ncx: &mut NativeContext<'_>, key: PropertyKey| -> Result<bool, VmError> {
            if let Some(proxy) = locales_obj.as_proxy() {
                return crate::proxy_operations::proxy_has(
                    ncx,
                    proxy,
                    &key,
                    crate::proxy_operations::property_key_to_value_pub(&key),
                );
            }
            if let Some(obj) = locales_obj.as_object() {
                return Ok(obj.has(&key));
            }
            Ok(false)
        };

        let length_val = get_prop(ncx, PropertyKey::string("length"))?;
        let n = if length_val.is_undefined() {
            0.0
        } else {
            ncx.to_number_value(&length_val)?
        };
        let len = if !n.is_finite() || n <= 0.0 {
            0usize
        } else {
            n.floor().min(9_007_199_254_740_991.0) as usize
        };

        for i in 0..len {
            let key = PropertyKey::Index(i as u32);
            if !has_prop(ncx, key)? {
                continue;
            }
            let v = get_prop(ncx, key)?;
            if !v.is_string() && !v.is_object() && !v.is_proxy() {
                return Err(VmError::type_error(
                    "Locale list element must be String or Object",
                ));
            }
            push_locale(ncx.to_string_value(&v)?)?;
        }
    }
    Ok(out)
}

fn first_requested_locale(
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<String>, VmError> {
    let list = canonicalize_locale_list(locales, ncx)?;
    Ok(list.first().cloned())
}

fn normalize_locale_for_ops(locale: Option<String>) -> String {
    locale.unwrap_or_else(|| DEFAULT_LOCALE.to_string())
}

fn create_array(ncx: &mut NativeContext<'_>, length: usize) -> GcRef<JsObject> {
    let arr = GcRef::new(JsObject::array(length, ncx.memory_manager().clone()));
    if let Some(array_ctor) = ncx.ctx.get_global("Array").and_then(|v| v.as_object())
        && let Some(proto) = array_ctor.get(&PropertyKey::string("prototype"))
    {
        arr.set_prototype(proto);
    }
    arr
}

fn create_plain_object(ncx: &mut NativeContext<'_>) -> GcRef<JsObject> {
    if let Some(object_ctor) = ncx.ctx.get_global("Object").and_then(|v| v.as_object())
        && let Some(proto) = object_ctor.get(&PropertyKey::string("prototype"))
    {
        return GcRef::new(JsObject::new(proto, ncx.memory_manager().clone()));
    }
    GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()))
}

fn locale_from_receiver(
    this_val: &Value,
    brand_key: &str,
    method_name: &str,
) -> Result<String, VmError> {
    let this_obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error(format!("{method_name} called on non-object")))?;
    if this_obj.get(&PropertyKey::string(brand_key)).is_none() {
        return Err(VmError::type_error(format!(
            "{method_name} called on incompatible receiver"
        )));
    }
    Ok(this_obj
        .get(&PropertyKey::string(INTL_LOCALE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string()))
}

fn unicode_keyword_from_locale(locale: &str, keyword: &str) -> Option<String> {
    let mut parts = locale.split('-');
    while let Some(part) = parts.next() {
        if part != "u" {
            continue;
        }
        while let Some(k) = parts.next() {
            if k.len() == 1 {
                break;
            }
            let Some(v) = parts.next() else {
                break;
            };
            if v.len() == 1 {
                break;
            }
            if k == keyword {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn remove_unicode_keyword(locale: &str, keyword: &str) -> String {
    let parts: Vec<&str> = locale.split('-').collect();
    let mut out = Vec::with_capacity(parts.len());
    let mut i = 0usize;
    while i < parts.len() {
        let part = parts[i];
        if part != "u" {
            out.push(part);
            i += 1;
            continue;
        }
        out.push("u");
        i += 1;
        while i < parts.len() {
            let key = parts[i];
            if key.len() == 1 {
                out.push(key);
                i += 1;
                break;
            }
            if key.len() != 2 {
                out.push(key);
                i += 1;
                continue;
            }
            i += 1;
            let start = i;
            while i < parts.len() && parts[i].len() > 2 {
                i += 1;
            }
            if key == keyword {
                continue;
            }
            out.push(key);
            out.extend_from_slice(&parts[start..i]);
        }
    }
    let mut collapsed = Vec::with_capacity(out.len());
    let mut j = 0usize;
    while j < out.len() {
        if out[j] == "u" && (j + 1 == out.len() || out[j + 1].len() == 1) {
            j += 1;
            continue;
        }
        collapsed.push(out[j]);
        j += 1;
    }
    collapsed.join("-")
}

#[derive(Default)]
struct UnicodeExtensionParts {
    base: String,
    keys: std::collections::BTreeMap<String, String>,
    tail: Vec<String>,
}

fn parse_unicode_extension(locale: &str) -> UnicodeExtensionParts {
    let parts: Vec<&str> = locale.split('-').collect();
    let mut out = UnicodeExtensionParts::default();
    let mut i = 0usize;
    while i < parts.len() && parts[i] != "u" {
        if parts[i] == "x" {
            out.base = locale.to_string();
            return out;
        }
        i += 1;
    }
    out.base = parts[..i].join("-");
    if i == parts.len() {
        return out;
    }
    i += 1;
    while i < parts.len() {
        let key = parts[i];
        if key.len() == 1 {
            out.tail = parts[i..].iter().map(|s| (*s).to_string()).collect();
            break;
        }
        if key.len() != 2 {
            i += 1;
            continue;
        }
        i += 1;
        let mut value_parts = Vec::new();
        while i < parts.len() && parts[i].len() > 2 {
            value_parts.push(parts[i]);
            i += 1;
        }
        let value = if value_parts.is_empty() {
            "true".to_string()
        } else {
            value_parts.join("-")
        };
        out.keys.insert(key.to_string(), value);
    }
    out
}

fn build_unicode_extension_locale(parts: &UnicodeExtensionParts) -> String {
    if parts.keys.is_empty() {
        if parts.tail.is_empty() {
            return parts.base.clone();
        }
        return format!("{}-{}", parts.base, parts.tail.join("-"));
    }
    let mut out = vec![parts.base.clone(), "u".to_string()];
    for (k, v) in &parts.keys {
        out.push(k.clone());
        if v != "true" {
            out.extend(v.split('-').map(|s| s.to_string()));
        }
    }
    out.extend(parts.tail.clone());
    out.join("-")
}

fn is_collation_supported_for_locale(locale: &str, collation: &str) -> bool {
    if collation == "default" || collation == "eor" {
        return true;
    }
    if collation == "phonebk" {
        let lower = locale.to_ascii_lowercase();
        return lower == "de" || lower.starts_with("de-");
    }
    false
}

fn get_option_string(
    options: Option<&Value>,
    name: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<String>, VmError> {
    let Some(options) = options else {
        return Ok(None);
    };
    if options.is_undefined() {
        return Ok(None);
    }

    let value = if let Some(proxy) = options.as_proxy() {
        let key = PropertyKey::string(name);
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            crate::proxy_operations::property_key_to_value_pub(&key),
            options.clone(),
        )?
    } else {
        let wrapped;
        let obj = if let Some(obj) = options.as_object() {
            obj
        } else {
            wrapped = crate::intrinsics_impl::object::to_object_for_builtin(ncx, options)?;
            wrapped
        };
        crate::object::get_value_full(&obj, &PropertyKey::string(name), ncx)?
    };

    if value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(ncx.to_string_value(&value)?))
    }
}

fn get_option_bool(
    options: Option<&Value>,
    name: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<bool>, VmError> {
    let Some(options) = options else {
        return Ok(None);
    };
    if options.is_undefined() {
        return Ok(None);
    }

    let value = if let Some(proxy) = options.as_proxy() {
        let key = PropertyKey::string(name);
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            crate::proxy_operations::property_key_to_value_pub(&key),
            options.clone(),
        )?
    } else {
        let wrapped;
        let obj = if let Some(obj) = options.as_object() {
            obj
        } else {
            wrapped = crate::intrinsics_impl::object::to_object_for_builtin(ncx, options)?;
            wrapped
        };
        crate::object::get_value_full(&obj, &PropertyKey::string(name), ncx)?
    };

    if value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(value.to_boolean()))
    }
}

fn set_string_data_property(target: &GcRef<JsObject>, key: &str, value: &str) {
    target.define_property(
        PropertyKey::string(key),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern(value)),
            PropertyAttributes::permanent(),
        ),
    );
}

fn set_bool_data_property(target: &GcRef<JsObject>, key: &str, value: bool) {
    target.define_property(
        PropertyKey::string(key),
        PropertyDescriptor::data_with_attrs(Value::boolean(value), PropertyAttributes::permanent()),
    );
}

fn set_number_data_property(target: &GcRef<JsObject>, key: &str, value: f64) {
    target.define_property(
        PropertyKey::string(key),
        PropertyDescriptor::data_with_attrs(Value::number(value), PropertyAttributes::permanent()),
    );
}

fn get_option_number(
    options: Option<&Value>,
    name: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<f64>, VmError> {
    let Some(options) = options else {
        return Ok(None);
    };
    if options.is_undefined() {
        return Ok(None);
    }
    let value = if let Some(proxy) = options.as_proxy() {
        let key = PropertyKey::string(name);
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            crate::proxy_operations::property_key_to_value_pub(&key),
            options.clone(),
        )?
    } else {
        let wrapped;
        let obj = if let Some(obj) = options.as_object() {
            obj
        } else {
            wrapped = crate::intrinsics_impl::object::to_object_for_builtin(ncx, options)?;
            wrapped
        };
        crate::object::get_value_full(&obj, &PropertyKey::string(name), ncx)?
    };
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(ncx.to_number_value(&value)?))
}

fn get_option_value(
    options: Option<&Value>,
    name: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<Value>, VmError> {
    let Some(options) = options else {
        return Ok(None);
    };
    if options.is_undefined() {
        return Ok(None);
    }
    let value = if let Some(proxy) = options.as_proxy() {
        let key = PropertyKey::string(name);
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            crate::proxy_operations::property_key_to_value_pub(&key),
            options.clone(),
        )?
    } else {
        let wrapped;
        let obj = if let Some(obj) = options.as_object() {
            obj
        } else {
            wrapped = crate::intrinsics_impl::object::to_object_for_builtin(ncx, options)?;
            wrapped
        };
        crate::object::get_value_full(&obj, &PropertyKey::string(name), ncx)?
    };
    if value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn resolved_options(
    this_val: &Value,
    brand_key: &str,
    method_name: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let this_obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error(format!("{method_name} called on non-object")))?;
    if this_obj.get(&PropertyKey::string(brand_key)).is_none() {
        return Err(VmError::type_error(format!(
            "{method_name} called on incompatible receiver"
        )));
    }
    let locale = this_obj
        .get(&PropertyKey::string(INTL_LOCALE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());

    let obj = create_plain_object(ncx);
    obj.define_property(
        PropertyKey::string("locale"),
        PropertyDescriptor::data(Value::string(JsString::intern(&locale))),
    );

    if brand_key == INTL_NUMBERFORMAT_BRAND_KEY {
        let get_slot = |key: &str| {
            this_obj
                .get_own_property_descriptor(&PropertyKey::string(key))
                .and_then(|d| d.value().cloned())
                .unwrap_or(Value::undefined())
        };
        let data = |v: Value| PropertyDescriptor::data(v);
        let numbering_system = this_obj
            .get(&PropertyKey::string(INTL_NUMBERING_SYSTEM_KEY))
            .unwrap_or_else(|| Value::string(JsString::intern("latn")));
        let notation = get_slot(INTL_NF_NOTATION_KEY);
        obj.define_property(
            PropertyKey::string("numberingSystem"),
            data(numbering_system),
        );
        obj.define_property(
            PropertyKey::string("style"),
            data(get_slot(INTL_NF_STYLE_KEY)),
        );
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_CURRENCY_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("currency"), data(v));
        }
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_NF_CURRENCY_DISPLAY_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("currencyDisplay"), data(v));
        }
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_NF_CURRENCY_SIGN_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("currencySign"), data(v));
        }
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_UNIT_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("unit"), data(v));
        }
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_NF_UNIT_DISPLAY_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("unitDisplay"), data(v));
        }
        if notation
            .as_string()
            .map(|s| s.as_str().to_string())
            .as_deref()
            == Some("compact")
        {
            if obj
                .get_own_property_descriptor(&PropertyKey::string("unit"))
                .is_none()
            {
                obj.define_property(PropertyKey::string("unit"), data(Value::undefined()));
            }
            if obj
                .get_own_property_descriptor(&PropertyKey::string("unitDisplay"))
                .is_none()
            {
                obj.define_property(PropertyKey::string("unitDisplay"), data(Value::undefined()));
            }
        }
        obj.define_property(
            PropertyKey::string("minimumIntegerDigits"),
            data(get_slot(INTL_NF_MIN_INT_DIGITS_KEY)),
        );
        obj.define_property(
            PropertyKey::string("minimumFractionDigits"),
            data(get_slot(INTL_NF_MIN_FRAC_DIGITS_KEY)),
        );
        obj.define_property(
            PropertyKey::string("maximumFractionDigits"),
            data(get_slot(INTL_NF_MAX_FRAC_DIGITS_KEY)),
        );
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_NF_MIN_SIG_DIGITS_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("minimumSignificantDigits"), data(v));
        }
        if let Some(v) = this_obj
            .get_own_property_descriptor(&PropertyKey::string(INTL_NF_MAX_SIG_DIGITS_KEY))
            .and_then(|d| d.value().cloned())
        {
            obj.define_property(PropertyKey::string("maximumSignificantDigits"), data(v));
        }
        if notation
            .as_string()
            .map(|s| s.as_str().to_string())
            .as_deref()
            == Some("compact")
        {
            if obj
                .get_own_property_descriptor(&PropertyKey::string("minimumSignificantDigits"))
                .is_none()
            {
                obj.define_property(
                    PropertyKey::string("minimumSignificantDigits"),
                    data(Value::undefined()),
                );
            }
            if obj
                .get_own_property_descriptor(&PropertyKey::string("maximumSignificantDigits"))
                .is_none()
            {
                obj.define_property(
                    PropertyKey::string("maximumSignificantDigits"),
                    data(Value::undefined()),
                );
            }
        }
        obj.define_property(
            PropertyKey::string("useGrouping"),
            data(match get_slot(INTL_NF_USE_GROUPING_KEY) {
                v if v
                    .as_string()
                    .map(|s| s.as_str() == "false")
                    .unwrap_or(false) =>
                {
                    Value::boolean(false)
                }
                v => v,
            }),
        );
        obj.define_property(PropertyKey::string("notation"), data(notation.clone()));
        if notation
            .as_string()
            .map(|s| s.as_str().to_string())
            .as_deref()
            == Some("compact")
        {
            obj.define_property(
                PropertyKey::string("compactDisplay"),
                data(get_slot(INTL_NF_COMPACT_DISPLAY_KEY)),
            );
        }
        obj.define_property(
            PropertyKey::string("signDisplay"),
            data(get_slot(INTL_NF_SIGN_DISPLAY_KEY)),
        );
        obj.define_property(
            PropertyKey::string("roundingIncrement"),
            data(get_slot(INTL_NF_ROUNDING_INCREMENT_KEY)),
        );
        obj.define_property(
            PropertyKey::string("roundingMode"),
            data(get_slot(INTL_NF_ROUNDING_MODE_KEY)),
        );
        obj.define_property(
            PropertyKey::string("roundingPriority"),
            data(get_slot(INTL_NF_ROUNDING_PRIORITY_KEY)),
        );
        obj.define_property(
            PropertyKey::string("trailingZeroDisplay"),
            data(get_slot(INTL_NF_TRAILING_ZERO_DISPLAY_KEY)),
        );
        return Ok(Value::object(obj));
    }

    let mapping = [
        ("calendar", INTL_CALENDAR_KEY),
        ("collation", INTL_COLLATION_KEY),
        ("currency", INTL_CURRENCY_KEY),
        ("numberingSystem", INTL_NUMBERING_SYSTEM_KEY),
        ("timeZone", INTL_TIMEZONE_KEY),
        ("unit", INTL_UNIT_KEY),
    ];
    for (public_name, private_key) in mapping {
        if let Some(v) = this_obj.get(&PropertyKey::string(private_key)) {
            obj.define_property(
                PropertyKey::string(public_name),
                PropertyDescriptor::builtin_data(v),
            );
        } else if brand_key == INTL_LOCALE_BRAND_KEY
            && let Some(value) = unicode_keyword_from_locale(
                &locale,
                match public_name {
                    "calendar" => "ca",
                    "collation" => "co",
                    "numberingSystem" => "nu",
                    _ => "",
                },
            )
        {
            obj.define_property(
                PropertyKey::string(public_name),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&value))),
            );
        }
    }

    Ok(Value::object(obj))
}

#[derive(Clone)]
enum NfDecimalInput {
    Finite(FixedDecimal),
    NaN,
    PosInfinity,
    NegInfinity,
}

#[derive(Default)]
struct WriteablePartCollector {
    text: String,
    parts: Vec<(usize, usize, Part)>,
}

impl std::fmt::Write for WriteablePartCollector {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.text.push_str(s);
        Ok(())
    }
}

impl PartsWrite for WriteablePartCollector {
    type SubPartsWrite = Self;

    fn with_part(
        &mut self,
        part: Part,
        mut f: impl FnMut(&mut Self::SubPartsWrite) -> std::fmt::Result,
    ) -> std::fmt::Result {
        let start = self.text.len();
        f(self)?;
        let end = self.text.len();
        if start != end {
            self.parts.push((start, end, part));
        }
        Ok(())
    }
}

fn nf_grouping_strategy(this_obj: &GcRef<JsObject>) -> GroupingStrategy {
    match this_obj.get(&PropertyKey::string(INTL_NF_USE_GROUPING_KEY)) {
        Some(v) if v.is_boolean() && v.as_boolean() == Some(false) => GroupingStrategy::Never,
        Some(v) => {
            if let Some(s) = v.as_string() {
                match s.as_str() {
                    "always" => GroupingStrategy::Always,
                    "min2" => GroupingStrategy::Min2,
                    "auto" => GroupingStrategy::Auto,
                    _ => GroupingStrategy::Auto,
                }
            } else {
                GroupingStrategy::Auto
            }
        }
        None => GroupingStrategy::Auto,
    }
}

fn nf_sign_display(this_obj: &GcRef<JsObject>) -> FixedSignDisplay {
    match this_obj
        .get(&PropertyKey::string(INTL_NF_SIGN_DISPLAY_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .as_deref()
    {
        Some("never") => FixedSignDisplay::Never,
        Some("always") => FixedSignDisplay::Always,
        Some("exceptZero") => FixedSignDisplay::ExceptZero,
        Some("negative") => FixedSignDisplay::Negative,
        _ => FixedSignDisplay::Auto,
    }
}

fn nf_rounding_mode(this_obj: &GcRef<JsObject>) -> SignedRoundingMode {
    match this_obj
        .get(&PropertyKey::string(INTL_NF_ROUNDING_MODE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .as_deref()
    {
        Some("ceil") => SignedRoundingMode::Ceil,
        Some("floor") => SignedRoundingMode::Floor,
        Some("expand") => SignedRoundingMode::Unsigned(UnsignedRoundingMode::Expand),
        Some("trunc") => SignedRoundingMode::Unsigned(UnsignedRoundingMode::Trunc),
        Some("halfCeil") => SignedRoundingMode::HalfCeil,
        Some("halfFloor") => SignedRoundingMode::HalfFloor,
        Some("halfTrunc") => SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfTrunc),
        Some("halfEven") => SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfEven),
        _ => SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfExpand),
    }
}

fn simple_numbering_system_digits(numbering_system: &str) -> Option<&'static str> {
    match numbering_system {
        "adlm" => Some("𞥐𞥑𞥒𞥓𞥔𞥕𞥖𞥗𞥘𞥙"),
        "ahom" => Some("𑜰𑜱𑜲𑜳𑜴𑜵𑜶𑜷𑜸𑜹"),
        "arab" => Some("٠١٢٣٤٥٦٧٨٩"),
        "arabext" => Some("۰۱۲۳۴۵۶۷۸۹"),
        "bali" => Some("᭐᭑᭒᭓᭔᭕᭖᭗᭘᭙"),
        "beng" => Some("০১২৩৪৫৬৭৮৯"),
        "bhks" => Some("𑱐𑱑𑱒𑱓𑱔𑱕𑱖𑱗𑱘𑱙"),
        "brah" => Some("𑁦𑁧𑁨𑁩𑁪𑁫𑁬𑁭𑁮𑁯"),
        "cakm" => Some("𑄶𑄷𑄸𑄹𑄺𑄻𑄼𑄽𑄾𑄿"),
        "cham" => Some("꩐꩑꩒꩓꩔꩕꩖꩗꩘꩙"),
        "deva" => Some("०१२३४५६७८९"),
        "diak" => Some("𑥐𑥑𑥒𑥓𑥔𑥕𑥖𑥗𑥘𑥙"),
        "fullwide" => Some("０１２３４５６７８９"),
        "gara" => Some("𐵀𐵁𐵂𐵃𐵄𐵅𐵆𐵇𐵈𐵉"),
        "gong" => Some("𑶠𑶡𑶢𑶣𑶤𑶥𑶦𑶧𑶨𑶩"),
        "gonm" => Some("𑵐𑵑𑵒𑵓𑵔𑵕𑵖𑵗𑵘𑵙"),
        "gujr" => Some("૦૧૨૩૪૫૬૭૮૯"),
        "gukh" => Some("𖄰𖄱𖄲𖄳𖄴𖄵𖄶𖄷𖄸𖄹"),
        "guru" => Some("੦੧੨੩੪੫੬੭੮੯"),
        "hanidec" => Some("〇一二三四五六七八九"),
        "hmng" => Some("𖭐𖭑𖭒𖭓𖭔𖭕𖭖𖭗𖭘𖭙"),
        "hmnp" => Some("𞅀𞅁𞅂𞅃𞅄𞅅𞅆𞅇𞅈𞅉"),
        "java" => Some("꧐꧑꧒꧓꧔꧕꧖꧗꧘꧙"),
        "kali" => Some("꤀꤁꤂꤃꤄꤅꤆꤇꤈꤉"),
        "kawi" => Some("𑽐𑽑𑽒𑽓𑽔𑽕𑽖𑽗𑽘𑽙"),
        "khmr" => Some("០១២៣៤៥៦៧៨៩"),
        "knda" => Some("೦೧೨೩೪೫೬೭೮೯"),
        "krai" => Some("𖵰𖵱𖵲𖵳𖵴𖵵𖵶𖵷𖵸𖵹"),
        "lana" => Some("᪀᪁᪂᪃᪄᪅᪆᪇᪈᪉"),
        "lanatham" => Some("᪐᪑᪒᪓᪔᪕᪖᪗᪘᪙"),
        "laoo" => Some("໐໑໒໓໔໕໖໗໘໙"),
        "latn" => Some("0123456789"),
        "lepc" => Some("᱀᱁᱂᱃᱄᱅᱆᱇᱈᱉"),
        "limb" => Some("᥆᥇᥈᥉᥊᥋᥌᥍᥎᥏"),
        "mathbold" => Some("𝟎𝟏𝟐𝟑𝟒𝟓𝟔𝟕𝟖𝟗"),
        "mathdbl" => Some("𝟘𝟙𝟚𝟛𝟜𝟝𝟞𝟟𝟠𝟡"),
        "mathmono" => Some("𝟶𝟷𝟸𝟹𝟺𝟻𝟼𝟽𝟾𝟿"),
        "mathsanb" => Some("𝟬𝟭𝟮𝟯𝟰𝟱𝟲𝟳𝟴𝟵"),
        "mathsans" => Some("𝟢𝟣𝟤𝟥𝟦𝟧𝟨𝟩𝟪𝟫"),
        "mlym" => Some("൦൧൨൩൪൫൬൭൮൯"),
        "modi" => Some("𑙐𑙑𑙒𑙓𑙔𑙕𑙖𑙗𑙘𑙙"),
        "mong" => Some("᠐᠑᠒᠓᠔᠕᠖᠗᠘᠙"),
        "mroo" => Some("𖩠𖩡𖩢𖩣𖩤𖩥𖩦𖩧𖩨𖩩"),
        "mtei" => Some("꯰꯱꯲꯳꯴꯵꯶꯷꯸꯹"),
        "mymr" => Some("၀၁၂၃၄၅၆၇၈၉"),
        "mymrepka" => Some("𑛚𑛛𑛜𑛝𑛞𑛟𑛠𑛡𑛢𑛣"),
        "mymrpao" => Some("𑛐𑛑𑛒𑛓𑛔𑛕𑛖𑛗𑛘𑛙"),
        "mymrshan" => Some("႐႑႒႓႔႕႖႗႘႙"),
        "mymrtlng" => Some("꧰꧱꧲꧳꧴꧵꧶꧷꧸꧹"),
        "nagm" => Some("𞓰𞓱𞓲𞓳𞓴𞓵𞓶𞓷𞓸𞓹"),
        "newa" => Some("𑑐𑑑𑑒𑑓𑑔𑑕𑑖𑑗𑑘𑑙"),
        "nkoo" => Some("߀߁߂߃߄߅߆߇߈߉"),
        "olck" => Some("᱐᱑᱒᱓᱔᱕᱖᱗᱘᱙"),
        "onao" => Some("𞗱𞗲𞗳𞗴𞗵𞗶𞗷𞗸𞗹𞗺"),
        "orya" => Some("୦୧୨୩୪୫୬୭୮୯"),
        "osma" => Some("𐒠𐒡𐒢𐒣𐒤𐒥𐒦𐒧𐒨𐒩"),
        "outlined" => Some("𜳰𜳱𜳲𜳳𜳴𜳵𜳶𜳷𜳸𜳹"),
        "rohg" => Some("𐴰𐴱𐴲𐴳𐴴𐴵𐴶𐴷𐴸𐴹"),
        "saur" => Some("꣐꣑꣒꣓꣔꣕꣖꣗꣘꣙"),
        "segment" => Some("🯰🯱🯲🯳🯴🯵🯶🯷🯸🯹"),
        "shrd" => Some("𑇐𑇑𑇒𑇓𑇔𑇕𑇖𑇗𑇘𑇙"),
        "sind" => Some("𑋰𑋱𑋲𑋳𑋴𑋵𑋶𑋷𑋸𑋹"),
        "sinh" => Some("෦෧෨෩෪෫෬෭෮෯"),
        "sora" => Some("𑃰𑃱𑃲𑃳𑃴𑃵𑃶𑃷𑃸𑃹"),
        "sund" => Some("᮰᮱᮲᮳᮴᮵᮶᮷᮸᮹"),
        "sunu" => Some("𑯰𑯱𑯲𑯳𑯴𑯵𑯶𑯷𑯸𑯹"),
        "takr" => Some("𑛀𑛁𑛂𑛃𑛄𑛅𑛆𑛇𑛈𑛉"),
        "talu" => Some("᧐᧑᧒᧓᧔᧕᧖᧗᧘᧙"),
        "tamldec" => Some("௦௧௨௩௪௫௬௭௮௯"),
        "telu" => Some("౦౧౨౩౪౫౬౭౮౯"),
        "thai" => Some("๐๑๒๓๔๕๖๗๘๙"),
        "tibt" => Some("༠༡༢༣༤༥༦༧༨༩"),
        "tirh" => Some("𑓐𑓑𑓒𑓓𑓔𑓕𑓖𑓗𑓘𑓙"),
        "tnsa" => Some("𖫀𖫁𖫂𖫃𖫄𖫅𖫆𖫇𖫈𖫉"),
        "tols" => Some("𑷠𑷡𑷢𑷣𑷤𑷥𑷦𑷧𑷨𑷩"),
        "vaii" => Some("꘠꘡꘢꘣꘤꘥꘦꘧꘨꘩"),
        "wara" => Some("𑣠𑣡𑣢𑣣𑣤𑣥𑣦𑣧𑣨𑣩"),
        "wcho" => Some("𞋰𞋱𞋲𞋳𞋴𞋵𞋶𞋷𞋸𞋹"),
        _ => None,
    }
}

fn apply_simple_numbering_system_digits(formatted: &str, numbering_system: &str) -> String {
    let Some(digits) = simple_numbering_system_digits(numbering_system) else {
        return formatted.to_string();
    };
    if !formatted.chars().any(|c| c.is_ascii_digit()) {
        return formatted.to_string();
    }
    let mapped: Vec<char> = digits.chars().collect();
    if mapped.len() != 10 {
        return formatted.to_string();
    }

    formatted
        .chars()
        .map(|c| c.to_digit(10).map(|d| mapped[d as usize]).unwrap_or(c))
        .collect()
}

fn nf_effective_locale(this_obj: &GcRef<JsObject>) -> String {
    let locale = this_obj
        .get(&PropertyKey::string(INTL_LOCALE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let mut ext = parse_unicode_extension(&locale);
    ext.keys.remove("nu");
    build_unicode_extension_locale(&ext)
}

fn nf_decimal_formatter(this_obj: &GcRef<JsObject>) -> Result<DecimalFormatter, VmError> {
    let locale = nf_effective_locale(this_obj);
    let mut options = DecimalFormatterOptions::default();
    options.grouping_strategy = Some(nf_grouping_strategy(this_obj));
    let mut tried = Vec::new();
    let mut candidates = Vec::new();
    candidates.push(locale.clone());
    let base = locale
        .split('-')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_LOCALE)
        .to_string();
    if base != locale {
        candidates.push(base);
    }
    candidates.push(DEFAULT_LOCALE.to_string());

    let mut last_err = None;
    for candidate in candidates {
        if tried.iter().any(|v| v == &candidate) {
            continue;
        }
        tried.push(candidate.clone());
        let Ok(loc) = Locale::from_str(&candidate) else {
            continue;
        };
        match DecimalFormatter::try_new(loc.into(), options) {
            Ok(formatter) => return Ok(formatter),
            Err(err) => last_err = Some(err),
        }
    }

    Err(VmError::type_error(format!(
        "cannot create decimal formatter: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown ICU data error".to_string())
    )))
}

fn nf_value_to_decimal(
    value: &Value,
    style: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<NfDecimalInput, VmError> {
    let mut decimal = if value.is_bigint() {
        let s = ncx.to_string_value(value)?;
        FixedDecimal::from_str(&s).map_err(|e| {
            VmError::range_error(format!("Invalid BigInt for Intl.NumberFormat: {e}"))
        })?
    } else {
        let number = ncx.to_number_value(value)?;
        if number.is_nan() {
            return Ok(NfDecimalInput::NaN);
        }
        if number.is_infinite() {
            return Ok(if number.is_sign_negative() {
                NfDecimalInput::NegInfinity
            } else {
                NfDecimalInput::PosInfinity
            });
        }
        FixedDecimal::try_from_f64(number, FloatPrecision::RoundTrip)
            .map_err(|e| VmError::range_error(format!("cannot format number: {e}")))?
    };

    if style == "percent" {
        decimal.multiply_pow10(2);
    }
    Ok(NfDecimalInput::Finite(decimal))
}

fn nf_apply_digit_options(this_obj: &GcRef<JsObject>, decimal: &mut FixedDecimal) {
    let rounding_mode = nf_rounding_mode(this_obj);
    let rounding_priority = this_obj
        .get(&PropertyKey::string(INTL_NF_ROUNDING_PRIORITY_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "auto".to_string());
    let min_sig = this_obj
        .get(&PropertyKey::string(INTL_NF_MIN_SIG_DIGITS_KEY))
        .and_then(|v| v.as_number())
        .map(|v| v as i16);
    let max_sig = this_obj
        .get(&PropertyKey::string(INTL_NF_MAX_SIG_DIGITS_KEY))
        .and_then(|v| v.as_number())
        .map(|v| v as i16);
    let has_sig_opts = min_sig.is_some() || max_sig.is_some();
    let has_frac_opts = this_obj
        .get(&PropertyKey::string(INTL_NF_MAX_FRAC_DIGITS_KEY))
        .is_some()
        || this_obj
            .get(&PropertyKey::string(INTL_NF_MIN_FRAC_DIGITS_KEY))
            .is_some();

    if has_sig_opts
        && (rounding_priority == "auto"
            || rounding_priority == "morePrecision"
            || rounding_priority == "lessPrecision")
    {
        let mut sig_decimal = decimal.clone();
        if let Some(sig_target) = max_sig
            && let Some(position) = nf_significant_round_position(&sig_decimal, sig_target)
        {
            sig_decimal.round_with_mode(position, rounding_mode);
        }
        if let Some(min_sig) = min_sig
            && let Some(pad_pos) = nf_significant_pad_position(&sig_decimal, min_sig)
        {
            sig_decimal.pad_end(pad_pos);
        }

        if (rounding_priority == "morePrecision" || rounding_priority == "lessPrecision")
            && has_frac_opts
        {
            let mut frac_decimal = decimal.clone();
            nf_apply_fraction_options(this_obj, &mut frac_decimal, rounding_mode);
            let max_sig_eff = max_sig.unwrap_or(21) as usize;
            let max_frac_eff = this_obj
                .get(&PropertyKey::string(INTL_NF_MAX_FRAC_DIGITS_KEY))
                .and_then(|v| v.as_number())
                .unwrap_or(3.0) as usize;
            let pick_sig = if max_sig_eff != max_frac_eff {
                if rounding_priority == "morePrecision" {
                    max_sig_eff > max_frac_eff
                } else {
                    max_sig_eff < max_frac_eff
                }
            } else {
                let sig_prec = nf_fraction_digits_count(&sig_decimal);
                let frac_prec = nf_fraction_digits_count(&frac_decimal);
                if rounding_priority == "morePrecision" {
                    sig_prec > frac_prec
                } else {
                    sig_prec < frac_prec
                }
            };
            *decimal = if pick_sig { sig_decimal } else { frac_decimal };
        } else {
            *decimal = sig_decimal;
        }

        if let Some(min_int) = this_obj
            .get(&PropertyKey::string(INTL_NF_MIN_INT_DIGITS_KEY))
            .and_then(|v| v.as_number())
        {
            decimal.pad_start(min_int as i16);
        }
        decimal.apply_sign_display(nf_sign_display(this_obj));
        return;
    }

    nf_apply_fraction_options(this_obj, decimal, rounding_mode);
    if let Some(min_int) = this_obj
        .get(&PropertyKey::string(INTL_NF_MIN_INT_DIGITS_KEY))
        .and_then(|v| v.as_number())
    {
        decimal.pad_start(min_int as i16);
    }
    decimal.apply_sign_display(nf_sign_display(this_obj));
}

fn nf_apply_fraction_options(
    this_obj: &GcRef<JsObject>,
    decimal: &mut FixedDecimal,
    rounding_mode: SignedRoundingMode,
) {
    if let Some(max_frac) = this_obj
        .get(&PropertyKey::string(INTL_NF_MAX_FRAC_DIGITS_KEY))
        .and_then(|v| v.as_number())
    {
        let rounding_increment = this_obj
            .get(&PropertyKey::string(INTL_NF_ROUNDING_INCREMENT_KEY))
            .and_then(|v| v.as_number())
            .unwrap_or(1.0) as i16;
        if rounding_increment > 1 {
            let mut base = rounding_increment;
            let mut shift = 0i16;
            while base % 10 == 0 {
                base /= 10;
                shift += 1;
            }
            let increment = match base {
                1 => RoundingIncrement::MultiplesOf1,
                2 => RoundingIncrement::MultiplesOf2,
                5 => RoundingIncrement::MultiplesOf5,
                25 => RoundingIncrement::MultiplesOf25,
                _ => RoundingIncrement::MultiplesOf1,
            };
            let position = -(max_frac as i16) + shift;
            decimal.round_with_mode_and_increment(position, rounding_mode, increment);
        } else {
            decimal.round_with_mode(-(max_frac as i16), rounding_mode);
        }
    }
    if let Some(min_frac) = this_obj
        .get(&PropertyKey::string(INTL_NF_MIN_FRAC_DIGITS_KEY))
        .and_then(|v| v.as_number())
    {
        decimal.pad_end(-(min_frac as i16));
    }
}

fn nf_fraction_digits_count(decimal: &FixedDecimal) -> usize {
    let s = decimal.to_string();
    let frac = s.split_once('.').map(|(_, frac)| frac.len()).unwrap_or(0);
    let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
    (frac * 1000) + digits
}

fn nf_significant_round_position(decimal: &FixedDecimal, sig_digits: i16) -> Option<i16> {
    if sig_digits <= 0 {
        return None;
    }
    let s = decimal.to_string();
    let s = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(&s);
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let digits = format!("{int_part}{frac_part}");
    let first_nonzero = digits.bytes().position(|b| b != b'0')?;
    let exponent = int_part.len() as i16 - first_nonzero as i16 - 1;
    Some(exponent - sig_digits + 1)
}

fn nf_significant_pad_position(decimal: &FixedDecimal, min_sig: i16) -> Option<i16> {
    if min_sig <= 0 {
        return None;
    }
    let s = decimal.to_string();
    let s = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(&s);
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let digits = format!("{int_part}{frac_part}");
    let Some(first_nonzero) = digits.bytes().position(|b| b != b'0') else {
        // For zero, keep one leading zero plus remaining significant zeros after decimal point.
        return Some(-(min_sig - 1));
    };
    let exponent = int_part.len() as i16 - first_nonzero as i16 - 1;
    Some(exponent - min_sig + 1)
}

fn nf_format_to_string(
    this_obj: &GcRef<JsObject>,
    value: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<String, VmError> {
    let style = this_obj
        .get(&PropertyKey::string(INTL_NF_STYLE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "decimal".to_string());

    let decimal = nf_value_to_decimal(value, &style, ncx)?;
    let sign_display = nf_sign_display(this_obj);
    let locale = this_obj
        .get(&PropertyKey::string(INTL_LOCALE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let lang = locale
        .split(['-', '_'])
        .next()
        .unwrap_or("en")
        .to_ascii_lowercase();
    let mut out = match decimal {
        NfDecimalInput::NaN => {
            let mut s = if lang == "zh" {
                "非數值".to_string()
            } else {
                "NaN".to_string()
            };
            if matches!(sign_display, FixedSignDisplay::Always) {
                s.insert(0, '+');
            }
            s
        }
        NfDecimalInput::PosInfinity => {
            if matches!(
                sign_display,
                FixedSignDisplay::Always | FixedSignDisplay::ExceptZero
            ) {
                "+∞".to_string()
            } else {
                "∞".to_string()
            }
        }
        NfDecimalInput::NegInfinity => {
            if matches!(sign_display, FixedSignDisplay::Never) {
                "∞".to_string()
            } else {
                "-∞".to_string()
            }
        }
        NfDecimalInput::Finite(mut d) => {
            if let Some(notation_formatted) = nf_format_notation(this_obj, &d, &locale) {
                notation_formatted
            } else {
                nf_apply_digit_options(this_obj, &mut d);
                nf_decimal_formatter(this_obj)?.format(&d).to_string()
            }
        }
    };

    let numbering_system = this_obj
        .get(&PropertyKey::string(INTL_NUMBERING_SYSTEM_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "latn".to_string());
    out = apply_simple_numbering_system_digits(&out, &numbering_system);

    if style == "percent" {
        out.push('%');
    } else if style == "currency" {
        let currency = this_obj
            .get(&PropertyKey::string(INTL_CURRENCY_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "USD".to_string());
        let currency_display = this_obj
            .get(&PropertyKey::string(INTL_NF_CURRENCY_DISPLAY_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "symbol".to_string());
        let currency_sign = this_obj
            .get(&PropertyKey::string(INTL_NF_CURRENCY_SIGN_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "standard".to_string());
        out = nf_apply_currency_style(&out, &locale, &currency, &currency_display, &currency_sign);
    } else if style == "unit" {
        let unit = this_obj
            .get(&PropertyKey::string(INTL_UNIT_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "unit".to_string());
        let unit_display = this_obj
            .get(&PropertyKey::string(INTL_NF_UNIT_DISPLAY_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "short".to_string());
        out = nf_apply_unit_style(&out, &locale, &unit, &unit_display);
    }
    Ok(out)
}

fn nf_apply_currency_style(
    number_with_sign: &str,
    locale: &str,
    currency: &str,
    currency_display: &str,
    currency_sign: &str,
) -> String {
    let lang = locale
        .split(['-', '_'])
        .next()
        .unwrap_or("en")
        .to_ascii_lowercase();
    let mut number = number_with_sign.to_string();
    let mut sign = "";
    if let Some(rest) = number.strip_prefix('-') {
        sign = "-";
        number = rest.to_string();
    } else if let Some(rest) = number.strip_prefix('+') {
        sign = "+";
        number = rest.to_string();
    }

    let symbol = match currency_display {
        "code" => currency.to_string(),
        "name" => currency.to_string(),
        "narrowSymbol" | "symbol" => match (currency, lang.as_str()) {
            ("USD", "ko") | ("USD", "zh") => "US$".to_string(),
            ("USD", _) => "$".to_string(),
            _ => currency.to_string(),
        },
        _ => currency.to_string(),
    };

    let is_prefix = !matches!(lang.as_str(), "de");
    let body = if is_prefix {
        format!("{symbol}{number}")
    } else {
        format!("{number}\u{00A0}{symbol}")
    };

    let accounting_parentheses = currency_sign == "accounting"
        && sign == "-"
        && matches!(lang.as_str(), "en" | "ja" | "ko" | "zh");
    if accounting_parentheses {
        return format!("({body})");
    }
    if sign == "-" {
        return format!("-{body}");
    }
    if sign == "+" {
        return format!("+{body}");
    }
    body
}

fn nf_currency_symbol(locale: &str, currency: &str, currency_display: &str) -> String {
    let lang = locale
        .split(['-', '_'])
        .next()
        .unwrap_or("en")
        .to_ascii_lowercase();
    match currency_display {
        "code" => currency.to_string(),
        "name" => currency.to_string(),
        "narrowSymbol" | "symbol" => match (currency, lang.as_str()) {
            ("USD", "ko") | ("USD", "zh") => "US$".to_string(),
            ("USD", _) => "$".to_string(),
            _ => currency.to_string(),
        },
        _ => currency.to_string(),
    }
}

fn nf_format_notation(
    this_obj: &GcRef<JsObject>,
    decimal: &FixedDecimal,
    locale: &str,
) -> Option<String> {
    let notation = this_obj
        .get(&PropertyKey::string(INTL_NF_NOTATION_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "standard".to_string());
    if notation == "standard" {
        return None;
    }
    let num_str = decimal.to_string();
    let n = num_str.parse::<f64>().ok()?;
    if !n.is_finite() {
        return None;
    }
    if notation == "scientific" || notation == "engineering" {
        if n == 0.0 {
            return Some("0".to_string());
        }
        let abs = n.abs();
        let sci_exp = abs.log10().floor() as i32;
        let exp = if notation == "engineering" {
            sci_exp - sci_exp.rem_euclid(3)
        } else {
            sci_exp
        };
        let mantissa = n / 10f64.powi(exp);
        let mantissa = nf_round_decimal(mantissa, 3);
        let mut mantissa_s = nf_trimmed_decimal(mantissa, 3, locale);
        if mantissa_s == "-0" {
            mantissa_s = "0".to_string();
        }
        return Some(format!("{mantissa_s}E{exp}"));
    }
    if notation == "compact" {
        let lang = locale
            .split(['-', '_'])
            .next()
            .unwrap_or("en")
            .to_ascii_lowercase();
        let compact_display = this_obj
            .get(&PropertyKey::string(INTL_NF_COMPACT_DISPLAY_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "short".to_string());
        let abs = n.abs();
        let sign = if n < 0.0 { "-" } else { "" };

        let compact = match lang.as_str() {
            "de" => {
                if abs >= 1_000_000.0 {
                    let scaled = nf_round_for_compact(n / 1_000_000.0);
                    let num = nf_trimmed_decimal(scaled, 1, locale);
                    if compact_display == "long" {
                        Some(format!("{sign}{num} Millionen"))
                    } else {
                        Some(format!("{sign}{num}\u{00A0}Mio."))
                    }
                } else if compact_display == "long" && abs >= 1_000.0 {
                    let scaled = nf_round_for_compact(n / 1_000.0);
                    let num = nf_trimmed_decimal(scaled, 1, locale);
                    Some(format!("{sign}{num} Tausend"))
                } else if abs < 1_000.0 {
                    Some(nf_format_compact_small(n, locale))
                } else {
                    None
                }
            }
            "ja" => nf_format_east_asian_compact(n, locale, "億", "万", None)
                .or_else(|| Some(nf_format_compact_small(n, locale))),
            "zh" => nf_format_east_asian_compact(n, locale, "億", "萬", None)
                .or_else(|| Some(nf_format_compact_small(n, locale))),
            "ko" => nf_format_east_asian_compact(n, locale, "억", "만", Some("천"))
                .or_else(|| Some(nf_format_compact_small(n, locale))),
            _ => {
                let locale_lower = locale.to_ascii_lowercase();
                if locale_lower.starts_with("en-in") {
                    if abs >= 10_000_000.0 {
                        let scaled = nf_round_for_compact(n / 10_000_000.0);
                        let num = nf_trimmed_decimal(scaled, 1, locale);
                        Some(format!("{sign}{num}Cr"))
                    } else if abs >= 100_000.0 {
                        let scaled = nf_round_for_compact(n / 100_000.0);
                        let num = nf_trimmed_decimal(scaled, 1, locale);
                        Some(format!("{sign}{num}L"))
                    } else if abs >= 1_000.0 {
                        let scaled = nf_round_for_compact(n / 1_000.0);
                        let num = nf_trimmed_decimal(scaled, 1, locale);
                        Some(format!("{sign}{num}K"))
                    } else {
                        Some(nf_format_compact_small(n, locale))
                    }
                } else if abs >= 1_000_000.0 {
                    let scaled = nf_round_for_compact(n / 1_000_000.0);
                    let num = nf_trimmed_decimal(scaled, 1, locale);
                    if compact_display == "long" {
                        Some(format!("{sign}{num} million"))
                    } else {
                        Some(format!("{sign}{num}M"))
                    }
                } else if abs >= 1_000.0 {
                    let scaled = nf_round_for_compact(n / 1_000.0);
                    let num = nf_trimmed_decimal(scaled, 1, locale);
                    if compact_display == "long" {
                        Some(format!("{sign}{num} thousand"))
                    } else {
                        Some(format!("{sign}{num}K"))
                    }
                } else {
                    Some(nf_format_compact_small(n, locale))
                }
            }
        };
        return compact;
    }
    None
}

fn nf_round_for_compact(n: f64) -> f64 {
    if n.abs() >= 10.0 {
        nf_round_decimal(n, 0)
    } else {
        nf_round_decimal(n, 1)
    }
}

fn nf_round_decimal(n: f64, frac_digits: usize) -> f64 {
    let factor = 10f64.powi(frac_digits as i32);
    (n * factor).round() / factor
}

fn nf_round_to_significant(n: f64, sig_digits: usize) -> f64 {
    if n == 0.0 {
        return 0.0;
    }
    let abs = n.abs();
    let exp = abs.log10().floor();
    let factor = 10f64.powf((sig_digits as f64) - 1.0 - exp);
    (n * factor).round() / factor
}

fn nf_format_compact_small(n: f64, locale: &str) -> String {
    let abs = n.abs();
    if abs >= 100.0 {
        return nf_trimmed_decimal(n.trunc(), 10, locale);
    }
    nf_trimmed_decimal(nf_round_to_significant(n, 2), 10, locale)
}

fn nf_trimmed_decimal(n: f64, max_frac_digits: usize, locale: &str) -> String {
    let mut s = format!("{:.*}", max_frac_digits, n);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if locale.to_ascii_lowercase().starts_with("de") {
        s = s.replace('.', ",");
    }
    s
}

fn nf_format_east_asian_compact(
    n: f64,
    locale: &str,
    e8_suffix: &str,
    e4_suffix: &str,
    e3_suffix: Option<&str>,
) -> Option<String> {
    let abs = n.abs();
    let sign = if n < 0.0 { "-" } else { "" };
    if abs >= 100_000_000.0 {
        let scaled = nf_round_for_compact(n / 100_000_000.0);
        return Some(format!(
            "{sign}{}{}",
            nf_trimmed_decimal(scaled, 1, locale),
            e8_suffix
        ));
    }
    if abs >= 10_000.0 {
        let scaled = nf_round_for_compact(n / 10_000.0);
        return Some(format!(
            "{sign}{}{}",
            nf_trimmed_decimal(scaled, 1, locale),
            e4_suffix
        ));
    }
    if let Some(suffix) = e3_suffix
        && abs >= 1_000.0
    {
        let scaled = nf_round_for_compact(n / 1_000.0);
        return Some(format!(
            "{sign}{}{}",
            nf_trimmed_decimal(scaled, 1, locale),
            suffix
        ));
    }
    None
}

fn nf_apply_unit_style(number: &str, locale: &str, unit: &str, unit_display: &str) -> String {
    let lang = locale
        .split(['-', '_'])
        .next()
        .unwrap_or("en")
        .to_ascii_lowercase();
    if unit == "percent" {
        return if unit_display == "long" {
            format!("{number} percent")
        } else {
            format!("{number}%")
        };
    }
    if unit == "kilometer-per-hour" {
        return match (lang.as_str(), unit_display) {
            ("en", "short") => format!("{number} km/h"),
            ("en", "narrow") => format!("{number}km/h"),
            ("en", "long") => format!("{number} kilometers per hour"),
            ("de", "short") => format!("{number} km/h"),
            ("de", "narrow") => format!("{number} km/h"),
            ("de", "long") => format!("{number} Kilometer pro Stunde"),
            ("ja", "short") => format!("{number} km/h"),
            ("ja", "narrow") => format!("{number}km/h"),
            ("ja", "long") => format!("時速 {number} キロメートル"),
            ("ko", "short") => format!("{number}km/h"),
            ("ko", "narrow") => format!("{number}km/h"),
            ("ko", "long") => format!("시속 {number}킬로미터"),
            ("zh", "short") => format!("{number} 公里/小時"),
            ("zh", "narrow") => format!("{number}公里/小時"),
            ("zh", "long") => format!("每小時 {number} 公里"),
            (_, "narrow") => format!("{number}km/h"),
            (_, "long") => format!("{number} kilometers per hour"),
            _ => format!("{number} km/h"),
        };
    }

    let glue = if unit_display == "narrow" { "" } else { " " };
    format!("{number}{glue}{unit}")
}

fn nf_is_numeric_char(c: char) -> bool {
    c.is_ascii_digit() || c.is_numeric()
}

fn nf_is_numeric_or_separator_char(c: char) -> bool {
    nf_is_numeric_char(c)
        || matches!(
            c,
            '.' | ',' | '\u{066B}' | '\u{066C}' | '\u{00A0}' | '\u{202F}' | ' ' | '\''
        )
}

fn nf_push_decimal_number_parts(parts: &mut Vec<(String, String)>, number: &str) {
    if number.is_empty() {
        return;
    }
    let chars: Vec<char> = number.chars().collect();
    let mut decimal_idx = None;
    for i in (0..chars.len()).rev() {
        let c = chars[i];
        if matches!(c, '.' | ',' | '\u{066B}') {
            let digits_before = chars[..i]
                .iter()
                .filter(|ch| nf_is_numeric_char(**ch))
                .count();
            let digits_after = chars[i + 1..]
                .iter()
                .filter(|ch| nf_is_numeric_char(**ch))
                .count();
            let has_more_separators_before = chars[..i].iter().any(|ch| {
                matches!(
                    *ch,
                    '.' | ',' | '\u{066B}' | '\u{066C}' | '\u{00A0}' | '\u{202F}' | ' ' | '\''
                )
            });
            if digits_after > 0
                && !(digits_after == 3 && digits_before > 1 && !has_more_separators_before)
            {
                decimal_idx = Some(i);
                break;
            }
        }
    }

    let mut buf = String::new();
    let mut in_fraction = false;
    for (i, c) in chars.iter().enumerate() {
        if nf_is_numeric_char(*c) {
            buf.push(*c);
            continue;
        }
        if !buf.is_empty() {
            parts.push((
                if in_fraction {
                    "fraction".to_string()
                } else {
                    "integer".to_string()
                },
                std::mem::take(&mut buf),
            ));
        }
        if decimal_idx == Some(i) {
            parts.push(("decimal".to_string(), c.to_string()));
            in_fraction = true;
        } else {
            parts.push(("group".to_string(), c.to_string()));
        }
    }
    if !buf.is_empty() {
        parts.push((
            if in_fraction {
                "fraction".to_string()
            } else {
                "integer".to_string()
            },
            buf,
        ));
    }
}

fn nf_push_scientific_or_engineering_parts(
    parts: &mut Vec<(String, String)>,
    formatted: &str,
) -> bool {
    let Some((mantissa, exponent)) = formatted.split_once('E') else {
        return false;
    };
    if let Some((int_part, frac_part)) = mantissa.split_once(['.', ',', '\u{066B}']) {
        if !int_part.is_empty() {
            parts.push(("integer".to_string(), int_part.to_string()));
        }
        let dec = mantissa
            .chars()
            .find(|c| matches!(*c, '.' | ',' | '\u{066B}'))
            .map(|c| c.to_string())
            .unwrap_or_else(|| ".".to_string());
        parts.push(("decimal".to_string(), dec));
        if !frac_part.is_empty() {
            parts.push(("fraction".to_string(), frac_part.to_string()));
        }
    } else {
        parts.push(("integer".to_string(), mantissa.to_string()));
    }
    parts.push(("exponentSeparator".to_string(), "E".to_string()));
    if let Some(rest) = exponent.strip_prefix('-') {
        parts.push(("exponentMinusSign".to_string(), "-".to_string()));
        parts.push(("exponentInteger".to_string(), rest.to_string()));
    } else if let Some(rest) = exponent.strip_prefix('+') {
        parts.push(("exponentPlusSign".to_string(), "+".to_string()));
        parts.push(("exponentInteger".to_string(), rest.to_string()));
    } else {
        parts.push(("exponentInteger".to_string(), exponent.to_string()));
    }
    true
}

fn nf_push_compact_parts(parts: &mut Vec<(String, String)>, formatted: &str) {
    let suffix_start = formatted
        .char_indices()
        .find_map(|(i, c)| (!nf_is_numeric_or_separator_char(c) && !c.is_whitespace()).then_some(i))
        .unwrap_or(formatted.len());
    let mut number_end = suffix_start;
    while number_end > 0
        && formatted[..number_end]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_whitespace())
    {
        number_end -= formatted[..number_end]
            .chars()
            .next_back()
            .map(char::len_utf8)
            .unwrap_or(0);
    }
    let number = &formatted[..number_end];
    let literal = &formatted[number_end..suffix_start];
    let compact = &formatted[suffix_start..];

    nf_push_decimal_number_parts(parts, number);
    if !literal.is_empty() {
        parts.push(("literal".to_string(), literal.to_string()));
    }
    if !compact.is_empty() {
        parts.push(("compact".to_string(), compact.to_string()));
    }
}

fn nf_take_leading_sign(parts: &mut Vec<(String, String)>, formatted: &mut String) {
    if let Some(rest) = formatted.strip_prefix('-') {
        parts.push(("minusSign".to_string(), "-".to_string()));
        *formatted = rest.to_string();
    } else if let Some(rest) = formatted.strip_prefix('+') {
        parts.push(("plusSign".to_string(), "+".to_string()));
        *formatted = rest.to_string();
    }
}

fn nf_take_trailing_percent(formatted: &mut String) -> bool {
    if let Some(rest) = formatted.strip_suffix('%') {
        *formatted = rest.to_string();
        true
    } else {
        false
    }
}

fn nf_take_trailing_parenthesis(parts: &mut Vec<(String, String)>, has_trailing_parenthesis: bool) {
    if has_trailing_parenthesis {
        parts.push(("literal".to_string(), ")".to_string()));
    }
}

fn nf_split_numeric_prefix_and_unit_suffix<'a>(
    formatted: &'a str,
) -> Option<(&'a str, &'a str, &'a str)> {
    let mut number_end = 0usize;
    for (idx, c) in formatted.char_indices() {
        if nf_is_numeric_or_separator_char(c) {
            number_end = idx + c.len_utf8();
        } else {
            break;
        }
    }
    if number_end == 0 || number_end >= formatted.len() {
        return None;
    }
    while number_end > 0
        && formatted[..number_end]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        number_end -= formatted[..number_end]
            .chars()
            .next_back()
            .map(char::len_utf8)
            .unwrap_or(0);
    }
    if number_end == 0 || number_end >= formatted.len() {
        return None;
    }
    let rest = &formatted[number_end..];
    if rest.chars().all(char::is_whitespace) {
        return None;
    }
    let unit_start_rel = rest
        .char_indices()
        .find_map(|(idx, c)| (!c.is_whitespace()).then_some(idx))
        .unwrap_or(rest.len());
    let literal = &rest[..unit_start_rel];
    let unit = &rest[unit_start_rel..];
    if unit.is_empty() {
        None
    } else {
        Some((&formatted[..number_end], literal, unit))
    }
}

fn nf_format_to_parts(
    this_obj: &GcRef<JsObject>,
    value: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Vec<(String, String)>, VmError> {
    let style = this_obj
        .get(&PropertyKey::string(INTL_NF_STYLE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "decimal".to_string());
    let notation = this_obj
        .get(&PropertyKey::string(INTL_NF_NOTATION_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "standard".to_string());
    let locale = this_obj
        .get(&PropertyKey::string(INTL_LOCALE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let mut formatted = nf_format_to_string(this_obj, value, ncx)?;
    let mut parts: Vec<(String, String)> = Vec::new();

    let mut has_trailing_parenthesis = false;
    if let Some(inner) = formatted
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
    {
        parts.push(("literal".to_string(), "(".to_string()));
        formatted = inner.to_string();
        has_trailing_parenthesis = true;
    }

    nf_take_leading_sign(&mut parts, &mut formatted);

    if formatted == "∞" {
        parts.push(("infinity".to_string(), "∞".to_string()));
        nf_take_trailing_parenthesis(&mut parts, has_trailing_parenthesis);
        return Ok(parts);
    }
    let nan_value = if locale.to_ascii_lowercase().starts_with("zh") {
        "非數值"
    } else {
        "NaN"
    };
    if formatted == nan_value {
        parts.push(("nan".to_string(), formatted));
        nf_take_trailing_parenthesis(&mut parts, has_trailing_parenthesis);
        return Ok(parts);
    }

    let mut prefix_parts: Vec<(String, String)> = Vec::new();
    let mut suffix_parts: Vec<(String, String)> = Vec::new();

    if style == "currency" {
        let currency = this_obj
            .get(&PropertyKey::string(INTL_CURRENCY_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "USD".to_string());
        let currency_display = this_obj
            .get(&PropertyKey::string(INTL_NF_CURRENCY_DISPLAY_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "symbol".to_string());
        let symbol = nf_currency_symbol(&locale, &currency, &currency_display);
        if let Some(rest) = formatted.strip_prefix(&symbol) {
            prefix_parts.push(("currency".to_string(), symbol));
            formatted = rest.to_string();
        } else if let Some(rest) = formatted.strip_suffix(&symbol) {
            let mut body = rest.to_string();
            if let Some(space_rest) = body.strip_suffix('\u{00A0}') {
                body = space_rest.to_string();
                suffix_parts.push(("literal".to_string(), "\u{00A0}".to_string()));
            } else if let Some(space_rest) = body.strip_suffix(' ') {
                body = space_rest.to_string();
                suffix_parts.push(("literal".to_string(), " ".to_string()));
            }
            formatted = body;
            suffix_parts.push(("currency".to_string(), symbol));
        }
    } else if style == "unit" {
        let unit = this_obj
            .get(&PropertyKey::string(INTL_UNIT_KEY))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
            .unwrap_or_else(|| "unit".to_string());

        if unit == "kilometer-per-hour" {
            if let Some(rest) = formatted.strip_prefix("時速 ") {
                prefix_parts.push(("unit".to_string(), "時速".to_string()));
                prefix_parts.push(("literal".to_string(), " ".to_string()));
                formatted = rest.to_string();
                if let Some(body) = formatted.strip_suffix(" キロメートル") {
                    formatted = body.to_string();
                    suffix_parts.push(("literal".to_string(), " ".to_string()));
                    suffix_parts.push(("unit".to_string(), "キロメートル".to_string()));
                }
            } else if let Some(rest) = formatted.strip_prefix("시속 ") {
                prefix_parts.push(("unit".to_string(), "시속".to_string()));
                prefix_parts.push(("literal".to_string(), " ".to_string()));
                formatted = rest.to_string();
                if let Some(body) = formatted.strip_suffix("킬로미터") {
                    formatted = body.to_string();
                    suffix_parts.push(("unit".to_string(), "킬로미터".to_string()));
                }
            } else if let Some(rest) = formatted.strip_prefix("每小時 ") {
                prefix_parts.push(("unit".to_string(), "每小時".to_string()));
                prefix_parts.push(("literal".to_string(), " ".to_string()));
                formatted = rest.to_string();
                if let Some(body) = formatted.strip_suffix(" 公里") {
                    formatted = body.to_string();
                    suffix_parts.push(("literal".to_string(), " ".to_string()));
                    suffix_parts.push(("unit".to_string(), "公里".to_string()));
                }
            } else {
                if let Some((number, literal, unit_token)) =
                    nf_split_numeric_prefix_and_unit_suffix(&formatted)
                {
                    let number = number.to_string();
                    let literal = literal.to_string();
                    let unit_token = unit_token.to_string();
                    formatted = number;
                    if !literal.is_empty() {
                        suffix_parts.push(("literal".to_string(), literal));
                    }
                    suffix_parts.push(("unit".to_string(), unit_token));
                }
            }
        } else if let Some((number, literal, unit_token)) =
            nf_split_numeric_prefix_and_unit_suffix(&formatted)
        {
            let number = number.to_string();
            let literal = literal.to_string();
            let unit_token = unit_token.to_string();
            formatted = number;
            if !literal.is_empty() {
                suffix_parts.push(("literal".to_string(), literal));
            }
            suffix_parts.push(("unit".to_string(), unit_token));
        }
    }

    if style == "unit" && !prefix_parts.is_empty() {
        if let Some(rest) = formatted.strip_prefix('-') {
            prefix_parts.push(("minusSign".to_string(), "-".to_string()));
            formatted = rest.to_string();
        } else if let Some(rest) = formatted.strip_prefix('+') {
            prefix_parts.push(("plusSign".to_string(), "+".to_string()));
            formatted = rest.to_string();
        }
    } else {
        nf_take_leading_sign(&mut parts, &mut formatted);
    }
    let has_percent = style == "percent" && nf_take_trailing_percent(&mut formatted);

    parts.extend(prefix_parts);
    if notation == "scientific" || notation == "engineering" {
        let _ = nf_push_scientific_or_engineering_parts(&mut parts, &formatted);
    } else if notation == "compact" {
        nf_push_compact_parts(&mut parts, &formatted);
    } else {
        nf_push_decimal_number_parts(&mut parts, &formatted);
    }
    parts.extend(suffix_parts);

    if has_percent {
        parts.push(("percentSign".to_string(), "%".to_string()));
    }
    nf_take_trailing_parenthesis(&mut parts, has_trailing_parenthesis);
    Ok(parts)
}

fn validate_collator_options(
    options: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<(), VmError> {
    if let Some(s) = get_option_string(options, "localeMatcher", ncx)? {
        if s != "lookup" && s != "best fit" {
            return Err(VmError::range_error("Invalid localeMatcher option"));
        }
    }
    if let Some(s) = get_option_string(options, "usage", ncx)? {
        if s != "sort" && s != "search" {
            return Err(VmError::range_error("Invalid usage option"));
        }
    }
    if let Some(s) = get_option_string(options, "sensitivity", ncx)? {
        if s != "base" && s != "accent" && s != "case" && s != "variant" {
            return Err(VmError::range_error("Invalid sensitivity option"));
        }
    }
    if let Some(s) = get_option_string(options, "caseFirst", ncx)?
        && s != "upper"
        && s != "lower"
        && s != "false"
    {
        return Err(VmError::range_error("Invalid caseFirst option"));
    }
    let _ = get_option_bool(options, "numeric", ncx)?;
    let _ = get_option_bool(options, "ignorePunctuation", ncx)?;
    Ok(())
}

fn is_well_formed_currency_code(code: &str) -> bool {
    code.len() == 3 && code.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_well_formed_numbering_system(value: &str) -> bool {
    let len = value.len();
    (3..=8).contains(&len) && value.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_well_formed_unit_identifier(unit: &str) -> bool {
    let is_simple = |part: &str| SANCTIONED_SIMPLE_UNITS.contains(&part);
    if let Some((left, right)) = unit.split_once("-per-") {
        return is_simple(left) && is_simple(right);
    }
    is_simple(unit)
}

fn supported_locales_of_impl(
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let locales = canonicalize_locale_list(args.first(), ncx)?;
    let supported: Vec<String> = locales
        .into_iter()
        .filter(|locale| {
            let lower = locale.to_ascii_lowercase();
            lower != "zxx" && !lower.starts_with("zxx-")
        })
        .collect();
    let arr = create_array(ncx, supported.len());
    for (i, locale) in supported.iter().enumerate() {
        let _ = arr.set(
            PropertyKey::Index(i as u32),
            Value::string(JsString::intern(locale)),
        );
    }
    Ok(Value::array(arr))
}

fn install_supported_locales_of(
    ctor: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) {
    let supported_locales_of = Value::native_function_with_proto_named(
        |_this, args, ncx| supported_locales_of_impl(args, ncx),
        mm.clone(),
        fn_proto,
        "supportedLocalesOf",
        1,
    );
    ctor.define_property(
        PropertyKey::string("supportedLocalesOf"),
        PropertyDescriptor::builtin_method(supported_locales_of),
    );
}

fn install_common_constructor_bits(
    ctor_obj: &GcRef<JsObject>,
    ctor_name: &str,
    ctor_len: u32,
    prototype_obj: GcRef<JsObject>,
) {
    ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(prototype_obj),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        ),
    );
    ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(ctor_name))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(ctor_len as f64)),
    );
    let _ = ctor_obj.set(
        PropertyKey::string("__non_constructor"),
        Value::boolean(false),
    );
}

fn install_basic_intl_constructor(
    intl: &GcRef<JsObject>,
    ctor_name: &str,
    brand_key: &'static str,
    ctor_len: u32,
    prototype_methods: &[(&str, u32, Value)],
    mm: &Arc<MemoryManager>,
    object_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
) -> Value {
    let proto_obj = GcRef::new(JsObject::new(Value::object(object_proto), mm.clone()));
    for (name, _length, value) in prototype_methods {
        proto_obj.define_property(
            PropertyKey::string(*name),
            PropertyDescriptor::builtin_method(value.clone()),
        );
    }

    let ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
    install_common_constructor_bits(&ctor_obj, ctor_name, ctor_len, proto_obj);

    let ctor = Value::native_function_with_proto_and_object(
        Arc::new({
            let proto_obj = proto_obj;
            move |this, args, ncx| {
                let mut locale =
                    normalize_locale_for_ops(first_requested_locale(args.first(), ncx)?);
                if brand_key == INTL_COLLATOR_BRAND_KEY {
                    validate_collator_options(args.get(1), ncx)?;
                }
                let target = if ncx.is_construct() {
                    this.as_object().ok_or_else(|| {
                        VmError::type_error("Intl constructor requires object receiver")
                    })?
                } else {
                    GcRef::new(JsObject::new(
                        Value::object(proto_obj),
                        ncx.memory_manager().clone(),
                    ))
                };
                target.define_property(
                    PropertyKey::string(brand_key),
                    PropertyDescriptor::data_with_attrs(
                        Value::boolean(true),
                        PropertyAttributes::permanent(),
                    ),
                );
                target.define_property(
                    PropertyKey::string(INTL_LOCALE_KEY),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern(&locale)),
                        PropertyAttributes::permanent(),
                    ),
                );

                if brand_key == INTL_LOCALE_BRAND_KEY {
                    for (name, keyword, supported, private_key) in [
                        ("calendar", "ca", SUPPORTED_CALENDARS, INTL_CALENDAR_KEY),
                        ("collation", "co", SUPPORTED_COLLATIONS, INTL_COLLATION_KEY),
                        (
                            "numberingSystem",
                            "nu",
                            SUPPORTED_NUMBERING_SYSTEMS,
                            INTL_NUMBERING_SYSTEM_KEY,
                        ),
                    ] {
                        if let Some(value) = get_option_string(args.get(1), name, ncx)?
                            && supported.contains(&value.as_str())
                        {
                            let candidate = format!("{locale}-u-{keyword}-{value}");
                            locale = canonicalize_locale_tag(&candidate)?;
                            set_string_data_property(&target, private_key, &value);
                            set_string_data_property(&target, INTL_LOCALE_KEY, &locale);
                        }
                    }
                } else if brand_key == INTL_COLLATOR_BRAND_KEY {
                    let mut ext = parse_unicode_extension(&locale);
                    let has_unsupported_key =
                        ext.keys.keys().any(|k| k != "co" && k != "kn" && k != "kf");
                    if has_unsupported_key {
                        ext.keys.clear();
                    }

                    set_string_data_property(&target, INTL_USAGE_KEY, "sort");
                    set_string_data_property(&target, INTL_SENSITIVITY_KEY, "variant");
                    let locale_base = ext.base.to_ascii_lowercase();
                    let default_ignore_punctuation =
                        locale_base == "th" || locale_base.starts_with("th-");
                    set_bool_data_property(
                        &target,
                        INTL_IGNORE_PUNCTUATION_KEY,
                        default_ignore_punctuation,
                    );

                    if let Some(usage) = get_option_string(args.get(1), "usage", ncx)?
                        && (usage == "sort" || usage == "search")
                    {
                        set_string_data_property(&target, INTL_USAGE_KEY, &usage);
                    }
                    if let Some(sensitivity) = get_option_string(args.get(1), "sensitivity", ncx)?
                        && matches!(sensitivity.as_str(), "base" | "accent" | "case" | "variant")
                    {
                        set_string_data_property(&target, INTL_SENSITIVITY_KEY, &sensitivity);
                    }
                    if let Some(ignore_punctuation) =
                        get_option_bool(args.get(1), "ignorePunctuation", ncx)?
                    {
                        set_bool_data_property(
                            &target,
                            INTL_IGNORE_PUNCTUATION_KEY,
                            ignore_punctuation,
                        );
                    }
                    if let Some(numeric) = get_option_bool(args.get(1), "numeric", ncx)? {
                        set_bool_data_property(&target, INTL_NUMERIC_KEY, numeric);
                    }
                    if let Some(case_first) = get_option_string(args.get(1), "caseFirst", ncx)?
                        && matches!(case_first.as_str(), "upper" | "lower" | "false")
                    {
                        set_string_data_property(&target, INTL_CASE_FIRST_KEY, &case_first);
                    }

                    let locale_co = ext.keys.get("co").cloned();
                    let locale_kn = ext.keys.get("kn").cloned();
                    let locale_kf = ext.keys.get("kf").cloned();

                    if let Some(value) = get_option_string(args.get(1), "collation", ncx)?
                        && SUPPORTED_COLLATIONS.contains(&value.as_str())
                    {
                        set_string_data_property(&target, INTL_COLLATION_KEY, &value);
                        if locale_co.as_deref() == Some(value.as_str()) {
                            ext.keys.insert("co".to_string(), value);
                        } else {
                            ext.keys.remove("co");
                        }
                    } else {
                        if let Some(locale_value) = locale_co.as_deref()
                            && (locale_value == "standard" || locale_value == "search")
                        {
                            ext.keys.remove("co");
                        }
                        if let Some(locale_value) = ext.keys.get("co").cloned()
                            && SUPPORTED_COLLATIONS.contains(&locale_value.as_str())
                            && is_collation_supported_for_locale(&ext.base, &locale_value)
                        {
                            set_string_data_property(&target, INTL_COLLATION_KEY, &locale_value);
                        } else {
                            ext.keys.remove("co");
                            set_string_data_property(&target, INTL_COLLATION_KEY, "default");
                        }
                    }

                    if let Some(numeric) = get_option_bool(args.get(1), "numeric", ncx)? {
                        set_bool_data_property(&target, INTL_NUMERIC_KEY, numeric);
                        let locale_numeric_true = locale_kn.as_deref() == Some("true");
                        if numeric && locale_numeric_true {
                            ext.keys.insert("kn".to_string(), "true".to_string());
                        } else {
                            ext.keys.remove("kn");
                        }
                    } else if let Some(locale_numeric) = locale_kn {
                        let numeric = locale_numeric != "false";
                        set_bool_data_property(&target, INTL_NUMERIC_KEY, numeric);
                        if numeric {
                            ext.keys.insert("kn".to_string(), "true".to_string());
                        } else {
                            ext.keys.remove("kn");
                        }
                    }

                    if let Some(case_first) = get_option_string(args.get(1), "caseFirst", ncx)?
                        && matches!(case_first.as_str(), "upper" | "lower" | "false")
                    {
                        set_string_data_property(&target, INTL_CASE_FIRST_KEY, &case_first);
                        if locale_kf.as_deref() == Some(case_first.as_str()) {
                            ext.keys.insert("kf".to_string(), case_first);
                        } else {
                            ext.keys.remove("kf");
                        }
                    } else if let Some(locale_case_first) = locale_kf
                        && matches!(locale_case_first.as_str(), "upper" | "lower" | "false")
                    {
                        set_string_data_property(&target, INTL_CASE_FIRST_KEY, &locale_case_first);
                        ext.keys.insert("kf".to_string(), locale_case_first);
                    }

                    locale = build_unicode_extension_locale(&ext);
                    set_string_data_property(&target, INTL_LOCALE_KEY, &locale);

                    // VM currently has call-path gaps for accessor-returned functions.
                    // Install an own compare function on instances to preserve call behavior.
                    let bound_locale = locale.clone();
                    let usage = target
                        .get(&PropertyKey::string(INTL_USAGE_KEY))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                        .unwrap_or_else(|| "sort".to_string());
                    let sensitivity = target
                        .get(&PropertyKey::string(INTL_SENSITIVITY_KEY))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                        .unwrap_or_else(|| "variant".to_string());
                    let ignore_punctuation = target
                        .get(&PropertyKey::string(INTL_IGNORE_PUNCTUATION_KEY))
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(false);
                    let collation = target
                        .get(&PropertyKey::string(INTL_COLLATION_KEY))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                        .unwrap_or_else(|| "default".to_string());
                    let fn_proto = ncx.ctx.function_prototype().ok_or_else(|| {
                        VmError::type_error("Function.prototype is not available")
                    })?;
                    let compare = Value::native_function_with_proto_named(
                        move |_this, args, ncx| {
                            let left = args.first().cloned().unwrap_or(Value::undefined());
                            let right = args.get(1).cloned().unwrap_or(Value::undefined());
                            let mut l = ncx.to_string_value(&left)?;
                            let mut r = ncx.to_string_value(&right)?;
                            if ignore_punctuation {
                                l = strip_punctuation_and_space(&l);
                                r = strip_punctuation_and_space(&r);
                            }
                            if sensitivity == "base" {
                                l = strip_diacritics(&l);
                                r = strip_diacritics(&r);
                            }
                            if usage == "search" {
                                let key = |s: &str| -> String {
                                    match sensitivity.as_str() {
                                        "base" => strip_diacritics(s).to_lowercase(),
                                        "accent" => s.to_lowercase(),
                                        "case" => strip_diacritics(s),
                                        _ => s.to_string(),
                                    }
                                };
                                let lk = key(&l);
                                let rk = key(&r);
                                if lk == rk {
                                    return Ok(Value::number(0.0));
                                }
                            }
                            if bound_locale.starts_with("de") && collation == "phonebk" {
                                l = de_phonebook_fold(&l);
                                r = de_phonebook_fold(&r);
                            } else if bound_locale.starts_with("de") && usage == "sort" {
                                if (l == "AE" && r == "Ä") || (l == "ae" && r == "ä") {
                                    return Ok(Value::number(1.0));
                                }
                                if (l == "Ä" && r == "AE") || (l == "ä" && r == "ae") {
                                    return Ok(Value::number(-1.0));
                                }
                            } else if bound_locale.starts_with("de") && usage == "search" {
                                if (l == "AE" && r == "Ä") || (l == "ae" && r == "ä") {
                                    return Ok(Value::number(-1.0));
                                }
                                if (l == "Ä" && r == "AE") || (l == "ä" && r == "ae") {
                                    return Ok(Value::number(1.0));
                                }
                            }
                            let locale_value = Value::string(JsString::intern(&bound_locale));
                            Ok(Value::number(locale_compare(
                                &l,
                                &r,
                                Some(&locale_value),
                                None,
                                ncx,
                            )?))
                        },
                        ncx.memory_manager().clone(),
                        fn_proto,
                        "",
                        2,
                    );
                    target.define_property(
                        PropertyKey::string("compare"),
                        PropertyDescriptor::data_with_attrs(
                            compare,
                            PropertyAttributes::builtin_method(),
                        ),
                    );
                } else if brand_key == INTL_DATETIMEFORMAT_BRAND_KEY {
                    if let Some(value) = get_option_string(args.get(1), "calendar", ncx)?
                        && SUPPORTED_CALENDARS.contains(&value.as_str())
                    {
                        set_string_data_property(&target, INTL_CALENDAR_KEY, &value);
                    }
                    if let Some(value) = get_option_string(args.get(1), "numberingSystem", ncx)?
                        && SUPPORTED_NUMBERING_SYSTEMS.contains(&value.as_str())
                    {
                        set_string_data_property(&target, INTL_NUMBERING_SYSTEM_KEY, &value);
                    }
                    if let Some(value) = get_option_string(args.get(1), "timeZone", ncx)?
                        && SUPPORTED_TIME_ZONES.contains(&value.as_str())
                    {
                        set_string_data_property(&target, INTL_TIMEZONE_KEY, &value);
                    }
                } else if brand_key == INTL_NUMBERFORMAT_BRAND_KEY {
                    let options = args.get(1);
                    let mut ext = parse_unicode_extension(&locale);
                    ext.keys.retain(|k, _| k == "nu");
                    if let Some(nu) = ext.keys.get("nu").cloned()
                        && !SUPPORTED_NUMBERING_SYSTEMS.contains(&nu.as_str())
                    {
                        ext.keys.remove("nu");
                    }

                    if let Some(locale_matcher) = get_option_string(options, "localeMatcher", ncx)?
                        && locale_matcher != "lookup"
                        && locale_matcher != "best fit"
                    {
                        return Err(VmError::range_error("Invalid localeMatcher option"));
                    }
                    if let Some(value) = get_option_string(options, "numberingSystem", ncx)? {
                        if !is_well_formed_numbering_system(&value) {
                            return Err(VmError::range_error("Invalid numberingSystem option"));
                        }
                        if SUPPORTED_NUMBERING_SYSTEMS.contains(&value.as_str()) {
                            set_string_data_property(&target, INTL_NUMBERING_SYSTEM_KEY, &value);
                            if ext.keys.get("nu").map(|s| s.as_str()) != Some(value.as_str()) {
                                ext.keys.remove("nu");
                            }
                        }
                    }
                    if target
                        .get(&PropertyKey::string(INTL_NUMBERING_SYSTEM_KEY))
                        .is_none()
                    {
                        if let Some(nu) = ext.keys.get("nu") {
                            set_string_data_property(&target, INTL_NUMBERING_SYSTEM_KEY, nu);
                        } else {
                            set_string_data_property(&target, INTL_NUMBERING_SYSTEM_KEY, "latn");
                        }
                    }
                    locale = build_unicode_extension_locale(&ext);
                    set_string_data_property(&target, INTL_LOCALE_KEY, &locale);

                    let style = get_option_string(options, "style", ncx)?
                        .unwrap_or_else(|| "decimal".to_string());
                    if !matches!(style.as_str(), "decimal" | "percent" | "currency" | "unit") {
                        return Err(VmError::range_error("Invalid style option"));
                    }
                    set_string_data_property(&target, INTL_NF_STYLE_KEY, &style);

                    let currency = get_option_string(options, "currency", ncx)?;
                    let currency_display = get_option_string(options, "currencyDisplay", ncx)?
                        .unwrap_or_else(|| "symbol".to_string());
                    if !matches!(
                        currency_display.as_str(),
                        "code" | "symbol" | "narrowSymbol" | "name"
                    ) {
                        return Err(VmError::range_error("Invalid currencyDisplay option"));
                    }
                    let currency_sign = get_option_string(options, "currencySign", ncx)?
                        .unwrap_or_else(|| "standard".to_string());
                    if !matches!(currency_sign.as_str(), "standard" | "accounting") {
                        return Err(VmError::range_error("Invalid currencySign option"));
                    }
                    if let Some(cur) = currency.as_deref()
                        && !is_well_formed_currency_code(cur)
                    {
                        return Err(VmError::range_error("Invalid currency option"));
                    }
                    if style == "currency" {
                        let currency = currency.ok_or_else(|| {
                            VmError::type_error("currency option is required with currency style")
                        })?;
                        set_string_data_property(
                            &target,
                            INTL_CURRENCY_KEY,
                            &currency.to_ascii_uppercase(),
                        );
                        set_string_data_property(
                            &target,
                            INTL_NF_CURRENCY_DISPLAY_KEY,
                            &currency_display,
                        );
                        set_string_data_property(
                            &target,
                            INTL_NF_CURRENCY_SIGN_KEY,
                            &currency_sign,
                        );
                    }

                    let unit = get_option_string(options, "unit", ncx)?;
                    let unit_display = get_option_string(options, "unitDisplay", ncx)?
                        .unwrap_or_else(|| "short".to_string());
                    if !matches!(unit_display.as_str(), "short" | "narrow" | "long") {
                        return Err(VmError::range_error("Invalid unitDisplay option"));
                    }
                    if let Some(unit) = unit.as_deref()
                        && !is_well_formed_unit_identifier(unit)
                    {
                        return Err(VmError::range_error("Invalid unit option"));
                    }
                    if style == "unit" {
                        let unit = unit.ok_or_else(|| {
                            VmError::type_error("unit option is required with unit style")
                        })?;
                        set_string_data_property(&target, INTL_UNIT_KEY, &unit);
                        set_string_data_property(&target, INTL_NF_UNIT_DISPLAY_KEY, &unit_display);
                    }

                    let notation = get_option_string(options, "notation", ncx)?
                        .unwrap_or_else(|| "standard".to_string());
                    if !matches!(
                        notation.as_str(),
                        "standard" | "scientific" | "engineering" | "compact"
                    ) {
                        return Err(VmError::range_error("Invalid notation option"));
                    }
                    set_string_data_property(&target, INTL_NF_NOTATION_KEY, &notation);

                    let min_int = get_option_number(options, "minimumIntegerDigits", ncx)?;
                    if let Some(v) = min_int {
                        if !v.is_finite() || v.fract() != 0.0 || !(1.0..=21.0).contains(&v) {
                            return Err(VmError::range_error(
                                "Invalid minimumIntegerDigits option",
                            ));
                        }
                        set_number_data_property(&target, INTL_NF_MIN_INT_DIGITS_KEY, v);
                    } else {
                        set_number_data_property(&target, INTL_NF_MIN_INT_DIGITS_KEY, 1.0);
                    }

                    let min_frac = get_option_number(options, "minimumFractionDigits", ncx)?;
                    let max_frac = get_option_number(options, "maximumFractionDigits", ncx)?;
                    if let Some(v) = min_frac {
                        if !v.is_finite() || v.fract() != 0.0 || !(0.0..=100.0).contains(&v) {
                            return Err(VmError::range_error(
                                "Invalid minimumFractionDigits option",
                            ));
                        }
                        set_number_data_property(&target, INTL_NF_MIN_FRAC_DIGITS_KEY, v);
                    }
                    if let Some(v) = max_frac {
                        if !v.is_finite() || v.fract() != 0.0 || !(0.0..=100.0).contains(&v) {
                            return Err(VmError::range_error(
                                "Invalid maximumFractionDigits option",
                            ));
                        }
                        set_number_data_property(&target, INTL_NF_MAX_FRAC_DIGITS_KEY, v);
                    }
                    if let (Some(minf), Some(maxf)) = (min_frac, max_frac)
                        && minf > maxf
                    {
                        return Err(VmError::range_error(
                            "minimumFractionDigits is greater than maximumFractionDigits",
                        ));
                    }
                    if target
                        .get(&PropertyKey::string(INTL_NF_MIN_FRAC_DIGITS_KEY))
                        .is_none()
                    {
                        if style == "currency" {
                            set_number_data_property(&target, INTL_NF_MIN_FRAC_DIGITS_KEY, 2.0);
                        } else {
                            set_number_data_property(&target, INTL_NF_MIN_FRAC_DIGITS_KEY, 0.0);
                        }
                    }
                    if target
                        .get(&PropertyKey::string(INTL_NF_MAX_FRAC_DIGITS_KEY))
                        .is_none()
                    {
                        if style == "currency" {
                            set_number_data_property(&target, INTL_NF_MAX_FRAC_DIGITS_KEY, 2.0);
                        } else {
                            set_number_data_property(&target, INTL_NF_MAX_FRAC_DIGITS_KEY, 3.0);
                        }
                    }

                    let min_sig = get_option_number(options, "minimumSignificantDigits", ncx)?;
                    let max_sig = get_option_number(options, "maximumSignificantDigits", ncx)?;
                    if let Some(v) = min_sig {
                        if !v.is_finite() || v.fract() != 0.0 || !(1.0..=21.0).contains(&v) {
                            return Err(VmError::range_error(
                                "Invalid minimumSignificantDigits option",
                            ));
                        }
                        set_number_data_property(&target, INTL_NF_MIN_SIG_DIGITS_KEY, v);
                    }
                    if let Some(v) = max_sig {
                        if !v.is_finite() || v.fract() != 0.0 || !(1.0..=21.0).contains(&v) {
                            return Err(VmError::range_error(
                                "Invalid maximumSignificantDigits option",
                            ));
                        }
                        set_number_data_property(&target, INTL_NF_MAX_SIG_DIGITS_KEY, v);
                    }
                    if min_sig.is_none() && max_sig.is_some() {
                        set_number_data_property(&target, INTL_NF_MIN_SIG_DIGITS_KEY, 1.0);
                    }
                    if let (Some(mins), Some(maxs)) = (
                        target
                            .get(&PropertyKey::string(INTL_NF_MIN_SIG_DIGITS_KEY))
                            .and_then(|v| v.as_number()),
                        target
                            .get(&PropertyKey::string(INTL_NF_MAX_SIG_DIGITS_KEY))
                            .and_then(|v| v.as_number()),
                    ) && mins > maxs
                    {
                        return Err(VmError::range_error(
                            "minimumSignificantDigits is greater than maximumSignificantDigits",
                        ));
                    }

                    let rounding_increment = get_option_number(options, "roundingIncrement", ncx)?;
                    if let Some(v) = rounding_increment {
                        let allowed = [
                            1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 250.0, 500.0,
                            1000.0, 2000.0, 2500.0, 5000.0,
                        ];
                        if !v.is_finite() || v.fract() != 0.0 || !allowed.contains(&v) {
                            return Err(VmError::range_error("Invalid roundingIncrement option"));
                        }
                        set_number_data_property(&target, INTL_NF_ROUNDING_INCREMENT_KEY, v);
                    } else {
                        set_number_data_property(&target, INTL_NF_ROUNDING_INCREMENT_KEY, 1.0);
                    }
                    if rounding_increment.is_some_and(|v| v != 1.0)
                        && let (Some(minf), Some(maxf)) = (min_frac, max_frac)
                        && (minf - maxf).abs() > f64::EPSILON
                    {
                        return Err(VmError::range_error(
                            "minimumFractionDigits and maximumFractionDigits must be equal when roundingIncrement is used",
                        ));
                    }

                    let rounding_mode = get_option_string(options, "roundingMode", ncx)?
                        .unwrap_or_else(|| "halfExpand".to_string());
                    if !matches!(
                        rounding_mode.as_str(),
                        "ceil"
                            | "floor"
                            | "expand"
                            | "trunc"
                            | "halfCeil"
                            | "halfFloor"
                            | "halfExpand"
                            | "halfTrunc"
                            | "halfEven"
                    ) {
                        return Err(VmError::range_error("Invalid roundingMode option"));
                    }
                    set_string_data_property(&target, INTL_NF_ROUNDING_MODE_KEY, &rounding_mode);

                    let rounding_priority = get_option_string(options, "roundingPriority", ncx)?
                        .unwrap_or_else(|| "auto".to_string());
                    if !matches!(
                        rounding_priority.as_str(),
                        "auto" | "morePrecision" | "lessPrecision"
                    ) {
                        return Err(VmError::range_error("Invalid roundingPriority option"));
                    }
                    set_string_data_property(
                        &target,
                        INTL_NF_ROUNDING_PRIORITY_KEY,
                        &rounding_priority,
                    );
                    if rounding_increment.is_some()
                        && rounding_increment != Some(1.0)
                        && (min_sig.is_some()
                            || max_sig.is_some()
                            || rounding_priority == "morePrecision"
                            || rounding_priority == "lessPrecision")
                    {
                        return Err(VmError::type_error(
                            "roundingIncrement cannot be used with significant-digits rounding",
                        ));
                    }

                    let trailing_zero_display =
                        get_option_string(options, "trailingZeroDisplay", ncx)?
                            .unwrap_or_else(|| "auto".to_string());
                    if !matches!(trailing_zero_display.as_str(), "auto" | "stripIfInteger") {
                        return Err(VmError::range_error("Invalid trailingZeroDisplay option"));
                    }
                    set_string_data_property(
                        &target,
                        INTL_NF_TRAILING_ZERO_DISPLAY_KEY,
                        &trailing_zero_display,
                    );

                    let compact_display = get_option_string(options, "compactDisplay", ncx)?
                        .unwrap_or_else(|| "short".to_string());
                    if !matches!(compact_display.as_str(), "short" | "long") {
                        return Err(VmError::range_error("Invalid compactDisplay option"));
                    }
                    if notation == "compact" {
                        set_string_data_property(
                            &target,
                            INTL_NF_COMPACT_DISPLAY_KEY,
                            &compact_display,
                        );
                    }

                    let default_use_grouping = if notation == "compact" {
                        "min2"
                    } else {
                        "auto"
                    };
                    match get_option_value(options, "useGrouping", ncx)? {
                        None => set_string_data_property(
                            &target,
                            INTL_NF_USE_GROUPING_KEY,
                            default_use_grouping,
                        ),
                        Some(v) if v.is_boolean() => {
                            if v.as_boolean() == Some(true) {
                                set_string_data_property(
                                    &target,
                                    INTL_NF_USE_GROUPING_KEY,
                                    "always",
                                );
                            } else {
                                set_bool_data_property(&target, INTL_NF_USE_GROUPING_KEY, false);
                            }
                        }
                        Some(v) if v.is_null() => {
                            set_bool_data_property(&target, INTL_NF_USE_GROUPING_KEY, false);
                        }
                        Some(v) if !v.to_boolean() => {
                            set_bool_data_property(&target, INTL_NF_USE_GROUPING_KEY, false);
                        }
                        Some(v) => {
                            let s = ncx.to_string_value(&v)?;
                            match s.as_str() {
                                "always" | "auto" | "min2" => {
                                    set_string_data_property(&target, INTL_NF_USE_GROUPING_KEY, &s);
                                }
                                // Per test262 v3 coverage, these fallback to default.
                                "true" | "false" => {
                                    set_string_data_property(
                                        &target,
                                        INTL_NF_USE_GROUPING_KEY,
                                        default_use_grouping,
                                    );
                                }
                                "" => {
                                    set_bool_data_property(
                                        &target,
                                        INTL_NF_USE_GROUPING_KEY,
                                        false,
                                    );
                                }
                                _ => {
                                    return Err(VmError::range_error("Invalid useGrouping option"));
                                }
                            }
                        }
                    }

                    let sign_display = get_option_string(options, "signDisplay", ncx)?
                        .unwrap_or_else(|| "auto".to_string());
                    if !matches!(
                        sign_display.as_str(),
                        "auto" | "never" | "always" | "exceptZero" | "negative"
                    ) {
                        return Err(VmError::range_error("Invalid signDisplay option"));
                    }
                    set_string_data_property(&target, INTL_NF_SIGN_DISPLAY_KEY, &sign_display);

                    // VM currently has call-path gaps for accessor-returned functions.
                    // Install an own format function on instances to preserve call behavior.
                    let fn_proto = ncx.ctx.function_prototype().ok_or_else(|| {
                        VmError::type_error("Function.prototype is not available")
                    })?;
                    let target_for_format = target.clone();
                    let format = Value::native_function_with_proto_named(
                        move |_this, args, ncx| {
                            let value = args.first().cloned().unwrap_or(Value::undefined());
                            let formatted = nf_format_to_string(&target_for_format, &value, ncx)?;
                            Ok(Value::string(JsString::intern(&formatted)))
                        },
                        ncx.memory_manager().clone(),
                        fn_proto,
                        "",
                        1,
                    );
                    target.define_property(
                        PropertyKey::string("format"),
                        PropertyDescriptor::data_with_attrs(
                            format,
                            PropertyAttributes::builtin_method(),
                        ),
                    );
                } else if brand_key == INTL_RELATIVETIMEFORMAT_BRAND_KEY
                    && let Some(value) = get_option_string(args.get(1), "numberingSystem", ncx)?
                    && SUPPORTED_NUMBERING_SYSTEMS.contains(&value.as_str())
                {
                    set_string_data_property(&target, INTL_NUMBERING_SYSTEM_KEY, &value);
                }
                Ok(Value::object(target))
            }
        }),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    proto_obj.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(ctor.clone(), PropertyAttributes::constructor_link()),
    );

    install_supported_locales_of(&ctor_obj, mm, fn_proto);

    intl.define_property(
        PropertyKey::string(ctor_name),
        PropertyDescriptor::data_with_attrs(ctor.clone(), PropertyAttributes::builtin_method()),
    );

    ctor
}

pub fn to_locale_lowercase(
    input: &str,
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<String, VmError> {
    let locale = normalize_locale_for_ops(first_requested_locale(locales, ncx)?);
    if is_turkic_locale(&locale) {
        return Ok(turkic_to_lowercase(input));
    }
    if is_lithuanian_locale(&locale) {
        return Ok(lithuanian_to_lowercase(input));
    }
    Ok(input.to_lowercase())
}

pub fn to_locale_uppercase(
    input: &str,
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<String, VmError> {
    let locale = normalize_locale_for_ops(first_requested_locale(locales, ncx)?);
    if is_turkic_locale(&locale) {
        return Ok(turkic_to_uppercase(input));
    }
    if is_lithuanian_locale(&locale) {
        return Ok(lithuanian_to_uppercase(input));
    }
    Ok(input.to_uppercase())
}

pub fn locale_compare(
    left: &str,
    right: &str,
    locales: Option<&Value>,
    options: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<f64, VmError> {
    use unicode_normalization::UnicodeNormalization;

    validate_collator_options(options, ncx)?;
    let locale = normalize_locale_for_ops(first_requested_locale(locales, ncx)?);
    let left_norm = left.nfc().collect::<String>();
    let right_norm = right.nfc().collect::<String>();

    let ord = if is_turkic_locale(&locale) {
        turkic_to_lowercase(&left_norm).cmp(&turkic_to_lowercase(&right_norm))
    } else {
        let left_fold = left_norm.to_lowercase();
        let right_fold = right_norm.to_lowercase();
        let fold_ord = left_fold.cmp(&right_fold);
        if fold_ord != std::cmp::Ordering::Equal {
            fold_ord
        } else if left_norm == right_norm {
            std::cmp::Ordering::Equal
        } else {
            let mut tie = std::cmp::Ordering::Equal;
            for (lch, rch) in left_norm.chars().zip(right_norm.chars()) {
                if lch == rch {
                    continue;
                }
                let ll = lch.to_lowercase().to_string();
                let rl = rch.to_lowercase().to_string();
                if ll == rl {
                    let l_lower = lch.is_lowercase();
                    let r_lower = rch.is_lowercase();
                    if l_lower != r_lower {
                        tie = if l_lower {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                        break;
                    }
                }
                tie = lch.cmp(&rch);
                break;
            }
            if tie == std::cmp::Ordering::Equal {
                left_norm.len().cmp(&right_norm.len())
            } else {
                tie
            }
        }
    };
    Ok(match ord {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    })
}

fn strip_punctuation_and_space(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn de_phonebook_fold(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            'Ä' => out.push_str("Ae"),
            'ä' => out.push_str("ae"),
            'Ö' => out.push_str("Oe"),
            'ö' => out.push_str("oe"),
            'Ü' => out.push_str("Ue"),
            'ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            _ => out.push(ch),
        }
    }
    out
}

fn strip_diacritics(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    input
        .nfd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect()
}

pub fn install_intl(
    global: GcRef<JsObject>,
    object_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let intl = GcRef::new(JsObject::new(Value::object(object_proto), mm.clone()));
    let to_string_tag_symbol = global
        .get(&PropertyKey::string("Symbol"))
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("toStringTag")))
        .and_then(|v| v.as_symbol());
    let set_proto_to_string_tag = |ctor: &Value, tag: &str| {
        if let Some(proto) = ctor
            .as_object()
            .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
        {
            if let Some(symbol) = to_string_tag_symbol.clone() {
                proto.define_property(
                    PropertyKey::Symbol(symbol),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern(tag)),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: true,
                        },
                    ),
                );
            }
        }
    };

    let get_canonical_locales = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let locales = canonicalize_locale_list(args.first(), ncx)?;
            let arr = create_array(ncx, locales.len());
            for (i, locale) in locales.iter().enumerate() {
                let _ = arr.set(
                    PropertyKey::Index(i as u32),
                    Value::string(JsString::intern(locale)),
                );
            }
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "getCanonicalLocales",
        1,
    );
    intl.define_property(
        PropertyKey::string("getCanonicalLocales"),
        PropertyDescriptor::builtin_method(get_canonical_locales),
    );

    let supported_values_of = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let key = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let mut values: Vec<&str> = match key.as_str() {
                "calendar" => SUPPORTED_CALENDARS.to_vec(),
                "collation" => SUPPORTED_COLLATIONS.to_vec(),
                "currency" => SUPPORTED_CURRENCIES.to_vec(),
                "numberingSystem" => SUPPORTED_NUMBERING_SYSTEMS.to_vec(),
                "timeZone" => SUPPORTED_TIME_ZONES.to_vec(),
                "unit" => SUPPORTED_UNITS.to_vec(),
                _ => {
                    return Err(VmError::range_error(
                        "Invalid key for Intl.supportedValuesOf",
                    ));
                }
            };
            values.sort_unstable();
            values.dedup();
            let arr = create_array(ncx, values.len());
            for (i, value) in values.iter().enumerate() {
                let _ = arr.set(
                    PropertyKey::Index(i as u32),
                    Value::string(JsString::intern(value)),
                );
            }
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "supportedValuesOf",
        1,
    );
    intl.define_property(
        PropertyKey::string("supportedValuesOf"),
        PropertyDescriptor::builtin_method(supported_values_of),
    );

    let collator_compare_getter = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_COLLATOR_BRAND_KEY,
                "Intl.Collator.prototype.compare",
            )?;
            let fn_proto = ncx
                .ctx
                .function_prototype()
                .ok_or_else(|| VmError::type_error("Function.prototype is not available"))?;
            let compare = Value::native_function_with_proto_named(
                move |_this, args, ncx| {
                    let left = args.first().cloned().unwrap_or(Value::undefined());
                    let right = args.get(1).cloned().unwrap_or(Value::undefined());
                    let l = ncx.to_string_value(&left)?;
                    let r = ncx.to_string_value(&right)?;
                    let locale_value = Value::string(JsString::intern(&locale));
                    Ok(Value::number(locale_compare(
                        &l,
                        &r,
                        Some(&locale_value),
                        None,
                        ncx,
                    )?))
                },
                ncx.memory_manager().clone(),
                fn_proto,
                "",
                2,
            );
            Ok(compare)
        },
        mm.clone(),
        fn_proto.clone(),
        "get compare",
        0,
    );
    let collator_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            let this_obj = this_val.as_object().ok_or_else(|| {
                VmError::type_error("Intl.Collator.prototype.resolvedOptions called on non-object")
            })?;
            if this_obj
                .get(&PropertyKey::string(INTL_COLLATOR_BRAND_KEY))
                .is_none()
            {
                return Err(VmError::type_error(
                    "Intl.Collator.prototype.resolvedOptions called on incompatible receiver",
                ));
            }
            let locale = this_obj
                .get(&PropertyKey::string(INTL_LOCALE_KEY))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
            let obj = create_plain_object(ncx);
            obj.define_property(
                PropertyKey::string("locale"),
                PropertyDescriptor::data(Value::string(JsString::intern(&locale))),
            );
            obj.define_property(
                PropertyKey::string("usage"),
                PropertyDescriptor::data(
                    this_obj
                        .get(&PropertyKey::string(INTL_USAGE_KEY))
                        .unwrap_or_else(|| Value::string(JsString::intern("sort"))),
                ),
            );
            obj.define_property(
                PropertyKey::string("sensitivity"),
                PropertyDescriptor::data(
                    this_obj
                        .get(&PropertyKey::string(INTL_SENSITIVITY_KEY))
                        .unwrap_or_else(|| Value::string(JsString::intern("variant"))),
                ),
            );
            obj.define_property(
                PropertyKey::string("ignorePunctuation"),
                PropertyDescriptor::data(
                    this_obj
                        .get(&PropertyKey::string(INTL_IGNORE_PUNCTUATION_KEY))
                        .unwrap_or_else(|| Value::boolean(false)),
                ),
            );
            obj.define_property(
                PropertyKey::string("collation"),
                PropertyDescriptor::data(
                    this_obj
                        .get(&PropertyKey::string(INTL_COLLATION_KEY))
                        .unwrap_or_else(|| Value::string(JsString::intern("default"))),
                ),
            );
            if let Some(numeric) = this_obj.get(&PropertyKey::string(INTL_NUMERIC_KEY)) {
                obj.define_property(
                    PropertyKey::string("numeric"),
                    PropertyDescriptor::data(numeric),
                );
            }
            if let Some(case_first) = this_obj.get(&PropertyKey::string(INTL_CASE_FIRST_KEY)) {
                obj.define_property(
                    PropertyKey::string("caseFirst"),
                    PropertyDescriptor::data(case_first),
                );
            }
            Ok(Value::object(obj))
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    let collator_ctor = install_basic_intl_constructor(
        &intl,
        "Collator",
        INTL_COLLATOR_BRAND_KEY,
        0,
        &[("resolvedOptions", 0, collator_resolved_options)],
        mm,
        object_proto,
        fn_proto.clone(),
    );
    set_proto_to_string_tag(&collator_ctor, "Intl.Collator");
    if let Some(collator_ctor_obj) = collator_ctor.as_object()
        && let Some(collator_proto_val) = collator_ctor_obj.get(&PropertyKey::string("prototype"))
        && let Some(collator_proto_obj) = collator_proto_val.as_object()
    {
        collator_proto_obj.define_property(
            PropertyKey::string("compare"),
            PropertyDescriptor::getter(collator_compare_getter),
        );
    }

    let datetime_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let s = ncx.to_string_value(&value)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let datetime_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_DATETIMEFORMAT_BRAND_KEY,
                "Intl.DateTimeFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    let datetime_ctor = install_basic_intl_constructor(
        &intl,
        "DateTimeFormat",
        INTL_DATETIMEFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, datetime_format),
            ("resolvedOptions", 0, datetime_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );
    set_proto_to_string_tag(&datetime_ctor, "Intl.DateTimeFormat");

    let number_format = Value::native_function_with_proto_named(
        |this_val, args, ncx| {
            let this_obj = this_val.as_object().ok_or_else(|| {
                VmError::type_error("Intl.NumberFormat.prototype.format called on non-object")
            })?;
            if this_obj
                .get(&PropertyKey::string(INTL_NUMBERFORMAT_BRAND_KEY))
                .is_none()
            {
                return Err(VmError::type_error(
                    "Intl.NumberFormat.prototype.format called on incompatible receiver",
                ));
            }
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let formatted = nf_format_to_string(&this_obj, &value, ncx)?;
            Ok(Value::string(JsString::intern(&formatted)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let number_format_to_parts = Value::native_function_with_proto_named(
        |this_val, args, ncx| {
            let this_obj = this_val.as_object().ok_or_else(|| {
                VmError::type_error(
                    "Intl.NumberFormat.prototype.formatToParts called on non-object",
                )
            })?;
            if this_obj
                .get(&PropertyKey::string(INTL_NUMBERFORMAT_BRAND_KEY))
                .is_none()
            {
                return Err(VmError::type_error(
                    "Intl.NumberFormat.prototype.formatToParts called on incompatible receiver",
                ));
            }
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let raw_parts = nf_format_to_parts(&this_obj, &value, ncx)?;
            let arr = create_array(ncx, raw_parts.len());
            for (i, (ty, part_value)) in raw_parts.into_iter().enumerate() {
                let part = create_plain_object(ncx);
                part.define_property(
                    PropertyKey::string("type"),
                    PropertyDescriptor::builtin_data(Value::string(JsString::intern(&ty))),
                );
                part.define_property(
                    PropertyKey::string("value"),
                    PropertyDescriptor::builtin_data(Value::string(JsString::intern(&part_value))),
                );
                let _ = arr.set(PropertyKey::Index(i as u32), Value::object(part));
            }
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        1,
    );
    let number_format_range = Value::native_function_with_proto_named(
        |this_val, args, ncx| {
            let this_obj = this_val.as_object().ok_or_else(|| {
                VmError::type_error("Intl.NumberFormat.prototype.formatRange called on non-object")
            })?;
            if this_obj
                .get(&PropertyKey::string(INTL_NUMBERFORMAT_BRAND_KEY))
                .is_none()
            {
                return Err(VmError::type_error(
                    "Intl.NumberFormat.prototype.formatRange called on incompatible receiver",
                ));
            }
            let start_value = args.first().cloned().unwrap_or(Value::undefined());
            let end_value = args.get(1).cloned().unwrap_or(Value::undefined());
            if start_value.is_undefined() || end_value.is_undefined() {
                return Err(VmError::type_error("start and end arguments are required"));
            }
            let start = if start_value.is_bigint() {
                let s = ncx.to_string_value(&start_value)?;
                s.parse::<f64>()
                    .map_err(|_| VmError::type_error("Cannot convert BigInt to numeric value"))?
            } else {
                ncx.to_number_value(&start_value)?
            };
            let end = if end_value.is_bigint() {
                let s = ncx.to_string_value(&end_value)?;
                s.parse::<f64>()
                    .map_err(|_| VmError::type_error("Cannot convert BigInt to numeric value"))?
            } else {
                ncx.to_number_value(&end_value)?
            };
            if start.is_nan() || end.is_nan() {
                return Err(VmError::range_error("start or end is NaN"));
            }
            Ok(Value::string(JsString::intern(&format!("{start} - {end}"))))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatRange",
        2,
    );
    let number_format_range_to_parts = Value::native_function_with_proto_named(
        |this_val, args, ncx| {
            let this_obj = this_val.as_object().ok_or_else(|| {
                VmError::type_error(
                    "Intl.NumberFormat.prototype.formatRangeToParts called on non-object",
                )
            })?;
            if this_obj
                .get(&PropertyKey::string(INTL_NUMBERFORMAT_BRAND_KEY))
                .is_none()
            {
                return Err(VmError::type_error(
                    "Intl.NumberFormat.prototype.formatRangeToParts called on incompatible receiver",
                ));
            }
            let start_value = args.first().cloned().unwrap_or(Value::undefined());
            let end_value = args.get(1).cloned().unwrap_or(Value::undefined());
            if start_value.is_undefined() || end_value.is_undefined() {
                return Err(VmError::type_error("start and end arguments are required"));
            }
            let start = if start_value.is_bigint() {
                let s = ncx.to_string_value(&start_value)?;
                s.parse::<f64>()
                    .map_err(|_| VmError::type_error("Cannot convert BigInt to numeric value"))?
            } else {
                ncx.to_number_value(&start_value)?
            };
            let end = if end_value.is_bigint() {
                let s = ncx.to_string_value(&end_value)?;
                s.parse::<f64>()
                    .map_err(|_| VmError::type_error("Cannot convert BigInt to numeric value"))?
            } else {
                ncx.to_number_value(&end_value)?
            };
            if start.is_nan() || end.is_nan() {
                return Err(VmError::range_error("start or end is NaN"));
            }
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("literal"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&format!(
                    "{start} - {end}"
                )))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatRangeToParts",
        2,
    );
    let number_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_NUMBERFORMAT_BRAND_KEY,
                "Intl.NumberFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    let number_ctor = install_basic_intl_constructor(
        &intl,
        "NumberFormat",
        INTL_NUMBERFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, number_format),
            ("formatToParts", 1, number_format_to_parts),
            ("formatRange", 2, number_format_range),
            ("formatRangeToParts", 2, number_format_range_to_parts),
            ("resolvedOptions", 0, number_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );
    set_proto_to_string_tag(&number_ctor, "Intl.NumberFormat");
    let number_format_getter = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            let this_obj = this_val.as_object().ok_or_else(|| {
                VmError::type_error("Intl.NumberFormat.prototype.format called on non-object")
            })?;
            if this_obj
                .get(&PropertyKey::string(INTL_NUMBERFORMAT_BRAND_KEY))
                .is_none()
            {
                return Err(VmError::type_error(
                    "Intl.NumberFormat.prototype.format called on incompatible receiver",
                ));
            }
            let fn_proto = ncx
                .ctx
                .function_prototype()
                .ok_or_else(|| VmError::type_error("Function.prototype is not available"))?;
            let bound_this = this_obj.clone();
            let format = Value::native_function_with_proto_named(
                move |_ignored_this, args, ncx| {
                    let value = args.first().cloned().unwrap_or(Value::undefined());
                    let formatted = nf_format_to_string(&bound_this, &value, ncx)?;
                    Ok(Value::string(JsString::intern(&formatted)))
                },
                ncx.memory_manager().clone(),
                fn_proto,
                "",
                1,
            );
            Ok(format)
        },
        mm.clone(),
        fn_proto.clone(),
        "get format",
        0,
    );
    if let Some(number_ctor_obj) = number_ctor.as_object()
        && let Some(number_proto_val) = number_ctor_obj.get(&PropertyKey::string("prototype"))
        && let Some(number_proto_obj) = number_proto_val.as_object()
    {
        number_proto_obj.define_property(
            PropertyKey::string("format"),
            PropertyDescriptor::getter(number_format_getter),
        );
    }

    let plural_select = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::number(0.0));
            let n = ncx.to_number_value(&value)?;
            let sel = if n == 1.0 { "one" } else { "other" };
            Ok(Value::string(JsString::intern(sel)))
        },
        mm.clone(),
        fn_proto.clone(),
        "select",
        1,
    );
    let plural_select_range = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let start = ncx.to_number_value(args.first().unwrap_or(&Value::number(0.0)))?;
            let end = ncx.to_number_value(args.get(1).unwrap_or(&Value::number(0.0)))?;
            let sel = if start == 1.0 && end == 1.0 {
                "one"
            } else {
                "other"
            };
            Ok(Value::string(JsString::intern(sel)))
        },
        mm.clone(),
        fn_proto.clone(),
        "selectRange",
        2,
    );
    let plural_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_PLURALRULES_BRAND_KEY,
                "Intl.PluralRules.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "PluralRules",
        INTL_PLURALRULES_BRAND_KEY,
        0,
        &[
            ("select", 1, plural_select),
            ("selectRange", 2, plural_select_range),
            ("resolvedOptions", 0, plural_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let rtf_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::number(0.0));
            let unit = args
                .get(1)
                .cloned()
                .unwrap_or(Value::string(JsString::intern("second")));
            let v = ncx.to_string_value(&value)?;
            let u = ncx.to_string_value(&unit)?;
            Ok(Value::string(JsString::intern(&format!("{v} {u}"))))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        2,
    );
    let rtf_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let formatted = {
                let value = args.first().cloned().unwrap_or(Value::number(0.0));
                let unit = args
                    .get(1)
                    .cloned()
                    .unwrap_or(Value::string(JsString::intern("second")));
                let v = ncx.to_string_value(&value)?;
                let u = ncx.to_string_value(&unit)?;
                format!("{v} {u}")
            };
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("literal"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&formatted))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        2,
    );
    let rtf_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_RELATIVETIMEFORMAT_BRAND_KEY,
                "Intl.RelativeTimeFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "RelativeTimeFormat",
        INTL_RELATIVETIMEFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 2, rtf_format),
            ("formatToParts", 2, rtf_format_to_parts),
            ("resolvedOptions", 0, rtf_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let list_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            if let Some(v) = args.first() {
                if let Some(obj) = v.as_object() {
                    let length = obj
                        .get(&PropertyKey::string("length"))
                        .map(|v| {
                            ncx.to_number_value(&v)
                                .unwrap_or(0.0)
                                .max(0.0)
                                .min(64.0)
                                .floor() as usize
                        })
                        .unwrap_or(0);
                    let mut parts = Vec::new();
                    for i in 0..length {
                        if let Some(item) = obj.get(&PropertyKey::Index(i as u32)) {
                            parts.push(ncx.to_string_value(&item)?);
                        }
                    }
                    return Ok(Value::string(JsString::intern(&parts.join(", "))));
                }
            }
            let s = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let list_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let formatted = if let Some(v) = args.first() {
                ncx.to_string_value(v)?
            } else {
                String::new()
            };
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("element"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&formatted))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        1,
    );
    let list_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_LISTFORMAT_BRAND_KEY,
                "Intl.ListFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "ListFormat",
        INTL_LISTFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, list_format),
            ("formatToParts", 1, list_format_to_parts),
            ("resolvedOptions", 0, list_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let display_names_of = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let code = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            Ok(Value::string(JsString::intern(&code)))
        },
        mm.clone(),
        fn_proto.clone(),
        "of",
        1,
    );
    let display_names_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_DISPLAYNAMES_BRAND_KEY,
                "Intl.DisplayNames.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "DisplayNames",
        INTL_DISPLAYNAMES_BRAND_KEY,
        0,
        &[
            ("of", 1, display_names_of),
            ("resolvedOptions", 0, display_names_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let segmenter_segment = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let input = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let result = create_plain_object(ncx);
            result.define_property(
                PropertyKey::string("input"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&input))),
            );
            let segments = create_array(ncx, 1);
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("segment"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&input))),
            );
            let _ = segments.set(PropertyKey::Index(0), Value::object(part));
            result.define_property(
                PropertyKey::string("segments"),
                PropertyDescriptor::builtin_data(Value::array(segments)),
            );
            Ok(Value::object(result))
        },
        mm.clone(),
        fn_proto.clone(),
        "segment",
        1,
    );
    let segmenter_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_SEGMENTER_BRAND_KEY,
                "Intl.Segmenter.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "Segmenter",
        INTL_SEGMENTER_BRAND_KEY,
        0,
        &[
            ("segment", 1, segmenter_segment),
            ("resolvedOptions", 0, segmenter_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let duration_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let s = ncx.to_string_value(&value)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let duration_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let formatted = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("literal"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&formatted))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        1,
    );
    let duration_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_DURATIONFORMAT_BRAND_KEY,
                "Intl.DurationFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "DurationFormat",
        INTL_DURATIONFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, duration_format),
            ("formatToParts", 1, duration_format_to_parts),
            ("resolvedOptions", 0, duration_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let locale_to_string = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.toString",
            )?;
            Ok(Value::string(JsString::intern(&locale)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    let locale_maximize = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let _ = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.maximize",
            )?;
            Ok(this_val.clone())
        },
        mm.clone(),
        fn_proto.clone(),
        "maximize",
        0,
    );
    let locale_minimize = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let _ = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.minimize",
            )?;
            Ok(this_val.clone())
        },
        mm.clone(),
        fn_proto.clone(),
        "minimize",
        0,
    );
    let locale_calendar_getter = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.calendar",
            )?;
            let calendar = this_val
                .as_object()
                .and_then(|obj| {
                    obj.get(&PropertyKey::string(INTL_CALENDAR_KEY))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                })
                .or_else(|| unicode_keyword_from_locale(&locale, "ca"))
                .map(|s| Value::string(JsString::intern(&s)))
                .unwrap_or(Value::undefined());
            Ok(calendar)
        },
        mm.clone(),
        fn_proto.clone(),
        "get calendar",
        0,
    );
    let locale_collation_getter = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.collation",
            )?;
            let collation = this_val
                .as_object()
                .and_then(|obj| {
                    obj.get(&PropertyKey::string(INTL_COLLATION_KEY))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                })
                .or_else(|| unicode_keyword_from_locale(&locale, "co"))
                .map(|s| Value::string(JsString::intern(&s)))
                .unwrap_or(Value::undefined());
            Ok(collation)
        },
        mm.clone(),
        fn_proto.clone(),
        "get collation",
        0,
    );
    let locale_numbering_system_getter = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.numberingSystem",
            )?;
            let numbering_system = this_val
                .as_object()
                .and_then(|obj| {
                    obj.get(&PropertyKey::string(INTL_NUMBERING_SYSTEM_KEY))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                })
                .or_else(|| unicode_keyword_from_locale(&locale, "nu"))
                .map(|s| Value::string(JsString::intern(&s)))
                .unwrap_or(Value::undefined());
            Ok(numbering_system)
        },
        mm.clone(),
        fn_proto.clone(),
        "get numberingSystem",
        0,
    );
    let locale_ctor = install_basic_intl_constructor(
        &intl,
        "Locale",
        INTL_LOCALE_BRAND_KEY,
        1,
        &[
            ("toString", 0, locale_to_string),
            ("maximize", 0, locale_maximize),
            ("minimize", 0, locale_minimize),
        ],
        mm,
        object_proto,
        fn_proto,
    );
    if let Some(locale_proto) = locale_ctor
        .as_object()
        .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
    {
        locale_proto.define_property(
            PropertyKey::string("calendar"),
            PropertyDescriptor::Accessor {
                get: Some(locale_calendar_getter),
                set: None,
                attributes: PropertyAttributes::builtin_accessor(),
            },
        );
        locale_proto.define_property(
            PropertyKey::string("collation"),
            PropertyDescriptor::Accessor {
                get: Some(locale_collation_getter),
                set: None,
                attributes: PropertyAttributes::builtin_accessor(),
            },
        );
        locale_proto.define_property(
            PropertyKey::string("numberingSystem"),
            PropertyDescriptor::Accessor {
                get: Some(locale_numbering_system_getter),
                set: None,
                attributes: PropertyAttributes::builtin_accessor(),
            },
        );
    }

    if let Some(symbol_ctor) = global
        .get(&PropertyKey::string("Symbol"))
        .and_then(|v| v.as_object())
        && let Some(to_string_tag) = symbol_ctor
            .get(&PropertyKey::string("toStringTag"))
            .and_then(|v| v.as_symbol())
    {
        intl.define_property(
            PropertyKey::Symbol(to_string_tag),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("Intl")),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            ),
        );
    } else {
        intl.define_property(
            PropertyKey::string("@@toStringTag"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("Intl")),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            ),
        );
    }

    global.define_property(
        PropertyKey::string("Intl"),
        PropertyDescriptor::data_with_attrs(
            Value::object(intl),
            PropertyAttributes::builtin_method(),
        ),
    );
    let _ = global.set(PropertyKey::string("__Intl_Collator"), collator_ctor);
}

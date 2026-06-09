//! Unicode property-escape (`\p{...}` / `\P{...}`) resolution.
//!
//! Property data is sourced from the ICU4X `icu_properties` crate (UCD tables
//! under the permissive Unicode license) — we do **not** vendor any external
//! engine's generated table. Code-point properties (General_Category, Script,
//! Script_Extensions, binary properties) come from `icu_properties`; `v`-flag
//! *string* properties (`\p{Basic_Emoji}`) come from [`string_props`].
//!
//! # Contents
//! - [`resolve_property`] — map a `\p{...}` name (and optional value) to a
//!   code-point set.
//! - [`string_props`] — `v`-flag string-property sets.
//!
//! # Invariants
//! - An unknown or malformed property name is a compile-time error
//!   ([`crate::RegexError`]), not a silent empty set.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-characterclassescape> (§22.2.1)

#[cfg(feature = "unicode")]
pub(crate) mod string_props;

use crate::classes::CodePointSet;
use crate::error::RegexError;

#[cfg(feature = "unicode")]
use icu_properties::props::{GeneralCategory, GeneralCategoryGroup, Script};
#[cfg(feature = "unicode")]
use icu_properties::script::ScriptWithExtensions;
#[cfg(feature = "unicode")]
use icu_properties::{CodePointMapData, CodePointSetData, PropertyParser};

#[cfg(feature = "unicode")]
fn unknown(name: &str) -> RegexError {
    RegexError::Syntax {
        message: format!("unknown Unicode property `{name}`"),
        offset: usize::MAX,
    }
}

/// Resolve a property escape `\p{name}` or `\p{name=value}` to a code-point set.
///
/// `value` is `None` for binary properties and lone General_Category values
/// (e.g. `\p{Letter}`), `Some(_)` for the `name=value` form (e.g.
/// `\p{Script=Greek}`). Resolution follows ECMA-262 §22.2.1: lone names are a
/// binary property or a General_Category value; `gc`/`sc`/`scx` carry a value.
#[cfg(feature = "unicode")]
pub(crate) fn resolve_property(
    name: &str,
    value: Option<&str>,
) -> Result<CodePointSet, RegexError> {
    match value {
        None => binary_or_general_category(name).ok_or_else(|| unknown(name)),
        Some(v) => match name {
            "General_Category" | "gc" => general_category(v).ok_or_else(|| unknown(v)),
            "Script" | "sc" => script(v, false).ok_or_else(|| unknown(v)),
            "Script_Extensions" | "scx" => script(v, true).ok_or_else(|| unknown(v)),
            _ => Err(unknown(name)),
        },
    }
}

/// A lone `\p{name}`: an ECMA-262 binary property, else a General_Category value.
#[cfg(feature = "unicode")]
fn binary_or_general_category(name: &str) -> Option<CodePointSet> {
    // §22.2.1 lists `Any`, `ASCII`, and `Assigned` as binary properties, but
    // they are not real Unicode binary properties and `icu_properties` does
    // not surface them, so resolve them directly.
    match name {
        "Any" => {
            let mut set = CodePointSet::new();
            set.insert_range(0, 0x10_FFFF);
            return Some(set);
        }
        "ASCII" => {
            let mut set = CodePointSet::new();
            set.insert_range(0, 0x7F);
            return Some(set);
        }
        "Assigned" => {
            // Every code point that is not General_Category=Unassigned (Cn).
            return general_category("Cn").map(|cn| cn.negate());
        }
        _ => {}
    }
    if let Some(set) = CodePointSetData::new_for_ecma262(name.as_bytes()) {
        return Some(CodePointSet::from_ranges(set.iter_ranges()));
    }
    general_category(name)
}

/// A General_Category value (`Lu`, `Uppercase_Letter`) or group (`L`, `Letter`).
#[cfg(feature = "unicode")]
fn general_category(value: &str) -> Option<CodePointSet> {
    let map = CodePointMapData::<GeneralCategory>::new();
    if let Some(gc) = PropertyParser::<GeneralCategory>::new().get_strict(value) {
        let set = map.get_set_for_value(gc);
        return Some(CodePointSet::from_ranges(set.as_borrowed().iter_ranges()));
    }
    let group = general_category_group(value)?;
    let set = map.get_set_for_value_group(group);
    Some(CodePointSet::from_ranges(set.as_borrowed().iter_ranges()))
}

/// ECMA-262 General_Category mask aliases (the multi-category groups).
#[cfg(feature = "unicode")]
fn general_category_group(value: &str) -> Option<GeneralCategoryGroup> {
    Some(match value {
        "L" | "Letter" => GeneralCategoryGroup::Letter,
        "LC" | "Cased_Letter" => GeneralCategoryGroup::CasedLetter,
        "M" | "Mark" | "Combining_Mark" => GeneralCategoryGroup::Mark,
        "N" | "Number" => GeneralCategoryGroup::Number,
        "P" | "Punctuation" | "punct" => GeneralCategoryGroup::Punctuation,
        "S" | "Symbol" => GeneralCategoryGroup::Symbol,
        "Z" | "Separator" => GeneralCategoryGroup::Separator,
        "C" | "Other" => GeneralCategoryGroup::Other,
        _ => return None,
    })
}

/// A `Script` (or `Script_Extensions` when `extensions`) value.
#[cfg(feature = "unicode")]
fn script(value: &str, extensions: bool) -> Option<CodePointSet> {
    let sc = PropertyParser::<Script>::new().get_strict(value)?;
    if extensions {
        let swe = ScriptWithExtensions::new();
        Some(CodePointSet::from_ranges(
            swe.get_script_extensions_ranges(sc),
        ))
    } else {
        let set = CodePointMapData::<Script>::new().get_set_for_value(sc);
        Some(CodePointSet::from_ranges(set.as_borrowed().iter_ranges()))
    }
}

/// Property escapes are unavailable without the `unicode` feature.
#[cfg(not(feature = "unicode"))]
pub(crate) fn resolve_property(
    _name: &str,
    _value: Option<&str>,
) -> Result<CodePointSet, RegexError> {
    Err(RegexError::Syntax {
        message: "property escapes require the `unicode` feature".to_string(),
        offset: usize::MAX,
    })
}

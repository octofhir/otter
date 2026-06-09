//! `v`-flag string-property sets (`\p{Basic_Emoji}`, `\p{RGI_Emoji}`, …).
//!
//! These properties denote sets of *strings* (some multi-code-point, e.g. emoji
//! ZWJ sequences), not code points, so they cannot be expressed as a
//! [`crate::classes::CodePointSet`]. They are only valid inside a `v`-mode class
//! and contribute string alternatives. The underlying sequence data comes from
//! UCD `emoji-sequences.txt` / `emoji-zwj-sequences.txt` via this crate's own
//! codegen (no vendored external table).
//!
//! # Contents
//! - [`resolve_string_property`] — map a string-property name to its set.
//!
//! # Invariants
//! - Only valid in `v` mode; the parser rejects these names under `u`/non-`u`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-static-semantics-maybesimplecaseinsensitive>
//!   and the UTS#51 emoji properties referenced by §22.2.1.

use crate::classes::{ClassSet, CodePointSet};
use crate::error::RegexError;

/// The `v`-flag string-property names recognised by the parser (§22.2.1
/// `ClassSetExpression`). Only `Basic_Emoji` is backed by data today;
/// the RGI emoji-sequence properties need a UCD codegen pipeline that is
/// not yet wired, so they resolve to an error rather than silently
/// matching nothing.
#[must_use]
pub(crate) fn is_string_property(name: &str) -> bool {
    matches!(
        name,
        "Basic_Emoji"
            | "Emoji_Keycap_Sequence"
            | "RGI_Emoji"
            | "RGI_Emoji_Flag_Sequence"
            | "RGI_Emoji_Modifier_Sequence"
            | "RGI_Emoji_Tag_Sequence"
            | "RGI_Emoji_ZWJ_Sequence"
    )
}

/// Resolve a `v`-flag string property (e.g. `Basic_Emoji`) to its class
/// set (code points plus multi-code-point string alternatives).
#[cfg(feature = "unicode")]
pub(crate) fn resolve_string_property(name: &str) -> Result<ClassSet, RegexError> {
    use icu_properties::EmojiSetData;
    use icu_properties::props::BasicEmoji;

    let data = match name {
        "Basic_Emoji" => EmojiSetData::new::<BasicEmoji>().static_to_owned(),
        // §22.2.1 string properties whose sequence data `icu_properties`
        // does not ship as a usable set in this build.
        _ => {
            return Err(RegexError::Syntax {
                message: format!("unsupported Unicode string property `{name}`"),
                offset: usize::MAX,
            });
        }
    };
    let list = data
        .as_code_point_inversion_list_string_list()
        .ok_or_else(|| RegexError::Syntax {
            message: format!("string property `{name}` is unavailable"),
            offset: usize::MAX,
        })?;
    let mut code_points = CodePointSet::new();
    for range in list.code_points().iter_ranges() {
        code_points.insert_range(*range.start(), *range.end());
    }
    let mut set = ClassSet::from_code_points(code_points);
    for s in list.strings().iter() {
        set.add_alternative(s.chars().map(u32::from).collect());
    }
    Ok(set)
}

#[cfg(not(feature = "unicode"))]
pub(crate) fn resolve_string_property(_name: &str) -> Result<ClassSet, RegexError> {
    Err(RegexError::Syntax {
        message: "string properties require the `unicode` feature".to_string(),
        offset: usize::MAX,
    })
}

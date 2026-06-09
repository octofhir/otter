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

mod emoji_data {
    include!(concat!(env!("OUT_DIR"), "/emoji_data.rs"));
}

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

/// Build a [`ClassSet`] from generated `(ranges, strings)` tables.
fn class_set_from(ranges: &[(u32, u32)], strings: &[&[u32]]) -> ClassSet {
    let mut code_points = CodePointSet::new();
    for (lo, hi) in ranges {
        code_points.insert_range(*lo, *hi);
    }
    let mut set = ClassSet::from_code_points(code_points);
    for s in strings {
        set.add_alternative(s.to_vec());
    }
    set
}

/// Resolve a `v`-flag string property (e.g. `Basic_Emoji`, `RGI_Emoji`)
/// to its class set: single-code-point members plus multi-code-point
/// string alternatives. Backed by build-time codegen from the vendored
/// UCD `emoji-sequences.txt` / `emoji-zwj-sequences.txt`. `RGI_Emoji` is
/// the union of every emoji-sequence property (UTS#51).
pub(crate) fn resolve_string_property(name: &str) -> Result<ClassSet, RegexError> {
    use emoji_data as e;
    let set = match name {
        "Basic_Emoji" => class_set_from(e::BASIC_EMOJI_RANGES, e::BASIC_EMOJI_STRINGS),
        "Emoji_Keycap_Sequence" => class_set_from(
            e::EMOJI_KEYCAP_SEQUENCE_RANGES,
            e::EMOJI_KEYCAP_SEQUENCE_STRINGS,
        ),
        "RGI_Emoji_Flag_Sequence" => class_set_from(
            e::RGI_EMOJI_FLAG_SEQUENCE_RANGES,
            e::RGI_EMOJI_FLAG_SEQUENCE_STRINGS,
        ),
        "RGI_Emoji_Modifier_Sequence" => class_set_from(
            e::RGI_EMOJI_MODIFIER_SEQUENCE_RANGES,
            e::RGI_EMOJI_MODIFIER_SEQUENCE_STRINGS,
        ),
        "RGI_Emoji_Tag_Sequence" => class_set_from(
            e::RGI_EMOJI_TAG_SEQUENCE_RANGES,
            e::RGI_EMOJI_TAG_SEQUENCE_STRINGS,
        ),
        "RGI_Emoji_ZWJ_Sequence" => class_set_from(
            e::RGI_EMOJI_ZWJ_SEQUENCE_RANGES,
            e::RGI_EMOJI_ZWJ_SEQUENCE_STRINGS,
        ),
        "RGI_Emoji" => {
            // §22.2.1 RGI_Emoji is the union of every emoji-sequence set.
            let mut set = class_set_from(e::BASIC_EMOJI_RANGES, e::BASIC_EMOJI_STRINGS);
            for part in [
                class_set_from(
                    e::EMOJI_KEYCAP_SEQUENCE_RANGES,
                    e::EMOJI_KEYCAP_SEQUENCE_STRINGS,
                ),
                class_set_from(
                    e::RGI_EMOJI_FLAG_SEQUENCE_RANGES,
                    e::RGI_EMOJI_FLAG_SEQUENCE_STRINGS,
                ),
                class_set_from(
                    e::RGI_EMOJI_MODIFIER_SEQUENCE_RANGES,
                    e::RGI_EMOJI_MODIFIER_SEQUENCE_STRINGS,
                ),
                class_set_from(
                    e::RGI_EMOJI_TAG_SEQUENCE_RANGES,
                    e::RGI_EMOJI_TAG_SEQUENCE_STRINGS,
                ),
                class_set_from(
                    e::RGI_EMOJI_ZWJ_SEQUENCE_RANGES,
                    e::RGI_EMOJI_ZWJ_SEQUENCE_STRINGS,
                ),
            ] {
                set.union_with(&part);
            }
            set
        }
        _ => {
            return Err(RegexError::Syntax {
                message: format!("unknown Unicode string property `{name}`"),
                offset: usize::MAX,
            });
        }
    };
    Ok(set)
}

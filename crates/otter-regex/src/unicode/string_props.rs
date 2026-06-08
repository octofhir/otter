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

use crate::classes::ClassSet;
use crate::error::RegexError;

/// Resolve a `v`-flag string property (e.g. `Basic_Emoji`) to its class set.
pub(crate) fn resolve_string_property(_name: &str) -> Result<ClassSet, RegexError> {
    todo!("Milestone 2: load string-property sets from generated UCD emoji data")
}

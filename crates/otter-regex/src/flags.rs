//! ECMAScript RegExp flag bits relevant to the matcher.
//!
//! # Contents
//! - [`Flags`] — the engine-relevant subset: `i` (ignoreCase), `m` (multiline),
//!   `s` (dotAll), `u` (unicode), `v` (unicodeSets).
//!
//! # Invariants
//! - `u` and `v` are mutually exclusive (§22.2.1 early errors); the host or the
//!   pattern compiler rejects the combination before reaching the engine.
//! - The stateful JS flags `g` (global), `y` (sticky), and `d` (hasIndices) are
//!   spec state *above* the matcher and are intentionally absent here.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-get-regexp.prototype.flags>

/// Engine-relevant RegExp flags.
///
/// Constructed by the host from a parsed flag set, or from a flag string via
/// [`Flags::from_str_lossy`]. All fields are JS-named booleans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// `i` — case-insensitive matching (Simple Case Folding under `u`/`v`).
    pub ignore_case: bool,
    /// `m` — `^`/`$` match at line terminators, not just input boundaries.
    pub multiline: bool,
    /// `s` — `.` also matches line terminators (dotAll).
    pub dot_all: bool,
    /// `u` — Unicode mode: code-point-aware matching, `\u{...}`, strict syntax.
    pub unicode: bool,
    /// `v` — UnicodeSets mode: set notation (`--`, `&&`), `\q{...}` string
    /// alternatives, and string properties (`\p{Basic_Emoji}`).
    pub unicode_sets: bool,
}

impl Flags {
    /// Whether matching operates on Unicode code points (either `u` or `v`).
    #[must_use]
    pub fn is_unicode_mode(self) -> bool {
        self.unicode || self.unicode_sets
    }

    /// Build flags from a flag string, ignoring characters the engine does not
    /// model (`g`/`y`/`d` live above the matcher). Unknown characters are
    /// ignored; duplicate/early-error checking is the host's responsibility.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        let mut f = Self::default();
        for c in s.chars() {
            match c {
                'i' => f.ignore_case = true,
                'm' => f.multiline = true,
                's' => f.dot_all = true,
                'u' => f.unicode = true,
                'v' => f.unicode_sets = true,
                _ => {}
            }
        }
        f
    }
}

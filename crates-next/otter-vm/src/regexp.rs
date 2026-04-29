//! JavaScript `RegExp` value, backed by the `regress` engine.
//!
//! Slice 31 introduces the value type plus the bytecode-level
//! pre-compilation cache. The runtime never re-parses a regex
//! literal: the compiler emits a [`otter_bytecode::Constant::RegExp`],
//! the VM compiles it once on first `Op::LoadRegExp` (cached in the
//! constant-pool slot), and every subsequent literal load shares the
//! same compiled engine.
//!
//! `regress` does not implement the JavaScript `g` (global) or `y`
//! (sticky) flags — those are stateful and live above the engine
//! per spec. We model both flags here through [`JsRegExp::flag_global`]
//! / [`JsRegExp::flag_sticky`] and the [`JsRegExp::last_index`] cell;
//! method implementations consult these during pattern execution.
//!
//! # Contents
//! - [`JsRegExp`] — the cheap-to-clone handle used in [`crate::Value`].
//! - [`RegExpFlags`] — parsed flag bits.
//! - [`compile`] — pattern + flag-string → engine, surfaced as
//!   [`RegExpError`] on failure.
//!
//! # Invariants
//! - The flag string is restricted to the ASCII subset `"gimsuy"` —
//!   the compiler validates this at intern time.
//! - `last_index` is interior-mutable but never stashed across
//!   reentrant calls; native methods refresh it before returning.
//! - Cloning a [`JsRegExp`] shares the compiled engine and the
//!   `last_index` cell — same identity semantics as `JsString` /
//!   `JsArray`.
//!
//! # See also
//! - [`docs/new-engine/tasks/31-regexp-and-pattern-methods.md`](
//!     ../../../docs/new-engine/tasks/31-regexp-and-pattern-methods.md
//!   )

use std::cell::Cell;
use std::rc::Rc;

use regress::{Flags, Regex};

/// Outcome of a fallible regex compile.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum RegExpError {
    /// `regress` rejected the pattern.
    #[error("invalid regular expression: {message}")]
    InvalidPattern {
        /// `regress`-side diagnostic.
        message: String,
    },
    /// Flag string contained a character outside `"gimsuy"`.
    #[error("invalid regular expression flag `{flag}`")]
    InvalidFlag {
        /// The offending character.
        flag: char,
    },
    /// Same flag was specified twice.
    #[error("duplicate regular expression flag `{flag}`")]
    DuplicateFlag {
        /// The repeated flag.
        flag: char,
    },
}

/// Foundation flag bits.
///
/// We keep this tiny so `JsRegExp` stays cheap to clone. The struct
/// is `Copy`; the bool fields map to the JS-visible accessors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegExpFlags {
    /// `g` — global. Stateful; honoured by [`crate::regexp_prototype`]
    /// and the pattern-arg `String.prototype.*` methods.
    pub global: bool,
    /// `i` — case-insensitive.
    pub ignore_case: bool,
    /// `m` — multiline.
    pub multiline: bool,
    /// `s` — dot-all.
    pub dot_all: bool,
    /// `u` — unicode.
    pub unicode: bool,
    /// `y` — sticky. Match anchored at `lastIndex`.
    pub sticky: bool,
}

impl RegExpFlags {
    /// Parse the canonical ASCII flag string. Order does not matter;
    /// duplicate flags raise [`RegExpError::DuplicateFlag`].
    pub fn parse(flags: &str) -> Result<Self, RegExpError> {
        let mut out = Self::default();
        for c in flags.chars() {
            let slot = match c {
                'g' => &mut out.global,
                'i' => &mut out.ignore_case,
                'm' => &mut out.multiline,
                's' => &mut out.dot_all,
                'u' => &mut out.unicode,
                'y' => &mut out.sticky,
                other => return Err(RegExpError::InvalidFlag { flag: other }),
            };
            if *slot {
                return Err(RegExpError::DuplicateFlag { flag: c });
            }
            *slot = true;
        }
        Ok(out)
    }

    /// Render as the canonical JS spelling (`gimsuy` order).
    #[must_use]
    pub fn to_js_string(self) -> String {
        let mut s = String::with_capacity(6);
        if self.global {
            s.push('g');
        }
        if self.ignore_case {
            s.push('i');
        }
        if self.multiline {
            s.push('m');
        }
        if self.dot_all {
            s.push('s');
        }
        if self.unicode {
            s.push('u');
        }
        if self.sticky {
            s.push('y');
        }
        s
    }
}

/// Inner state shared by every clone of a [`JsRegExp`].
#[derive(Debug)]
pub struct JsRegExpBody {
    /// Compiled `regress` engine. Always present after construction —
    /// errors surface during [`compile`] before the body is built.
    pub regex: Regex,
    /// Pattern source code-units (the body between the slashes).
    pub pattern_utf16: Vec<u16>,
    /// Pattern source as a Rust string (lossy WTF-16 → UTF-8). Used
    /// by the `.source` JS getter.
    pub source: String,
    /// Parsed flag bits.
    pub flags: RegExpFlags,
    /// `RegExp.prototype.lastIndex` — interior-mutable so methods
    /// observe successive `g` / `y` walks.
    pub last_index: Cell<u32>,
}

/// Cheap-to-clone JS regex handle.
#[derive(Debug, Clone)]
pub struct JsRegExp {
    inner: Rc<JsRegExpBody>,
}

impl JsRegExp {
    /// Compile a pattern + flag string into a runtime regex value.
    pub fn compile(pattern_utf16: &[u16], flag_str: &str) -> Result<Self, RegExpError> {
        let flags = RegExpFlags::parse(flag_str)?;
        // `regress` parses from a Rust `&str`, so feed it the lossy
        // UTF-8 reading. JS-only escape sequences (`\u{...}`,
        // `\xNN`, surrogate pairs) survive the round-trip because
        // they are ASCII at the byte level.
        let source = String::from_utf16_lossy(pattern_utf16);
        let engine_flags = Flags {
            icase: flags.ignore_case,
            multiline: flags.multiline,
            dot_all: flags.dot_all,
            unicode: flags.unicode,
            ..Default::default()
        };
        // `g` and `y` are spec-level state that lives above the
        // matcher; `regress` would silently ignore them anyway.
        let regex =
            Regex::with_flags(&source, engine_flags).map_err(|e| RegExpError::InvalidPattern {
                message: format!("{e}"),
            })?;
        Ok(Self {
            inner: Rc::new(JsRegExpBody {
                regex,
                pattern_utf16: pattern_utf16.to_vec(),
                source,
                flags,
                last_index: Cell::new(0),
            }),
        })
    }

    /// Compiled engine for direct execution.
    #[must_use]
    pub fn regex(&self) -> &Regex {
        &self.inner.regex
    }

    /// Parsed flag bits.
    #[must_use]
    pub fn flags(&self) -> RegExpFlags {
        self.inner.flags
    }

    /// `RegExp.prototype.source` view (UTF-8). Note this is lossy
    /// for surrogate-bearing patterns; the canonical body is
    /// [`Self::pattern_utf16`].
    #[must_use]
    pub fn source(&self) -> &str {
        &self.inner.source
    }

    /// Original WTF-16 pattern body.
    #[must_use]
    pub fn pattern_utf16(&self) -> &[u16] {
        &self.inner.pattern_utf16
    }

    /// Read `lastIndex`.
    #[must_use]
    pub fn last_index(&self) -> u32 {
        self.inner.last_index.get()
    }

    /// Update `lastIndex`. Pattern-arg methods use this to step
    /// through successive `g` / `y` matches.
    pub fn set_last_index(&self, value: u32) {
        self.inner.last_index.set(value);
    }

    /// Identity comparison — two handles are equal iff they share
    /// the same body. `RegExp` is a reference type in JS so `===`
    /// follows handle identity.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// Raw `Rc`-data pointer for use as a hash / map key in
    /// identity-keyed collections (`WeakMap` / `WeakSet`). Anchor
    /// the originating handle for the lifetime of the pointer.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        Rc::as_ptr(&self.inner).cast()
    }
}

impl PartialEq for JsRegExp {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}

impl Eq for JsRegExp {}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_simple(pattern: &str, flags: &str) -> Result<JsRegExp, RegExpError> {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        JsRegExp::compile(&units, flags)
    }

    #[test]
    fn flags_parse_canonical() {
        let f = RegExpFlags::parse("gim").unwrap();
        assert!(f.global && f.ignore_case && f.multiline);
        assert!(!f.dot_all && !f.unicode && !f.sticky);
        assert_eq!(f.to_js_string(), "gim");
    }

    #[test]
    fn flags_reject_duplicate() {
        assert!(matches!(
            RegExpFlags::parse("gg"),
            Err(RegExpError::DuplicateFlag { flag: 'g' })
        ));
    }

    #[test]
    fn flags_reject_unknown() {
        assert!(matches!(
            RegExpFlags::parse("z"),
            Err(RegExpError::InvalidFlag { flag: 'z' })
        ));
    }

    #[test]
    fn compile_smoke() {
        let r = compile_simple("ab+c", "i").unwrap();
        assert_eq!(r.source(), "ab+c");
        assert!(r.flags().ignore_case);
        let utf16: Vec<u16> = "abbbcXabbbbc".encode_utf16().collect();
        let m = r.regex().find_from_utf16(&utf16, 0).next().unwrap();
        assert_eq!(m.range, 0..5);
    }

    #[test]
    fn compile_rejects_bad_pattern() {
        let err = compile_simple("[", "").unwrap_err();
        assert!(matches!(err, RegExpError::InvalidPattern { .. }));
    }

    #[test]
    fn last_index_round_trips() {
        let r = compile_simple("a", "g").unwrap();
        assert_eq!(r.last_index(), 0);
        r.set_last_index(7);
        assert_eq!(r.last_index(), 7);
        // Cloning shares the cell.
        let r2 = r.clone();
        r2.set_last_index(11);
        assert_eq!(r.last_index(), 11);
    }
}

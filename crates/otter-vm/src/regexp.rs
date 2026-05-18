//! JavaScript `RegExp` value, backed by the `regress` engine.
//!
//! `JsRegExp` is a GC-managed handle. The body owns the compiled
//! `regress::Regex`, original pattern, parsed flags, and `lastIndex`.
//! The regex engine is a GC leaf: `regress` owns external allocations
//! that are not traced and are not counted against the Otter heap cap.
//!
//! `regress` does not implement the JavaScript `g` (global) or `y`
//! (sticky) flags — those are stateful and live above the engine
//! per spec. We model both flags here through [`JsRegExp::flag_global`]
//! / [`JsRegExp::flag_sticky`] and the [`JsRegExp::last_index`] cell;
//! method implementations consult these during pattern execution.
//!
//! # Contents
//! - [`JsRegExp`] — the cheap-to-clone handle used in [`crate::Value`].
//! - [`JsRegExpBody`] — GC-managed compiled regex payload.
//! - [`RegExpFlags`] — parsed flag bits.
//! - [`compile`] — pattern + flag-string → engine, surfaced as
//!   [`RegExpError`] on failure.
//!
//! # Invariants
//! - The flag string is restricted to the ASCII subset `"dgimsuvy"` —
//!   the compiler validates this at intern time.
//! - `last_index` is interior-mutable but never stashed across
//!   reentrant calls; native methods refresh it before returning.
//! - Every operation that reads or mutates the body receives an
//!   explicit [`otter_gc::GcHeap`].
//! - Cloning a [`JsRegExp`] shares the compiled engine and the
//!   `last_index` cell through the same GC body.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp-regular-expression-objects>

use std::cell::RefCell;

use otter_gc::raw::{RawGc, SlotVisitor};
use regress::{ExecConfig, Flags, Regex};

/// ReDoS guard for every `regress` execution. Cuts pathological
/// backtracking patterns (`(a+)+b` against long inputs, nested
/// alternation explosions) at a fixed step budget. A budget of
/// `10_000_000` matches the value documented in `regress` and cuts
/// runaway inputs within a few milliseconds while leaving realistic
/// patterns untouched.
///
/// # See also
/// - <https://en.wikipedia.org/wiki/ReDoS>
/// - [`regress::ExecConfig`]
pub const REGEX_BACKTRACK_BUDGET: u64 = 10_000_000;

use crate::Value;
use crate::number::NumberValue;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsRegExpBody`].
pub const REGEXP_BODY_TYPE_TAG: u8 = 0x1e;

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
    /// Flag string contained a character outside `"dgimsuvy"`.
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
    /// The GC heap refused the regex body allocation.
    #[error("regular expression allocation failed")]
    OutOfMemory,
}

impl From<otter_gc::OutOfMemory> for RegExpError {
    fn from(_: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory
    }
}

/// Foundation flag bits.
///
/// We keep this tiny so `JsRegExp` stays cheap to clone. The struct
/// is `Copy`; the bool fields map to the JS-visible accessors.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-get-regexp.prototype.flags>
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegExpFlags {
    /// `d` — hasIndices. Adds `result.indices` on `exec` matches.
    pub has_indices: bool,
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
    /// `v` — unicode-sets. ES2024 set-notation character classes
    /// and string properties; mutually exclusive with `u`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-patterns-static-semantics-early-errors>
    pub unicode_sets: bool,
}

impl RegExpFlags {
    /// Parse the canonical ASCII flag string. Order does not matter;
    /// duplicate flags raise [`RegExpError::DuplicateFlag`].
    ///
    /// Supported flags: `d g i m s u v y`. Per §22.2.4
    /// [`RegExp Pattern Flags`](https://tc39.es/ecma262/#sec-patterns-static-semantics-early-errors)
    /// `u` and `v` are mutually exclusive; combining them raises a
    /// SyntaxError at construction time.
    pub fn parse(flags: &str) -> Result<Self, RegExpError> {
        let mut out = Self::default();
        for c in flags.chars() {
            let slot = match c {
                'd' => &mut out.has_indices,
                'g' => &mut out.global,
                'i' => &mut out.ignore_case,
                'm' => &mut out.multiline,
                's' => &mut out.dot_all,
                'u' => &mut out.unicode,
                'v' => &mut out.unicode_sets,
                'y' => &mut out.sticky,
                other => return Err(RegExpError::InvalidFlag { flag: other }),
            };
            if *slot {
                return Err(RegExpError::DuplicateFlag { flag: c });
            }
            *slot = true;
        }
        if out.unicode && out.unicode_sets {
            return Err(RegExpError::InvalidFlag { flag: 'v' });
        }
        Ok(out)
    }

    /// Render as the canonical JS spelling (`dgimsuvy` order — see
    /// §22.2.6.4 [`get RegExp.prototype.flags`](https://tc39.es/ecma262/#sec-get-regexp.prototype.flags)).
    #[must_use]
    pub fn to_js_string(self) -> String {
        let mut s = String::with_capacity(7);
        if self.has_indices {
            s.push('d');
        }
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
        if self.unicode_sets {
            s.push('v');
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
    /// `RegExp.prototype.lastIndex` — a writable own data property.
    /// RegExp execution coerces it numerically, but ordinary JS
    /// reads/writes observe the exact stored value.
    pub last_index: RefCell<Value>,
    /// Lazy expando bag for non-spec own properties — user
    /// installations such as `re.global = false` /
    /// `re.exec = fn` per the §22.2.6 spec-observable hook tests.
    /// Mutated through `heap.with_payload` like every other GC
    /// body field; never wrap in `Cell`.
    pub expando: Option<crate::object::JsObject>,
}

impl otter_gc::SafeTraceable for JsRegExpBody {
    const TYPE_TAG: u8 = REGEXP_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        self.last_index.borrow().trace_value_slots(visitor);
        if let Some(expando) = &self.expando {
            Value::Object(*expando).trace_value_slots(visitor);
        }
    }
}

/// Cheap-to-clone JS regex handle.
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct JsRegExp {
    inner: otter_gc::Gc<JsRegExpBody>,
}

impl JsRegExp {
    /// Compile a pattern + flag string into a runtime regex value.
    pub fn compile(
        heap: &mut otter_gc::GcHeap,
        pattern_utf16: &[u16],
        flag_str: &str,
    ) -> Result<Self, RegExpError> {
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
            unicode_sets: flags.unicode_sets,
            ..Default::default()
        };
        // `g` and `y` are spec-level state that lives above the
        // matcher; `regress` would silently ignore them anyway.
        let regex =
            Regex::with_flags(&source, engine_flags).map_err(|e| RegExpError::InvalidPattern {
                message: format!("{e}"),
            })?;
        Ok(Self {
            inner: heap.alloc_old(JsRegExpBody {
                regex,
                pattern_utf16: pattern_utf16.to_vec(),
                source,
                flags,
                last_index: RefCell::new(Value::Number(NumberValue::from_i32(0))),
                expando: None,
            })?,
        })
    }

    /// Re-initialize this regex with a new pattern and flags in
    /// place. Mirrors the `RegExpInitialize` abstract operation that
    /// `RegExp.prototype.compile` (§B.2.4.1) routes through. Resets
    /// `lastIndex` to 0 per step 12 of `RegExpInitialize` (§22.2.3.2).
    /// <https://tc39.es/ecma262/#sec-regexpinitialize>
    pub fn reinitialize(
        &self,
        heap: &mut otter_gc::GcHeap,
        pattern_utf16: &[u16],
        flag_str: &str,
    ) -> Result<(), RegExpError> {
        let flags = RegExpFlags::parse(flag_str)?;
        let source = String::from_utf16_lossy(pattern_utf16);
        let engine_flags = Flags {
            icase: flags.ignore_case,
            multiline: flags.multiline,
            dot_all: flags.dot_all,
            unicode: flags.unicode,
            unicode_sets: flags.unicode_sets,
            ..Default::default()
        };
        let regex =
            Regex::with_flags(&source, engine_flags).map_err(|e| RegExpError::InvalidPattern {
                message: format!("{e}"),
            })?;
        let pattern_units = pattern_utf16.to_vec();
        heap.with_payload(self.inner, |body| {
            body.regex = regex;
            body.pattern_utf16 = pattern_units;
            body.source = source;
            body.flags = flags;
            *body.last_index.borrow_mut() = Value::Number(NumberValue::from_i32(0));
        });
        Ok(())
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Run the compiled engine from a UTF-16 offset and collect
    /// owned matches. Bounded by [`REGEX_BACKTRACK_BUDGET`] so
    /// pathological ReDoS patterns abort with no matches instead of
    /// stalling the interpreter. Step-limit aborts surface as an
    /// empty match list — the spec-visible behaviour matches
    /// "no match at this position" while letting the caller move on.
    #[must_use]
    pub fn find_from_utf16(
        &self,
        heap: &otter_gc::GcHeap,
        text_units: &[u16],
        start: usize,
    ) -> Vec<regress::Match> {
        let config = ExecConfig {
            backtrack_limit: Some(REGEX_BACKTRACK_BUDGET),
        };
        heap.read_payload(self.inner, |body| {
            body.regex
                .find_from_utf16_with_config(text_units, start, config)
                .map_while(Result::ok)
                .collect()
        })
    }

    /// Parsed flag bits.
    #[must_use]
    /// Read the lazy expando bag, if any user installations
    /// (`re.global = false`, `re.exec = fn`) created one.
    #[must_use]
    pub fn expando(&self, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
        heap.read_payload(self.inner, |body| body.expando)
    }

    /// Install / replace the lazy expando bag.
    pub fn set_expando(&self, heap: &mut otter_gc::GcHeap, expando: crate::object::JsObject) {
        let barrier = Value::Object(expando);
        let _ = heap.with_payload(self.inner, |body| {
            body.expando = Some(expando);
        });
        heap.record_write(self.inner, &barrier);
    }

    /// Parsed flag bits.
    #[must_use]
    pub fn flags(&self, heap: &otter_gc::GcHeap) -> RegExpFlags {
        heap.read_payload(self.inner, |body| body.flags)
    }

    /// `RegExp.prototype.source` view (UTF-8). Note this is lossy
    /// for surrogate-bearing patterns; the canonical body is
    /// [`Self::pattern_utf16`].
    #[must_use]
    pub fn source(&self, heap: &otter_gc::GcHeap) -> String {
        heap.read_payload(self.inner, |body| body.source.clone())
    }

    /// Original WTF-16 pattern body.
    #[must_use]
    pub fn pattern_utf16(&self, heap: &otter_gc::GcHeap) -> Vec<u16> {
        heap.read_payload(self.inner, |body| body.pattern_utf16.clone())
    }

    /// Read `lastIndex`.
    #[must_use]
    pub fn last_index(&self, heap: &otter_gc::GcHeap) -> u32 {
        heap.read_payload(self.inner, |body| {
            last_index_to_u32(&body.last_index.borrow())
        })
    }

    /// Read the JS-visible `lastIndex` data-property value.
    #[must_use]
    pub fn last_index_value(&self, heap: &otter_gc::GcHeap) -> Value {
        heap.read_payload(self.inner, |body| body.last_index.borrow().clone())
    }

    /// Update `lastIndex`. Pattern-arg methods use this to step
    /// through successive `g` / `y` matches.
    pub fn set_last_index(&self, heap: &otter_gc::GcHeap, value: u32) {
        heap.read_payload(self.inner, |body| {
            *body.last_index.borrow_mut() = Value::Number(NumberValue::from_f64(value as f64));
        });
    }

    /// Store the JS-visible `lastIndex` data-property value.
    pub fn set_last_index_value(&self, heap: &mut otter_gc::GcHeap, value: Value) {
        let barrier_value = value.clone();
        heap.read_payload(self.inner, |body| {
            *body.last_index.borrow_mut() = value;
        });
        heap.record_write(self.inner, &barrier_value);
    }

    /// Identity comparison — two handles are equal iff they share
    /// the same body. `RegExp` is a reference type in JS so `===`
    /// follows handle identity.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Raw `Rc`-data pointer for use as a hash / map key in
    /// identity-keyed collections (`WeakMap` / `WeakSet`). Anchor
    /// the originating handle for the lifetime of the pointer.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = self as *const JsRegExp as *mut RawGc;
        visitor(p);
    }
}

fn last_index_to_u32(value: &Value) -> u32 {
    let raw = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .to_lossy_string()
            .trim()
            .parse::<f64>()
            .unwrap_or(f64::NAN),
        Value::Boolean(true) => 1.0,
        Value::Boolean(false) | Value::Null => 0.0,
        _ => f64::NAN,
    };
    if raw.is_nan() || raw <= 0.0 {
        0
    } else if raw > u32::MAX as f64 {
        u32::MAX
    } else {
        raw.trunc() as u32
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
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let units: Vec<u16> = pattern.encode_utf16().collect();
        JsRegExp::compile(&mut heap, &units, flags)
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
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let units: Vec<u16> = "ab+c".encode_utf16().collect();
        let r = JsRegExp::compile(&mut heap, &units, "i").unwrap();
        assert_eq!(r.source(&heap), "ab+c");
        assert!(r.flags(&heap).ignore_case);
        let utf16: Vec<u16> = "abbbcXabbbbc".encode_utf16().collect();
        let m = r
            .find_from_utf16(&heap, &utf16, 0)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(m.range, 0..5);
    }

    #[test]
    fn compile_rejects_bad_pattern() {
        let err = compile_simple("[", "").unwrap_err();
        assert!(matches!(err, RegExpError::InvalidPattern { .. }));
    }

    #[test]
    fn last_index_round_trips() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let units: Vec<u16> = "a".encode_utf16().collect();
        let r = JsRegExp::compile(&mut heap, &units, "g").unwrap();
        assert_eq!(r.last_index(&heap), 0);
        r.set_last_index(&heap, 7);
        assert_eq!(r.last_index(&heap), 7);
        // Cloning shares the cell.
        let r2 = r;
        r2.set_last_index(&heap, 11);
        assert_eq!(r.last_index(&heap), 11);
    }
}

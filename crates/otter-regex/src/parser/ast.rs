//! The parsed-pattern syntax tree (the engine's IR node type).
//!
//! A direct, allocation-light encoding of the ECMAScript Pattern grammar
//! (§22.2.1): alternation, concatenation, quantifiers, character classes,
//! capturing/non-capturing groups, assertions (anchors, word boundaries,
//! lookaround), and backreferences.
//!
//! # Contents
//! - [`Node`] — a pattern AST node.
//! - [`Quantifier`] — `{min,max}` greediness for a repeated subpattern.
//! - [`Assertion`] — zero-width assertions.
//! - [`GroupKind`] — capturing / non-capturing / lookaround group flavour.
//!
//! # Invariants
//! - Capture-group ids are assigned in source order, 1-based.
//! - A named group's name is non-empty; duplicate names across alternatives are
//!   permitted (ES2025) and share one capture slot per name.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-patterns> (§22.2.1 grammar)

use crate::classes::ClassSet;

/// A `{min,max}` repetition with a greediness flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quantifier {
    /// Minimum repetitions (inclusive).
    pub(crate) min: u32,
    /// Maximum repetitions (inclusive), or `None` for unbounded (`*`, `+`, `{n,}`).
    pub(crate) max: Option<u32>,
    /// Greedy (`true`) tries the longest match first; lazy (`false`) the shortest.
    pub(crate) greedy: bool,
}

/// A zero-width assertion.
#[derive(Debug, Clone)]
pub(crate) enum Assertion {
    /// `^` — start of input, or of a line when `m` is in effect at this
    /// position (the flag is captured per node so an inline
    /// `(?m:...)` / `(?-m:...)` modifier scopes it).
    StartOfLine {
        /// `true` when the `m` (multiline) flag is in effect here.
        multiline: bool,
    },
    /// `$` — end of input, or of a line when `m` is in effect here.
    EndOfLine {
        /// `true` when the `m` (multiline) flag is in effect here.
        multiline: bool,
    },
    /// `\b` / `\B` — word boundary (`invert` for `\B`).
    WordBoundary {
        /// `true` for `\B` (non-boundary).
        invert: bool,
        /// `true` when `i` is in effect here: under `u` the word-character
        /// set then also admits the two non-ASCII code points that
        /// case-fold into it (§22.2.2.7.3 — U+017F `ſ`→`s`, U+212A K→`k`).
        ignore_case: bool,
    },
}

/// The flavour of a parenthesised group.
#[derive(Debug, Clone)]
pub(crate) enum GroupKind {
    /// `(...)` — a capturing group with its 1-based id and optional name.
    Capturing {
        /// 1-based capture index.
        index: u32,
        /// Group name, if `(?<name>...)`.
        name: Option<String>,
    },
    /// `(?:...)` — non-capturing.
    NonCapturing,
    /// `(?=...)` / `(?!...)` — lookahead (`negate` for `(?!`).
    Lookahead {
        /// `true` for negative lookahead.
        negate: bool,
    },
    /// `(?<=...)` / `(?<!...)` — lookbehind (`negate` for `(?<!`).
    Lookbehind {
        /// `true` for negative lookbehind.
        negate: bool,
    },
}

/// A pattern AST node.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    /// Matches the empty string.
    Empty,
    /// A single literal code point. `ignore_case` records whether the `i`
    /// flag is in effect at this node (captured per node so an inline
    /// `(?i:...)` / `(?-i:...)` modifier scopes the case-insensitive
    /// comparison).
    Char {
        /// The literal code point.
        cp: u32,
        /// `true` when the `i` (ignoreCase) flag is in effect here.
        ignore_case: bool,
    },
    /// `.` — any character (any except line terminators unless `s` is set).
    AnyChar {
        /// `true` when the `s` (dotAll) flag is in effect.
        dot_all: bool,
    },
    /// A character class `[...]` (or a `\p{}` / `\d`-style escape), possibly
    /// negated, possibly carrying `v`-mode string alternatives.
    Class {
        /// The resolved class set.
        set: ClassSet,
        /// `true` for a negated class `[^...]`.
        negate: bool,
        /// `true` when the `i` (ignoreCase) flag is in effect here.
        ignore_case: bool,
    },
    /// A zero-width assertion.
    Assert(Assertion),
    /// `\1` / `\k<name>` — a backreference to a (possibly named) capture group.
    BackRef {
        /// 1-based candidate group indices this resolves to. Numeric
        /// backreferences have one entry; duplicate named groups may have more.
        indices: Vec<u32>,
        /// `true` when the `i` (ignoreCase) flag is in effect here.
        ignore_case: bool,
    },
    /// Sequential concatenation of subnodes.
    Concat(Vec<Node>),
    /// Alternation `a|b|c`.
    Alternate(Vec<Node>),
    /// A repeated subnode.
    Repeat {
        /// The repeated subpattern.
        node: Box<Node>,
        /// The repetition bounds and greediness.
        quant: Quantifier,
    },
    /// A parenthesised group.
    Group {
        /// The group flavour.
        kind: GroupKind,
        /// The group body.
        body: Box<Node>,
    },
}

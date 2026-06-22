//! Compiled instruction program — the matcher bytecode.
//!
//! A flat, cache-friendly instruction vector produced by [`crate::ir`] lowering
//! and consumed by the backtracking executor. Keeping the program flat (indices,
//! not pointers) makes it cheap to clone, share, and iterate, and lets lookaround
//! bodies live in the same vector as self-contained regions reached only via a
//! [`Insn::Look`] sub-search.
//!
//! # Contents
//! - [`Program`] — the instruction vector plus capture/loop metadata and the
//!   engine-relevant flag bits.
//! - [`Insn`] — a single matcher instruction.
//!
//! # Invariants
//! - Operands that reference other instructions are indices into the same
//!   [`Program::insns`] vector.
//! - Capture slots number `2 * (group_count + 1)`: slots `2*g` / `2*g+1` hold
//!   the start / end of group `g`, with group `0` the overall match.
//! - A lookaround body is a contiguous region beginning at [`Insn::Look`]'s
//!   `entry` and terminated by [`Insn::LookMatch`]; normal control flow never
//!   falls into it (a [`Insn::Jump`] hops over it).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (§22.2.2)

use crate::classes::{ClassSet, CodePointSet};

/// A single matcher instruction.
#[derive(Debug, Clone)]
pub(crate) enum Insn {
    /// Match one literal code point; case-folded comparison when
    /// `ignore_case` (the per-node effective `i` flag) is set.
    Char {
        /// The literal code point.
        cp: u32,
        /// `true` for a case-insensitive comparison.
        ignore_case: bool,
    },
    /// Match one code point against a class set; `negate` inverts membership.
    Class {
        /// The class set tested against the current code point.
        set: ClassSet,
        /// `true` for a negated class `[^...]`.
        negate: bool,
        /// `true` for case-insensitive class membership.
        ignore_case: bool,
    },
    /// Match any character (line terminators excluded unless `dot_all`).
    AnyChar {
        /// Whether the `s` (dotAll) flag is in effect.
        dot_all: bool,
    },
    /// Unconditional jump to an instruction index.
    Jump(usize),
    /// Try the first target; on backtrack, resume at the second.
    Split(usize, usize),
    /// Record the current position into capture slot `index`.
    Save(usize),
    /// Clear the start/end slots for capture group `index` before a repeated
    /// body tries a fresh iteration.
    ClearCapture(u32),
    /// Store the current position into loop-mark slot `index`, marking the start
    /// of an unbounded-quantifier iteration.
    SetMark(usize),
    /// Fail this path if the position equals loop-mark slot `index`: the loop
    /// body matched the empty string, so re-iterating cannot progress
    /// (§22.2.2.5.1 prevents the infinite loop).
    CheckProgress(usize),
    /// Match the text previously captured by one of `indices` (backreference).
    BackRef {
        /// 1-based group indices this resolves to. Duplicate named
        /// backreferences try the capture that participated; if none did, the
        /// backreference matches the empty string.
        indices: Vec<u32>,
        /// `true` for a case-insensitive comparison.
        ignore_case: bool,
    },
    /// `^` — start of input, or of a line when `multiline` is set.
    AssertStart {
        /// `true` when the `m` (multiline) flag is in effect here.
        multiline: bool,
    },
    /// `$` — end of input, or of a line when `multiline` is set.
    AssertEnd {
        /// `true` when the `m` (multiline) flag is in effect here.
        multiline: bool,
    },
    /// `\b` / `\B` — word boundary; `true` inverts (`\B`).
    WordBoundary(bool),
    /// Lookaround: run a sub-search of the body at `entry`. `behind` selects
    /// lookbehind, `negate` selects the negative form.
    Look {
        /// `true` for a negative assertion (`(?!`, `(?<!`).
        negate: bool,
        /// `true` for lookbehind (`(?<=`, `(?<!`).
        behind: bool,
        /// First instruction of the lookaround body (terminated by `LookMatch`).
        entry: usize,
    },
    /// Accepting terminator of a lookaround body.
    LookMatch,
    /// Accepting terminator of the whole pattern.
    Match,
}

/// A compiled program ready for execution.
#[derive(Debug, Clone)]
pub(crate) struct Program {
    /// The flat instruction vector; execution begins at index `0`.
    pub(crate) insns: Vec<Insn>,
    /// Number of capturing groups (group 0 excluded).
    pub(crate) group_count: u32,
    /// Capture-group names in source order; `None` for unnamed groups. Index `i`
    /// names group `i + 1`.
    pub(crate) group_names: Vec<Option<String>>,
    /// `u`/`v` — code-point (surrogate-pair-aware) traversal.
    pub(crate) unicode: bool,
    /// Number of loop-mark slots (one per unbounded quantifier), allocated after
    /// the capture slots.
    pub(crate) loop_marks: usize,
    /// Scan prefilter for the leftmost search: the set of code points that can
    /// begin a match, when the pattern starts with a single literal or
    /// non-negated class (including a leading alternation of such). Positions
    /// whose code point is not a member cannot start a match, so the executor
    /// skips them without running. `None` when no such prefilter applies
    /// (anchored, empty-matching, or an uncharacterizable leading instruction).
    pub(crate) prefilter: Option<Prefilter>,
}

/// A scan prefilter: the set of code points that can begin a match, in a form
/// the leftmost search dispatches on cheaply.
///
/// Replaces the per-position binary search over code-point ranges with an O(1)
/// table lookup for code units below `TABLE`, and offers a single-literal fast
/// path that the leftmost search turns into a vectorizable equality scan.
#[derive(Debug, Clone)]
pub(crate) struct Prefilter {
    /// Membership for code units `0..TABLE` (covers ASCII and Latin-1).
    table: [bool; Self::TABLE],
    /// Whether any member code point is `>= TABLE`; when `false`, a code unit
    /// at or above the table can never start a match.
    has_high: bool,
    /// The full set, consulted only for code points `>= TABLE` when `has_high`.
    high: CodePointSet,
    /// `Some(u)` when the set is exactly one BMP, non-surrogate code point: the
    /// scan reduces to a single-unit equality search. Always `None` when
    /// [`Self::canon`] is set (the input must be canonicalized first).
    single: Option<u16>,
    /// Case-folding mode for `i`-flag patterns: `Some(true)` folds the input by
    /// the unicode rule, `Some(false)` by the non-unicode rule, before the
    /// membership test — the stored set already holds canonicalized members, so
    /// this mirrors `char_eq` exactly. `None` for case-sensitive prefilters.
    canon: Option<bool>,
}

impl Prefilter {
    const TABLE: usize = 256;

    /// Build a case-sensitive prefilter from a first-set. Cheap; runs once at
    /// lowering.
    #[must_use]
    pub(crate) fn from_set(set: &CodePointSet) -> Self {
        Self::build(set, None)
    }

    /// Build a case-insensitive prefilter: `set` holds the canonicalized member
    /// code points and the scan canonicalizes each input code point by the
    /// `unicode` rule before testing membership.
    #[must_use]
    pub(crate) fn from_set_canon(set: &CodePointSet, unicode: bool) -> Self {
        Self::build(set, Some(unicode))
    }

    fn build(set: &CodePointSet, canon: Option<bool>) -> Self {
        let mut table = [false; Self::TABLE];
        let mut has_high = false;
        for r in set.ranges() {
            let hi = *r.end();
            for cp in *r.start()..=hi.min(Self::TABLE as u32 - 1) {
                table[cp as usize] = true;
            }
            if hi >= Self::TABLE as u32 {
                has_high = true;
            }
        }
        let single = match (canon, set.ranges()) {
            (None, [r])
                if r.start() == r.end()
                    && *r.start() < 0x1_0000
                    && !(0xD800..=0xDFFF).contains(r.start()) =>
            {
                Some(*r.start() as u16)
            }
            _ => None,
        };
        Self {
            table,
            has_high,
            high: set.clone(),
            single,
            canon,
        }
    }

    /// The single-literal code unit, when the prefilter is one BMP literal.
    #[must_use]
    pub(crate) fn single(&self) -> Option<u16> {
        self.single
    }

    /// Whether decoded code point `cp` can begin a match. Canonicalizes first
    /// for an `i`-flag prefilter. O(1) for the common BMP range; a set test only
    /// for high code points when the set has any.
    #[inline]
    #[must_use]
    pub(crate) fn cp_may_start(&self, cp: u32) -> bool {
        let cp = match self.canon {
            Some(true) => crate::casefold::fold_unicode(cp),
            Some(false) => crate::casefold::canonicalize(cp),
            None => cp,
        };
        if (cp as usize) < Self::TABLE {
            self.table[cp as usize]
        } else {
            self.has_high && self.high.contains(cp)
        }
    }
}

impl Program {
    /// Number of slots: capture slots `2 * (group_count + 1)` plus one per
    /// unbounded-quantifier progress mark.
    #[must_use]
    pub(crate) fn slot_count(&self) -> usize {
        2 * (self.group_count as usize + 1) + self.loop_marks
    }
}

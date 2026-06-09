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
    /// Store the current position into loop-mark slot `index`, marking the start
    /// of an unbounded-quantifier iteration.
    SetMark(usize),
    /// Fail this path if the position equals loop-mark slot `index`: the loop
    /// body matched the empty string, so re-iterating cannot progress
    /// (§22.2.2.5.1 prevents the infinite loop).
    CheckProgress(usize),
    /// Match the text previously captured by group `index` (backreference).
    BackRef {
        /// 1-based group index this resolves to.
        index: u32,
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
    /// `true` if any instruction is a backreference (engine-selection input).
    pub(crate) has_backref: bool,
    /// `m` — multiline anchors.
    pub(crate) multiline: bool,
    /// `i` — case-insensitive matching.
    pub(crate) ignore_case: bool,
    /// `u`/`v` — code-point (surrogate-pair-aware) traversal.
    pub(crate) unicode: bool,
    /// Number of loop-mark slots (one per unbounded quantifier), allocated after
    /// the capture slots.
    pub(crate) loop_marks: usize,
    /// Code points that can begin a match, when the pattern starts with a single
    /// literal or non-negated class. Used as a scan prefilter: positions whose
    /// code point is not a member cannot start a match, so the executor skips
    /// them without running. `None` when no such prefilter applies (anchored,
    /// empty-matching, alternation, or case-insensitive starts).
    pub(crate) first_set: Option<CodePointSet>,
}

impl Program {
    /// Number of slots: capture slots `2 * (group_count + 1)` plus one per
    /// unbounded-quantifier progress mark.
    #[must_use]
    pub(crate) fn slot_count(&self) -> usize {
        2 * (self.group_count as usize + 1) + self.loop_marks
    }
}

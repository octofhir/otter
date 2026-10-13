//! Target-independent planning shared by the native emitters.
//!
//! # Contents
//! - [`Pred`] / [`High`] — a unit test as a low table plus a rule for the
//!   units the table does not cover.
//! - [`char_pred`], [`class_pred`], [`start_pred`], [`word_table`] — the unit
//!   tests of literals, classes, the start prefilter and `\b`.
//! - [`fixed_width`] / [`targets`] — what runs of instructions an emitter may
//!   fuse.
//!
//! # Invariants
//! - Every table entry is the interpreter's own predicate evaluated on that
//!   unit, and every [`High`] rule agrees with it on every unit from `0x100`
//!   to `0xFFFF`.
//!
//! # See also
//! - [`super::backtrack`](crate::exec::backtrack) — the predicates mirrored.

use super::HighPred;
use crate::exec::backtrack::{char_eq, class_member, is_word};
use crate::program::{Insn, Prefilter, Program};

/// A unit test: membership of units below `0x100` by table, of the rest by
/// `high` (consulted only for two-byte subjects).
pub(super) struct Pred {
    pub(super) table: [u8; 256],
    pub(super) high: High,
}

/// Membership of a code unit at or above `0x100`.
pub(super) enum High {
    Never,
    Always,
    /// Within one of the inclusive ranges, inverted when `negate`.
    Ranges {
        ranges: Vec<(u32, u32)>,
        negate: bool,
    },
    /// Decided by [`super::high_member`].
    Call(HighPred),
}

/// A membership table over the units below `0x100`.
pub(super) fn table(member: impl Fn(u32) -> bool) -> [u8; 256] {
    core::array::from_fn(|unit| u8::from(member(unit as u32)))
}

/// The test of a literal, case-folded under `ignore_case`.
pub(super) fn char_pred(cp: u32, ignore_case: bool) -> Pred {
    let table = table(|unit| char_eq(cp, unit, ignore_case, false));
    let high = if !ignore_case {
        if cp > 0xFF {
            High::Ranges {
                ranges: vec![(cp, cp)],
                negate: false,
            }
        } else {
            High::Never
        }
    } else if cp < 0x80 {
        // Canonicalization never maps a non-ASCII unit onto an ASCII one.
        High::Never
    } else {
        High::Call(HighPred::Char { cp, ignore_case })
    };
    Pred { table, high }
}

/// The test of class `class` of `program`.
pub(super) fn class_pred(program: &Program, class: u32, negate: bool, ignore_case: bool) -> Pred {
    let set = &program.classes[class as usize];
    let table = table(|unit| class_member(set, negate, unit, ignore_case, false));
    let call = High::Call(HighPred::Class {
        class,
        negate,
        ignore_case,
    });
    if ignore_case {
        return Pred { table, high: call };
    }
    let ranges: Vec<(u32, u32)> = set
        .code_points
        .ranges()
        .iter()
        .map(|range| ((*range.start()).max(0x100), (*range.end()).min(0xFFFF)))
        .filter(|(lo, hi)| lo <= hi)
        .collect();
    let high = match ranges.as_slice() {
        [] if negate => High::Always,
        [] => High::Never,
        [(0x100, 0xFFFF)] if negate => High::Never,
        [(0x100, 0xFFFF)] => High::Always,
        _ if ranges.len() <= 4 => High::Ranges { ranges, negate },
        _ => call,
    };
    Pred { table, high }
}

/// The test of the units that can begin a match.
pub(super) fn start_pred(prefilter: &Prefilter) -> Pred {
    Pred {
        table: table(|unit| prefilter.cp_may_start(unit)),
        high: if prefilter.may_start_above_latin1() {
            High::Call(HighPred::Start)
        } else {
            High::Never
        },
    }
}

/// Word characters among the units below `0x100`; no unit above is one in
/// code-unit mode.
pub(super) fn word_table() -> [u8; 256] {
    table(is_word)
}

/// Width in code units of a fixed-width consuming instruction.
pub(super) fn fixed_width(insn: &Insn) -> Option<usize> {
    match insn {
        Insn::Char { .. } | Insn::Class { .. } | Insn::AnyChar { .. } => Some(1),
        Insn::CharSeq(seq) => Some(seq.len()),
        _ => None,
    }
}

/// Instructions some branch or resume reaches other than by falling through.
pub(super) fn targets(program: &Program) -> Vec<bool> {
    let mut targets = vec![false; program.insns.len()];
    targets[0] = true;
    for (pc, insn) in program.insns.iter().enumerate() {
        match insn {
            Insn::Jump(target) => targets[*target] = true,
            Insn::Split(first, second) => {
                targets[*first] = true;
                targets[*second] = true;
            }
            Insn::Repeat { .. } => targets[pc + 1] = true,
            _ => {}
        }
    }
    targets
}

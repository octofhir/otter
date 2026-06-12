//! Bounded backtracking executor — the primary matcher backend.
//!
//! This backend implements every ECMAScript matching feature exactly: capturing
//! groups, backreferences, lookahead, lookbehind, and precise greedy/lazy
//! quantifier priority. Backtracking uses an **explicit stack** (not native
//! recursion) so a long input under a quantifier loop cannot overflow the Rust
//! stack; native recursion is used only to evaluate a lookaround body, whose
//! depth is bounded by pattern nesting. A step budget
//! ([`crate::ExecConfig::step_limit`]) bounds worst-case time so a
//! catastrophic-backtracking input aborts instead of hanging.
//!
//! # Contents
//! - [`attempt`] — try to match a program anchored at one start offset.
//!
//! # Invariants
//! - Greedy quantifiers explore the longer match first; lazy the shorter.
//! - Every instruction dispatch counts one step against the budget.
//! - Reported positions are UTF-16 code-unit offsets.
//!
//! # Phase-1 note
//! Lookbehind is evaluated by scanning candidate start positions and requiring
//! the body to end exactly at the assertion point. This is boolean-correct and
//! variable-length-capable; spec-exact right-to-left capture selection inside a
//! lookbehind is a later-phase refinement (reverse-compiled bodies).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching>

use super::ExecConfig;
use crate::casefold::{ascii_other_case, canonicalize, fold_unicode};
use crate::classes::ClassSet;
use crate::cursor::Input;
use crate::error::ExecError;
use crate::program::{Insn, Program};

/// Capture slots: `Some(pos)` once written, `None` if the group is unset.
type Caps = Vec<Option<usize>>;

/// Try to match `program` against `input` beginning exactly at code-unit offset
/// `at`. Returns the filled capture slots on success.
pub(crate) fn attempt(
    program: &Program,
    input: &Input<'_>,
    at: usize,
    config: ExecConfig,
) -> Result<Option<Caps>, ExecError> {
    let mut m = Matcher {
        program,
        input,
        steps: 0,
        step_limit: config.step_limit,
    };
    let caps = vec![None; program.slot_count()];
    match m.run(0, at, None, caps)? {
        Some((_, caps)) => Ok(Some(caps)),
        None => Ok(None),
    }
}

struct Matcher<'p, 't> {
    program: &'p Program,
    input: &'p Input<'t>,
    steps: u64,
    step_limit: Option<u64>,
}

/// A pending alternative on the backtrack stack.
struct Frame {
    pc: usize,
    pos: usize,
    caps: Caps,
}

/// The decoded action of one instruction, computed while the instruction is
/// borrowed so the borrow can end before any `&mut self` work (lookaround).
enum Act {
    Accept,
    Backtrack,
    Goto(usize),
    Consume(usize, usize),
    Split(usize, usize),
    Save(usize),
    ClearCapture(u32),
    CheckProgress(usize),
    Look(bool, bool, usize),
}

impl Matcher<'_, '_> {
    /// Run from `entry` at `start`. `end_anchor`, when set, only accepts a
    /// terminator reached exactly at that offset (used by lookbehind). Returns
    /// the accepting position and capture slots.
    fn run(
        &mut self,
        entry: usize,
        start: usize,
        end_anchor: Option<usize>,
        caps0: Caps,
    ) -> Result<Option<(usize, Caps)>, ExecError> {
        let mut stack = vec![Frame {
            pc: entry,
            pos: start,
            caps: caps0,
        }];

        while let Some(frame) = stack.pop() {
            let (mut pc, mut pos, mut caps) = (frame.pc, frame.pos, frame.caps);
            let accepted = loop {
                self.steps += 1;
                if let Some(limit) = self.step_limit
                    && self.steps > limit
                {
                    return Err(ExecError::StepLimitExceeded);
                }

                let act = {
                    let insn = &self.program.insns[pc];
                    match insn {
                        Insn::Match | Insn::LookMatch => {
                            if end_anchor.is_none_or(|t| pos == t) {
                                Act::Accept
                            } else {
                                Act::Backtrack
                            }
                        }
                        Insn::Char { cp: c, ignore_case } => match self.decode(pos) {
                            Some((cp, w)) if self.char_eq(*c, cp, *ignore_case) => {
                                Act::Consume(pc + 1, pos + w)
                            }
                            _ => Act::Backtrack,
                        },
                        Insn::AnyChar { dot_all } => match self.decode(pos) {
                            Some((cp, w)) if *dot_all || !is_line_terminator(cp) => {
                                Act::Consume(pc + 1, pos + w)
                            }
                            _ => Act::Backtrack,
                        },
                        Insn::Class {
                            set,
                            negate,
                            ignore_case,
                        } => match self.decode(pos) {
                            Some((cp, w)) if self.class_member(set, *negate, cp, *ignore_case) => {
                                Act::Consume(pc + 1, pos + w)
                            }
                            _ => Act::Backtrack,
                        },
                        Insn::Jump(t) => Act::Goto(*t),
                        Insn::Split(a, b) => Act::Split(*a, *b),
                        Insn::ClearCapture(index) => Act::ClearCapture(*index),
                        Insn::Save(slot) | Insn::SetMark(slot) => Act::Save(*slot),
                        Insn::CheckProgress(slot) => Act::CheckProgress(*slot),
                        Insn::AssertStart { multiline } => {
                            if self.at_start(pos, *multiline) {
                                Act::Goto(pc + 1)
                            } else {
                                Act::Backtrack
                            }
                        }
                        Insn::AssertEnd { multiline } => {
                            if self.at_end(pos, *multiline) {
                                Act::Goto(pc + 1)
                            } else {
                                Act::Backtrack
                            }
                        }
                        Insn::WordBoundary(invert) => {
                            if self.word_boundary(pos) != *invert {
                                Act::Goto(pc + 1)
                            } else {
                                Act::Backtrack
                            }
                        }
                        Insn::BackRef { index, ignore_case } => {
                            match self.match_backref(*index, pos, &caps, *ignore_case) {
                                Some(next) => Act::Consume(pc + 1, next),
                                None => Act::Backtrack,
                            }
                        }
                        Insn::Look {
                            negate,
                            behind,
                            entry,
                        } => Act::Look(*negate, *behind, *entry),
                    }
                };

                match act {
                    Act::Accept => break Some((pos, caps)),
                    Act::Backtrack => break None,
                    Act::Goto(t) => pc = t,
                    Act::Consume(next_pc, next_pos) => {
                        pc = next_pc;
                        pos = next_pos;
                    }
                    Act::Split(a, b) => {
                        stack.push(Frame {
                            pc: b,
                            pos,
                            caps: caps.clone(),
                        });
                        pc = a;
                    }
                    Act::Save(slot) => {
                        caps[slot] = Some(pos);
                        pc += 1;
                    }
                    Act::ClearCapture(index) => {
                        let g = index as usize;
                        caps[2 * g] = None;
                        caps[2 * g + 1] = None;
                        pc += 1;
                    }
                    Act::CheckProgress(slot) => {
                        if caps[slot] == Some(pos) {
                            break None;
                        }
                        pc += 1;
                    }
                    Act::Look(negate, behind, look_entry) => {
                        match self.eval_look(negate, behind, look_entry, pos, &caps)? {
                            Some(updated) => {
                                caps = updated;
                                pc += 1;
                            }
                            None => break None,
                        }
                    }
                }
            };

            if let Some(result) = accepted {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Evaluate a lookaround, returning the (possibly capture-updated) slots when
    /// the assertion holds.
    fn eval_look(
        &mut self,
        negate: bool,
        behind: bool,
        entry: usize,
        pos: usize,
        caps: &Caps,
    ) -> Result<Option<Caps>, ExecError> {
        let found = if behind {
            let mut hit = None;
            for s in 0..=pos {
                if let Some((_, updated)) = self.run(entry, s, Some(pos), caps.clone())? {
                    hit = Some(updated);
                    break;
                }
            }
            hit
        } else {
            self.run(entry, pos, None, caps.clone())?.map(|(_, c)| c)
        };

        Ok(match (negate, found) {
            (false, Some(updated)) => Some(updated),
            (false, None) => None,
            (true, Some(_)) => None,
            (true, None) => Some(caps.clone()),
        })
    }

    // --- Input helpers -------------------------------------------------------

    fn units(&self) -> &[u16] {
        self.input.units()
    }

    /// Decode the code point at `pos`, returning it and its code-unit width.
    fn decode(&self, pos: usize) -> Option<(u32, usize)> {
        let units = self.units();
        let hi = *units.get(pos)?;
        if self.input.is_code_point_mode()
            && (0xD800..=0xDBFF).contains(&hi)
            && let Some(&lo) = units.get(pos + 1)
            && (0xDC00..=0xDFFF).contains(&lo)
        {
            let cp = 0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
            return Some((cp, 2));
        }
        Some((u32::from(hi), 1))
    }

    /// The code point ending just before `pos` (for boundary / multiline tests).
    fn prev_codepoint(&self, pos: usize) -> Option<u32> {
        if pos == 0 {
            return None;
        }
        let units = self.units();
        let lo = units[pos - 1];
        if self.input.is_code_point_mode()
            && (0xDC00..=0xDFFF).contains(&lo)
            && pos >= 2
            && (0xD800..=0xDBFF).contains(&units[pos - 2])
        {
            let hi = units[pos - 2];
            let cp = 0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
            return Some(cp);
        }
        Some(u32::from(lo))
    }

    /// Case-fold canonical form for the current flags.
    fn canon(&self, cp: u32) -> u32 {
        if self.program.unicode {
            fold_unicode(cp)
        } else {
            canonicalize(cp)
        }
    }

    fn char_eq(&self, target: u32, cp: u32, ignore_case: bool) -> bool {
        cp == target || (ignore_case && self.canon(cp) == self.canon(target))
    }

    fn class_member(&self, set: &ClassSet, negate: bool, cp: u32, ignore_case: bool) -> bool {
        let mut inside = set.code_points.contains(cp);
        if !inside && ignore_case {
            inside = set.code_points.contains(ascii_other_case(cp));
            if !inside && self.program.unicode {
                inside = set.code_points.contains(fold_unicode(cp));
            }
        }
        inside != negate
    }

    fn at_start(&self, pos: usize, multiline: bool) -> bool {
        if pos == 0 {
            return true;
        }
        multiline && self.prev_codepoint(pos).is_some_and(is_line_terminator)
    }

    fn at_end(&self, pos: usize, multiline: bool) -> bool {
        if pos >= self.units().len() {
            return true;
        }
        multiline
            && self
                .decode(pos)
                .is_some_and(|(cp, _)| is_line_terminator(cp))
    }

    fn word_boundary(&self, pos: usize) -> bool {
        let before = self.prev_codepoint(pos).is_some_and(is_word);
        let after = self.decode(pos).is_some_and(|(cp, _)| is_word(cp));
        before != after
    }

    /// Match the text previously captured by `group`, returning the new
    /// position. An unset group matches the empty string (succeeds).
    fn match_backref(
        &self,
        group: u32,
        pos: usize,
        caps: &Caps,
        ignore_case: bool,
    ) -> Option<usize> {
        let g = group as usize;
        let (start, end) = match (
            caps.get(2 * g).copied().flatten(),
            caps.get(2 * g + 1).copied().flatten(),
        ) {
            (Some(s), Some(e)) => (s, e),
            _ => return Some(pos),
        };
        let len = end.saturating_sub(start);
        if len == 0 {
            return Some(pos);
        }
        let units = self.units();
        if pos + len > units.len() {
            return None;
        }
        for k in 0..len {
            let a = u32::from(units[start + k]);
            let b = u32::from(units[pos + k]);
            let eq = a == b || (ignore_case && self.canon(a) == self.canon(b));
            if !eq {
                return None;
            }
        }
        Some(pos + len)
    }
}

fn is_line_terminator(cp: u32) -> bool {
    matches!(cp, 0x0A | 0x0D | 0x2028 | 0x2029)
}

fn is_word(cp: u32) -> bool {
    matches!(cp,
        0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x5F)
}

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
//! - Each backtrack point explored counts one step against the budget, so a
//!   catastrophic-backtracking input aborts while a linear forward match pays
//!   nothing per character.
//! - Reported positions are UTF-16 code-unit offsets.
//!
//! # Lookbehind
//! Lookbehind is evaluated by scanning candidate start positions and requiring
//! the body to end exactly at the assertion point. This is boolean-correct and
//! variable-length-capable; capture slots inside the body keep their first write
//! so repeated captures report the leftmost text selected by ECMAScript's
//! right-to-left lookbehind matching semantics.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching>

use smallvec::{SmallVec, smallvec};

use super::ExecConfig;
use crate::casefold::{ascii_other_case, canonicalize, fold_unicode};
use crate::classes::ClassSet;
use crate::cursor::Input;
use crate::error::ExecError;
use crate::program::{Insn, Program, RepeatAtom};

/// Capture slots: `Some(pos)` once written, `None` if the group is unset.
///
/// A single instance is mutated in place for an entire match attempt. Rather
/// than snapshot the whole array onto the backtrack stack at every alternation
/// / quantifier split, each write is recorded in an undo log (see [`UndoLog`])
/// and rolled back when the matcher backtracks past it — so a split is O(1)
/// regardless of how many capture groups the pattern has. Inlining the common
/// case (up to three capturing groups — `slot_count <= 8`) keeps the array on
/// the stack with no heap traffic; wider patterns spill to the heap as a `Vec`.
type Caps = SmallVec<[Option<usize>; 8]>;

/// One recorded capture-slot write: `(slot, previous_value)`. Replaying the log
/// in reverse restores the slots to an earlier point.
type UndoEntry = (usize, Option<usize>);

/// Reusable per-attempt buffers.
///
/// The backtrack stack and undo log are the same shape for every match attempt,
/// so a global search (`/g`) that probes many start offsets keeps one of each
/// and clears them per attempt instead of allocating fresh `Vec`s each time.
/// Nested lookaround evaluation still uses its own transient buffers.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    stack: Vec<Frame>,
    log: Vec<UndoEntry>,
}

impl Scratch {
    /// Fresh, empty scratch buffer.
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

/// Try to match `program` against `input` beginning exactly at code-unit offset
/// `at`. Returns the filled capture slots on success. `scratch` is reused across
/// successive attempts on the same subject to avoid a per-attempt allocation.
pub(crate) fn attempt(
    program: &Program,
    input: &Input<'_>,
    at: usize,
    config: ExecConfig,
    scratch: &mut Scratch,
) -> Result<Option<Caps>, ExecError> {
    let mut m = Matcher {
        program,
        input,
        steps: 0,
        step_limit: config.step_limit,
    };
    let caps: Caps = smallvec![None; program.slot_count()];
    let mut stack = core::mem::take(&mut scratch.stack);
    let mut log = core::mem::take(&mut scratch.log);
    let result = m.run(0, at, None, false, caps, &mut stack, &mut log);
    scratch.stack = stack;
    scratch.log = log;
    match result? {
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
///
/// `log_mark` is the undo-log length at the moment this alternative was pushed;
/// resuming it first rolls the capture slots back to that point, undoing every
/// write the abandoned path made.
#[derive(Debug)]
struct Frame {
    pc: usize,
    pos: usize,
    log_mark: usize,
    resume: Resume,
}

/// How a popped [`Frame`] continues.
#[derive(Debug, Clone, Copy)]
enum Resume {
    /// Ordinary alternative: run the inner loop from `(pc, pos)`.
    At,
    /// One give-back step of a greedy fused repeat: `pos` is the length to try
    /// now; on resume, schedule the next-shorter length (down to `low`) before
    /// running the continuation at `pc`.
    RepeatGreedy { low: usize },
    /// One give-forward step of a lazy fused repeat: `pos` is the length to try
    /// now; on resume, schedule the next-longer length (up to `high`).
    RepeatLazy { high: usize },
}

impl Matcher<'_, '_> {
    /// Run from `entry` at `start`. `end_anchor`, when set, only accepts a
    /// terminator reached exactly at that offset (used by lookbehind).
    /// `freeze_capture_saves` preserves the first write to capture slots while
    /// still allowing internal progress marks to update.
    fn run(
        &mut self,
        entry: usize,
        start: usize,
        end_anchor: Option<usize>,
        freeze_capture_saves: bool,
        mut caps: Caps,
        stack: &mut Vec<Frame>,
        log: &mut Vec<UndoEntry>,
    ) -> Result<Option<(usize, Caps)>, ExecError> {
        stack.clear();
        log.clear();
        stack.push(Frame {
            pc: entry,
            pos: start,
            log_mark: 0,
            resume: Resume::At,
        });

        // Copy the program reference into a local so instruction borrows are
        // independent of `&mut self`: the consuming arms can mutate `pc`/`pos`
        // and the lookaround arm can call `&mut self` `eval_look` without a
        // separate decode-then-act dispatch step.
        let prog = self.program;
        // `u64::MAX` sentinel turns the unbounded case into a never-taken
        // compare instead of an `Option` test.
        let limit = self.step_limit.unwrap_or(u64::MAX);

        while let Some(frame) = stack.pop() {
            // Account one step per backtrack point explored, not per
            // instruction: catastrophic backtracking is an explosion in the
            // number of alternatives tried (frames popped here), and a
            // quadratic lookbehind scan re-pops a frame per candidate start, so
            // both are bounded — while a linear forward match (which pushes no
            // frame it later pops) pays nothing on its hot per-character path.
            self.steps += 1;
            if self.steps > limit {
                return Err(ExecError::StepLimitExceeded);
            }
            // Undo every capture write made on the abandoned path so this
            // alternative resumes from the slot state captured at its split.
            while log.len() > frame.log_mark {
                let (slot, old) = log.pop().unwrap();
                caps[slot] = old;
            }
            // For a fused-repeat give-back, chain the next length before running
            // the continuation, so the stack holds at most one give-back frame
            // per active repeat instead of one per matched character.
            match frame.resume {
                Resume::At => {}
                Resume::RepeatGreedy { low } => {
                    if frame.pos > low {
                        stack.push(Frame {
                            pc: frame.pc,
                            pos: frame.pos - 1,
                            log_mark: frame.log_mark,
                            resume: Resume::RepeatGreedy { low },
                        });
                    }
                }
                Resume::RepeatLazy { high } => {
                    if frame.pos < high {
                        stack.push(Frame {
                            pc: frame.pc,
                            pos: frame.pos + 1,
                            log_mark: frame.log_mark,
                            resume: Resume::RepeatLazy { high },
                        });
                    }
                }
            }
            let (mut pc, mut pos) = (frame.pc, frame.pos);
            let accepted = loop {
                match &prog.insns[pc] {
                    Insn::Match | Insn::LookMatch => {
                        if end_anchor.is_none_or(|t| pos == t) {
                            break Some(pos);
                        }
                        break None;
                    }
                    Insn::Char { cp: c, ignore_case } => match self.decode(pos) {
                        Some((cp, w)) if self.char_eq(*c, cp, *ignore_case) => {
                            pc += 1;
                            pos += w;
                        }
                        _ => break None,
                    },
                    Insn::CharSeq(seq) => {
                        let end = pos + seq.len();
                        let units = self.units();
                        if end <= units.len() && units[pos..end] == **seq {
                            pc += 1;
                            pos = end;
                        } else {
                            break None;
                        }
                    }
                    Insn::AnyChar { dot_all } => match self.decode(pos) {
                        Some((cp, w)) if *dot_all || !is_line_terminator(cp) => {
                            pc += 1;
                            pos += w;
                        }
                        _ => break None,
                    },
                    Insn::Repeat { atom, min, greedy } => {
                        // Tight one-code-unit scan (non-Unicode), then a single
                        // chained give-back frame rather than a split per char.
                        // The atom kind is matched once, outside the per-unit
                        // loop, so each loop is a single specialized test.
                        let units = self.units();
                        let len = units.len();
                        let mut end = pos;
                        match atom {
                            RepeatAtom::Char { cp, ignore_case } => {
                                let (cp, ic) = (*cp, *ignore_case);
                                while end < len && self.char_eq(cp, u32::from(units[end]), ic) {
                                    end += 1;
                                }
                            }
                            RepeatAtom::Any { dot_all } => {
                                let da = *dot_all;
                                while end < len
                                    && (da || !is_line_terminator(u32::from(units[end])))
                                {
                                    end += 1;
                                }
                            }
                            RepeatAtom::Class {
                                class,
                                negate,
                                ignore_case,
                            } => {
                                let set = &prog.classes[*class as usize];
                                let (neg, ic) = (*negate, *ignore_case);
                                while end < len
                                    && self.class_member(set, neg, u32::from(units[end]), ic)
                                {
                                    end += 1;
                                }
                            }
                        }
                        if (end - pos) < *min as usize {
                            break None;
                        }
                        let low = pos + *min as usize;
                        if *greedy {
                            if end > low {
                                stack.push(Frame {
                                    pc: pc + 1,
                                    pos: end - 1,
                                    log_mark: log.len(),
                                    resume: Resume::RepeatGreedy { low },
                                });
                            }
                            pos = end;
                        } else {
                            if end > low {
                                stack.push(Frame {
                                    pc: pc + 1,
                                    pos: low + 1,
                                    log_mark: log.len(),
                                    resume: Resume::RepeatLazy { high: end },
                                });
                            }
                            pos = low;
                        }
                        pc += 1;
                    }
                    Insn::Class {
                        class,
                        negate,
                        ignore_case,
                    } => match self.decode(pos) {
                        Some((cp, w))
                            if self.class_member(
                                &prog.classes[*class as usize],
                                *negate,
                                cp,
                                *ignore_case,
                            ) =>
                        {
                            pc += 1;
                            pos += w;
                        }
                        _ => break None,
                    },
                    Insn::Jump(t) => pc = *t,
                    Insn::Split(a, b) => {
                        // O(1): record where to resume and the undo-log mark to
                        // roll back to, instead of cloning the whole slot array.
                        stack.push(Frame {
                            pc: *b,
                            pos,
                            log_mark: log.len(),
                            resume: Resume::At,
                        });
                        pc = *a;
                    }
                    Insn::Save(slot) => {
                        let slot = *slot;
                        if !freeze_capture_saves || caps[slot].is_none() {
                            log.push((slot, caps[slot]));
                            caps[slot] = Some(pos);
                        }
                        pc += 1;
                    }
                    Insn::SetMark(slot) => {
                        let slot = *slot;
                        log.push((slot, caps[slot]));
                        caps[slot] = Some(pos);
                        pc += 1;
                    }
                    Insn::ClearCapture(index) => {
                        let g = *index as usize;
                        if !freeze_capture_saves
                            || (caps[2 * g].is_none() && caps[2 * g + 1].is_none())
                        {
                            log.push((2 * g, caps[2 * g]));
                            caps[2 * g] = None;
                            log.push((2 * g + 1, caps[2 * g + 1]));
                            caps[2 * g + 1] = None;
                        }
                        pc += 1;
                    }
                    Insn::CheckProgress(slot) => {
                        if caps[*slot] == Some(pos) {
                            break None;
                        }
                        pc += 1;
                    }
                    Insn::AssertStart { multiline } => {
                        if self.at_start(pos, *multiline) {
                            pc += 1;
                        } else {
                            break None;
                        }
                    }
                    Insn::AssertEnd { multiline } => {
                        if self.at_end(pos, *multiline) {
                            pc += 1;
                        } else {
                            break None;
                        }
                    }
                    Insn::WordBoundary(invert) => {
                        if self.word_boundary(pos) != *invert {
                            pc += 1;
                        } else {
                            break None;
                        }
                    }
                    Insn::BackRef {
                        indices,
                        ignore_case,
                    } => match self.match_backref(indices, pos, &caps, *ignore_case) {
                        Some(next) => {
                            pc += 1;
                            pos = next;
                        }
                        None => break None,
                    },
                    Insn::Look {
                        negate,
                        behind,
                        entry,
                    } => {
                        let (negate, behind, look_entry) = (*negate, *behind, *entry);
                        match self.eval_look(negate, behind, look_entry, pos, &caps)? {
                            Some(updated) => {
                                // Apply the lookaround's capture writes, logging
                                // each overwrite so a later backtrack restores it.
                                for i in 0..caps.len() {
                                    if caps[i] != updated[i] {
                                        log.push((i, caps[i]));
                                        caps[i] = updated[i];
                                    }
                                }
                                pc += 1;
                            }
                            None => break None,
                        }
                    }
                }
            };

            if let Some(pos) = accepted {
                return Ok(Some((pos, caps)));
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
        // Lookaround bodies recurse into `run` while the caller's stack and undo
        // log are live, so they evaluate on their own transient buffers.
        let mut look_stack = Vec::new();
        let mut look_log = Vec::new();
        let found = if behind {
            let mut hit = None;
            for s in 0..=pos {
                if let Some((_, updated)) = self.run(
                    entry,
                    s,
                    Some(pos),
                    true,
                    caps.clone(),
                    &mut look_stack,
                    &mut look_log,
                )? {
                    hit = Some(updated);
                    break;
                }
            }
            hit
        } else {
            self.run(
                entry,
                pos,
                None,
                false,
                caps.clone(),
                &mut look_stack,
                &mut look_log,
            )?
            .map(|(_, c)| c)
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
        let inside = if cp < 128 {
            // ASCII fast path: an O(1) bitmap check. Both case-fold directions
            // for ASCII letters stay in ASCII, so the opposite-case probe covers
            // the unicode fold here too (a non-ASCII member can never fold into
            // the ASCII range).
            set.ascii_contains(cp) || (ignore_case && set.ascii_contains(ascii_other_case(cp)))
        } else {
            let mut m = set.code_points.contains(cp);
            if !m && ignore_case {
                m = set.code_points.contains(ascii_other_case(cp));
                if !m && self.program.unicode {
                    m = set.code_points.contains(fold_unicode(cp));
                }
            }
            m
        };
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

    /// Match the text previously captured by one of `groups`, returning the new
    /// position. If no candidate group participated, the backreference matches
    /// the empty string (succeeds).
    fn match_backref(
        &self,
        groups: &[u32],
        pos: usize,
        caps: &Caps,
        ignore_case: bool,
    ) -> Option<usize> {
        let Some((start, end)) = groups.iter().find_map(|group| {
            let g = *group as usize;
            match (
                caps.get(2 * g).copied().flatten(),
                caps.get(2 * g + 1).copied().flatten(),
            ) {
                (Some(s), Some(e)) => Some((s, e)),
                _ => None,
            }
        }) else {
            return Some(pos);
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

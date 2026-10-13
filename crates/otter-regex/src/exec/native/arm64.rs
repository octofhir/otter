//! AArch64 emitter of the native matcher tier.
//!
//! Lowers a [`Program`] instruction by instruction into one function
//! `extern "C" fn(*mut State) -> u64` that runs a window of candidate starts:
//! `0` when the window is exhausted (the next start in [`State`]), `1` on a
//! match (its slots in the state's capture array), `2` when the step budget
//! ran out.
//!
//! # Contents
//! - [`compile`] — emit and finalize the search function of a program.
//!
//! # Invariants
//! - Registers fixed for the whole function: x19 subject, x20 length, x21
//!   position, x22 capture slots, x23 backtrack-stack top, x24 undo-log base,
//!   x25 undo-log length in bytes, x26 step budget, x27 backtrack-stack base,
//!   x28 [`State`]. x9–x16 are scratch.
//! - x13 and x14 are the only scratch registers live across a unit test (a
//!   run's cursor and end, a repeat's table and start); every path that calls
//!   into Rust preserves them, and the stack-growth paths preserve x9–x15.
//! - A backtrack frame is `[resume, position, log mark, aux]`, pushed upward;
//!   an undo-log entry is `[slot, old value]`. Popping a frame takes one step
//!   and then rolls the log back to the frame's mark, exactly as the
//!   interpreter does.
//! - A run of fixed-width consuming instructions that no branch enters checks
//!   the subject's length once for the whole run, then reads unit by unit
//!   through a post-incremented cursor. A run fails as a whole when the
//!   subject is too short, which is where the interpreter would fail it too:
//!   consuming instructions have no other effect.
//!
//! # See also
//! - [`super::search`] — the Rust side of the call.

// `dynasm!` converts register operands with `.into()` even when they are
// already the operand type.
#![allow(clippy::useless_conversion)]

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};

use super::plan::{
    High, Pred, char_pred, class_pred, fixed_width, start_pred, targets, word_table,
};
use super::{
    HighPred, NativeCode, backref_ignore_case, grow_log, grow_stack, high_member, offsets,
};
use crate::program::{Insn, Program, RepeatAtom};

/// Largest immediate an `add`/`sub`/`cmp` encodes directly.
const IMM12: u64 = 4095;

/// Emit and finalize the search function of `program` for subjects of
/// two-byte units (`wide`) or one-byte units.
pub(super) fn compile(program: &Program, wide: bool) -> Option<NativeCode> {
    let mut ops = Assembler::new().ok()?;
    let labels = (0..program.insns.len())
        .map(|_| ops.new_dynamic_label())
        .collect();
    let fail = ops.new_dynamic_label();
    let grow_stack = ops.new_dynamic_label();
    let grow_log = ops.new_dynamic_label();
    let exit = ops.new_dynamic_label();
    let entry = ops.offset();
    let mut emitter = Emitter {
        ops,
        program,
        wide,
        labels,
        fail,
        grow_stack,
        grow_log,
        exit,
        tables: Vec::new(),
        preds: Vec::new(),
        stubs: Vec::new(),
    };
    emitter.function();
    let Emitter { ops, preds, .. } = emitter;
    let buf = ops.finalize().ok()?;
    Some(NativeCode {
        buf,
        entry,
        preds: preds.into_boxed_slice(),
        finalized_on: super::thread_tag(),
    })
}

struct Emitter<'p> {
    ops: Assembler,
    program: &'p Program,
    wide: bool,
    /// The code of each instruction; bound only for branch targets.
    labels: Vec<DynamicLabel>,
    /// Backtrack: pop a frame, or end the attempt when none is left.
    fail: DynamicLabel,
    grow_stack: DynamicLabel,
    grow_log: DynamicLabel,
    exit: DynamicLabel,
    /// Membership tables, emitted after the code.
    tables: Vec<(DynamicLabel, [u8; 256])>,
    /// Predicates [`high_member`] decides, indexed by the emitted call.
    preds: Vec<HighPred>,
    /// Resume stubs of fused repeats: label, instruction, greedy.
    stubs: Vec<(DynamicLabel, usize, bool)>,
}

impl Emitter<'_> {
    fn function(&mut self) {
        let candidate = self.ops.new_dynamic_label();
        let attempt = self.ops.new_dynamic_label();
        let reject = self.ops.new_dynamic_label();
        let attempt_failed = self.ops.new_dynamic_label();
        let advance = self.ops.new_dynamic_label();
        let no_match = self.ops.new_dynamic_label();
        let step_error = self.ops.new_dynamic_label();
        let (text, len, start, caps) = (offsets::TEXT, offsets::LEN, offsets::START, offsets::CAPS);
        let (stack_base, log_base, budget) =
            (offsets::STACK_BASE, offsets::LOG_BASE, offsets::BUDGET);
        dynasm!(self.ops
            ; .arch aarch64
            ; stp x29, x30, [sp, #-96]!
            ; mov x29, sp
            ; stp x19, x20, [sp, #16]
            ; stp x21, x22, [sp, #32]
            ; stp x23, x24, [sp, #48]
            ; stp x25, x26, [sp, #64]
            ; stp x27, x28, [sp, #80]
            ; mov x28, x0
            ; ldr x19, [x28, text]
            ; ldr x20, [x28, len]
            ; ldr x21, [x28, start]
            ; ldr x22, [x28, caps]
            ; ldr x27, [x28, stack_base]
            ; ldr x24, [x28, log_base]
            ; ldr x26, [x28, budget]
            ; =>candidate
        );
        let start_pred = self.program.prefilter.as_ref().map(start_pred);
        if let Some(pred) = &start_pred {
            let filter = offsets::FILTER;
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr x9, [x28, filter]
                ; cbz x9, =>attempt
                ; cmp x21, x20
                ; b.hs =>attempt
            );
            self.load_unit(9, 21);
            self.test(pred, reject, None);
        }
        // Entering an attempt pops its initial frame in the interpreter: one
        // step.
        dynasm!(self.ops
            ; .arch aarch64
            ; =>attempt
            ; cbz x26, =>step_error
            ; sub x26, x26, #1
            ; movn x9, 0
        );
        for slot in 1..self.program.slot_count() {
            let offset = (slot * 8) as u32;
            dynasm!(self.ops ; .arch aarch64 ; str x9, [x22, offset]);
        }
        dynasm!(self.ops
            ; .arch aarch64
            ; str x21, [x22]
            ; mov x23, x27
            ; movz x25, 0
        );
        self.insns();

        let fail = self.fail;
        let restore = self.ops.new_dynamic_label();
        let restored = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; =>fail
            ; cmp x23, x27
            ; b.eq =>attempt_failed
            ; cbz x26, =>step_error
            ; sub x26, x26, #1
            ; ldp x11, x12, [x23, #-16]
            ; ldp x10, x21, [x23, #-32]!
            ; =>restore
            ; cmp x25, x11
            ; b.ls =>restored
            ; sub x25, x25, #16
            ; add x13, x24, x25
            ; ldp x14, x15, [x13]
            ; str x15, [x22, x14, lsl #3]
            ; b =>restore
            ; =>restored
            ; br x10
        );

        // The next candidate after a failed attempt: one unit on, or past the
        // whole run of start units when the leading repeat is possessive.
        dynasm!(self.ops
            ; .arch aarch64
            ; =>attempt_failed
            ; ldr x21, [x22]
            ; add x21, x21, #1
        );
        if let (true, Some(pred)) = (self.program.lead_possessive_run, &start_pred) {
            let skip = self.ops.new_dynamic_label();
            let skipped = self.ops.new_dynamic_label();
            dynasm!(self.ops
                ; .arch aarch64
                ; =>skip
                ; cmp x21, x20
                ; b.hs =>skipped
            );
            self.load_unit(9, 21);
            self.test(pred, skipped, None);
            dynasm!(self.ops
                ; .arch aarch64
                ; add x21, x21, #1
                ; b =>skip
                ; =>skipped
            );
        }
        let (window_end, next) = (offsets::WINDOW_END, offsets::NEXT);
        let exit = self.exit;
        dynasm!(self.ops
            ; .arch aarch64
            ; =>advance
            ; ldr x9, [x28, window_end]
            ; cmp x21, x9
            ; b.ls =>candidate
            ; =>no_match
            ; str x21, [x28, next]
            ; movz x0, 0
            ; b =>exit
            ; =>reject
            ; add x21, x21, #1
            ; b =>advance
            ; =>step_error
            ; movz x0, 2
            ; =>exit
            ; str x26, [x28, budget]
            ; ldp x27, x28, [sp, #80]
            ; ldp x25, x26, [sp, #64]
            ; ldp x23, x24, [sp, #48]
            ; ldp x21, x22, [sp, #32]
            ; ldp x19, x20, [sp, #16]
            ; ldp x29, x30, [sp], #96
            ; ret
        );
        self.repeat_stubs();
        self.growth_paths();
        for (label, table) in std::mem::take(&mut self.tables) {
            dynasm!(self.ops ; .arch aarch64 ; =>label ; .bytes table.iter());
        }
    }

    /// Emit every instruction in order, grouping runs of fixed-width
    /// consuming instructions.
    fn insns(&mut self) {
        let targets = targets(self.program);
        let count = self.program.insns.len();
        let mut pc = 0;
        while pc < count {
            if targets[pc] {
                let label = self.labels[pc];
                dynasm!(self.ops ; .arch aarch64 ; =>label);
            }
            let mut end = pc;
            while end < count
                && fixed_width(&self.program.insns[end]).is_some()
                && (end == pc || !targets[end])
            {
                end += 1;
            }
            if end > pc {
                self.run(pc, end);
                pc = end;
            } else {
                self.insn(pc);
                pc += 1;
            }
        }
    }

    /// Instructions `from..to`, all fixed-width consuming and entered only at
    /// `from`: one length check, then a cursor through the units.
    fn run(&mut self, from: usize, to: usize) {
        let program = self.program;
        let total: u64 = program.insns[from..to]
            .iter()
            .map(|insn| fixed_width(insn).unwrap_or(0) as u64)
            .sum();
        if total <= IMM12 {
            let total = total as u32;
            dynasm!(self.ops ; .arch aarch64 ; add x14, x21, total);
        } else {
            self.load_u64(14, total);
            dynasm!(self.ops ; .arch aarch64 ; add x14, x21, x14);
        }
        let fail = self.fail;
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp x14, x20
            ; b.hi =>fail
        );
        if self.wide {
            dynasm!(self.ops ; .arch aarch64 ; add x13, x19, x21, lsl #1);
        } else {
            dynasm!(self.ops ; .arch aarch64 ; add x13, x19, x21);
        }
        for insn in &program.insns[from..to] {
            match insn {
                Insn::Char {
                    cp,
                    ignore_case: false,
                } => {
                    if !self.wide && *cp > 0xFF {
                        dynasm!(self.ops ; .arch aarch64 ; b =>fail);
                        continue;
                    }
                    self.next_unit();
                    self.compare_unit(*cp);
                    dynasm!(self.ops ; .arch aarch64 ; b.ne =>fail);
                }
                Insn::Char {
                    cp,
                    ignore_case: true,
                } => {
                    let pred = char_pred(*cp, true);
                    self.next_unit();
                    self.test(&pred, fail, None);
                }
                Insn::Class {
                    class,
                    negate,
                    ignore_case,
                } => {
                    let pred = class_pred(self.program, *class, *negate, *ignore_case);
                    self.next_unit();
                    self.test(&pred, fail, None);
                }
                Insn::AnyChar { dot_all: true } => {
                    let width = if self.wide { 2 } else { 1 };
                    dynasm!(self.ops ; .arch aarch64 ; add x13, x13, width);
                }
                Insn::AnyChar { dot_all: false } => {
                    self.next_unit();
                    self.line_terminator(fail);
                }
                Insn::CharSeq(seq) => self.char_seq(seq),
                _ => unreachable!("runs hold fixed-width consuming instructions"),
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; mov x21, x14);
    }

    /// Compare a literal run at the cursor, eight bytes at a time.
    fn char_seq(&mut self, seq: &[u16]) {
        let fail = self.fail;
        let bytes: Vec<u8> = if self.wide {
            seq.iter().flat_map(|unit| unit.to_le_bytes()).collect()
        } else {
            match seq.iter().map(|&unit| u8::try_from(unit).ok()).collect() {
                Some(bytes) => bytes,
                // A unit no one-byte subject holds: the literal never occurs.
                None => {
                    dynasm!(self.ops ; .arch aarch64 ; b =>fail);
                    return;
                }
            }
        };
        let mut rest = bytes.as_slice();
        while rest.len() >= 8 {
            let (chunk, tail) = rest.split_at(8);
            let value = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
            dynasm!(self.ops ; .arch aarch64 ; ldr x9, [x13], #8);
            self.load_u64(10, value);
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp x9, x10
                ; b.ne =>fail
            );
            rest = tail;
        }
        if rest.len() >= 4 {
            let (chunk, tail) = rest.split_at(4);
            let value = u32::from_le_bytes(chunk.try_into().expect("four bytes"));
            dynasm!(self.ops ; .arch aarch64 ; ldr w9, [x13], #4);
            self.load_u64(10, u64::from(value));
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w9, w10
                ; b.ne =>fail
            );
            rest = tail;
        }
        if rest.len() >= 2 {
            let (chunk, tail) = rest.split_at(2);
            let value = u16::from_le_bytes(chunk.try_into().expect("two bytes"));
            dynasm!(self.ops ; .arch aarch64 ; ldrh w9, [x13], #2);
            self.compare_unit(u32::from(value));
            dynasm!(self.ops ; .arch aarch64 ; b.ne =>fail);
            rest = tail;
        }
        if let [byte] = rest {
            dynasm!(self.ops ; .arch aarch64 ; ldrb w9, [x13], #1);
            self.compare_unit(u32::from(*byte));
            dynasm!(self.ops ; .arch aarch64 ; b.ne =>fail);
        }
    }

    /// Emit one instruction that is not part of a run.
    fn insn(&mut self, pc: usize) {
        let program = self.program;
        let fail = self.fail;
        match &program.insns[pc] {
            Insn::Repeat {
                atom,
                min,
                greedy,
                possessive,
            } => self.repeat(pc, atom, *min, *greedy, *possessive),
            Insn::Fail => dynasm!(self.ops ; .arch aarch64 ; b =>fail),
            Insn::Jump(target) => {
                if *target != pc + 1 {
                    let label = self.labels[*target];
                    dynasm!(self.ops ; .arch aarch64 ; b =>label);
                }
            }
            Insn::Split(first, second) => {
                let resume = self.labels[*second];
                self.push_frame(resume, 21, None);
                if *first != pc + 1 {
                    let label = self.labels[*first];
                    dynasm!(self.ops ; .arch aarch64 ; b =>label);
                }
            }
            Insn::Save(slot) => {
                if *slot > 1 {
                    self.log(*slot);
                }
                let offset = (*slot * 8) as u32;
                dynasm!(self.ops ; .arch aarch64 ; str x21, [x22, offset]);
            }
            Insn::SetMark(slot) => {
                self.log(*slot);
                let offset = (*slot * 8) as u32;
                dynasm!(self.ops ; .arch aarch64 ; str x21, [x22, offset]);
            }
            Insn::ClearCapture(group) => {
                for slot in [2 * *group as usize, 2 * *group as usize + 1] {
                    self.log(slot);
                    let offset = (slot * 8) as u32;
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; movn x9, 0
                        ; str x9, [x22, offset]
                    );
                }
            }
            Insn::CheckProgress(slot) => {
                let offset = (*slot * 8) as u32;
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr x9, [x22, offset]
                    ; cmp x9, x21
                    ; b.eq =>fail
                );
            }
            Insn::AssertStart { multiline } => {
                let pass = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch aarch64 ; cbz x21, =>pass);
                if *multiline {
                    dynasm!(self.ops ; .arch aarch64 ; sub x10, x21, #1);
                    self.load_unit(9, 10);
                    self.line_terminator(pass);
                }
                dynasm!(self.ops
                    ; .arch aarch64
                    ; b =>fail
                    ; =>pass
                );
            }
            Insn::AssertEnd { multiline } => {
                let pass = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp x21, x20
                    ; b.hs =>pass
                );
                if *multiline {
                    self.load_unit(9, 21);
                    self.line_terminator(pass);
                }
                dynasm!(self.ops
                    ; .arch aarch64
                    ; b =>fail
                    ; =>pass
                );
            }
            Insn::WordBoundary { invert, .. } => self.word_boundary(*invert),
            Insn::BackRef {
                indices,
                ignore_case,
            } => self.backref(indices, *ignore_case),
            Insn::Match => {
                let (next, exit) = (offsets::NEXT, self.exit);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; str x21, [x22, #8]
                    ; str x21, [x28, next]
                    ; movz x0, 1
                    ; b =>exit
                );
            }
            Insn::Char { .. }
            | Insn::CharSeq(_)
            | Insn::Class { .. }
            | Insn::AnyChar { .. }
            | Insn::Look { .. }
            | Insn::LookMatch => {
                unreachable!("runs and uncovered instructions are never emitted alone")
            }
        }
    }

    /// A fused repeat: consume the longest run of the atom, then leave the
    /// give-back (greedy) or give-forward (lazy) frame the interpreter would.
    fn repeat(&mut self, pc: usize, atom: &RepeatAtom, min: u32, greedy: bool, possessive: bool) {
        let fail = self.fail;
        let done = self.ops.new_dynamic_label();
        let next = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch aarch64 ; mov x14, x21);
        match atom {
            RepeatAtom::Any { dot_all: true } => {
                dynasm!(self.ops ; .arch aarch64 ; mov x21, x20);
            }
            RepeatAtom::Char {
                cp,
                ignore_case: false,
            } if !self.wide && *cp > 0xFF => {}
            _ => {
                let pred = match atom {
                    RepeatAtom::Char {
                        cp,
                        ignore_case: true,
                    } => Some(char_pred(*cp, true)),
                    RepeatAtom::Class {
                        class,
                        negate,
                        ignore_case,
                    } => Some(class_pred(self.program, *class, *negate, *ignore_case)),
                    _ => None,
                };
                if let Some(pred) = &pred {
                    let table = self.table_label(&pred.table);
                    dynasm!(self.ops ; .arch aarch64 ; adr x13, =>table);
                }
                dynasm!(self.ops
                    ; .arch aarch64
                    ; =>next
                    ; cmp x21, x20
                    ; b.hs =>done
                );
                self.load_unit(9, 21);
                match (atom, &pred) {
                    (_, Some(pred)) => self.test(pred, done, Some(13)),
                    (RepeatAtom::Char { cp, .. }, None) => {
                        self.compare_unit(*cp);
                        dynasm!(self.ops ; .arch aarch64 ; b.ne =>done);
                    }
                    (_, None) => self.line_terminator(done),
                }
                dynasm!(self.ops
                    ; .arch aarch64
                    ; add x21, x21, #1
                    ; b =>next
                );
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        // x12: the position after exactly `min` repetitions.
        if u64::from(min) <= IMM12 {
            dynasm!(self.ops ; .arch aarch64 ; add x12, x14, min);
        } else {
            self.load_u64(12, u64::from(min));
            dynasm!(self.ops ; .arch aarch64 ; add x12, x14, x12);
        }
        if min > 0 {
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp x21, x12
                ; b.lo =>fail
            );
        }
        let skip = self.ops.new_dynamic_label();
        let stub = self.ops.new_dynamic_label();
        if greedy {
            if !possessive {
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp x21, x12
                    ; b.eq =>skip
                    ; sub x9, x21, #1
                );
                self.push_frame(stub, 9, Some(12));
                dynasm!(self.ops ; .arch aarch64 ; =>skip);
            }
        } else {
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp x21, x12
                ; b.eq =>skip
                ; add x9, x12, #1
            );
            self.push_frame(stub, 9, Some(21));
            dynasm!(self.ops
                ; .arch aarch64
                ; =>skip
                ; mov x21, x12
            );
        }
        self.stubs.push((stub, pc, greedy));
    }

    /// The resume code of each fused repeat's frame: schedule the next length
    /// (until the bound in the frame's aux word), then continue after the
    /// repeat at the popped length.
    fn repeat_stubs(&mut self) {
        for (stub, pc, greedy) in std::mem::take(&mut self.stubs) {
            let after = self.labels[pc + 1];
            dynasm!(self.ops
                ; .arch aarch64
                ; =>stub
                ; cmp x21, x12
                ; b.eq =>after
            );
            if greedy {
                dynasm!(self.ops ; .arch aarch64 ; sub x9, x21, #1);
            } else {
                dynasm!(self.ops ; .arch aarch64 ; add x9, x21, #1);
            }
            self.push_frame(stub, 9, Some(12));
            dynasm!(self.ops ; .arch aarch64 ; b =>after);
        }
    }

    /// `\b` / `\B` in code-unit mode.
    fn word_boundary(&mut self, invert: bool) {
        let fail = self.fail;
        let before = self.ops.new_dynamic_label();
        let after = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; movz w11, 0
            ; cbz x21, =>before
            ; sub x10, x21, #1
        );
        self.load_unit(9, 10);
        self.word_unit(11, before);
        dynasm!(self.ops
            ; .arch aarch64
            ; =>before
            ; movz w12, 0
            ; cmp x21, x20
            ; b.hs =>after
        );
        self.load_unit(9, 21);
        self.word_unit(12, after);
        dynasm!(self.ops
            ; .arch aarch64
            ; =>after
            ; cmp w11, w12
        );
        if invert {
            dynasm!(self.ops ; .arch aarch64 ; b.ne =>fail);
        } else {
            dynasm!(self.ops ; .arch aarch64 ; b.eq =>fail);
        }
    }

    /// `W(dst)` = whether the unit in w9 is a word character; `skip` is where
    /// to continue when it is not a table unit (leaving `dst` clear).
    fn word_unit(&mut self, dst: u8, skip: DynamicLabel) {
        let label = self.table_label(&word_table());
        if self.wide {
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w9, 256
                ; b.hs =>skip
            );
        }
        dynasm!(self.ops
            ; .arch aarch64
            ; adr x10, =>label
            ; ldrb W(dst), [x10, x9]
        );
    }

    /// A backreference: the first participating group's text again at the
    /// position, or the empty string when none participated.
    fn backref(&mut self, indices: &[u32], ignore_case: bool) {
        let fail = self.fail;
        let found = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        for group in indices {
            let next_group = self.ops.new_dynamic_label();
            let (start, end) = (*group * 16, *group * 16 + 8);
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr x9, [x22, start]
                ; cmn x9, #1
                ; b.eq =>next_group
                ; ldr x10, [x22, end]
                ; cmn x10, #1
                ; b.ne =>found
                ; =>next_group
            );
        }
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>found
            ; cmp x10, x9
            ; b.ls =>done
        );
        if ignore_case {
            let helper = if self.wide {
                backref_ignore_case::<u16> as *const () as u64
            } else {
                backref_ignore_case::<u8> as *const () as u64
            };
            dynasm!(self.ops
                ; .arch aarch64
                ; mov x0, x28
                ; mov x1, x9
                ; mov x2, x10
                ; mov x3, x21
            );
            self.load_u64(16, helper);
            dynasm!(self.ops
                ; .arch aarch64
                ; blr x16
                ; cmn x0, #1
                ; b.eq =>fail
                ; mov x21, x0
            );
        } else {
            let compare = self.ops.new_dynamic_label();
            dynasm!(self.ops
                ; .arch aarch64
                ; sub x11, x10, x9
                ; add x12, x21, x11
                ; cmp x12, x20
                ; b.hi =>fail
                ; =>compare
            );
            self.load_unit(13, 9);
            self.load_unit(14, 21);
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w13, w14
                ; b.ne =>fail
                ; add x9, x9, #1
                ; add x21, x21, #1
                ; cmp x21, x12
                ; b.lo =>compare
            );
        }
        dynasm!(self.ops ; .arch aarch64 ; =>done);
    }

    /// Branch to `miss` unless the unit in w9 passes `pred`. `table` names a
    /// register already holding the table's address.
    fn test(&mut self, pred: &Pred, miss: DynamicLabel, table: Option<u8>) {
        let done = self.ops.new_dynamic_label();
        if self.wide {
            let high = self.ops.new_dynamic_label();
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w9, 256
                ; b.hs =>high
            );
            self.table_test(pred, miss, table);
            dynasm!(self.ops
                ; .arch aarch64
                ; b =>done
                ; =>high
            );
            self.high_test(&pred.high, miss, done);
        } else {
            self.table_test(pred, miss, table);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>done);
    }

    fn table_test(&mut self, pred: &Pred, miss: DynamicLabel, table: Option<u8>) {
        if pred.table.iter().all(|&member| member == 1) {
            return;
        }
        if pred.table.iter().all(|&member| member == 0) {
            dynasm!(self.ops ; .arch aarch64 ; b =>miss);
            return;
        }
        let base = match table {
            Some(base) => base,
            None => {
                let label = self.table_label(&pred.table);
                dynasm!(self.ops ; .arch aarch64 ; adr x10, =>label);
                10
            }
        };
        dynasm!(self.ops
            ; .arch aarch64
            ; ldrb w11, [X(base), x9]
            ; cbz w11, =>miss
        );
    }

    fn high_test(&mut self, high: &High, miss: DynamicLabel, done: DynamicLabel) {
        match high {
            High::Never => dynasm!(self.ops ; .arch aarch64 ; b =>miss),
            High::Always => {}
            High::Ranges { ranges, negate } => {
                let inside = self.ops.new_dynamic_label();
                for &(lo, hi) in ranges {
                    self.load_u64(10, u64::from(lo));
                    self.load_u64(11, u64::from(hi - lo));
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; sub w10, w9, w10
                        ; cmp w10, w11
                        ; b.ls =>inside
                    );
                }
                if *negate {
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; b =>done
                        ; =>inside
                        ; b =>miss
                    );
                } else {
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; b =>miss
                        ; =>inside
                    );
                }
            }
            High::Call(pred) => {
                let id = self.preds.len() as u64;
                self.preds.push(*pred);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; stp x13, x14, [sp, #-16]!
                    ; mov x0, x28
                    ; mov x2, x9
                );
                self.load_u64(1, id);
                self.load_u64(16, high_member as *const () as u64);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; blr x16
                    ; ldp x13, x14, [sp], #16
                    ; cbz x0, =>miss
                );
            }
        }
    }

    /// Branch to `target` when the unit in w9 is a line terminator.
    fn line_terminator(&mut self, target: DynamicLabel) {
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp w9, 0x0A
            ; b.eq =>target
            ; cmp w9, 0x0D
            ; b.eq =>target
        );
        if self.wide {
            dynasm!(self.ops
                ; .arch aarch64
                ; movz w10, 0x2028
                ; sub w10, w9, w10
                ; cmp w10, 1
                ; b.ls =>target
            );
        }
    }

    /// Set the flags comparing the unit in w9 with `unit`.
    fn compare_unit(&mut self, unit: u32) {
        if u64::from(unit) <= IMM12 {
            dynasm!(self.ops ; .arch aarch64 ; cmp w9, unit);
        } else {
            dynasm!(self.ops
                ; .arch aarch64
                ; movz w10, unit
                ; cmp w9, w10
            );
        }
    }

    /// `W(dst)` = the subject unit at index `X(index)`.
    fn load_unit(&mut self, dst: u8, index: u8) {
        if self.wide {
            dynasm!(self.ops ; .arch aarch64 ; ldrh W(dst), [x19, X(index), lsl #1]);
        } else {
            dynasm!(self.ops ; .arch aarch64 ; ldrb W(dst), [x19, X(index)]);
        }
    }

    /// w9 = the unit at the run cursor x13, which moves past it.
    fn next_unit(&mut self) {
        if self.wide {
            dynasm!(self.ops ; .arch aarch64 ; ldrh w9, [x13], #2);
        } else {
            dynasm!(self.ops ; .arch aarch64 ; ldrb w9, [x13], #1);
        }
    }

    /// Push a backtrack frame resuming at `resume` with position `X(pos)`,
    /// the current log mark, and aux `X(aux)` (zero when `None`).
    fn push_frame(&mut self, resume: DynamicLabel, pos: u8, aux: Option<u8>) {
        let (limit, grow) = (offsets::STACK_LIMIT, self.grow_stack);
        let room = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr x10, [x28, limit]
            ; cmp x23, x10
            ; b.ls =>room
            ; bl =>grow
            ; =>room
            ; adr x10, =>resume
            ; stp x10, X(pos), [x23]
        );
        match aux {
            Some(aux) => dynasm!(self.ops ; .arch aarch64 ; stp x25, X(aux), [x23, #16]),
            None => dynasm!(self.ops ; .arch aarch64 ; stp x25, xzr, [x23, #16]),
        }
        dynasm!(self.ops ; .arch aarch64 ; add x23, x23, #32);
    }

    /// Record slot `slot`'s current value in the undo log.
    fn log(&mut self, slot: usize) {
        let (cap, grow) = (offsets::LOG_CAP, self.grow_log);
        let room = self.ops.new_dynamic_label();
        let offset = (slot * 8) as u32;
        let slot = slot as u32;
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr x10, [x28, cap]
            ; cmp x25, x10
            ; b.lo =>room
            ; bl =>grow
            ; =>room
            ; add x10, x24, x25
            ; movz x11, slot
            ; ldr x12, [x22, offset]
            ; stp x11, x12, [x10]
            ; add x25, x25, #16
        );
    }

    /// The out-of-line paths that double the backtrack stack or undo log.
    /// Called with `bl`; they preserve x9–x15.
    fn growth_paths(&mut self) {
        let (grow_stack_label, grow_log_label) = (self.grow_stack, self.grow_log);
        let (spill, stack_base, log_base) =
            (offsets::SPILL_SP, offsets::STACK_BASE, offsets::LOG_BASE);
        dynasm!(self.ops ; .arch aarch64 ; =>grow_stack_label);
        self.save_scratch();
        dynasm!(self.ops
            ; .arch aarch64
            ; str x23, [x28, spill]
            ; mov x0, x28
        );
        self.load_u64(16, grow_stack as *const () as u64);
        dynasm!(self.ops
            ; .arch aarch64
            ; blr x16
            ; ldr x23, [x28, spill]
            ; ldr x27, [x28, stack_base]
        );
        self.restore_scratch();
        dynasm!(self.ops
            ; .arch aarch64
            ; ret
            ; =>grow_log_label
        );
        self.save_scratch();
        dynasm!(self.ops ; .arch aarch64 ; mov x0, x28);
        self.load_u64(16, grow_log as *const () as u64);
        dynasm!(self.ops
            ; .arch aarch64
            ; blr x16
            ; ldr x24, [x28, log_base]
        );
        self.restore_scratch();
        dynasm!(self.ops ; .arch aarch64 ; ret);
    }

    fn save_scratch(&mut self) {
        dynasm!(self.ops
            ; .arch aarch64
            ; stp x9, x10, [sp, #-64]!
            ; stp x11, x12, [sp, #16]
            ; stp x13, x14, [sp, #32]
            ; stp x15, x30, [sp, #48]
        );
    }

    fn restore_scratch(&mut self) {
        dynasm!(self.ops
            ; .arch aarch64
            ; ldp x15, x30, [sp, #48]
            ; ldp x13, x14, [sp, #32]
            ; ldp x11, x12, [sp, #16]
            ; ldp x9, x10, [sp], #64
        );
    }

    /// The label of a membership table, shared between identical tables.
    fn table_label(&mut self, table: &[u8; 256]) -> DynamicLabel {
        if let Some((label, _)) = self.tables.iter().find(|(_, known)| known == table) {
            return *label;
        }
        let label = self.ops.new_dynamic_label();
        self.tables.push((label, *table));
        label
    }

    /// Materialize `value` into `X(reg)`.
    fn load_u64(&mut self, reg: u8, value: u64) {
        let half = |index: u32| ((value >> (16 * index)) & 0xFFFF) as u32;
        dynasm!(self.ops ; .arch aarch64 ; movz X(reg), half(0));
        if half(1) != 0 {
            dynasm!(self.ops ; .arch aarch64 ; movk X(reg), half(1), lsl #16);
        }
        if half(2) != 0 {
            dynasm!(self.ops ; .arch aarch64 ; movk X(reg), half(2), lsl #32);
        }
        if half(3) != 0 {
            dynasm!(self.ops ; .arch aarch64 ; movk X(reg), half(3), lsl #48);
        }
    }
}

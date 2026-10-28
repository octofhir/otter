//! x86-64 emitter of the native matcher tier.
//!
//! Lowers a [`Program`] instruction by instruction into one function
//! `extern "sysv64" fn(*mut State) -> u64` (the C convention of the targets
//! it runs on) that runs a window of candidate starts: `0` when the window
//! is exhausted (the next start in [`super::State`]), `1` on a match (its
//! slots in the state's capture array), `2` when the step budget ran out.
//!
//! # Contents
//! - [`compile`] — emit and finalize the search function of a program.
//!
//! # Invariants
//! - Registers fixed for the whole function: r12 subject, r13 length, r14
//!   position, r15 capture slots, rbx backtrack-stack top, rbp
//!   [`super::State`], r8 undo-log base, r9 undo-log length in bytes, r10
//!   step budget, r11 backtrack-stack base. rax, rcx and rdx are scratch.
//! - rsi and rdi are the only scratch registers live across a unit test (a
//!   run's cursor and end, a repeat's table and start). Every path that calls
//!   into Rust saves the caller-saved registers it needs (r8–r11 always,
//!   rsi/rdi where live) and keeps the stack 16-byte aligned at the call.
//! - A backtrack frame is `[resume, position, log mark, aux]`, pushed upward;
//!   an undo-log entry is `[slot, old value]`. Popping a frame takes one step
//!   and then rolls the log back to the frame's mark, exactly as the
//!   interpreter does.
//! - A run of fixed-width consuming instructions that no branch enters checks
//!   the subject's length once for the whole run, then reads each unit at a
//!   fixed displacement from the run's start.
//!
//! # See also
//! - [`super::arm64`] — the same lowering for AArch64.

// `dynasm!` converts register operands with `.into()` even when they are
// already the operand type.
#![allow(clippy::useless_conversion)]

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};

use super::plan::{
    High, Pred, char_pred, class_pred, fixed_width, start_pred, targets, word_table,
};
use super::{
    HighPred, NativeCode, backref_ignore_case, grow_log, grow_stack, high_member, offsets,
};
use crate::program::{Insn, Program, RepeatAtom};

/// rax, rcx, rdx, rsi, rdi as dynamic register numbers.
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R14: u8 = 14;

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
    Some(NativeCode::new(buf, entry, preds))
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
        let text = offsets::TEXT as i32;
        let len = offsets::LEN as i32;
        let start = offsets::START as i32;
        let caps = offsets::CAPS as i32;
        let stack_base = offsets::STACK_BASE as i32;
        let log_base = offsets::LOG_BASE as i32;
        let budget = offsets::BUDGET as i32;
        // Six pushes after the return address leave the stack 8 bytes off
        // alignment; the `sub` restores it for every call the body makes.
        dynasm!(self.ops
            ; .arch x64
            ; push rbp
            ; push rbx
            ; push r12
            ; push r13
            ; push r14
            ; push r15
            ; sub rsp, 8
            ; mov rbp, rdi
            ; mov r12, [rbp + text]
            ; mov r13, [rbp + len]
            ; mov r14, [rbp + start]
            ; mov r15, [rbp + caps]
            ; mov r11, [rbp + stack_base]
            ; mov r8, [rbp + log_base]
            ; mov r10, [rbp + budget]
            ; =>candidate
        );
        let start_pred = self.program.prefilter.as_ref().map(start_pred);
        if let Some(pred) = &start_pred {
            let filter = offsets::FILTER as i32;
            dynasm!(self.ops
                ; .arch x64
                ; cmp QWORD [rbp + filter], 0
                ; je =>attempt
                ; cmp r14, r13
                ; jae =>attempt
            );
            self.load_unit(RAX, R14);
            self.test(pred, reject, None);
        }
        // Entering an attempt pops its initial frame in the interpreter: one
        // step.
        dynasm!(self.ops
            ; .arch x64
            ; =>attempt
            ; test r10, r10
            ; jz =>step_error
            ; sub r10, 1
            ; mov rax, -1
        );
        for slot in 1..self.program.slot_count() {
            let offset = (slot * 8) as i32;
            dynasm!(self.ops ; .arch x64 ; mov [r15 + offset], rax);
        }
        dynasm!(self.ops
            ; .arch x64
            ; mov [r15], r14
            ; mov rbx, r11
            ; xor r9d, r9d
        );
        self.insns();

        let fail = self.fail;
        let restore = self.ops.new_dynamic_label();
        let restored = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; =>fail
            ; cmp rbx, r11
            ; je =>attempt_failed
            ; test r10, r10
            ; jz =>step_error
            ; sub r10, 1
            ; sub rbx, 32
            ; mov rax, [rbx]
            ; mov r14, [rbx + 8]
            ; mov rcx, [rbx + 16]
            ; mov rdx, [rbx + 24]
            ; =>restore
            ; cmp r9, rcx
            ; jbe =>restored
            ; sub r9, 16
            ; mov rsi, [r8 + r9]
            ; mov rdi, [r8 + r9 + 8]
            ; mov [r15 + rsi * 8], rdi
            ; jmp =>restore
            ; =>restored
            ; jmp rax
        );

        // The next candidate after a failed attempt: one unit on, or past the
        // whole run of start units when the leading repeat is possessive.
        dynasm!(self.ops
            ; .arch x64
            ; =>attempt_failed
            ; mov r14, [r15]
            ; add r14, 1
        );
        if let (true, Some(pred)) = (self.program.lead_possessive_run, &start_pred) {
            let skip = self.ops.new_dynamic_label();
            let skipped = self.ops.new_dynamic_label();
            dynasm!(self.ops
                ; .arch x64
                ; =>skip
                ; cmp r14, r13
                ; jae =>skipped
            );
            self.load_unit(RAX, R14);
            self.test(pred, skipped, None);
            dynasm!(self.ops
                ; .arch x64
                ; add r14, 1
                ; jmp =>skip
                ; =>skipped
            );
        }
        let window_end = offsets::WINDOW_END as i32;
        let next = offsets::NEXT as i32;
        let exit = self.exit;
        dynasm!(self.ops
            ; .arch x64
            ; =>advance
            ; cmp r14, [rbp + window_end]
            ; jbe =>candidate
            ; =>no_match
            ; mov [rbp + next], r14
            ; xor eax, eax
            ; jmp =>exit
            ; =>reject
            ; add r14, 1
            ; jmp =>advance
            ; =>step_error
            ; mov eax, 2
            ; =>exit
            ; mov [rbp + budget], r10
            ; add rsp, 8
            ; pop r15
            ; pop r14
            ; pop r13
            ; pop r12
            ; pop rbx
            ; pop rbp
            ; ret
        );
        self.repeat_stubs();
        self.growth_paths();
        for (label, table) in std::mem::take(&mut self.tables) {
            dynasm!(self.ops ; .arch x64 ; =>label ; .bytes table.iter());
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
                dynasm!(self.ops ; .arch x64 ; =>label);
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
    /// `from`: one length check, then each unit at its displacement from the
    /// run's start in rsi.
    fn run(&mut self, from: usize, to: usize) {
        let program = self.program;
        let total: u64 = program.insns[from..to]
            .iter()
            .map(|insn| fixed_width(insn).unwrap_or(0) as u64)
            .sum();
        let fail = self.fail;
        dynasm!(self.ops
            ; .arch x64
            ; mov rdi, QWORD total as i64
            ; add rdi, r14
            ; cmp rdi, r13
            ; ja =>fail
        );
        if self.wide {
            dynasm!(self.ops ; .arch x64 ; lea rsi, [r12 + r14 * 2]);
        } else {
            dynasm!(self.ops ; .arch x64 ; lea rsi, [r12 + r14]);
        }
        let unit_bytes = if self.wide { 2 } else { 1 };
        let mut offset: usize = 0;
        for insn in &program.insns[from..to] {
            let at = (offset * unit_bytes) as i32;
            match insn {
                Insn::Char {
                    cp,
                    ignore_case: false,
                } => {
                    if !self.wide && *cp > 0xFF {
                        dynasm!(self.ops ; .arch x64 ; jmp =>fail);
                    } else {
                        self.unit_at(at);
                        self.compare_unit(*cp);
                        dynasm!(self.ops ; .arch x64 ; jne =>fail);
                    }
                }
                Insn::Char {
                    cp,
                    ignore_case: true,
                } => {
                    let pred = char_pred(*cp, true);
                    self.unit_at(at);
                    self.test(&pred, fail, None);
                }
                Insn::Class {
                    class,
                    negate,
                    ignore_case,
                } => {
                    let pred = class_pred(program, *class, *negate, *ignore_case);
                    self.unit_at(at);
                    self.test(&pred, fail, None);
                }
                Insn::AnyChar { dot_all: true } => {}
                Insn::AnyChar { dot_all: false } => {
                    self.unit_at(at);
                    self.line_terminator(fail);
                }
                Insn::CharSeq(seq) => self.char_seq(seq, at),
                _ => unreachable!("runs hold fixed-width consuming instructions"),
            }
            offset += fixed_width(insn).unwrap_or(0);
        }
        dynasm!(self.ops ; .arch x64 ; mov r14, rdi);
    }

    /// Compare a literal run at displacement `at` from the run's start,
    /// eight bytes at a time.
    fn char_seq(&mut self, seq: &[u16], at: i32) {
        let fail = self.fail;
        let bytes: Vec<u8> = if self.wide {
            seq.iter().flat_map(|unit| unit.to_le_bytes()).collect()
        } else {
            match seq.iter().map(|&unit| u8::try_from(unit).ok()).collect() {
                Some(bytes) => bytes,
                // A unit no one-byte subject holds: the literal never occurs.
                None => {
                    dynasm!(self.ops ; .arch x64 ; jmp =>fail);
                    return;
                }
            }
        };
        let mut rest = bytes.as_slice();
        let mut at = at;
        while rest.len() >= 8 {
            let (chunk, tail) = rest.split_at(8);
            let value = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
            dynasm!(self.ops
                ; .arch x64
                ; mov rcx, QWORD value as i64
                ; cmp rcx, [rsi + at]
                ; jne =>fail
            );
            rest = tail;
            at += 8;
        }
        if rest.len() >= 4 {
            let (chunk, tail) = rest.split_at(4);
            let value = u32::from_le_bytes(chunk.try_into().expect("four bytes"));
            dynasm!(self.ops
                ; .arch x64
                ; cmp DWORD [rsi + at], value as i32
                ; jne =>fail
            );
            rest = tail;
            at += 4;
        }
        if rest.len() >= 2 {
            let (chunk, tail) = rest.split_at(2);
            let value = u16::from_le_bytes(chunk.try_into().expect("two bytes"));
            dynasm!(self.ops
                ; .arch x64
                ; cmp WORD [rsi + at], value as i16
                ; jne =>fail
            );
            rest = tail;
            at += 2;
        }
        if let [byte] = rest {
            dynasm!(self.ops
                ; .arch x64
                ; cmp BYTE [rsi + at], *byte as i8
                ; jne =>fail
            );
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
                follow,
            } => self.repeat(pc, atom, *min, *greedy, *possessive, *follow),
            Insn::Fail => dynasm!(self.ops ; .arch x64 ; jmp =>fail),
            Insn::Jump(target) => {
                if *target != pc + 1 {
                    let label = self.labels[*target];
                    dynasm!(self.ops ; .arch x64 ; jmp =>label);
                }
            }
            Insn::Split(first, second) => {
                let resume = self.labels[*second];
                self.push_frame(resume, R14, None);
                if *first != pc + 1 {
                    let label = self.labels[*first];
                    dynasm!(self.ops ; .arch x64 ; jmp =>label);
                }
            }
            Insn::Save(slot) => {
                if *slot > 1 {
                    self.log(*slot);
                }
                let offset = (*slot * 8) as i32;
                dynasm!(self.ops ; .arch x64 ; mov [r15 + offset], r14);
            }
            Insn::SetMark(slot) => {
                self.log(*slot);
                let offset = (*slot * 8) as i32;
                dynasm!(self.ops ; .arch x64 ; mov [r15 + offset], r14);
            }
            Insn::ClearCapture(group) => {
                for slot in [2 * *group as usize, 2 * *group as usize + 1] {
                    self.log(slot);
                    let offset = (slot * 8) as i32;
                    dynasm!(self.ops ; .arch x64 ; mov QWORD [r15 + offset], -1);
                }
            }
            Insn::CheckProgress(slot) => {
                let offset = (*slot * 8) as i32;
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp [r15 + offset], r14
                    ; je =>fail
                );
            }
            Insn::AssertStart { multiline } => {
                let pass = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch x64
                    ; test r14, r14
                    ; jz =>pass
                );
                if *multiline {
                    dynasm!(self.ops ; .arch x64 ; lea rcx, [r14 - 1]);
                    self.load_unit(RAX, RCX);
                    self.line_terminator(pass);
                }
                dynasm!(self.ops
                    ; .arch x64
                    ; jmp =>fail
                    ; =>pass
                );
            }
            Insn::AssertEnd { multiline } => {
                let pass = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp r14, r13
                    ; jae =>pass
                );
                if *multiline {
                    self.load_unit(RAX, R14);
                    self.line_terminator(pass);
                }
                dynasm!(self.ops
                    ; .arch x64
                    ; jmp =>fail
                    ; =>pass
                );
            }
            Insn::WordBoundary { invert, .. } => self.word_boundary(*invert),
            Insn::BackRef {
                indices,
                ignore_case,
            } => self.backref(indices, *ignore_case),
            Insn::Match => {
                let (next, exit) = (offsets::NEXT as i32, self.exit);
                dynasm!(self.ops
                    ; .arch x64
                    ; mov [r15 + 8], r14
                    ; mov [rbp + next], r14
                    ; mov eax, 1
                    ; jmp =>exit
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
    fn repeat(
        &mut self,
        pc: usize,
        atom: &RepeatAtom,
        min: u32,
        greedy: bool,
        possessive: bool,
        follow: Option<u32>,
    ) {
        let fail = self.fail;
        let done = self.ops.new_dynamic_label();
        let next = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64 ; mov rdi, r14);
        match atom {
            RepeatAtom::Any { dot_all: true } => {
                dynasm!(self.ops ; .arch x64 ; mov r14, r13);
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
                    dynasm!(self.ops ; .arch x64 ; lea rsi, [=>table]);
                }
                dynasm!(self.ops
                    ; .arch x64
                    ; =>next
                    ; cmp r14, r13
                    ; jae =>done
                );
                self.load_unit(RAX, R14);
                match (atom, &pred) {
                    (_, Some(pred)) => self.test(pred, done, Some(RSI)),
                    (RepeatAtom::Char { cp, .. }, None) => {
                        self.compare_unit(*cp);
                        dynasm!(self.ops ; .arch x64 ; jne =>done);
                    }
                    (_, None) => self.line_terminator(done),
                }
                dynasm!(self.ops
                    ; .arch x64
                    ; add r14, 1
                    ; jmp =>next
                );
            }
        }
        // rdx: the position after exactly `min` repetitions.
        dynasm!(self.ops
            ; .arch x64
            ; =>done
            ; mov rdx, QWORD i64::from(min)
            ; add rdx, rdi
        );
        if min > 0 {
            dynasm!(self.ops
                ; .arch x64
                ; cmp r14, rdx
                ; jb =>fail
            );
        }
        let skip = self.ops.new_dynamic_label();
        let stub = self.ops.new_dynamic_label();
        if greedy {
            if !possessive {
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp r14, rdx
                    ; je =>skip
                    ; lea rcx, [r14 - 1]
                );
                self.follow_back(follow, RDX, skip);
                self.push_frame(stub, RCX, Some(RDX));
                dynasm!(self.ops ; .arch x64 ; =>skip);
            }
        } else {
            dynasm!(self.ops
                ; .arch x64
                ; cmp r14, rdx
                ; je =>skip
                ; lea rcx, [rdx + 1]
            );
            self.follow_forward(follow, R14, skip);
            self.push_frame(stub, RCX, Some(R14));
            dynasm!(self.ops
                ; .arch x64
                ; =>skip
                ; mov r14, rdx
            );
        }
        self.stubs.push((stub, pc, greedy));
    }

    /// The resume code of each fused repeat's frame: schedule the next length
    /// (until the bound in the frame's aux word, in rdx), then continue after
    /// the repeat at the popped length.
    fn repeat_stubs(&mut self) {
        for (stub, pc, greedy) in std::mem::take(&mut self.stubs) {
            let after = self.labels[pc + 1];
            dynasm!(self.ops
                ; .arch x64
                ; =>stub
                ; cmp r14, rdx
                ; je =>after
            );
            let follow = match &self.program.insns[pc] {
                Insn::Repeat { follow, .. } => *follow,
                _ => None,
            };
            if greedy {
                dynasm!(self.ops ; .arch x64 ; lea rcx, [r14 - 1]);
                self.follow_back(follow, RDX, after);
            } else {
                dynasm!(self.ops ; .arch x64 ; lea rcx, [r14 + 1]);
                self.follow_forward(follow, RDX, after);
            }
            self.push_frame(stub, RCX, Some(RDX));
            dynasm!(self.ops ; .arch x64 ; jmp =>after);
        }
    }

    /// Move the give-back length in rcx down to the longest one, at least
    /// `low`, whose next unit the follow set admits; jump to `none` when there
    /// is none. Every such length leaves a unit. Clobbers rax and rsi.
    fn follow_back(&mut self, follow: Option<u32>, low: u8, none: DynamicLabel) {
        let Some(follow) = follow else {
            return;
        };
        let table = self.table_label(&self.program.follows[follow as usize]);
        let (scan, admitted) = (self.ops.new_dynamic_label(), self.ops.new_dynamic_label());
        dynasm!(self.ops ; .arch x64 ; lea rsi, [=>table] ; =>scan);
        self.follow_unit(admitted);
        dynasm!(self.ops
            ; .arch x64
            ; cmp rcx, Rq(low)
            ; je =>none
            ; sub rcx, 1
            ; jmp =>scan
            ; =>admitted
        );
    }

    /// Move the give-forward length in rcx up to the shortest one, at most
    /// `high`, that leaves a unit the follow set admits; jump to `none` when
    /// there is none. Clobbers rax and rsi.
    fn follow_forward(&mut self, follow: Option<u32>, high: u8, none: DynamicLabel) {
        let Some(follow) = follow else {
            return;
        };
        let table = self.table_label(&self.program.follows[follow as usize]);
        let (scan, refused, admitted) = (
            self.ops.new_dynamic_label(),
            self.ops.new_dynamic_label(),
            self.ops.new_dynamic_label(),
        );
        dynasm!(self.ops
            ; .arch x64
            ; lea rsi, [=>table]
            ; =>scan
            ; cmp rcx, r13
            ; jae =>refused
        );
        self.follow_unit(admitted);
        dynasm!(self.ops
            ; .arch x64
            ; =>refused
            ; cmp rcx, Rq(high)
            ; je =>none
            ; add rcx, 1
            ; jmp =>scan
            ; =>admitted
        );
    }

    /// Jump to `admitted` when the follow table at rsi admits the unit at
    /// rcx, which lies inside the subject; a unit above the table always is.
    fn follow_unit(&mut self, admitted: DynamicLabel) {
        self.load_unit(RAX, RCX);
        if self.wide {
            dynasm!(self.ops
                ; .arch x64
                ; cmp eax, 256
                ; jae =>admitted
            );
        }
        dynasm!(self.ops
            ; .arch x64
            ; cmp BYTE [rsi + rax], 0
            ; jne =>admitted
        );
    }

    /// `\b` / `\B` in code-unit mode.
    fn word_boundary(&mut self, invert: bool) {
        let fail = self.fail;
        let before = self.ops.new_dynamic_label();
        let after = self.ops.new_dynamic_label();
        let table = self.table_label(&word_table());
        dynasm!(self.ops
            ; .arch x64
            ; lea rsi, [=>table]
            ; xor ecx, ecx
            ; test r14, r14
            ; jz =>before
            ; lea rdx, [r14 - 1]
        );
        self.load_unit(RAX, RDX);
        if self.wide {
            dynasm!(self.ops
                ; .arch x64
                ; cmp eax, 256
                ; jae =>before
            );
        }
        dynasm!(self.ops
            ; .arch x64
            ; movzx ecx, BYTE [rsi + rax]
            ; =>before
            ; xor edx, edx
            ; cmp r14, r13
            ; jae =>after
        );
        self.load_unit(RAX, R14);
        if self.wide {
            dynasm!(self.ops
                ; .arch x64
                ; cmp eax, 256
                ; jae =>after
            );
        }
        dynasm!(self.ops
            ; .arch x64
            ; movzx edx, BYTE [rsi + rax]
            ; =>after
            ; cmp ecx, edx
        );
        if invert {
            dynasm!(self.ops ; .arch x64 ; jne =>fail);
        } else {
            dynasm!(self.ops ; .arch x64 ; je =>fail);
        }
    }

    /// A backreference: the first participating group's text again at the
    /// position, or the empty string when none participated.
    fn backref(&mut self, indices: &[u32], ignore_case: bool) {
        let fail = self.fail;
        let found = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        for group in indices {
            let next_group = self.ops.new_dynamic_label();
            let (start, end) = ((*group * 16) as i32, (*group * 16 + 8) as i32);
            dynasm!(self.ops
                ; .arch x64
                ; mov rax, [r15 + start]
                ; cmp rax, -1
                ; je =>next_group
                ; mov rcx, [r15 + end]
                ; cmp rcx, -1
                ; jne =>found
                ; =>next_group
            );
        }
        dynasm!(self.ops
            ; .arch x64
            ; jmp =>done
            ; =>found
            ; cmp rcx, rax
            ; jbe =>done
        );
        if ignore_case {
            let helper = if self.wide {
                backref_ignore_case::<u16> as *const () as i64
            } else {
                backref_ignore_case::<u8> as *const () as i64
            };
            dynasm!(self.ops
                ; .arch x64
                ; push r8
                ; push r9
                ; push r10
                ; push r11
                ; mov rdi, rbp
                ; mov rsi, rax
                ; mov rdx, rcx
                ; mov rcx, r14
                ; mov rax, QWORD helper
                ; call rax
                ; pop r11
                ; pop r10
                ; pop r9
                ; pop r8
                ; cmp rax, -1
                ; je =>fail
                ; mov r14, rax
            );
        } else {
            let compare = self.ops.new_dynamic_label();
            dynasm!(self.ops
                ; .arch x64
                ; mov rdx, rcx
                ; sub rdx, rax
                ; add rdx, r14
                ; cmp rdx, r13
                ; ja =>fail
                ; =>compare
            );
            self.load_unit(RSI, RAX);
            self.load_unit(RDI, R14);
            dynasm!(self.ops
                ; .arch x64
                ; cmp esi, edi
                ; jne =>fail
                ; add rax, 1
                ; add r14, 1
                ; cmp r14, rdx
                ; jb =>compare
            );
        }
        dynasm!(self.ops ; .arch x64 ; =>done);
    }

    /// Branch to `miss` unless the unit in eax passes `pred`. `table` names a
    /// register already holding the table's address.
    fn test(&mut self, pred: &Pred, miss: DynamicLabel, table: Option<u8>) {
        let done = self.ops.new_dynamic_label();
        if self.wide {
            let high = self.ops.new_dynamic_label();
            dynasm!(self.ops
                ; .arch x64
                ; cmp eax, 256
                ; jae =>high
            );
            self.table_test(pred, miss, table);
            dynasm!(self.ops
                ; .arch x64
                ; jmp =>done
                ; =>high
            );
            self.high_test(&pred.high, miss, done);
        } else {
            self.table_test(pred, miss, table);
        }
        dynasm!(self.ops ; .arch x64 ; =>done);
    }

    fn table_test(&mut self, pred: &Pred, miss: DynamicLabel, table: Option<u8>) {
        if pred.table.iter().all(|&member| member == 1) {
            return;
        }
        if pred.table.iter().all(|&member| member == 0) {
            dynasm!(self.ops ; .arch x64 ; jmp =>miss);
            return;
        }
        let base = match table {
            Some(base) => base,
            None => {
                let label = self.table_label(&pred.table);
                dynasm!(self.ops ; .arch x64 ; lea rcx, [=>label]);
                RCX
            }
        };
        dynasm!(self.ops
            ; .arch x64
            ; cmp BYTE [Rq(base) + rax], 0
            ; je =>miss
        );
    }

    fn high_test(&mut self, high: &High, miss: DynamicLabel, done: DynamicLabel) {
        match high {
            High::Never => dynasm!(self.ops ; .arch x64 ; jmp =>miss),
            High::Always => {}
            High::Ranges { ranges, negate } => {
                let inside = self.ops.new_dynamic_label();
                for &(lo, hi) in ranges {
                    let (lo, span) = (lo as i32, (hi - lo) as i32);
                    dynasm!(self.ops
                        ; .arch x64
                        ; lea ecx, [rax - lo]
                        ; cmp ecx, span
                        ; jbe =>inside
                    );
                }
                if *negate {
                    dynasm!(self.ops
                        ; .arch x64
                        ; jmp =>done
                        ; =>inside
                        ; jmp =>miss
                    );
                } else {
                    dynasm!(self.ops
                        ; .arch x64
                        ; jmp =>miss
                        ; =>inside
                    );
                }
            }
            High::Call(pred) => {
                let id = self.preds.len() as i32;
                self.preds.push(*pred);
                let helper = high_member as *const () as i64;
                // Six pushes keep the stack aligned for the call.
                dynasm!(self.ops
                    ; .arch x64
                    ; push rsi
                    ; push rdi
                    ; push r8
                    ; push r9
                    ; push r10
                    ; push r11
                    ; mov rdi, rbp
                    ; mov esi, id
                    ; mov edx, eax
                    ; mov rax, QWORD helper
                    ; call rax
                    ; pop r11
                    ; pop r10
                    ; pop r9
                    ; pop r8
                    ; pop rdi
                    ; pop rsi
                    ; test rax, rax
                    ; jz =>miss
                );
            }
        }
    }

    /// Branch to `target` when the unit in eax is a line terminator.
    fn line_terminator(&mut self, target: DynamicLabel) {
        dynasm!(self.ops
            ; .arch x64
            ; cmp eax, 0x0A
            ; je =>target
            ; cmp eax, 0x0D
            ; je =>target
        );
        if self.wide {
            dynasm!(self.ops
                ; .arch x64
                ; lea ecx, [rax - 0x2028]
                ; cmp ecx, 1
                ; jbe =>target
            );
        }
    }

    /// Set the flags comparing the unit in eax with `unit`.
    fn compare_unit(&mut self, unit: u32) {
        dynasm!(self.ops ; .arch x64 ; cmp eax, unit as i32);
    }

    /// `dst` = the subject unit at index `index`, zero-extended.
    fn load_unit(&mut self, dst: u8, index: u8) {
        if self.wide {
            dynasm!(self.ops ; .arch x64 ; movzx Rd(dst), WORD [r12 + Rq(index) * 2]);
        } else {
            dynasm!(self.ops ; .arch x64 ; movzx Rd(dst), BYTE [r12 + Rq(index)]);
        }
    }

    /// eax = the run unit at displacement `at` from the run's start.
    fn unit_at(&mut self, at: i32) {
        if self.wide {
            dynasm!(self.ops ; .arch x64 ; movzx eax, WORD [rsi + at]);
        } else {
            dynasm!(self.ops ; .arch x64 ; movzx eax, BYTE [rsi + at]);
        }
    }

    /// Push a backtrack frame resuming at `resume` with position `pos`, the
    /// current log mark, and aux `aux` (zero when `None`). Clobbers rax.
    fn push_frame(&mut self, resume: DynamicLabel, pos: u8, aux: Option<u8>) {
        let (limit, grow) = (offsets::STACK_LIMIT as i32, self.grow_stack);
        let room = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; cmp rbx, [rbp + limit]
            ; jbe =>room
            ; call =>grow
            ; =>room
            ; lea rax, [=>resume]
            ; mov [rbx], rax
            ; mov [rbx + 8], Rq(pos)
            ; mov [rbx + 16], r9
        );
        match aux {
            Some(aux) => dynasm!(self.ops ; .arch x64 ; mov [rbx + 24], Rq(aux)),
            None => dynasm!(self.ops ; .arch x64 ; mov QWORD [rbx + 24], 0),
        }
        dynasm!(self.ops ; .arch x64 ; add rbx, 32);
    }

    /// Record slot `slot`'s current value in the undo log. Clobbers rax.
    fn log(&mut self, slot: usize) {
        let (cap, grow) = (offsets::LOG_CAP as i32, self.grow_log);
        let room = self.ops.new_dynamic_label();
        let offset = (slot * 8) as i32;
        let slot = slot as i32;
        dynasm!(self.ops
            ; .arch x64
            ; cmp r9, [rbp + cap]
            ; jb =>room
            ; call =>grow
            ; =>room
            ; mov rax, [r15 + offset]
            ; mov QWORD [r8 + r9], slot
            ; mov [r8 + r9 + 8], rax
            ; add r9, 16
        );
    }

    /// The out-of-line paths that double the backtrack stack or undo log.
    /// Called with `call`; they preserve every register but the ones they
    /// reload.
    fn growth_paths(&mut self) {
        let (grow_stack_label, grow_log_label) = (self.grow_stack, self.grow_log);
        let spill = offsets::SPILL_SP as i32;
        let stack_base = offsets::STACK_BASE as i32;
        let log_base = offsets::LOG_BASE as i32;
        dynasm!(self.ops ; .arch x64 ; =>grow_stack_label);
        self.save_scratch();
        dynasm!(self.ops
            ; .arch x64
            ; mov [rbp + spill], rbx
            ; mov rdi, rbp
            ; mov rax, QWORD grow_stack as *const () as i64
            ; call rax
        );
        self.restore_scratch();
        dynasm!(self.ops
            ; .arch x64
            ; mov rbx, [rbp + spill]
            ; mov r11, [rbp + stack_base]
            ; ret
            ; =>grow_log_label
        );
        self.save_scratch();
        dynasm!(self.ops
            ; .arch x64
            ; mov rdi, rbp
            ; mov rax, QWORD grow_log as *const () as i64
            ; call rax
        );
        self.restore_scratch();
        dynasm!(self.ops
            ; .arch x64
            ; mov r8, [rbp + log_base]
            ; ret
        );
    }

    /// Nine pushes after the stub's return address keep the stack aligned.
    fn save_scratch(&mut self) {
        dynasm!(self.ops
            ; .arch x64
            ; push rax
            ; push rcx
            ; push rdx
            ; push rsi
            ; push rdi
            ; push r8
            ; push r9
            ; push r10
            ; push r11
        );
    }

    fn restore_scratch(&mut self) {
        dynasm!(self.ops
            ; .arch x64
            ; pop r11
            ; pop r10
            ; pop r9
            ; pop r8
            ; pop rdi
            ; pop rsi
            ; pop rdx
            ; pop rcx
            ; pop rax
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
}

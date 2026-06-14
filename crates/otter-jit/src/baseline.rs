//! Sparkplug-style baseline emitter (arm64): the integer register-machine core.
//!
//! Lowers a [`otter_vm::JitFunctionView`] to native arm64 with **no IR, no
//! register allocation, and no deopt** — one linear pass, one emit routine per
//! supported opcode, branch fixups via dynasm dynamic labels. Operands and
//! results flow through the interpreter's own register window (a `*mut u64`
//! handed in at call time *is* `Frame.registers.as_mut_ptr()`), so this tier
//! reuses the precise `FrameRoots` rooting the interpreter already provides and
//! needs **no GC stack maps**.
//!
//! This module compiles the **allocation-free integer subset** that the hot
//! loops of `fib`/`mandelbrot` are built from: tagged-int32 arithmetic with an
//! inline int32 guard, int32 comparisons producing boolean Values, register
//! moves (`LoadLocal`/`StoreLocal`), inline-immediate int loads, and
//! intra-function branches. There are **no safepoints** in this subset (no
//! allocation, no `Call`), so no value is ever live across a move — the GC
//! cannot observe this code. `Call`, property ICs, allocation, and
//! reload-after-safepoint land in the next Phase 1 step on top of this core.
//!
//! # Contents
//! - [`compile`] — compile one function view, or report why it is unsupported.
//! - [`BaselineCode`] — the [`otter_vm::JitFunctionCode`] handle wrapping the
//!   finalized machine code.
//! - [`JitEntry`] / [`BaselineExit`] — the C ABI of compiled code.
//!
//! # Invariants
//! - **Whole-function opt-in.** Any opcode or operand shape outside the
//!   supported subset aborts the whole compile with [`Unsupported`]; the VM then
//!   silently runs the interpreter. Compiled code never executes a partial
//!   function.
//! - **No safepoints in this subset.** Every emitted op reads operands from and
//!   writes results to the caller-owned register array and never allocates or
//!   calls, so there is no point at which a live value sits in a machine
//!   register across a GC move. The register array stays the single source of
//!   truth, exactly as the interpreter frame does.
//! - **Guard failure = bail, not deopt.** A typed fast-path guard that fails
//!   (non-int32 operand, int32 overflow, non-boolean branch condition) sets the
//!   ABI bail flag and returns; the caller re-runs the function on the
//!   interpreter. Because every guard in this subset fires *before* its result
//!   is stored, a bailed register array is never left partially mutated by the
//!   failing op. (The optimizing direction — fall through to the shared runtime
//!   arith slow path instead of bailing — arrives with the call ABI.)
//!
//! # See also
//! - `JIT_DESIGN.md` §3.2 (backend), §3.5 (GC contract), §4 Phase 1.
//! - [`crate::CompiledCode`] — the executable-memory owner this wraps.

use otter_bytecode::{Op, Operand};
use otter_vm::{JitFunctionCode, JitFunctionView};

use crate::CompiledCode;

/// NaN-box tag for a 32-bit signed integer immediate (`value/tag.rs`).
const TAG_INT32: u64 = 0x7FF9;
/// NaN-box tag for special immediates (undefined/null/hole/boolean).
const TAG_SPECIAL: u64 = 0x7FFA;
/// `SPECIAL` payload for `false`.
const SPECIAL_FALSE: u32 = 3;
/// `SPECIAL` payload for `true`.
const SPECIAL_TRUE: u32 = 4;

/// C ABI of compiled baseline code.
///
/// `regs` points at the function's register window (`Frame.registers`, a
/// contiguous run of [`u64`] NaN-boxed Values). The compiled body reads and
/// writes that array in place. On a normal `Return` it writes `0` through
/// `bailed` and returns the boxed completion Value; on a guard failure it
/// writes `1` through `bailed` and returns `0` (the caller must then run the
/// interpreter). `bailed` is written exactly once per call.
pub type JitEntry = extern "C" fn(regs: *mut u64, bailed: *mut u32) -> u64;

/// Outcome of running compiled baseline code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineExit {
    /// `Return` reached; carries the boxed completion Value.
    Returned(u64),
    /// A typed guard failed; the caller must re-run on the interpreter.
    Bailed,
}

/// Why a function could not be baseline-compiled. Always maps to a silent
/// interpreter fallback; never a JS-visible error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    /// An opcode outside the supported integer subset.
    Opcode(Op),
    /// An operand whose kind/shape the emitter does not handle here.
    OperandShape(&'static str),
    /// A branch whose target byte-PC does not land on an instruction boundary.
    BranchTarget(i64),
    /// A register index whose byte offset exceeds the inline load/store range.
    RegisterRange(u16),
}

/// Finalized baseline machine code for one function.
pub struct BaselineCode {
    code: CompiledCode,
}

impl std::fmt::Debug for BaselineCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaselineCode")
            .field("code_len", &self.code.len())
            .finish()
    }
}

impl JitFunctionCode for BaselineCode {
    fn code_len(&self) -> usize {
        self.code.len()
    }
}

impl BaselineCode {
    /// Raw entry pointer for direct invocation by in-crate execution tests.
    ///
    /// # Safety
    /// The caller transmutes this to [`JitEntry`] and must pass a `regs`
    /// pointer to a register array at least as large as the function's
    /// `register_count`, and a valid `bailed` out-pointer. The code must only
    /// be called while `self` is alive.
    #[must_use]
    pub unsafe fn entry(&self) -> *const u8 {
        // SAFETY: forwarding the documented contract to the owner.
        unsafe { self.code.entry_ptr() }
    }

    /// Run the compiled code over `regs`, returning a structured exit.
    ///
    /// # Safety
    /// `regs` must point at a writable register array with at least the
    /// function's `register_count` `u64` slots.
    #[must_use]
    pub unsafe fn run(&self, regs: *mut u64) -> BaselineExit {
        let mut bailed: u32 = 0;
        // SAFETY: `entry` is the function start of this live mapping; the ABI is
        // `JitEntry` by construction (see `emit_*`). `regs`/`&mut bailed` meet
        // the documented contract.
        let f: JitEntry = unsafe { std::mem::transmute(self.entry()) };
        let ret = f(regs, &mut bailed);
        if bailed == 0 {
            BaselineExit::Returned(ret)
        } else {
            BaselineExit::Bailed
        }
    }
}

/// Byte offset of register `idx` within the register array.
///
/// Returns `Err` when the offset exceeds the unsigned-offset `ldr`/`str` range
/// (scaled imm12 → max element index 4095), which no real frame reaches.
fn reg_offset(idx: u16) -> Result<u32, Unsupported> {
    let off = u32::from(idx) * 8;
    if off > 32760 {
        return Err(Unsupported::RegisterRange(idx));
    }
    Ok(off)
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use super::{
        BaselineCode, Op, Operand, SPECIAL_FALSE, SPECIAL_TRUE, TAG_INT32, TAG_SPECIAL,
        Unsupported, reg_offset,
    };
    use crate::CompiledCode;
    use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
    use otter_vm::JitFunctionView;
    use std::collections::BTreeMap;

    /// Comparison flavors that emit a `cset` from integer `cmp` flags.
    enum Cmp {
        Lt,
        Le,
        Gt,
        Ge,
        Eq,
        Ne,
    }

    /// Emit `Xt = boxed(low32(Xt), tag)`: OR the NaN-box tag into the top 16
    /// bits. The producing op must have written `Xt` through its `W` view, which
    /// on AArch64 already zeroes bits [63:32]; only the tag OR remains. `t` and
    /// `scratch` are x-register numbers.
    macro_rules! box_low32 {
        ($ops:expr, $t:literal, $scratch:literal, $tag:expr) => {
            dynasm!($ops
                ; .arch aarch64
                ; movz X($scratch), ($tag) as u32, lsl #48
                ; orr X($t), X($t), X($scratch)
            );
        };
    }

    /// Emit an int32-tag guard on x-register `r`: bail when `top16(r) != INT32`.
    macro_rules! guard_int32 {
        ($ops:expr, $r:literal, $bail:expr) => {
            dynasm!($ops
                ; .arch aarch64
                ; lsr x14, X($r), #48
                ; movz x15, TAG_INT32 as u32
                ; cmp x14, x15
                ; b.ne =>$bail
            );
        };
    }

    pub(super) fn compile(view: &JitFunctionView) -> Result<BaselineCode, Unsupported> {
        let mut ops = Assembler::new().expect("assembler alloc");
        let bail = ops.new_dynamic_label();

        // A dynamic label per instruction byte-PC, so branches resolve to exact
        // instruction boundaries. BTreeMap keeps emission deterministic.
        let mut labels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
        for instr in &view.instructions {
            labels.insert(instr.byte_pc, ops.new_dynamic_label());
        }
        let target_label = |byte_pc: i64| -> Result<DynamicLabel, Unsupported> {
            u32::try_from(byte_pc)
                .ok()
                .and_then(|pc| labels.get(&pc).copied())
                .ok_or(Unsupported::BranchTarget(byte_pc))
        };

        let entry = ops.offset();
        // Prologue: clear the bail flag once; Return leaves it cleared.
        dynasm!(ops ; .arch aarch64 ; str wzr, [x1]);

        for instr in &view.instructions {
            // Place this instruction's branch-target label.
            dynasm!(ops ; .arch aarch64 ; =>labels[&instr.byte_pc]);
            let ops_ref = instr.operands.as_slice();
            match instr.op {
                Op::LoadInt32 => {
                    let dst = reg(ops_ref, 0)?;
                    let v = imm32(ops_ref, 1)?;
                    let boxed = (TAG_INT32 << 48) | u64::from(v as u32);
                    emit_load_u64(&mut ops, 9, boxed);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadLocal => {
                    let dst = reg(ops_ref, 0)?;
                    let idx = u16::try_from(imm32(ops_ref, 1)?)
                        .map_err(|_| Unsupported::OperandShape("LoadLocal idx"))?;
                    load_reg(&mut ops, 9, idx)?;
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::StoreLocal => {
                    let src = reg(ops_ref, 0)?;
                    let idx = u16::try_from(imm32(ops_ref, 1)?)
                        .map_err(|_| Unsupported::OperandShape("StoreLocal idx"))?;
                    load_reg(&mut ops, 9, src)?;
                    store_reg(&mut ops, 9, idx)?;
                }
                Op::Add | Op::Sub | Op::Mul => {
                    let (dst, lhs, rhs) = reg3(ops_ref)?;
                    load_reg(&mut ops, 9, lhs)?;
                    load_reg(&mut ops, 10, rhs)?;
                    guard_int32!(ops, 9, bail);
                    guard_int32!(ops, 10, bail);
                    match instr.op {
                        Op::Add => dynasm!(ops ; .arch aarch64 ; adds w13, w9, w10 ; b.vs =>bail),
                        Op::Sub => dynasm!(ops ; .arch aarch64 ; subs w13, w9, w10 ; b.vs =>bail),
                        Op::Mul => dynasm!(ops
                            ; .arch aarch64
                            ; smull x13, w9, w10
                            ; cmp x13, w13, sxtw
                            ; b.ne =>bail
                        ),
                        _ => unreachable!(),
                    }
                    box_low32!(ops, 13, 12, TAG_INT32);
                    store_reg(&mut ops, 13, dst)?;
                }
                Op::LessThan => emit_cmp(&mut ops, ops_ref, bail, Cmp::Lt)?,
                Op::LessEq => emit_cmp(&mut ops, ops_ref, bail, Cmp::Le)?,
                Op::GreaterThan => emit_cmp(&mut ops, ops_ref, bail, Cmp::Gt)?,
                Op::GreaterEq => emit_cmp(&mut ops, ops_ref, bail, Cmp::Ge)?,
                Op::Equal => emit_cmp(&mut ops, ops_ref, bail, Cmp::Eq)?,
                Op::NotEqual => emit_cmp(&mut ops, ops_ref, bail, Cmp::Ne)?,
                Op::Jump => {
                    let rel = imm32(ops_ref, 0)?;
                    let next = next_byte_pc(instr);
                    let tgt = target_label(next + i64::from(rel))?;
                    dynasm!(ops ; .arch aarch64 ; b =>tgt);
                }
                Op::JumpIfFalse | Op::JumpIfTrue => {
                    let rel = imm32(ops_ref, 0)?;
                    let cond = reg(ops_ref, 1)?;
                    let next = next_byte_pc(instr);
                    let tgt = target_label(next + i64::from(rel))?;
                    load_reg(&mut ops, 9, cond)?;
                    // Only boolean conditions are supported in this subset.
                    dynasm!(ops
                        ; .arch aarch64
                        ; lsr x14, x9, #48
                        ; movz x15, TAG_SPECIAL as u32
                        ; cmp x14, x15
                        ; b.ne =>bail
                        ; cmp w9, SPECIAL_TRUE
                    );
                    if matches!(instr.op, Op::JumpIfFalse) {
                        dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
                    }
                }
                Op::Return => {
                    let src = reg(ops_ref, 0)?;
                    let off = reg_offset(src)?;
                    // bail flag already cleared in the prologue.
                    dynasm!(ops ; .arch aarch64 ; ldr x0, [x0, off] ; ret);
                }
                other => return Err(Unsupported::Opcode(other)),
            }
        }

        // Shared bail epilogue: set *bailed = 1, return 0.
        dynasm!(ops
            ; .arch aarch64
            ; =>bail
            ; movz w9, #1
            ; str w9, [x1]
            ; movz x0, #0
            ; ret
        );

        let buf = ops.finalize().expect("finalize");
        Ok(BaselineCode {
            code: CompiledCode::new(buf, entry),
        })
    }

    fn emit_cmp(
        ops: &mut Assembler,
        operands: &[Operand],
        bail: DynamicLabel,
        cmp: Cmp,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        guard_int32!(ops, 9, bail);
        guard_int32!(ops, 10, bail);
        dynasm!(ops ; .arch aarch64 ; cmp w9, w10);
        match cmp {
            Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, lt),
            Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, le),
            Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
            Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
            Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
        }
        // cset → {0,1}; map to SPECIAL_FALSE(3)/SPECIAL_TRUE(4).
        debug_assert_eq!(SPECIAL_FALSE + 1, SPECIAL_TRUE);
        dynasm!(ops ; .arch aarch64 ; add w13, w13, SPECIAL_FALSE);
        box_low32!(ops, 13, 12, TAG_SPECIAL);
        store_reg(ops, 13, dst)?;
        Ok(())
    }

    /// `ldr X(t), [x0, #idx*8]`.
    fn load_reg(ops: &mut Assembler, t: u32, idx: u16) -> Result<(), Unsupported> {
        let off = reg_offset(idx)?;
        dynasm!(ops ; .arch aarch64 ; ldr X(t), [x0, off]);
        Ok(())
    }

    /// `str X(t), [x0, #idx*8]`.
    fn store_reg(ops: &mut Assembler, t: u32, idx: u16) -> Result<(), Unsupported> {
        let off = reg_offset(idx)?;
        dynasm!(ops ; .arch aarch64 ; str X(t), [x0, off]);
        Ok(())
    }

    /// Materialize a 64-bit constant into x-register `t` via movz/movk.
    fn emit_load_u64(ops: &mut Assembler, t: u32, v: u64) {
        dynasm!(ops ; .arch aarch64 ; movz X(t), (v & 0xFFFF) as u32);
        if (v >> 16) & 0xFFFF != 0 {
            dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 16) & 0xFFFF) as u32, lsl #16);
        }
        if (v >> 32) & 0xFFFF != 0 {
            dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 32) & 0xFFFF) as u32, lsl #32);
        }
        if (v >> 48) & 0xFFFF != 0 {
            dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 48) & 0xFFFF) as u32, lsl #48);
        }
    }

    fn next_byte_pc(instr: &otter_vm::JitInstrView) -> i64 {
        i64::from(instr.byte_pc) + i64::from(instr.byte_len)
    }

    fn reg(operands: &[Operand], i: usize) -> Result<u16, Unsupported> {
        match operands.get(i) {
            Some(Operand::Register(r)) => Ok(*r),
            _ => Err(Unsupported::OperandShape("expected register")),
        }
    }

    fn imm32(operands: &[Operand], i: usize) -> Result<i32, Unsupported> {
        match operands.get(i) {
            Some(Operand::Imm32(v)) => Ok(*v),
            _ => Err(Unsupported::OperandShape("expected imm32")),
        }
    }

    fn reg3(operands: &[Operand]) -> Result<(u16, u16, u16), Unsupported> {
        Ok((reg(operands, 0)?, reg(operands, 1)?, reg(operands, 2)?))
    }
}

/// Compile a function view to baseline arm64 code, or report why not.
///
/// On non-arm64 hosts this always reports [`Unsupported::Opcode`] for the first
/// instruction (no emitter yet); the x86_64 emitter follows arm64.
#[cfg(target_arch = "aarch64")]
pub fn compile(view: &JitFunctionView) -> Result<BaselineCode, Unsupported> {
    arm64::compile(view)
}

/// Non-arm64 stub: the emitter is arm64-only for now.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile(view: &JitFunctionView) -> Result<BaselineCode, Unsupported> {
    let _ = view;
    Err(Unsupported::OperandShape("baseline emitter is arm64-only"))
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    //! Execution tests: build a [`JitFunctionView`] by hand, compile it, run the
    //! native code over a register array, and check results against the tagged
    //! Value layout. Fixed 4-byte instruction stride keeps branch byte-deltas
    //! easy to compute (`rel = (target_idx - next_idx) * 4`).

    use super::{BaselineExit, SPECIAL_FALSE, TAG_INT32, TAG_SPECIAL, compile};
    use otter_vm::{JitFunctionView, JitInstrView};
    use otter_bytecode::{Op, Operand};

    const STRIDE: u32 = 4;

    fn box_i32(v: i32) -> u64 {
        (TAG_INT32 << 48) | u64::from(v as u32)
    }
    fn unbox_i32(bits: u64) -> i32 {
        bits as u32 as i32
    }

    /// Build a view from `(op, operands)` pairs, assigning a uniform 4-byte
    /// stride so each instruction's `byte_pc = idx * 4`.
    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitFunctionView {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| JitInstrView {
                op: *op,
                byte_pc: idx as u32 * STRIDE,
                byte_len: STRIDE,
                property_ic_site: None,
                operands: operands.clone(),
            })
            .collect();
        JitFunctionView {
            function_id: 0,
            param_count: 1,
            register_count: 8,
            code_byte_len: instrs.len() as u32 * STRIDE,
            is_strict: true,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            instructions,
        }
    }

    /// `rel` operand placing a branch from instruction `from` to instruction
    /// `to` under the uniform stride.
    fn rel(from: usize, to: usize) -> i32 {
        (to as i32 - (from as i32 + 1)) * STRIDE as i32
    }

    fn run(view: &JitFunctionView, regs: &mut [u64]) -> BaselineExit {
        let code = compile(view).expect("compiles");
        // SAFETY: `regs` has `register_count` slots; code outlives the call.
        unsafe { code.run(regs.as_mut_ptr()) }
    }

    #[test]
    fn add_two_ints() {
        // r2 = r0 + r1; return r2
        let v = view(&[
            (Op::Add, vec![Operand::Register(2), Operand::Register(0), Operand::Register(1)]),
            (Op::Return, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(10), box_i32(20), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            BaselineExit::Returned(bits) => assert_eq!(unbox_i32(bits), 30),
            other => panic!("expected Returned, got {other:?}"),
        }
    }

    #[test]
    fn immediate_load_and_sub() {
        // r0 = 100; r1 = 42; r2 = r0 - r1; return r2
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(100)]),
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(42)]),
            (Op::Sub, vec![Operand::Register(2), Operand::Register(0), Operand::Register(1)]),
            (Op::Return, vec![Operand::Register(2)]),
        ]);
        let mut regs = [0u64; 8];
        match run(&v, &mut regs) {
            BaselineExit::Returned(bits) => assert_eq!(unbox_i32(bits), 58),
            other => panic!("expected Returned, got {other:?}"),
        }
    }

    #[test]
    fn negative_immediate_roundtrips() {
        // r0 = -7; return r0
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(-7)]),
            (Op::Return, vec![Operand::Register(0)]),
        ]);
        let mut regs = [0u64; 8];
        match run(&v, &mut regs) {
            BaselineExit::Returned(bits) => assert_eq!(unbox_i32(bits), -7),
            other => panic!("expected Returned, got {other:?}"),
        }
    }

    #[test]
    fn counted_loop_sums_one_to_n() {
        // r0 = n (input); sum=r1, i=r2, one=r4, cond=r3
        // 0: r1 = 0
        // 1: r2 = 1
        // 2: r4 = 1
        // 3: r3 = (r2 <= r0)     ; loop header
        // 4: if !r3 goto 8       ; exit
        // 5: r1 = r1 + r2
        // 6: r2 = r2 + r4
        // 7: goto 3
        // 8: return r1
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
            (Op::LessEq, vec![Operand::Register(3), Operand::Register(2), Operand::Register(0)]),
            (Op::JumpIfFalse, vec![Operand::Imm32(rel(4, 8)), Operand::Register(3)]),
            (Op::Add, vec![Operand::Register(1), Operand::Register(1), Operand::Register(2)]),
            (Op::Add, vec![Operand::Register(2), Operand::Register(2), Operand::Register(4)]),
            (Op::Jump, vec![Operand::Imm32(rel(7, 3))]),
            (Op::Return, vec![Operand::Register(1)]),
        ]);

        for (n, expected) in [(0, 0), (1, 1), (5, 15), (10, 55), (100, 5050)] {
            let mut regs = [box_i32(n), 0, 0, 0, 0, 0, 0, 0];
            match run(&v, &mut regs) {
                BaselineExit::Returned(bits) => {
                    assert_eq!(unbox_i32(bits), expected, "sum 1..={n}");
                }
                other => panic!("n={n}: expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn less_than_produces_boolean() {
        // r2 = (r0 < r1); return r2
        let v = view(&[
            (Op::LessThan, vec![Operand::Register(2), Operand::Register(0), Operand::Register(1)]),
            (Op::Return, vec![Operand::Register(2)]),
        ]);
        let true_bits = (TAG_SPECIAL << 48) | u64::from(SPECIAL_FALSE + 1);
        let false_bits = (TAG_SPECIAL << 48) | u64::from(SPECIAL_FALSE);

        let mut regs = [box_i32(3), box_i32(9), 0, 0, 0, 0, 0, 0];
        assert_eq!(run(&v, &mut regs), BaselineExit::Returned(true_bits));
        let mut regs = [box_i32(9), box_i32(3), 0, 0, 0, 0, 0, 0];
        assert_eq!(run(&v, &mut regs), BaselineExit::Returned(false_bits));
    }

    #[test]
    fn bails_on_non_int_operand() {
        // r2 = r0 + r1; return r2 — but r1 holds a non-int (undefined-ish tag).
        let v = view(&[
            (Op::Add, vec![Operand::Register(2), Operand::Register(0), Operand::Register(1)]),
            (Op::Return, vec![Operand::Register(2)]),
        ]);
        // r1 = a double (top tag below TAG_INT32) → int32 guard fails → bail.
        let mut regs = [box_i32(10), 0x3FF0_0000_0000_0000, 0, 0, 0, 0, 0, 0];
        assert_eq!(run(&v, &mut regs), BaselineExit::Bailed);
    }

    #[test]
    fn unsupported_opcode_reports_not_bail() {
        // A call op is outside the subset → whole-function Unsupported.
        let v = view(&[
            (Op::Call, vec![Operand::Register(0), Operand::Register(1), Operand::Imm32(0)]),
        ]);
        assert!(compile(&v).is_err());
    }
}

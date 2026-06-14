//! Sparkplug-style baseline emitter (arm64).
//!
//! Lowers a [`otter_vm::JitFunctionView`] to native arm64 with **no IR, no
//! register allocation, and no deopt** — one linear pass, one emit routine per
//! supported opcode, branch fixups via dynasm dynamic labels. Operands and
//! results flow through the executing frame's register window; compiled code
//! re-enters the VM for `Call` and `MakeFunction` through safe bridge methods
//! on [`otter_vm::Interpreter`].
//!
//! # ABI
//! Compiled functions are `extern "C" fn(*mut JitCtx) -> JitRet`. The entry
//! loads the register base from `ctx.regs` into a callee-saved register and
//! addresses all locals off it. A normal `Return` yields `JitRet{value, status:
//! 0}`; a failed typed guard yields `status: 1` (the VM re-runs on the
//! interpreter); a re-entered VM call that threw yields `status: 2` with the
//! error parked in `ctx.error`.
//!
//! # GC contract
//! The register window stays rooted on the VM frame stack for the whole call
//! (recursive calls run on a *separate* internal stack and never grow this one,
//! so the register base is stable). Every op reads operands from and writes
//! results to that rooted array — no JS value is ever live in a machine
//! register across a `Call`/`MakeFunction` safepoint — so this tier needs **no
//! GC stack maps**, matching the interpreter's precise `FrameRoots` rooting.
//!
//! # Invariants
//! - **Whole-function opt-in.** Any opcode/operand shape outside the supported
//!   subset aborts the compile with [`Unsupported`]; the VM runs the
//!   interpreter. Compiled code never executes a partial function.
//! - **Guard failure = bail, not deopt.** Non-int32 operands / int32 overflow /
//!   non-boolean branch conditions set `status: 1` and return. Bailing re-runs
//!   the whole function on the interpreter.
//!
//! # See also
//! - `JIT_DESIGN.md` §3.2 (backend), §3.5 (GC contract), §4 Phase 1.

use otter_bytecode::{Op, Operand};
use otter_vm::{
    ExecutionContext, Interpreter, JitExecOutcome, JitFrameStack, JitFunctionCode, JitFunctionView,
    JitReentryPtrs, Value, VmError,
};

use crate::CompiledCode;

/// NaN-box tag for a 32-bit signed integer immediate (`value/tag.rs`).
const TAG_INT32: u64 = 0x7FF9;
/// NaN-box tag for special immediates (undefined/null/hole/boolean).
const TAG_SPECIAL: u64 = 0x7FFA;
/// `SPECIAL` payload for `false`.
const SPECIAL_FALSE: u32 = 3;
/// `SPECIAL` payload for `true`.
const SPECIAL_TRUE: u32 = 4;
/// Largest argument count the `Call` emitter inlines (args passed in registers
/// to the call stub). Functions called with more args fall back.
const MAX_INLINE_ARGS: usize = 4;

/// Re-entry context handed to compiled code. Only `regs` (offset 0) is read by
/// the machine code; the rest is used by the Rust call/make-function stubs.
#[repr(C)]
pub struct JitCtx {
    /// Base of the executing frame's register window (`*mut u64` over Values).
    regs: *mut u64,
    /// Erased back-pointer to the owning interpreter.
    vm: *mut Interpreter,
    /// The VM frame stack the executing frame lives on.
    stack: *mut JitFrameStack,
    /// Execution context for bridge calls.
    context: *const ExecutionContext,
    /// Index of the executing frame within `stack`.
    frame_index: usize,
    /// Error parked by a bridge stub when a re-entered call threw.
    error: Option<VmError>,
}

/// Two-word return of compiled code (`x0`/`x1` on arm64).
#[repr(C)]
struct JitRet {
    value: u64,
    status: u64,
}

/// `status` discriminants in [`JitRet`].
const STATUS_RETURNED: u64 = 0;
const STATUS_BAILED: u64 = 1;
const STATUS_THREW: u64 = 2;

/// Compiled-code entry signature.
type JitEntry = extern "C" fn(*mut JitCtx) -> JitRet;

/// Bridge stub: perform a `Call` from compiled code. Reconstructs VM references
/// from the context and delegates to the safe [`Interpreter::jit_runtime_call`].
/// Returns `0` on success, `1` when the call threw (error parked in `ctx`).
extern "C" fn jit_call_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee: u64,
    argc: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
) -> u64 {
    // SAFETY: `ctx` is the live context passed by `run_entry`; its `vm`/`stack`/
    // `context` pointers are valid for this call and non-aliased (the VM froze
    // its own borrows for the call's duration).
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    let all = [a0 as u16, a1 as u16, a2 as u16, a3 as u16];
    let argc = (argc as usize).min(MAX_INLINE_ARGS);
    match vm.jit_runtime_call(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        callee as u16,
        &all[..argc],
    ) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Bridge stub: build a `MakeFunction` closure from compiled code. Returns `0`
/// on success, `1` when construction threw (error parked in `ctx`).
extern "C" fn jit_make_fn_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_make_function(context, stack, ctx.frame_index, dst as u16, idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Why a function could not be baseline-compiled. Always maps to a silent
/// interpreter fallback; never a JS-visible error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    /// An opcode outside the supported subset.
    Opcode(Op),
    /// An operand whose kind/shape the emitter does not handle here.
    OperandShape(&'static str),
    /// A branch whose target byte-PC does not land on an instruction boundary.
    BranchTarget(i64),
    /// A register index whose byte offset exceeds the inline load/store range.
    RegisterRange(u16),
    /// A `Call` with more arguments than the emitter inlines.
    ArgCount(usize),
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

    fn run_entry(&self, ptrs: JitReentryPtrs) -> JitExecOutcome {
        let stack = ptrs.stack.cast::<JitFrameStack>();
        // SAFETY: `ptrs.stack` is a valid `*mut JitFrameStack` for this call.
        let regs = Interpreter::jit_frame_regs_ptr(unsafe { &mut *stack }, ptrs.frame_index);
        let mut ctx = JitCtx {
            regs,
            vm: ptrs.vm.cast::<Interpreter>(),
            stack,
            context: ptrs.context.cast::<ExecutionContext>(),
            frame_index: ptrs.frame_index,
            error: None,
        };
        // SAFETY: the mapping is live and was emitted with the `JitEntry` ABI.
        let entry: JitEntry = unsafe { std::mem::transmute(self.code.entry_ptr()) };
        let ret = entry(&mut ctx);
        match ret.status {
            STATUS_RETURNED => JitExecOutcome::Returned(Value::from_bits(ret.value)),
            STATUS_BAILED => JitExecOutcome::Bailed,
            _ => JitExecOutcome::Threw(ctx.error.take().unwrap_or(VmError::InvalidOperand)),
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
        BaselineCode, MAX_INLINE_ARGS, Op, Operand, SPECIAL_FALSE, SPECIAL_TRUE, STATUS_BAILED,
        STATUS_RETURNED, STATUS_THREW, TAG_INT32, TAG_SPECIAL, Unsupported, jit_call_stub,
        jit_make_fn_stub, reg_offset,
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

    /// Emit `Xt |= tag << 48`. The producing op wrote `Xt` through its `W` view,
    /// which on AArch64 already zeroes bits [63:32]; only the tag OR remains.
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

    /// Emit the function epilogue (restore callee-saved + frame, return). `x0`
    /// (value) and `x1` (status) must already be set.
    fn emit_epilogue(ops: &mut Assembler) {
        dynasm!(ops
            ; .arch aarch64
            ; ldp x19, x20, [sp, #16]
            ; ldp x29, x30, [sp], #32
            ; ret
        );
    }

    pub(super) fn compile(view: &JitFunctionView) -> Result<BaselineCode, Unsupported> {
        let mut ops = Assembler::new().expect("assembler alloc");
        let bail = ops.new_dynamic_label();
        let threw = ops.new_dynamic_label();

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
        // Prologue: save fp/lr + callee-saved bases; x20 = ctx, x19 = regs base.
        dynasm!(ops
            ; .arch aarch64
            ; stp x29, x30, [sp, #-32]!
            ; stp x19, x20, [sp, #16]
            ; mov x29, sp
            ; mov x20, x0
            ; ldr x19, [x20]
        );

        for instr in &view.instructions {
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
                    let idx = local_index(ops_ref, 1)?;
                    load_reg(&mut ops, 9, idx)?;
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::StoreLocal => {
                    let src = reg(ops_ref, 0)?;
                    let idx = local_index(ops_ref, 1)?;
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
                // `ToPrimitive`/`ToNumeric` are identity on a number; emit a
                // guarded move (int32 → copy, else bail).
                Op::ToPrimitive | Op::ToNumeric => {
                    let dst = reg(ops_ref, 0)?;
                    let src = reg(ops_ref, 1)?;
                    load_reg(&mut ops, 9, src)?;
                    guard_int32!(ops, 9, bail);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::Jump => {
                    let rel = imm32(ops_ref, 0)?;
                    let tgt = target_label(branch_target(instr, rel))?;
                    dynasm!(ops ; .arch aarch64 ; b =>tgt);
                }
                Op::JumpIfFalse | Op::JumpIfTrue => {
                    let rel = imm32(ops_ref, 0)?;
                    let cond = reg(ops_ref, 1)?;
                    let tgt = target_label(branch_target(instr, rel))?;
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
                Op::MakeFunction => {
                    let dst = reg(ops_ref, 0)?;
                    let idx = const_index(ops_ref, 1)?;
                    // jit_make_fn_stub(ctx=x20, dst, idx) -> status in x0.
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(&mut ops, 2, u64::from(idx));
                    emit_call_stub(&mut ops, jit_make_fn_stub as *const () as usize, threw);
                }
                Op::Call => {
                    emit_call(&mut ops, ops_ref, threw)?;
                }
                Op::Return | Op::ReturnValue => {
                    let src = reg(ops_ref, 0)?;
                    let off = reg_offset(src)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x0, [x19, off]
                        ; movz x1, STATUS_RETURNED as u32
                    );
                    emit_epilogue(&mut ops);
                }
                Op::ReturnUndefined => {
                    let undef = TAG_SPECIAL << 48; // SPECIAL_UNDEFINED == 0
                    emit_load_u64(&mut ops, 0, undef);
                    dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_RETURNED as u32);
                    emit_epilogue(&mut ops);
                }
                other => return Err(Unsupported::Opcode(other)),
            }
        }

        // Shared bail epilogue: status = 1, value = 0.
        dynasm!(ops
            ; .arch aarch64
            ; =>bail
            ; movz x0, #0
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(&mut ops);
        // Shared throw epilogue: status = 2 (error parked in ctx by the stub).
        dynasm!(ops
            ; .arch aarch64
            ; =>threw
            ; movz x0, #0
            ; movz x1, STATUS_THREW as u32
        );
        emit_epilogue(&mut ops);

        let buf = ops.finalize().expect("finalize");
        Ok(BaselineCode {
            code: CompiledCode::new(buf, entry),
        })
    }

    /// Emit `blr` to a Rust stub at `addr` and branch to `threw` on nonzero
    /// status. The stub's argument registers (`x0`..) must already be set.
    fn emit_call_stub(ops: &mut Assembler, addr: usize, threw: DynamicLabel) {
        emit_load_u64(ops, 16, addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x0, =>threw
        );
    }

    /// Emit a `Call`: gather operands, set the stub ABI registers, call.
    fn emit_call(
        ops: &mut Assembler,
        operands: &[Operand],
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let dst = reg(operands, 0)?;
        let callee = reg(operands, 1)?;
        let argc = const_index(operands, 2)? as usize;
        if argc > MAX_INLINE_ARGS {
            return Err(Unsupported::ArgCount(argc));
        }
        // jit_call_stub(ctx, dst, callee, argc, a0, a1, a2, a3) -> status.
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, x20
            ; movz x1, dst as u32
            ; movz x2, callee as u32
            ; movz x3, argc as u32
        );
        for slot in 0..MAX_INLINE_ARGS {
            let areg = if slot < argc {
                reg(operands, 3 + slot)?
            } else {
                0
            };
            // arg registers map to x4..x7.
            let xn = 4 + slot as u32;
            dynasm!(ops ; .arch aarch64 ; movz X(xn), areg as u32);
        }
        emit_call_stub(ops, jit_call_stub as *const () as usize, threw);
        Ok(())
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

    /// `ldr X(t), [x19, #idx*8]`.
    fn load_reg(ops: &mut Assembler, t: u32, idx: u16) -> Result<(), Unsupported> {
        let off = reg_offset(idx)?;
        dynasm!(ops ; .arch aarch64 ; ldr X(t), [x19, off]);
        Ok(())
    }

    /// `str X(t), [x19, #idx*8]`.
    fn store_reg(ops: &mut Assembler, t: u32, idx: u16) -> Result<(), Unsupported> {
        let off = reg_offset(idx)?;
        dynasm!(ops ; .arch aarch64 ; str X(t), [x19, off]);
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

    /// Target byte-PC of a relative branch. The interpreter computes
    /// `frame.pc + 1 + offset` (relative to the byte after the branch opcode,
    /// `operand_decode::apply_branch`), so byte_len is irrelevant here.
    fn branch_target(instr: &otter_vm::JitInstrView, rel: i32) -> i64 {
        i64::from(instr.byte_pc) + 1 + i64::from(rel)
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

    /// A local index encoded as an inline immediate (`LoadLocal`/`StoreLocal`).
    fn local_index(operands: &[Operand], i: usize) -> Result<u16, Unsupported> {
        u16::try_from(imm32(operands, i)?).map_err(|_| Unsupported::OperandShape("local index"))
    }

    /// A constant-pool index operand (`MakeFunction` body id, `Call` argc).
    fn const_index(operands: &[Operand], i: usize) -> Result<u32, Unsupported> {
        match operands.get(i) {
            Some(Operand::ConstIndex(n)) => Ok(*n),
            _ => Err(Unsupported::OperandShape("expected const index")),
        }
    }

    fn reg3(operands: &[Operand]) -> Result<(u16, u16, u16), Unsupported> {
        Ok((reg(operands, 0)?, reg(operands, 1)?, reg(operands, 2)?))
    }
}

/// Compile a function view to baseline arm64 code, or report why not.
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
    //! Execution tests for the call-free integer subset. They drive compiled
    //! code through a `JitCtx` whose `vm`/`stack`/`context` are null — valid
    //! because these functions never reach a `Call`/`MakeFunction` stub — and a
    //! `regs` pointer at a local register array. Fixed 4-byte instruction stride
    //! keeps branch byte-deltas trivial (`rel = (target - next) * 4`).

    use super::{JitCtx, JitEntry, JitRet, STATUS_RETURNED, TAG_INT32, TAG_SPECIAL, compile};
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitFunctionView, JitInstrView};

    const STRIDE: u32 = 4;

    enum Exit {
        Returned(u64),
        Bailed,
    }

    fn box_i32(v: i32) -> u64 {
        (TAG_INT32 << 48) | u64::from(v as u32)
    }
    fn unbox_i32(bits: u64) -> i32 {
        bits as u32 as i32
    }

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

    // Branch encoding: target = branch_byte_pc + 1 + rel (see `branch_target`),
    // with branch_byte_pc = from*STRIDE and target = to*STRIDE.
    fn rel(from: usize, to: usize) -> i32 {
        (to as i32 - from as i32) * STRIDE as i32 - 1
    }

    fn run(view: &JitFunctionView, regs: &mut [u64]) -> Exit {
        let code = compile(view).expect("compiles");
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            vm: std::ptr::null_mut(),
            stack: std::ptr::null_mut(),
            context: std::ptr::null(),
            frame_index: 0,
            error: None,
        };
        // SAFETY: integer-only function; never dereferences the null vm/stack.
        let entry: JitEntry = unsafe { std::mem::transmute(code.code.entry_ptr()) };
        let JitRet { value, status } = entry(&mut ctx);
        if status == STATUS_RETURNED {
            Exit::Returned(value)
        } else {
            Exit::Bailed
        }
    }

    fn expect_int(view: &JitFunctionView, regs: &mut [u64], expected: i32) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_i32(bits), expected),
            Exit::Bailed => panic!("expected Returned({expected}), got Bailed"),
        }
    }

    #[test]
    fn add_two_ints() {
        let v = view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(10), box_i32(20), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 30);
    }

    #[test]
    fn immediate_load_and_sub() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(100)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(1), Operand::Imm32(42)],
            ),
            (
                Op::Sub,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [0u64; 8];
        expect_int(&v, &mut regs, 58);
    }

    #[test]
    fn negative_immediate_roundtrips() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(-7)],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let mut regs = [0u64; 8];
        expect_int(&v, &mut regs, -7);
    }

    #[test]
    fn counted_loop_sums_one_to_n() {
        // r0=n; sum=r1, i=r2, one=r4, cond=r3
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
            (
                Op::LessEq,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(rel(4, 8)), Operand::Register(3)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(4),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(rel(7, 3))]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        for (n, expected) in [(0, 0), (1, 1), (5, 15), (10, 55), (100, 5050)] {
            let mut regs = [box_i32(n), 0, 0, 0, 0, 0, 0, 0];
            expect_int(&v, &mut regs, expected);
        }
    }

    #[test]
    fn less_than_produces_boolean() {
        let v = view(&[
            (
                Op::LessThan,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let true_bits = (TAG_SPECIAL << 48) | u64::from(super::SPECIAL_TRUE);
        let false_bits = (TAG_SPECIAL << 48) | u64::from(super::SPECIAL_FALSE);
        let mut regs = [box_i32(3), box_i32(9), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Returned(b) if b == true_bits));
        let mut regs = [box_i32(9), box_i32(3), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Returned(b) if b == false_bits));
    }

    #[test]
    fn bails_on_non_int_operand() {
        let v = view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(10), 0x3FF0_0000_0000_0000, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn unsupported_call_arg_overflow_reports_err() {
        // argc beyond MAX_INLINE_ARGS → Unsupported (not a compile success).
        let v = view(&[(
            Op::Call,
            vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(8),
                Operand::Register(2),
            ],
        )]);
        assert!(compile(&v).is_err());
    }
}

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

/// NaN-box high-16 for the canonical quiet NaN double (`value/tag.rs`).
/// A non-int double result whose own bits land in the tagged range is
/// canonicalised to this so it stays a valid `Number(NaN)`.
const TAG_NAN: u64 = 0x7FF8;
/// NaN-box tag for a 32-bit signed integer immediate (`value/tag.rs`).
const TAG_INT32: u64 = 0x7FF9;
/// NaN-box tag for special immediates (undefined/null/hole/boolean).
const TAG_SPECIAL: u64 = 0x7FFA;
/// `SPECIAL` payload for the internal array/`this` hole sentinel.
const SPECIAL_HOLE: u64 = 2;
/// `SPECIAL` payload for `false`.
const SPECIAL_FALSE: u32 = 3;
/// `SPECIAL` payload for `true`.
const SPECIAL_TRUE: u32 = 4;
/// Largest argument count the `Call` emitter inlines (args passed in registers
/// to the call stub). Functions called with more args fall back.
const MAX_INLINE_ARGS: usize = 4;

/// Re-entry context handed to compiled code. The machine code reads `regs`
/// (offset 0) and `self_closure` (offset 8) directly by offset — keep those two
/// first; the rest is used by the Rust call/make-function stubs.
#[repr(C)]
pub struct JitCtx {
    /// Base of the executing frame's register window (`*mut u64` over Values).
    regs: *mut u64,
    /// Boxed `Value` bits of this frame's SELF closure (the named-function self
    /// binding). Read directly by a `MakeFunction`-of-self at offset 8.
    self_closure: u64,
    /// Boxed `Value` bits of this frame's `this` binding, read once at entry.
    /// A `LoadThis` reads it directly at offset 16 (and bails on a hole).
    this_value: u64,
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

/// Bridge stub: perform a named `LoadProperty` from compiled code, delegating
/// to the safe [`Interpreter::jit_runtime_load_property`]. Returns `0` on
/// success, `1` when the read threw (error parked in `ctx`).
extern "C" fn jit_load_prop_stub(
    ctx: *mut JitCtx,
    dst: u64,
    obj: u64,
    name_idx: u64,
    site: u64,
) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_load_property(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        obj as u16,
        name_idx as u32,
        site as usize,
    ) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Bridge stub: perform a named `StoreProperty` from compiled code, delegating
/// to the safe [`Interpreter::jit_runtime_store_property`]. Returns `0` on
/// success, `1` when the write threw (error parked in `ctx`).
extern "C" fn jit_store_prop_stub(
    ctx: *mut JitCtx,
    obj: u64,
    name_idx: u64,
    src: u64,
    site: u64,
) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_store_property(
        context,
        stack,
        ctx.frame_index,
        obj as u16,
        name_idx as u32,
        src as u16,
        site as usize,
    ) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Bridge stub: perform a computed `LoadElement` (`recv[idx]`) from compiled
/// code, delegating to the safe [`Interpreter::jit_runtime_load_element`].
/// Returns `0` on success, `1` when the read threw (error parked in `ctx`).
extern "C" fn jit_load_element_stub(ctx: *mut JitCtx, dst: u64, recv: u64, idx: u64) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_load_element(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        recv as u16,
        idx as u16,
    ) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Bridge stub: perform a `LoadGlobalOrThrow` from compiled code, delegating to
/// the safe [`Interpreter::jit_runtime_load_global`]. Returns `0` on success,
/// `1` when the read threw (unbound identifier / throwing accessor; error
/// parked in `ctx`).
extern "C" fn jit_load_global_stub(ctx: *mut JitCtx, dst: u64, name_idx: u64) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_load_global(context, stack, ctx.frame_index, dst as u16, name_idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Bridge stub: perform a `CallMethodValue` (`recv.name(args…)`) from compiled
/// code, delegating to the safe [`Interpreter::jit_runtime_call_method`].
/// Returns `0` on success, `1` when the call threw (error parked in `ctx`).
/// At most [`MAX_INLINE_ARGS`] argument registers are passed (a0..a2 here,
/// since `recv` and `name_idx` consume two of the eight ABI registers).
#[allow(clippy::too_many_arguments)]
extern "C" fn jit_call_method_stub(
    ctx: *mut JitCtx,
    dst: u64,
    recv: u64,
    name_idx: u64,
    argc: u64,
    a0: u64,
    a1: u64,
    a2: u64,
) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    let all = [a0 as u16, a1 as u16, a2 as u16];
    let argc = (argc as usize).min(all.len());
    match vm.jit_runtime_call_method(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        recv as u16,
        name_idx as u32,
        &all[..argc],
    ) {
        Ok(()) => 0,
        Err(err) => {
            ctx.error = Some(err);
            1
        }
    }
}

/// Bridge stub: perform a computed `StoreElement` (`recv[idx] = src`) from
/// compiled code, delegating to the safe
/// [`Interpreter::jit_runtime_store_element`]. Returns `0` on success, `1` when
/// the write threw (error parked in `ctx`).
extern "C" fn jit_store_element_stub(
    ctx: *mut JitCtx,
    recv: u64,
    idx: u64,
    src: u64,
    scratch: u64,
) -> u64 {
    // SAFETY: see `jit_call_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_store_element(
        context,
        stack,
        ctx.frame_index,
        recv as u16,
        idx as u16,
        src as u16,
        scratch as u16,
    ) {
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
        let vm = ptrs.vm.cast::<Interpreter>();
        // SAFETY: `ptrs.stack` is a valid `*mut JitFrameStack` for this call.
        let regs = Interpreter::jit_frame_regs_ptr(unsafe { &mut *stack }, ptrs.frame_index);
        // SAFETY: `ptrs.vm`/`ptrs.stack` are valid for this call and not aliased
        // by a live `&mut` (the VM froze its borrows); read the self closure up
        // front so a `MakeFunction`-of-self needs no Rust round-trip.
        let self_closure = unsafe { (*vm).jit_frame_self_closure_bits(&*stack, ptrs.frame_index) };
        // SAFETY: same validity/aliasing contract as `self_closure` above.
        let this_value = unsafe { (*vm).jit_frame_this_bits(&*stack, ptrs.frame_index) };
        let mut ctx = JitCtx {
            regs,
            self_closure,
            this_value,
            vm,
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
        BaselineCode, MAX_INLINE_ARGS, Op, Operand, SPECIAL_FALSE, SPECIAL_HOLE, SPECIAL_TRUE,
        STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, TAG_INT32, TAG_NAN, TAG_SPECIAL, Unsupported,
        jit_call_method_stub, jit_call_stub, jit_load_element_stub, jit_load_global_stub,
        jit_load_prop_stub, jit_make_fn_stub, jit_store_element_stub, jit_store_prop_stub,
        reg_offset,
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

    /// Emit a "value is a Number" guard on x-register `r`: bail unless the
    /// high-16 is `int32` (`0x7FF9`) or any double pattern. Non-numbers
    /// (special / pointer / function-id, high-16 in `0x7FFA..=0x7FFF`) bail.
    macro_rules! guard_number {
        ($ops:expr, $r:literal, $bail:expr) => {
            dynasm!($ops
                ; .arch aarch64
                ; lsr x14, X($r), #48
                ; movz x15, 0x7FFA
                ; sub x14, x14, x15
                ; cmp x14, #5                 // 0x7FFF - 0x7FFA
                ; b.ls =>$bail                // high-16 in [0x7FFA, 0x7FFF] → non-number
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
                Op::LoadUndefined => {
                    let dst = reg(ops_ref, 0)?;
                    // SPECIAL payload 0 == undefined.
                    emit_load_u64(&mut ops, 9, TAG_SPECIAL << 48);
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
                    let float_path = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    // int32 fast path: take it only when both operands are int32.
                    // Any non-int32 operand — or an int32 result that overflows —
                    // falls through to the double path (numbers are f64; an
                    // overflowing integer product is just its exact f64 value),
                    // never to `bail`.
                    dynasm!(ops
                        ; .arch aarch64
                        ; lsr x14, x9, #48
                        ; movz x15, TAG_INT32 as u32
                        ; cmp x14, x15
                        ; b.ne =>float_path
                        ; lsr x14, x10, #48
                        ; cmp x14, x15
                        ; b.ne =>float_path
                    );
                    match instr.op {
                        Op::Add => {
                            dynasm!(ops ; .arch aarch64 ; adds w13, w9, w10 ; b.vs =>float_path)
                        }
                        Op::Sub => {
                            dynasm!(ops ; .arch aarch64 ; subs w13, w9, w10 ; b.vs =>float_path)
                        }
                        Op::Mul => dynasm!(ops
                            ; .arch aarch64
                            ; smull x13, w9, w10
                            ; cmp x13, w13, sxtw
                            ; b.ne =>float_path
                        ),
                        _ => unreachable!(),
                    }
                    box_low32!(ops, 13, 12, TAG_INT32);
                    store_reg(&mut ops, 13, dst)?;
                    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
                    // Double path: decode both operands to f64, compute, rebox.
                    emit_num_to_double(&mut ops, 9, 0, bail);
                    emit_num_to_double(&mut ops, 10, 1, bail);
                    match instr.op {
                        Op::Add => dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1),
                        Op::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
                        Op::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
                        _ => unreachable!(),
                    }
                    emit_box_double(&mut ops, 2, 13);
                    store_reg(&mut ops, 13, dst)?;
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                // Division always yields a Number (f64) in ECMAScript — even
                // `6 / 2` is the Number `3`, equal to int32 `3` — so there is no
                // int fast path; decode both operands to f64 and `fdiv`.
                Op::Div => {
                    let (dst, lhs, rhs) = reg3(ops_ref)?;
                    load_reg(&mut ops, 9, lhs)?;
                    load_reg(&mut ops, 10, rhs)?;
                    emit_num_to_double(&mut ops, 9, 0, bail);
                    emit_num_to_double(&mut ops, 10, 1, bail);
                    dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1);
                    emit_box_double(&mut ops, 2, 13);
                    store_reg(&mut ops, 13, dst)?;
                }
                Op::LessThan => emit_cmp(&mut ops, ops_ref, bail, Cmp::Lt)?,
                Op::LessEq => emit_cmp(&mut ops, ops_ref, bail, Cmp::Le)?,
                Op::GreaterThan => emit_cmp(&mut ops, ops_ref, bail, Cmp::Gt)?,
                Op::GreaterEq => emit_cmp(&mut ops, ops_ref, bail, Cmp::Ge)?,
                Op::Equal => emit_cmp(&mut ops, ops_ref, bail, Cmp::Eq)?,
                Op::NotEqual => emit_cmp(&mut ops, ops_ref, bail, Cmp::Ne)?,
                // `ToPrimitive`/`ToNumeric` are identity on a number (int32 or
                // double); emit a guarded move. Non-numbers (objects needing
                // `valueOf`, strings, etc.) bail to the interpreter.
                Op::ToPrimitive | Op::ToNumeric => {
                    let dst = reg(ops_ref, 0)?;
                    let src = reg(ops_ref, 1)?;
                    load_reg(&mut ops, 9, src)?;
                    guard_number!(ops, 9, bail);
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
                Op::MakeFunction if instr.make_self => {
                    // SELF binding: the closure value is precomputed in
                    // `JitCtx.self_closure` (offset 8 from x20), so read it
                    // straight into `dst` — no Rust round-trip through
                    // `jit_make_fn_stub`/`run_make_function_reg`.
                    let dst = reg(ops_ref, 0)?;
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, #8]);
                    store_reg(&mut ops, 9, dst)?;
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
                // `recv.name(args…)` — resolve + invoke via the safe bridge.
                // Operands: dst, recv, name-const, argc-const, then argc arg
                // registers. `recv` and `name_idx` consume two ABI registers, so
                // at most three inline args fit (x5..x7).
                Op::CallMethodValue => {
                    const MAX_METHOD_ARGS: usize = 3;
                    let dst = reg(ops_ref, 0)?;
                    let recv = reg(ops_ref, 1)?;
                    let name = const_index(ops_ref, 2)?;
                    let argc = const_index(ops_ref, 3)? as usize;
                    if argc > MAX_METHOD_ARGS {
                        return Err(Unsupported::ArgCount(argc));
                    }
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, recv as u32
                    );
                    emit_load_u64(&mut ops, 3, u64::from(name));
                    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
                    for slot in 0..MAX_METHOD_ARGS {
                        let areg = if slot < argc {
                            reg(ops_ref, 4 + slot)?
                        } else {
                            0
                        };
                        let xn = 5 + slot as u32;
                        dynasm!(ops ; .arch aarch64 ; movz X(xn), areg as u32);
                    }
                    emit_call_stub(&mut ops, jit_call_method_stub as *const () as usize, threw);
                }
                // `recv[idx]` — delegate to the safe element-load bridge (covers
                // dense/sparse arrays, typed arrays, strings, object `[[Get]]`).
                Op::LoadElement => {
                    let dst = reg(ops_ref, 0)?;
                    let recv = reg(ops_ref, 1)?;
                    let idx = reg(ops_ref, 2)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, recv as u32
                        ; movz x3, idx as u32
                    );
                    emit_call_stub(&mut ops, jit_load_element_stub as *const () as usize, threw);
                }
                // `recv[idx] = src` — delegate to the safe element-store bridge.
                // Operands: recv, idx, src, scratch.
                Op::StoreElement => {
                    let recv = reg(ops_ref, 0)?;
                    let idx = reg(ops_ref, 1)?;
                    let src = reg(ops_ref, 2)?;
                    let scratch = reg(ops_ref, 3)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, recv as u32
                        ; movz x2, idx as u32
                        ; movz x3, src as u32
                        ; movz x4, scratch as u32
                    );
                    emit_call_stub(
                        &mut ops,
                        jit_store_element_stub as *const () as usize,
                        threw,
                    );
                }
                // `dst = global[name]` or throw — delegate to the safe bridge.
                Op::LoadGlobalOrThrow => {
                    let dst = reg(ops_ref, 0)?;
                    let name = const_index(ops_ref, 1)?;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(&mut ops, 2, u64::from(name));
                    emit_call_stub(&mut ops, jit_load_global_stub as *const () as usize, threw);
                }
                // `dst = ToNumeric(src) + delta` (§13.4 UpdateExpression). Int32
                // fast path with overflow → double; double path otherwise.
                Op::Increment => {
                    let dst = reg(ops_ref, 0)?;
                    let src = reg(ops_ref, 1)?;
                    let delta = imm32(ops_ref, 2)?;
                    load_reg(&mut ops, 9, src)?;
                    emit_load_u64(&mut ops, 12, u64::from(delta as u32));
                    let float_path = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        ; lsr x14, x9, #48
                        ; movz x15, TAG_INT32 as u32
                        ; cmp x14, x15
                        ; b.ne =>float_path
                        ; adds w13, w9, w12
                        ; b.vs =>float_path
                    );
                    box_low32!(ops, 13, 11, TAG_INT32);
                    store_reg(&mut ops, 13, dst)?;
                    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
                    emit_num_to_double(&mut ops, 9, 0, bail);
                    dynasm!(ops ; .arch aarch64 ; scvtf d1, w12 ; fadd d2, d0, d1);
                    emit_box_double(&mut ops, 2, 13);
                    store_reg(&mut ops, 13, dst)?;
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::LoadThis => {
                    // `this` bits are precomputed in `JitCtx.this_value`
                    // (offset 16 from x20). Bail on a hole — a derived-ctor
                    // `this`-before-super, which the interpreter resolves.
                    let dst = reg(ops_ref, 0)?;
                    let hole = (TAG_SPECIAL << 48) | SPECIAL_HOLE;
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, #16]);
                    emit_load_u64(&mut ops, 12, hole);
                    dynasm!(ops ; .arch aarch64 ; cmp x9, x12 ; b.eq =>bail);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadProperty => {
                    // jit_load_prop_stub(ctx=x20, dst, obj, name_idx, site).
                    // `site` is the dense IC index from the snapshot, used by
                    // the bridge for the monomorphic fast path (PC-keyed lookup
                    // is unavailable at PC 0); `usize::MAX` means "no site".
                    let dst = reg(ops_ref, 0)?;
                    let obj = reg(ops_ref, 1)?;
                    let name = const_index(ops_ref, 2)?;
                    let site = instr.property_ic_site.unwrap_or(usize::MAX) as u64;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, obj as u32
                    );
                    emit_load_u64(&mut ops, 3, u64::from(name));
                    emit_load_u64(&mut ops, 4, site);
                    emit_call_stub(&mut ops, jit_load_prop_stub as *const () as usize, threw);
                }
                Op::StoreProperty => {
                    // Operands: obj, name_const, src, scratch_dst.
                    // jit_store_prop_stub(ctx=x20, obj, name_idx, src) -> status.
                    let obj = reg(ops_ref, 0)?;
                    let name = const_index(ops_ref, 1)?;
                    let src = reg(ops_ref, 2)?;
                    let site = instr.property_ic_site.unwrap_or(usize::MAX) as u64;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, obj as u32
                    );
                    emit_load_u64(&mut ops, 2, u64::from(name));
                    dynasm!(ops ; .arch aarch64 ; movz x3, src as u32);
                    emit_load_u64(&mut ops, 4, site);
                    emit_call_stub(&mut ops, jit_store_prop_stub as *const () as usize, threw);
                }
                Op::BitwiseOr => {
                    let (dst, lhs, rhs) = reg3(ops_ref)?;
                    load_reg(&mut ops, 9, lhs)?;
                    load_reg(&mut ops, 10, rhs)?;
                    guard_int32!(ops, 9, bail);
                    guard_int32!(ops, 10, bail);
                    dynasm!(ops ; .arch aarch64 ; orr w13, w9, w10);
                    box_low32!(ops, 13, 12, TAG_INT32);
                    store_reg(&mut ops, 13, dst)?;
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
        let float_path = ops.new_dynamic_label();
        let have_bool = ops.new_dynamic_label();
        // int32 fast path: both operands int32 → signed integer compare.
        dynasm!(ops
            ; .arch aarch64
            ; lsr x14, x9, #48
            ; movz x15, TAG_INT32 as u32
            ; cmp x14, x15
            ; b.ne =>float_path
            ; lsr x14, x10, #48
            ; cmp x14, x15
            ; b.ne =>float_path
            ; cmp w9, w10
        );
        match cmp {
            Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, lt),
            Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, le),
            Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
            Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
            Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
        }
        dynasm!(ops ; .arch aarch64 ; b =>have_bool ; =>float_path);
        // Double path: decode both to f64 and `fcmp`. The FP condition codes
        // differ from the integer ones so an unordered (NaN) compare yields the
        // ECMAScript result (every relational compare false, `!=` true):
        // Lt→mi, Le→ls, Gt→gt, Ge→ge, Eq→eq, Ne→ne.
        emit_num_to_double(ops, 9, 0, bail);
        emit_num_to_double(ops, 10, 1, bail);
        dynasm!(ops ; .arch aarch64 ; fcmp d0, d1);
        match cmp {
            Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, mi),
            Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, ls),
            Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
            Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
            Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
        }
        dynasm!(ops ; .arch aarch64 ; =>have_bool);
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

    /// Decode the `Number` in x-register `src_x` into f64 register `dst_d`.
    ///
    /// `int32` payloads sign-convert (`scvtf`); any non-tagged bit pattern is a
    /// double used verbatim (`fmov`); a tagged non-number (special / pointer /
    /// function-id, high-16 in `0x7FFA..=0x7FFF`) bails to the interpreter.
    /// Uses scratch GPRs x14/x15.
    fn emit_num_to_double(ops: &mut Assembler, src_x: u32, dst_d: u32, bail: DynamicLabel) {
        let is_non_int = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; lsr x14, X(src_x), #48
            ; movz x15, TAG_INT32 as u32
            ; cmp x14, x15
            ; b.ne =>is_non_int
            ; scvtf D(dst_d), W(src_x)          // int32: signed 32-bit → f64
            ; b =>done
            ; =>is_non_int
            // Double iff high-16 ∉ [0x7FF9, 0x7FFF]; we already know it is not
            // 0x7FF9. `x14 - 0x7FF9` lands in 1..=6 (unsigned) for the tagged
            // non-number range 0x7FFA..=0x7FFF; everything else (incl. the NaN
            // tag 0x7FF8, which wraps high) is a real double.
            ; sub x14, x14, x15
            ; cmp x14, #6
            ; b.ls =>bail
            ; fmov D(dst_d), X(src_x)
            ; =>done
        );
    }

    /// Box the f64 in register `src_d` into x-register `dst_x` as a `Value`.
    ///
    /// A non-NaN double's bits are a valid `Value` verbatim; a NaN result is
    /// canonicalised to the single quiet-NaN pattern so it never aliases a tag.
    /// Uses no scratch GPRs beyond `dst_x`.
    fn emit_box_double(ops: &mut Assembler, src_d: u32, dst_x: u32) {
        let ready = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; fmov X(dst_x), D(src_d)
            ; fcmp D(src_d), D(src_d)
            ; b.vc =>ready                       // ordered (not NaN) → keep bits
            ; movz X(dst_x), TAG_NAN as u32, lsl #48
            ; =>ready
        );
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
                make_self: false,
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
            self_closure: 0,
            this_value: 0,
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

    fn box_f64(v: f64) -> u64 {
        v.to_bits()
    }
    fn unbox_f64(bits: u64) -> f64 {
        f64::from_bits(bits)
    }
    fn add_view() -> JitFunctionView {
        view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ])
    }

    #[test]
    fn float_comparisons_including_nan() {
        let t = (TAG_SPECIAL << 48) | u64::from(super::SPECIAL_TRUE);
        let f = (TAG_SPECIAL << 48) | u64::from(super::SPECIAL_FALSE);
        let cmp_view = |op: Op| {
            view(&[
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ])
        };
        let run_cmp = |op: Op, a: u64, b: u64| {
            let v = cmp_view(op);
            let mut regs = [a, b, 0, 0, 0, 0, 0, 0];
            match run(&v, &mut regs) {
                Exit::Returned(bits) => bits,
                Exit::Bailed => panic!("cmp bailed"),
            }
        };
        // ordered doubles
        assert_eq!(run_cmp(Op::LessThan, box_f64(1.5), box_f64(2.5)), t);
        assert_eq!(run_cmp(Op::LessThan, box_f64(2.5), box_f64(1.5)), f);
        assert_eq!(run_cmp(Op::LessEq, box_f64(2.5), box_f64(2.5)), t);
        assert_eq!(run_cmp(Op::GreaterThan, box_f64(3.0), box_f64(2.0)), t);
        assert_eq!(run_cmp(Op::Equal, box_f64(2.0), box_f64(2.0)), t);
        // mixed int/double
        assert_eq!(run_cmp(Op::LessThan, box_i32(1), box_f64(2.5)), t);
        assert_eq!(run_cmp(Op::GreaterEq, box_f64(4.0), box_i32(4)), t);
        // NaN: every relational compare is false, `!=` is true.
        let nan = box_f64(f64::NAN);
        assert_eq!(run_cmp(Op::LessThan, nan, box_f64(1.0)), f);
        assert_eq!(run_cmp(Op::LessEq, nan, box_f64(1.0)), f);
        assert_eq!(run_cmp(Op::GreaterThan, nan, box_f64(1.0)), f);
        assert_eq!(run_cmp(Op::Equal, nan, nan), f);
        assert_eq!(run_cmp(Op::NotEqual, nan, box_f64(1.0)), t);
    }

    #[test]
    fn bails_on_non_number_operand() {
        // A tagged non-number (undefined = TAG_SPECIAL, payload 0) must bail to
        // the interpreter — only int32 and doubles take the compiled arith path.
        let v = add_view();
        let mut regs = [box_i32(10), TAG_SPECIAL << 48, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn adds_two_doubles() {
        let v = add_view();
        let mut regs = [box_f64(1.5), box_f64(2.25), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.75),
            Exit::Bailed => panic!("expected 3.75, bailed"),
        }
    }

    #[test]
    fn mixes_int_and_double() {
        // int32(10) + double(2.5) → double(12.5): the int operand sign-converts.
        let v = add_view();
        let mut regs = [box_i32(10), box_f64(2.5), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 12.5),
            Exit::Bailed => panic!("expected 12.5, bailed"),
        }
    }

    #[test]
    fn divides_doubles_and_ints() {
        let v = view(&[
            (
                Op::Div,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(7.0), box_f64(2.0), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.5),
            Exit::Bailed => panic!("expected 3.5, bailed"),
        }
        // 6 / 2 yields the Number 3 (an f64), not an int32.
        let mut regs = [box_i32(6), box_i32(2), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.0),
            Exit::Bailed => panic!("expected 3.0, bailed"),
        }
    }

    #[test]
    fn to_numeric_passes_double_through() {
        let v = view(&[
            (
                Op::ToNumeric,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let mut regs = [box_f64(2.5), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 2.5),
            Exit::Bailed => panic!("expected 2.5, bailed"),
        }
        // A non-number (undefined) still bails.
        let mut regs = [TAG_SPECIAL << 48, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn increment_int_double_and_overflow() {
        let v = view(&[
            (
                Op::Increment,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        // int + 1 stays int32.
        let mut regs = [box_i32(41), 0, 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 42);
        // double + 1 stays double.
        let mut regs = [box_f64(2.5), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.5),
            Exit::Bailed => panic!("expected 3.5, bailed"),
        }
        // i32::MAX + 1 overflows → exact double.
        let mut regs = [box_i32(i32::MAX), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), i32::MAX as f64 + 1.0),
            Exit::Bailed => panic!("expected overflow→double, bailed"),
        }
        // Decrement (delta = -1).
        let vd = view(&[
            (
                Op::Increment,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(-1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let mut regs = [box_i32(10), 0, 0, 0, 0, 0, 0, 0];
        expect_int(&vd, &mut regs, 9);
    }

    #[test]
    fn int_multiply_overflow_promotes_to_double() {
        // 100000 * 100000 = 1e10 overflows i32; the result is its exact f64
        // value via the double path, not a bail.
        let v = view(&[
            (
                Op::Mul,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(100_000), box_i32(100_000), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 1e10),
            Exit::Bailed => panic!("expected 1e10, bailed"),
        }
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

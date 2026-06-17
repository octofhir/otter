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
//! (recursive compiled calls append frames to the same reservation-stable
//! HoltStack, so the register base is stable). Every op reads operands from and
//! writes results to that rooted array — no JS value is ever live in a machine
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
/// NaN-box tag for a closure-less bytecode function reference (`value/tag.rs`
/// `TAG_FUNCTION_ID`). The low 32 bits are the function id; the whole Value is
/// `(TAG_FUNCTION_ID << 48) | fid`, so an inlined call site guards identity by
/// comparing the callee register to that exact immediate.
const TAG_FUNCTION_ID: u64 = 0x7FFB;
/// NaN-box tag for object-class heap pointers (`value/tag.rs`). The low 32
/// bits are a `Gc` offset; the body type is discriminated by the GC header
/// tag, so inline property loads must also check [`OBJECT_BODY_TYPE_TAG`].
const TAG_PTR_OBJECT: u64 = 0x7FFC;
/// NaN-box tag for callable heap pointers (`value/tag.rs` `TAG_PTR_FUNCTION`).
/// A prototype method slot holds one of these; the low 32 bits are a `Gc`
/// offset to the callable body, discriminated by the GC header tag.
const TAG_PTR_FUNCTION: u64 = 0x7FFE;
/// GC header type tag for an ordinary `ObjectBody` (mirrors
/// `otter_vm::object::OBJECT_BODY_TYPE_TAG`). Guarded before an inline
/// shape-slot read so a non-object body sharing `TAG_PTR_OBJECT` cannot be
/// misread.
const OBJECT_BODY_TYPE_TAG: u32 = 0x11;
/// GC header type tag for a `JsClosureBody` (mirrors
/// `otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG`). Guarded before reading a
/// resolved method's `function_id` so a native callable sharing
/// [`TAG_PTR_FUNCTION`] is never misread as a bytecode closure.
const JS_CLOSURE_BODY_TYPE_TAG: u32 = 0x23;
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
/// first. The full struct is machine-constructible: nested direct calls copy
/// plain pointers/scalars and share the caller's initialized `error` slot.
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
    /// Byte-PC of the instruction currently executing, written by compiled
    /// code before each op (offset [`BAIL_PC_OFFSET`]). On a guard bail the
    /// interpreter resumes here — the exact instruction, not the entry/loop
    /// header — which is what makes bailing out of a loop body that has
    /// already committed side effects (or out of an unsupported opcode)
    /// correct. Read by `enter_at` on `STATUS_BAILED`.
    bail_pc: u32,
    /// Erased back-pointer to the owning interpreter.
    vm: *mut Interpreter,
    /// The VM frame stack the executing frame lives on.
    stack: *mut JitFrameStack,
    /// Execution context for bridge calls.
    context: *const ExecutionContext,
    /// Index of the executing frame within `stack`.
    frame_index: usize,
    /// Base of this frame's upvalue spine (`Box<[UpvalueCell]>` data; each a
    /// 4-byte compressed cell handle), or `0` when the frame captures nothing
    /// or the ctx was built on the direct-call path (which leaves it `0` so
    /// upvalue ops fall back to the runtime stub). Inline `LoadUpvalue` /
    /// `StoreUpvalue` read `[upvalues_ptr + idx*4]`.
    upvalues_ptr: usize,
    /// Error slot shared by direct callees and bridge stubs when a re-entered
    /// operation throws. Pointer form keeps `JitCtx` constructible by emitted
    /// code; assembly never initializes a Rust enum in place.
    error: *mut Option<VmError>,
    /// Prepared direct-call callee entry address.
    direct_entry_addr: usize,
    /// Prepared direct-call callee register base.
    direct_regs: *mut u64,
    /// Prepared direct-call callee SELF bits.
    direct_self_closure: u64,
    /// Prepared direct-call callee `this` bits.
    direct_this_value: u64,
    /// Prepared direct-call callee frame index.
    direct_frame_index: usize,
    /// Prepared direct-call callee upvalue-spine base (staged from
    /// [`otter_vm::JitPreparedDirectCall::upvalues_ptr`]); the dispatch tail
    /// copies it into the callee `JitCtx.upvalues_ptr`.
    direct_upvalues_ptr: usize,
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

/// Byte offset of [`JitCtx::bail_pc`] — where compiled code stamps the current
/// instruction's byte-PC before each op so a bail resumes at the exact site.
const BAIL_PC_OFFSET: u32 = std::mem::offset_of!(JitCtx, bail_pc) as u32;
/// Byte offset of [`JitCtx::error`] for nested direct-call context construction.
#[allow(dead_code)]
const ERROR_SLOT_OFFSET: u32 = std::mem::offset_of!(JitCtx, error) as u32;
const VM_OFFSET: u32 = std::mem::offset_of!(JitCtx, vm) as u32;
const STACK_OFFSET: u32 = std::mem::offset_of!(JitCtx, stack) as u32;
const CONTEXT_OFFSET: u32 = std::mem::offset_of!(JitCtx, context) as u32;
const FRAME_INDEX_OFFSET: u32 = std::mem::offset_of!(JitCtx, frame_index) as u32;
/// Byte offset of [`JitCtx::upvalues_ptr`] for inline upvalue access.
const UPVALUES_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, upvalues_ptr) as u32;
/// Size of one `UpvalueCell` (a 4-byte compressed `Gc<UpvalueCellBody>`).
const UPVALUE_CELL_SIZE: u32 = 4;
/// Byte offset of the single `Value` inside an `UpvalueCellBody` from its
/// decompressed pointer (just past the 8-byte `GcHeader`).
const UPVALUE_VALUE_OFFSET: u32 = 8;
const DIRECT_ENTRY_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_entry_addr) as u32;
const DIRECT_REGS_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_regs) as u32;
const DIRECT_SELF_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_self_closure) as u32;
const DIRECT_THIS_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_this_value) as u32;
const DIRECT_FRAME_INDEX_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_frame_index) as u32;
const DIRECT_UPVALUES_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_upvalues_ptr) as u32;
const JIT_CTX_STACK_SIZE: u32 = ((std::mem::size_of::<JitCtx>() + 15) & !15) as u32;

/// Compiled-code entry signature.
type JitEntry = extern "C" fn(*mut JitCtx) -> JitRet;

fn park_jit_error(ctx: &mut JitCtx, err: VmError) {
    // SAFETY: every `JitCtx` is built with an initialized error slot that lives
    // for the compiled entry's dynamic extent; nested direct-call contexts copy
    // the same pointer.
    unsafe {
        *ctx.error = Some(err);
    }
}

/// Prepare a direct compiled call. Returns:
/// - `0`: direct target prepared in `ctx.direct_*`.
/// - `1`: throw, error parked in `ctx.error`.
/// - `2`: ineligible/cold callee; caller should bail to the interpreter.
extern "C" fn jit_prepare_direct_call_stub(
    ctx: *mut JitCtx,
    callee: u64,
    argc: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    let all = [a0 as u16, a1 as u16, a2 as u16, a3 as u16];
    let argc = (argc as usize).min(MAX_INLINE_ARGS);
    match vm.jit_prepare_direct_call(context, stack, ctx.frame_index, callee as u16, &all[..argc]) {
        Ok(Some(prepared)) => {
            ctx.direct_entry_addr = prepared.entry_addr;
            ctx.direct_regs = prepared.regs;
            ctx.direct_self_closure = prepared.self_closure;
            ctx.direct_this_value = prepared.this_value;
            ctx.direct_frame_index = prepared.frame_index;
            ctx.direct_upvalues_ptr = prepared.upvalues_ptr;
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Prepare a direct compiled **method** call (`recv.name(args…)`). Same
/// `ctx.direct_*` / status contract as [`jit_prepare_direct_call_stub`], but
/// status `2` means "ineligible — use the in-place full method-call stub"
/// rather than "bail to the interpreter" (a native/polymorphic method in a hot
/// loop must keep running compiled).
#[allow(clippy::too_many_arguments)]
extern "C" fn jit_prepare_direct_method_call_stub(
    ctx: *mut JitCtx,
    recv: u64,
    name_idx: u64,
    site: u64,
    argc: u64,
    a0: u64,
    a1: u64,
    a2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    let all = [a0 as u16, a1 as u16, a2 as u16];
    let argc = (argc as usize).min(all.len());
    match vm.jit_prepare_direct_method_call(
        context,
        stack,
        ctx.frame_index,
        recv as u16,
        name_idx as u32,
        site as usize,
        &all[..argc],
    ) {
        Ok(Some(prepared)) => {
            ctx.direct_entry_addr = prepared.entry_addr;
            ctx.direct_regs = prepared.regs;
            ctx.direct_self_closure = prepared.self_closure;
            ctx.direct_this_value = prepared.this_value;
            ctx.direct_frame_index = prepared.frame_index;
            ctx.direct_upvalues_ptr = prepared.upvalues_ptr;
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

extern "C" fn jit_finish_direct_call_returned_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    value: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    match vm.jit_finish_direct_call_returned(
        stack,
        ctx.frame_index,
        callee_frame_index as usize,
        dst as u16,
        Value::from_bits(value),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

extern "C" fn jit_finish_direct_call_bailed_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    bail_pc: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_finish_direct_call_bailed(
        context,
        stack,
        ctx.frame_index,
        callee_frame_index as usize,
        dst as u16,
        bail_pc as u32,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

extern "C" fn jit_abort_direct_call_stub(ctx: *mut JitCtx, callee_frame_index: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    vm.jit_abort_direct_call(stack, callee_frame_index as usize);
    0
}

/// Bridge stub: build a `MakeFunction` closure from compiled code. Returns `0`
/// on success, `1` when construction threw (error parked in `ctx`).
extern "C" fn jit_make_fn_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_make_function(context, stack, ctx.frame_index, dst as u16, idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// WhiskerIC self-patching cell for one named-property site (one per
/// `LoadProperty` / `StoreProperty` op in the compiled function). Emitted code
/// reads `shape` (offset 0); `0` means "empty — always miss to the stub". On a
/// monomorphic own-data inline-slot hit the stub fills `value_byte` then
/// `shape`, so the next execution inlines the access (shape guard +
/// fixed-offset slot read/write, no VM round-trip). The cell holds only
/// compressed offsets (no GC pointers), so it needs no tracing, and a shape
/// offset is a stable token (shapes are immortal and pinned in old space).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WhiskerIcCell {
    /// Cached receiver shape-handle compressed offset; `0` == empty.
    shape: u32,
    /// Byte offset from the value slab pointer to the value slot.
    value_byte: u32,
}

/// Bridge stub: perform a named `LoadProperty` from compiled code, delegating
/// to the safe [`Interpreter::jit_runtime_load_property`]. Returns `0` on
/// success, `1` when the read threw (error parked in `ctx`). `cell` is this
/// site's [`WhiskerIcCell`] address (or `0`): on a monomorphic own-data
/// inline-slot hit the VM returns a packed fill which this stub writes into the
/// cell so the next load inlines.
extern "C" fn jit_load_prop_stub(
    ctx: *mut JitCtx,
    dst: u64,
    obj: u64,
    name_idx: u64,
    site: u64,
    cell: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
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
        Ok(fill) => {
            // Low 32 = cached shape offset (non-zero validity flag), high 32 =
            // value byte offset. Write `value_byte` before `shape` so the
            // inline guard never observes a live shape with a stale offset.
            if cell != 0 && fill != 0 {
                let cell = cell as *mut WhiskerIcCell;
                // SAFETY: `cell` is a stable address baked into this site's
                // emitted code from the owning `BaselineCode::load_ic_cells`
                // slice, which outlives every execution of this code.
                unsafe {
                    (*cell).value_byte = (fill >> 32) as u32;
                    (*cell).shape = fill as u32;
                }
            }
            0
        }
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: perform a named `StoreProperty` from compiled code, delegating
/// to the safe [`Interpreter::jit_runtime_store_property`]. Returns `0` on
/// success, `1` when the write threw (error parked in `ctx`). `cell` is this
/// site's [`WhiskerIcCell`]: on a monomorphic existing-own-data inline-slot hit
/// the VM returns a packed fill which this stub writes so the next store
/// inlines (shape guard + slot write + value-gated barrier, no round-trip).
extern "C" fn jit_store_prop_stub(
    ctx: *mut JitCtx,
    obj: u64,
    name_idx: u64,
    src: u64,
    site: u64,
    cell: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
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
        Ok(fill) => {
            // Same packing as the load cell: low 32 = shape (validity flag),
            // high 32 = value byte offset. Write `value_byte` before `shape`.
            if cell != 0 && fill != 0 {
                let cell = cell as *mut WhiskerIcCell;
                // SAFETY: `cell` is a stable address baked into this site's
                // emitted code from `BaselineCode::store_ic_cells`, which
                // outlives every execution of this code.
                unsafe {
                    (*cell).value_byte = (fill >> 32) as u32;
                    (*cell).shape = fill as u32;
                }
            }
            0
        }
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: run the GC write barrier for an inline `StoreProperty` whose
/// stored value is a heap pointer. The emitted fast path skips this for
/// primitive values (the common case); a pointer store calls here so an
/// old→young edge marks the parent object's card. Always returns `0`.
extern "C" fn jit_write_barrier_stub(ctx: *mut JitCtx, obj: u64, src: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    vm.jit_runtime_write_barrier(stack, ctx.frame_index, obj as u16, src as u16);
    0
}

/// Bridge stub: perform a computed `LoadElement` (`recv[idx]`) from compiled
/// code, delegating to the safe [`Interpreter::jit_runtime_load_element`].
/// Returns `0` on success, `1` when the read threw (error parked in `ctx`).
extern "C" fn jit_load_element_stub(ctx: *mut JitCtx, dst: u64, recv: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
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
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: perform a `LoadGlobalOrThrow` from compiled code, delegating to
/// the safe [`Interpreter::jit_runtime_load_global`]. Returns `0` on success,
/// `1` when the read threw (unbound identifier / throwing accessor; error
/// parked in `ctx`).
extern "C" fn jit_load_global_stub(ctx: *mut JitCtx, dst: u64, name_idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_load_global(context, stack, ctx.frame_index, dst as u16, name_idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: perform a `LoadUpvalue` (captured-binding read) from compiled
/// code, delegating to [`Interpreter::jit_runtime_load_upvalue`]. `idx` carries
/// the bytecode's signed upvalue index. Returns `0` on success, `1` on throw
/// (TDZ `ReferenceError`, error parked in `ctx`).
extern "C" fn jit_load_upvalue_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    match vm.jit_runtime_load_upvalue(stack, ctx.frame_index, dst as u16, idx as i32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: perform a `StoreUpvalue` (captured-binding write) from compiled
/// code, delegating to [`Interpreter::jit_runtime_store_upvalue`]. Returns `0`
/// on success, `1` on throw (error parked in `ctx`).
extern "C" fn jit_store_upvalue_stub(ctx: *mut JitCtx, src: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    match vm.jit_runtime_store_upvalue(stack, ctx.frame_index, src as u16, idx as i32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: re-run one synchronous opcode (closure/object/array
/// construction, string constant, checked upvalue store, remainder, unsigned
/// shift) at `byte_pc` through [`Interpreter::jit_runtime_delegate_op`]. Returns
/// `0` on success, `1` on throw (error parked in `ctx`).
extern "C" fn jit_delegate_op_stub(ctx: *mut JitCtx, byte_pc: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.vm };
    let stack = unsafe { &mut *ctx.stack };
    let context = unsafe { &*ctx.context };
    match vm.jit_runtime_delegate_op(context, stack, ctx.frame_index, byte_pc as u32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
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
    // SAFETY: the live `JitCtx` reentry contract.
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
            park_jit_error(ctx, err);
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
    // SAFETY: the live `JitCtx` reentry contract.
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
            park_jit_error(ctx, err);
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
    /// Loop-header bytecode PC → assembler offset of its OSR-entry trampoline.
    /// Each trampoline runs the standard prologue then branches to the header's
    /// body label, so the VM can enter mid-loop with the live frame registers.
    osr_entries: std::collections::BTreeMap<u32, usize>,
    /// `true` when at least one opcode outside the supported subset was emitted
    /// as a bail-to-interpreter (not a hard compile failure). Such code is only
    /// sound to enter at a supported loop header via OSR — entering at function
    /// entry would just bail immediately. The function-entry path skips it; only
    /// loop OSR uses it.
    osr_only: bool,
    /// Stable backing store for the WhiskerIC `LoadProperty` cells — one per
    /// `LoadProperty` op, self-patched by [`jit_load_prop_stub`]. Emitted code
    /// holds raw addresses into this slice, so it must never be moved out or
    /// cloned after `compile` returns (the code object is only ever shared by
    /// `Arc`, never cloned by value). Boxed so the buffer address is fixed.
    #[allow(dead_code)]
    load_ic_cells: Box<[WhiskerIcCell]>,
    /// Stable backing store for the WhiskerIC `StoreProperty` cells — one per
    /// `StoreProperty` op, self-patched by [`jit_store_prop_stub`]. Same
    /// ownership / stability contract as [`Self::load_ic_cells`].
    #[allow(dead_code)]
    store_ic_cells: Box<[WhiskerIcCell]>,
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

    fn osr_only(&self) -> bool {
        self.osr_only
    }

    fn entry_addr(&self) -> Option<usize> {
        // SAFETY: the mapping is live for `self`; callers must keep the owning
        // code object installed while using this address.
        Some(unsafe { self.code.entry_ptr() as usize })
    }

    fn run_entry(&self, ptrs: JitReentryPtrs) -> JitExecOutcome {
        // SAFETY: the mapping is live and the main entry was emitted with the
        // `JitEntry` ABI.
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `ptrs` upholds the
        // reentry contract (valid, non-aliased for the call).
        unsafe { self.enter_at(ptrs, entry) }
    }

    fn osr_entry(&self, ptrs: JitReentryPtrs, byte_pc: u32) -> Option<JitExecOutcome> {
        let offset = *self.osr_entries.get(&byte_pc)?;
        // SAFETY: `offset` is an assembler offset recorded for this buffer and
        // points at a prologue trampoline emitted with the `JitEntry` ABI.
        let entry = unsafe { self.code.ptr_at(offset) };
        // SAFETY: same reentry contract as `run_entry`.
        Some(unsafe { self.enter_at(ptrs, entry) })
    }
}

impl BaselineCode {
    /// Build the `JitCtx` for `ptrs` and invoke compiled code at `entry`.
    ///
    /// Shared by the function-entry path ([`Self::run_entry`]) and the
    /// loop-header OSR path ([`Self::osr_entry`]); both ABIs are identical (the
    /// trampoline runs the same prologue), differing only in which instruction
    /// the prologue falls through / branches to.
    ///
    /// # Safety
    /// `entry` must point at a prologue emitted with the [`JitEntry`] ABI inside
    /// this code's live mapping, and `ptrs` must uphold the
    /// [`JitReentryPtrs`](otter_vm::JitReentryPtrs) contract.
    unsafe fn enter_at(&self, ptrs: JitReentryPtrs, entry: *const u8) -> JitExecOutcome {
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
        // SAFETY: same validity/aliasing contract; the spine `Box` outlives this
        // entry (frame-owned), and the cells it holds are old-space (immobile).
        let upvalues_ptr =
            Interpreter::jit_frame_upvalues_ptr(unsafe { &*stack }, ptrs.frame_index);
        let mut error = None;
        let mut ctx = JitCtx {
            regs,
            self_closure,
            this_value,
            vm,
            stack,
            context: ptrs.context.cast::<ExecutionContext>(),
            frame_index: ptrs.frame_index,
            upvalues_ptr,
            bail_pc: 0,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
        };
        // SAFETY: the mapping is live and `entry` was emitted with the
        // `JitEntry` ABI.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let ret = entry(&mut ctx);
        match ret.status {
            STATUS_RETURNED => JitExecOutcome::Returned(Value::from_bits(ret.value)),
            STATUS_BAILED => JitExecOutcome::Bailed(ctx.bail_pc),
            _ => JitExecOutcome::Threw(error.take().unwrap_or(VmError::InvalidOperand)),
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
        BAIL_PC_OFFSET, BaselineCode, CONTEXT_OFFSET, DIRECT_ENTRY_OFFSET,
        DIRECT_FRAME_INDEX_OFFSET, DIRECT_REGS_OFFSET, DIRECT_SELF_OFFSET, DIRECT_THIS_OFFSET,
        DIRECT_UPVALUES_OFFSET, ERROR_SLOT_OFFSET, FRAME_INDEX_OFFSET, JIT_CTX_STACK_SIZE,
        JS_CLOSURE_BODY_TYPE_TAG, MAX_INLINE_ARGS, OBJECT_BODY_TYPE_TAG, Op, Operand,
        SPECIAL_FALSE, SPECIAL_HOLE, SPECIAL_TRUE, STACK_OFFSET, STATUS_BAILED, STATUS_RETURNED,
        STATUS_THREW, TAG_FUNCTION_ID, TAG_INT32, TAG_NAN, TAG_PTR_FUNCTION, TAG_PTR_OBJECT,
        TAG_SPECIAL, UPVALUE_CELL_SIZE, UPVALUE_VALUE_OFFSET, UPVALUES_PTR_OFFSET, Unsupported,
        VM_OFFSET, WhiskerIcCell, jit_abort_direct_call_stub, jit_call_method_stub,
        jit_delegate_op_stub, jit_finish_direct_call_bailed_stub,
        jit_finish_direct_call_returned_stub, jit_load_element_stub, jit_load_global_stub,
        jit_load_prop_stub, jit_load_upvalue_stub, jit_make_fn_stub, jit_prepare_direct_call_stub,
        jit_prepare_direct_method_call_stub, jit_store_element_stub, jit_store_prop_stub,
        jit_store_upvalue_stub, jit_write_barrier_stub, reg_offset,
    };
    use crate::CompiledCode;
    use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
    use otter_vm::{JitFunctionView, JitInlineCallee, JitInlineMethod, JitTypedArrayLayout};
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

    /// Integer binary ops that share the int32 fast-path shape: guard both
    /// operands int32, apply a single 32-bit instruction, re-box as int32.
    enum IntBinOp {
        Or,
        And,
        Xor,
        Shl,
        Shr,
    }

    /// Emit `Add`/`Sub`/`Mul`: an int32 fast path that falls through to the
    /// f64 path on a non-int32 operand or an overflowing int32 result (never to
    /// `bail` — an overflowing integer product is just its exact f64 value). The
    /// double path decodes both operands to f64, computes, and reboxes; a
    /// non-number operand on that path bails to `bail`.
    fn emit_add_sub_mul(
        ops: &mut Assembler,
        operands: &[Operand],
        bail: DynamicLabel,
        op: Op,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        let float_path = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
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
        match op {
            Op::Add => dynasm!(ops ; .arch aarch64 ; adds w13, w9, w10 ; b.vs =>float_path),
            Op::Sub => dynasm!(ops ; .arch aarch64 ; subs w13, w9, w10 ; b.vs =>float_path),
            Op::Mul => dynasm!(ops
                ; .arch aarch64
                ; smull x13, w9, w10
                ; cmp x13, w13, sxtw
                ; b.ne =>float_path
            ),
            _ => return Err(Unsupported::ArgCount(0)),
        }
        box_low32!(ops, 13, 12, TAG_INT32);
        store_reg(ops, 13, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
        emit_num_to_double(ops, 9, 0, bail);
        emit_num_to_double(ops, 10, 1, bail);
        match op {
            Op::Add => dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1),
            Op::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
            Op::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
            _ => return Err(Unsupported::ArgCount(0)),
        }
        emit_box_double(ops, 2, 13);
        store_reg(ops, 13, dst)?;
        dynasm!(ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// Emit `Div`: division always yields a Number (f64) in ECMAScript — even
    /// `6 / 2` is the Number `3` — so there is no int fast path; decode both
    /// operands to f64 and `fdiv`. A non-number operand bails to `bail`.
    fn emit_div(
        ops: &mut Assembler,
        operands: &[Operand],
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        emit_num_to_double(ops, 9, 0, bail);
        emit_num_to_double(ops, 10, 1, bail);
        dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1);
        emit_box_double(ops, 2, 13);
        store_reg(ops, 13, dst)?;
        Ok(())
    }

    /// Emit an int32 bitwise/shift op (`BitwiseOr`/`And`/`Xor`/`Shl`/`Shr`).
    ///
    /// Both operands must already be int32-tagged Values; a non-int32 operand
    /// bails to the interpreter (which performs the full `ToInt32`/`ToUint32`
    /// coercion). Result is int32, matching JS semantics: the AArch64 32-bit
    /// `lsl`/`asr` mask the shift count to its low 5 bits exactly as JS masks
    /// the right operand to `& 31`.
    fn emit_int_binop(
        ops: &mut Assembler,
        operands: &[Operand],
        bail: DynamicLabel,
        kind: IntBinOp,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        guard_int32!(ops, 9, bail);
        guard_int32!(ops, 10, bail);
        match kind {
            IntBinOp::Or => dynasm!(ops ; .arch aarch64 ; orr w13, w9, w10),
            IntBinOp::And => dynasm!(ops ; .arch aarch64 ; and w13, w9, w10),
            IntBinOp::Xor => dynasm!(ops ; .arch aarch64 ; eor w13, w9, w10),
            IntBinOp::Shl => dynasm!(ops ; .arch aarch64 ; lsl w13, w9, w10),
            IntBinOp::Shr => dynasm!(ops ; .arch aarch64 ; asr w13, w9, w10),
        }
        box_low32!(ops, 13, 12, TAG_INT32);
        store_reg(ops, 13, dst)?;
        Ok(())
    }

    /// Emit the function prologue: save fp/lr + callee-saved bases, then set
    /// `x20 = ctx` (arg in `x0`) and `x19 = ctx.regs` (the frame register base).
    /// Shared by the main entry and every OSR trampoline so both honor the same
    /// [`JitEntry`] ABI.
    fn emit_prologue(ops: &mut Assembler) {
        dynasm!(ops
            ; .arch aarch64
            ; stp x29, x30, [sp, #-32]!
            ; stp x19, x20, [sp, #16]
            ; mov x29, sp
            ; mov x20, x0
            ; ldr x19, [x20]
        );
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

        // Set when an unsupported opcode is emitted as a bail (see the catch-all
        // arm); such code is OSR-only.
        let mut osr_only = false;

        // Loop headers = back-edge targets: the PCs an OSR entry can land on.
        // A branch whose resolved target sits at or before its own PC closes a
        // loop; that target is a basic-block boundary where the interpreter's
        // live registers match what compiled code expects (the baseline keeps
        // all live values in the frame array between ops). Collect them here so
        // a trampoline is emitted for each after the body.
        let mut loop_headers: BTreeMap<u32, ()> = BTreeMap::new();
        for instr in &view.instructions {
            if matches!(instr.op, Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue) {
                let rel = imm32(instr.operands.as_slice(), 0)?;
                let target = branch_target(instr, rel);
                if target >= 0
                    && target < i64::from(instr.byte_pc)
                    && let Ok(pc) = u32::try_from(target)
                    && labels.contains_key(&pc)
                {
                    loop_headers.insert(pc, ());
                }
            }
        }

        // One self-patching WhiskerIC cell per `LoadProperty` op. Allocated up
        // front (stable boxed buffer) so each site can bake its cell address;
        // filled at runtime by `jit_load_prop_stub` on a monomorphic own-data
        // inline-slot hit. `as_mut_ptr` gives a write-provenance base that
        // outlives every execution (the buffer is owned by the returned
        // `BaselineCode` and never re-formed as a `&[_]` slice).
        let load_property_count = view
            .instructions
            .iter()
            .filter(|i| i.op == Op::LoadProperty)
            .count();
        let mut load_ic_cells: Box<[WhiskerIcCell]> =
            vec![WhiskerIcCell::default(); load_property_count].into_boxed_slice();
        let cell_base = load_ic_cells.as_mut_ptr() as usize;
        let mut load_ic_idx: usize = 0;

        // One self-patching WhiskerIC cell per `StoreProperty` op, same scheme
        // as the load cells above.
        let store_property_count = view
            .instructions
            .iter()
            .filter(|i| i.op == Op::StoreProperty)
            .count();
        let mut store_ic_cells: Box<[WhiskerIcCell]> =
            vec![WhiskerIcCell::default(); store_property_count].into_boxed_slice();
        let store_cell_base = store_ic_cells.as_mut_ptr() as usize;
        let mut store_ic_idx: usize = 0;

        let entry = ops.offset();
        emit_prologue(&mut ops);

        // Stable GC cage base, baked for inline property-load decompression.
        let cage_base = view.cage_base;
        // Static typed-array body offsets for inline element access. Only used
        // when `cage_base != 0` (i.e. baked by the real compile path).
        let ta_layout = view.ta_layout;

        for instr in &view.instructions {
            dynasm!(ops ; .arch aarch64 ; =>labels[&instr.byte_pc]);
            // Stamp this op's byte-PC into the context so any bail (guard
            // failure or unsupported opcode) resumes the interpreter at the
            // exact instruction, preserving committed side effects.
            emit_load_u64(&mut ops, 9, u64::from(instr.byte_pc));
            dynasm!(ops ; .arch aarch64 ; str w9, [x20, BAIL_PC_OFFSET]);
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
                Op::LoadHole => {
                    let dst = reg(ops_ref, 0)?;
                    // SPECIAL payload `SPECIAL_HOLE` == the TDZ/uninitialized hole.
                    emit_load_u64(&mut ops, 9, (TAG_SPECIAL << 48) | SPECIAL_HOLE);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadTrue => {
                    let dst = reg(ops_ref, 0)?;
                    emit_load_u64(&mut ops, 9, (TAG_SPECIAL << 48) | u64::from(SPECIAL_TRUE));
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadFalse => {
                    let dst = reg(ops_ref, 0)?;
                    emit_load_u64(&mut ops, 9, (TAG_SPECIAL << 48) | u64::from(SPECIAL_FALSE));
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::StoreLocal => {
                    let src = reg(ops_ref, 0)?;
                    let idx = local_index(ops_ref, 1)?;
                    load_reg(&mut ops, 9, src)?;
                    store_reg(&mut ops, 9, idx)?;
                }
                Op::Add | Op::Sub | Op::Mul => {
                    emit_add_sub_mul(&mut ops, ops_ref, bail, instr.op)?;
                }
                Op::Div => emit_div(&mut ops, ops_ref, bail)?,
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
                    // Splice a tiny monomorphic leaf callee inline under an
                    // identity guard (no per-call bridge); fall back to the
                    // direct-call bridge for absent / ineligible sites.
                    let inlined = match view.inline_callees.get(&instr.byte_pc) {
                        Some(callee) => try_emit_inline_call(&mut ops, callee, ops_ref, bail)?,
                        None => false,
                    };
                    if !inlined {
                        emit_call(&mut ops, ops_ref, bail, threw)?;
                    }
                }
                // `recv.name(args…)` — IC-resolve the method + direct-branch to
                // its compiled entry (WhiskerIC method call), falling back to the
                // in-place full method-call stub when ineligible.
                Op::CallMethodValue => {
                    let site = instr.property_ic_site.unwrap_or(usize::MAX) as u64;
                    // Splice a tiny monomorphic read-only method inline under an
                    // identity + receiver-shape guard; fall back to the method
                    // bridge for absent / ineligible sites.
                    let inlined = match view.inline_methods.get(&instr.byte_pc) {
                        Some(method) => try_emit_inline_method_call(
                            &mut ops,
                            method,
                            ops_ref,
                            site,
                            cage_base,
                            view.object_shape_byte,
                            view.object_values_ptr_byte,
                            view.jit_proto_byte,
                            view.closure_fid_byte,
                            bail,
                            threw,
                        )?,
                        None => false,
                    };
                    if !inlined {
                        emit_method_call(&mut ops, ops_ref, site, threw)?;
                    }
                }
                // `recv[idx]` — inline dense-`Array` (raw `Value`) and
                // `Float64Array`/`Int32Array` element load (guarded, no
                // safepoint); every other case (sparse/hole, strings, object
                // `[[Get]]`, polymorphic/detached/OOB) misses to the safe
                // element-load bridge, which owns the spec-correct semantics.
                Op::LoadElement => {
                    let dst = reg(ops_ref, 0)?;
                    let recv = reg(ops_ref, 1)?;
                    let idx = reg(ops_ref, 2)?;
                    let el_miss = ops.new_dynamic_label();
                    let el_done = ops.new_dynamic_label();

                    if cage_base != 0 {
                        let recv_off = reg_offset(recv)?;
                        let idx_off = reg_offset(idx)?;
                        let dst_off = reg_offset(dst)?;
                        emit_element_load(
                            &mut ops, &ta_layout, cage_base, recv_off, idx_off, dst_off, el_miss,
                            el_done,
                        );
                    }

                    dynasm!(ops
                        ; .arch aarch64
                        ; =>el_miss
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, recv as u32
                        ; movz x3, idx as u32
                    );
                    emit_call_stub(&mut ops, jit_load_element_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>el_done);
                }
                // `recv[idx] = src` — inline `Float64Array`/`Int32Array` element
                // store (guarded, no safepoint); every other case misses to the
                // safe element-store bridge. Operands: recv, idx, src, scratch.
                Op::StoreElement => {
                    let recv = reg(ops_ref, 0)?;
                    let idx = reg(ops_ref, 1)?;
                    let src = reg(ops_ref, 2)?;
                    let scratch = reg(ops_ref, 3)?;
                    let el_miss = ops.new_dynamic_label();
                    let el_done = ops.new_dynamic_label();

                    if cage_base != 0 {
                        let recv_off = reg_offset(recv)?;
                        let idx_off = reg_offset(idx)?;
                        let src_off = reg_offset(src)?;
                        let f64_path = ops.new_dynamic_label();
                        let i32_path = ops.new_dynamic_label();
                        emit_ta_guard_chain(
                            &mut ops, &ta_layout, cage_base, recv_off, idx_off, el_miss, f64_path,
                            i32_path,
                        );
                        // Float64Array: coerce src to f64 (int32 or double; any
                        // other tag misses to the stub for full ToNumber), store.
                        // Address is held in x10, which `emit_num_to_double`'s
                        // scratch (x14/x15) does not clobber.
                        dynasm!(ops
                            ; .arch aarch64
                            ; =>f64_path
                            ; lsl x10, x12, #3            // index * 8
                            ; add x10, x10, x16           // + byte_offset
                            ; add x15, x10, #8            // + element size (bound)
                            ; cmp x15, x17
                            ; b.hi =>el_miss
                            ; add x10, x13, x10           // element address
                            ; ldr x9, [x19, src_off]
                        );
                        emit_num_to_double(&mut ops, 9, 0, el_miss);
                        dynasm!(ops
                            ; .arch aarch64
                            ; str d0, [x10]
                            ; b =>el_done
                            // Int32Array: src must be int32 (a double misses to
                            // the stub for ToInt32 truncation); store low-32.
                            ; =>i32_path
                            ; lsl x10, x12, #2            // index * 4
                            ; add x10, x10, x16           // + byte_offset
                            ; add x15, x10, #4            // + element size (bound)
                            ; cmp x15, x17
                            ; b.hi =>el_miss
                            ; add x10, x13, x10           // element address
                            ; ldr x9, [x19, src_off]
                        );
                        guard_int32!(ops, 9, el_miss);
                        dynasm!(ops
                            ; .arch aarch64
                            ; str w9, [x10]
                            ; b =>el_done
                        );
                    }

                    dynasm!(ops
                        ; .arch aarch64
                        ; =>el_miss
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
                    dynasm!(ops ; .arch aarch64 ; =>el_done);
                }
                // `dst = global[name]` or throw — delegate to the safe bridge.
                Op::LoadGlobalOrThrow => {
                    let dst = reg(ops_ref, 0)?;
                    let name = const_index(ops_ref, 1)?;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(&mut ops, 2, u64::from(name));
                    emit_call_stub(&mut ops, jit_load_global_stub as *const () as usize, threw);
                }
                // `dst = upvalue[idx]` (captured binding). Inline: read the cell
                // handle from the frame's upvalue spine, decompress (cells are
                // old-space, immobile), load the captured Value. A TDZ hole or a
                // `0` spine base (no upvalues / direct-call ctx) misses to the
                // bridge stub, which raises the `ReferenceError`. `idx` is the
                // signed bytecode index, passed as u32 bits and re-read as i32.
                Op::LoadUpvalue => {
                    let dst = reg(ops_ref, 0)?;
                    let idx = imm32(ops_ref, 1)?;
                    let up_miss = ops.new_dynamic_label();
                    let up_done = ops.new_dynamic_label();

                    if cage_base != 0 && idx >= 0 {
                        let dst_off = reg_offset(dst)?;
                        let idx_off = (idx as u32) * UPVALUE_CELL_SIZE;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x20, UPVALUES_PTR_OFFSET] // spine base
                            ; cbz x9, =>up_miss
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x10                  // cell body ptr
                            ; ldr x9, [x13, UPVALUE_VALUE_OFFSET] // captured Value
                        );
                        emit_load_u64(&mut ops, 11, (TAG_SPECIAL << 48) | SPECIAL_HOLE);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x9, x11                        // TDZ hole?
                            ; b.eq =>up_miss
                            ; str x9, [x19, dst_off]
                            ; b =>up_done
                        );
                    }

                    dynasm!(ops ; .arch aarch64 ; =>up_miss ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(&mut ops, 2, u64::from(idx as u32));
                    emit_call_stub(&mut ops, jit_load_upvalue_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>up_done);
                }
                // `upvalue[idx] = src` (captured binding). Inline the primitive
                // store: a non-pointer value written into the (old-space) cell
                // needs no write barrier. A pointer value or `0` spine base
                // misses to the bridge stub, which performs the barriered store.
                Op::StoreUpvalue => {
                    let src = reg(ops_ref, 0)?;
                    let idx = imm32(ops_ref, 1)?;
                    let up_miss = ops.new_dynamic_label();
                    let up_done = ops.new_dynamic_label();

                    if cage_base != 0 && idx >= 0 {
                        let src_off = reg_offset(src)?;
                        let idx_off = (idx as u32) * UPVALUE_CELL_SIZE;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x20, UPVALUES_PTR_OFFSET] // spine base
                            ; cbz x9, =>up_miss
                            ; ldr x12, [x19, src_off]            // value to store
                            ; lsr x10, x12, #48                  // tag
                            ; movz x11, TAG_PTR_OBJECT as u32
                            ; cmp x10, x11
                            ; b.hs =>up_miss                     // pointer → barriered stub
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x10                  // cell body ptr
                            ; str x12, [x13, UPVALUE_VALUE_OFFSET]
                            ; b =>up_done
                        );
                    }

                    dynasm!(ops ; .arch aarch64 ; =>up_miss ; mov x0, x20 ; movz x1, src as u32);
                    emit_load_u64(&mut ops, 2, u64::from(idx as u32));
                    emit_call_stub(
                        &mut ops,
                        jit_store_upvalue_stub as *const () as usize,
                        threw,
                    );
                    dynasm!(ops ; .arch aarch64 ; =>up_done);
                }
                // `upvalue[idx] = src` with a TDZ guard (assignment to a captured
                // `let`/`const`). Like `StoreUpvalue` but reads the cell first and
                // misses to the delegate bridge on a hole (raising the
                // `ReferenceError`). Inlines only the primitive store; a pointer
                // value misses to the bridge (barriered store inside).
                Op::StoreUpvalueChecked => {
                    let src = reg(ops_ref, 0)?;
                    let idx = imm32(ops_ref, 1)?;
                    let up_miss = ops.new_dynamic_label();
                    let up_done = ops.new_dynamic_label();

                    if cage_base != 0 && idx >= 0 {
                        let src_off = reg_offset(src)?;
                        let idx_off = (idx as u32) * UPVALUE_CELL_SIZE;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x20, UPVALUES_PTR_OFFSET] // spine base
                            ; cbz x9, =>up_miss
                            ; ldr x12, [x19, src_off]            // value to store
                            ; lsr x10, x12, #48                  // tag
                            ; movz x11, TAG_PTR_OBJECT as u32
                            ; cmp x10, x11
                            ; b.hs =>up_miss                     // pointer → barriered bridge
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        emit_load_u64(&mut ops, 11, (TAG_SPECIAL << 48) | SPECIAL_HOLE);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x10                  // cell body ptr
                            ; ldr x14, [x13, UPVALUE_VALUE_OFFSET] // current value
                            ; cmp x14, x11                       // TDZ hole?
                            ; b.eq =>up_miss
                            ; str x12, [x13, UPVALUE_VALUE_OFFSET]
                            ; b =>up_done
                        );
                    }

                    dynasm!(ops ; .arch aarch64 ; =>up_miss ; mov x0, x20);
                    emit_load_u64(&mut ops, 1, u64::from(instr.byte_pc));
                    emit_call_stub(&mut ops, jit_delegate_op_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>up_done);
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
                    // jit_load_prop_stub(ctx=x20, dst, obj, name_idx, site, cell).
                    // `site` is the dense IC index from the snapshot, used by
                    // the bridge for the monomorphic fast path (PC-keyed lookup
                    // is unavailable at PC 0); `usize::MAX` means "no site".
                    // `cell` is this site's self-patching WhiskerIC cell.
                    let dst = reg(ops_ref, 0)?;
                    let obj = reg(ops_ref, 1)?;
                    let name = const_index(ops_ref, 2)?;
                    let site = instr.property_ic_site.unwrap_or(usize::MAX) as u64;

                    // This site's WhiskerIC cell address (stable for the code's
                    // life). Filled by the stub on a monomorphic own-data hit.
                    let cell_addr = cell_base + load_ic_idx * std::mem::size_of::<WhiskerIcCell>();
                    load_ic_idx += 1;

                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();

                    if cage_base != 0 && instr.load_array_length {
                        let obj_off = reg_offset(obj)?;
                        let dst_off = reg_offset(dst)?;
                        let array_tag = u32::from(view.ta_layout.array_type_tag);
                        let length_byte = view.ta_layout.array_length_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; lsr x10, x9, #48
                            ; movz x11, TAG_PTR_OBJECT as u32
                            ; cmp x10, x11
                            ; b.ne =>miss
                            ; mov w12, w9              // low-32 Gc offset
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]
                            ; cmp w14, array_tag
                            ; b.ne =>miss
                            ; ldr x9, [x13, length_byte]
                        );
                        emit_load_u64(&mut ops, 12, i32::MAX as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x9, x12
                            ; b.hi =>miss
                        );
                        box_low32!(ops, 9, 12, TAG_INT32);
                        dynasm!(ops
                            ; .arch aarch64
                            ; str x9, [x19, dst_off]
                            ; b =>done
                        );
                    }

                    // Inline guarded own-data load through the self-patching
                    // cell: guard tag + GC type tag + cell shape, then read the
                    // value slab slot at the cell's byte offset. No allocation /
                    // call → no safepoint; the object pointer is recomputed from
                    // the (rooted) frame slot each time, never held across one.
                    // An empty cell (`shape == 0`) or any guard miss falls
                    // through to the shared stub, which fills the cell once the
                    // site is warm + monomorphic.
                    if cage_base != 0 {
                        let obj_off = reg_offset(obj)?;
                        let dst_off = reg_offset(dst)?;
                        let shape_byte = view.object_shape_byte;
                        let values_ptr_byte = view.object_values_ptr_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; lsr x10, x9, #48         // top-16 tag
                            ; movz x11, TAG_PTR_OBJECT as u32
                            ; cmp x10, x11
                            ; b.ne =>miss
                            ; mov w12, w9              // low-32 Gc offset (zero-ext)
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]          // header type tag
                            ; cmp w14, OBJECT_BODY_TYPE_TAG
                            ; b.ne =>miss
                            ; ldr w14, [x13, shape_byte] // receiver shape handle
                        );
                        emit_load_u64(&mut ops, 15, cell_addr as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr w16, [x15]           // cached shape (0 = empty)
                            ; cbz w16, =>miss
                            ; cmp w14, w16
                            ; b.ne =>miss
                            ; ldr w17, [x15, #4]       // cached value byte offset
                            ; ldr x13, [x13, values_ptr_byte] // value slab base
                            ; cbz x13, =>miss
                            ; ldr x9, [x13, x17]       // slab slot value
                            ; str x9, [x19, dst_off]
                            ; b =>done
                        );
                    }

                    // Miss / no cage base: shared runtime IC + general path,
                    // passing the cell so the stub can self-patch it.
                    dynasm!(ops ; .arch aarch64 ; =>miss);
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, obj as u32
                    );
                    emit_load_u64(&mut ops, 3, u64::from(name));
                    emit_load_u64(&mut ops, 4, site);
                    emit_load_u64(&mut ops, 5, cell_addr as u64);
                    emit_call_stub(&mut ops, jit_load_prop_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::StoreProperty => {
                    // Operands: obj, name_const, src, scratch_dst.
                    // jit_store_prop_stub(ctx=x20, obj, name_idx, src, site, cell).
                    let obj = reg(ops_ref, 0)?;
                    let name = const_index(ops_ref, 1)?;
                    let src = reg(ops_ref, 2)?;
                    let site = instr.property_ic_site.unwrap_or(usize::MAX) as u64;

                    let cell_addr =
                        store_cell_base + store_ic_idx * std::mem::size_of::<WhiskerIcCell>();
                    store_ic_idx += 1;

                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();

                    // Inline guarded existing-own-data store through the
                    // self-patching cell: guard tag + GC type tag + cell shape,
                    // write the value into the value slab slot, then a
                    // value-tag-gated write barrier (primitive stores skip it).
                    // No allocation → no safepoint; the object pointer is
                    // recomputed from the (rooted) frame slot, never held
                    // across one. Empty cell / guard miss → shared stub.
                    if cage_base != 0 {
                        let obj_off = reg_offset(obj)?;
                        let src_off = reg_offset(src)?;
                        let shape_byte = view.object_shape_byte;
                        let values_ptr_byte = view.object_values_ptr_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; lsr x10, x9, #48
                            ; movz x11, TAG_PTR_OBJECT as u32
                            ; cmp x10, x11
                            ; b.ne =>miss
                            ; mov w12, w9              // low-32 Gc offset
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]
                            ; cmp w14, OBJECT_BODY_TYPE_TAG
                            ; b.ne =>miss
                            ; ldr w14, [x13, shape_byte] // receiver shape handle
                        );
                        emit_load_u64(&mut ops, 15, cell_addr as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr w16, [x15]           // cached shape (0 = empty)
                            ; cbz w16, =>miss
                            ; cmp w14, w16
                            ; b.ne =>miss
                            ; ldr w17, [x15, #4]       // cached value byte offset
                            ; ldr x9, [x19, src_off]   // value to store
                            ; ldr x13, [x13, values_ptr_byte] // value slab base
                            ; cbz x13, =>miss
                            ; str x9, [x13, x17]       // write slab slot
                            ; lsr x10, x9, #48         // value tag
                            ; movz x11, TAG_PTR_OBJECT as u32
                            ; cmp x10, x11
                            ; b.lo =>done              // primitive → no barrier
                        );
                        // Pointer value: card-mark the parent header.
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov x0, x20
                            ; movz x1, obj as u32
                            ; movz x2, src as u32
                        );
                        emit_call_stub(
                            &mut ops,
                            jit_write_barrier_stub as *const () as usize,
                            threw,
                        );
                        dynasm!(ops ; .arch aarch64 ; b =>done);
                    }

                    // Miss / no cage base: shared runtime store path, passing
                    // the cell so the stub can self-patch it.
                    dynasm!(ops ; .arch aarch64 ; =>miss);
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, obj as u32
                    );
                    emit_load_u64(&mut ops, 2, u64::from(name));
                    dynasm!(ops ; .arch aarch64 ; movz x3, src as u32);
                    emit_load_u64(&mut ops, 4, site);
                    emit_load_u64(&mut ops, 5, cell_addr as u64);
                    emit_call_stub(&mut ops, jit_store_prop_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::BitwiseOr => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Or)?,
                Op::BitwiseAnd => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::And)?,
                Op::BitwiseXor => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Xor)?,
                Op::Shl => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Shl)?,
                Op::Shr => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Shr)?,
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
                // Synchronous opcodes with variable / awkward operands: re-run
                // through the interpreter at this instruction's byte_pc via the
                // generic delegate bridge (closure/object/array construction,
                // string constants, checked upvalue store, remainder, unsigned
                // shift). All run to completion without pushing a frame.
                Op::MakeClosure
                | Op::NewObject
                | Op::NewArray
                | Op::Rem
                | Op::Ushr
                | Op::LoadString
                | Op::LoadNumber
                | Op::DefineDataProperty
                | Op::FreshUpvalue
                | Op::LoadBuiltinError
                | Op::Neg
                | Op::DefineOwnProperty => {
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(&mut ops, 1, u64::from(instr.byte_pc));
                    emit_call_stub(&mut ops, jit_delegate_op_stub as *const () as usize, threw);
                }
                _other => {
                    // Opcode outside the subset: bail to the interpreter at this
                    // exact PC (stamped above) instead of failing the whole
                    // compile. This lets a function with a hot, fully-supported
                    // loop tier up via OSR even when its non-loop body uses
                    // unsupported opcodes (class definition, `new`, globals,
                    // etc.). Marked `osr_only` so the function-entry path skips
                    // it (entering at PC 0 would bail immediately).
                    osr_only = true;
                    dynasm!(ops ; .arch aarch64 ; b =>bail);
                }
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

        // OSR trampolines: one per loop header. Each runs the standard prologue
        // (set up x19/x20 from the ctx arg) then branches to the header's body
        // label, so the VM can re-enter mid-loop with the live frame registers.
        let mut osr_entries: BTreeMap<u32, usize> = BTreeMap::new();
        for (&pc, ()) in &loop_headers {
            let off = ops.offset().0;
            emit_prologue(&mut ops);
            let tgt = labels[&pc];
            dynasm!(ops ; .arch aarch64 ; b =>tgt);
            osr_entries.insert(pc, off);
        }

        let buf = ops.finalize().expect("finalize");
        Ok(BaselineCode {
            code: CompiledCode::new(buf, entry),
            osr_entries,
            osr_only,
            load_ic_cells,
            store_ic_cells,
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

    /// Largest callee register window the inliner accepts. Bounds the per-site
    /// scratch reservation and keeps a spliced body "tiny".
    const INLINE_MAX_REGS: u16 = 24;
    /// Largest callee instruction count the inliner accepts.
    const INLINE_MAX_INSTRS: usize = 48;
    /// Largest argument count an inlined call accepts.
    const INLINE_MAX_ARGS: usize = 8;

    /// Whether an op may appear in an inlined leaf callee: a pure, non-allocating
    /// operation with no `this`/upvalue/global/heap access and no further call,
    /// so the spliced body has no GC point and commits nothing observable before
    /// it can bail. Any op outside this set aborts the inline attempt.
    fn is_inline_pure_op(op: Op) -> bool {
        matches!(
            op,
            Op::LoadInt32
                | Op::LoadLocal
                | Op::LoadUndefined
                | Op::LoadHole
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::StoreLocal
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::BitwiseOr
                | Op::BitwiseAnd
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                | Op::ToPrimitive
                | Op::ToNumeric
                | Op::Jump
                | Op::JumpIfFalse
                | Op::JumpIfTrue
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    }

    /// Emit one op of an inlined callee body. The frame-register base `x19`
    /// already points at the callee scratch window, so `load_reg`/`store_reg`
    /// address callee registers. Bails route to `bail` (the site's scratch-aware
    /// bail) without restamping `bail_pc`, so a bail re-runs the whole call in
    /// the interpreter. `Return*` leaves the result in `x9` and branches to
    /// `inline_done`. Internal branches resolve through `clabels` (one private
    /// label per callee byte-PC).
    fn emit_inline_pure_op(
        ops: &mut Assembler,
        instr: &otter_vm::JitInstrView,
        bail: DynamicLabel,
        inline_done: DynamicLabel,
        clabels: &BTreeMap<u32, DynamicLabel>,
    ) -> Result<(), Unsupported> {
        let ops_ref = instr.operands.as_slice();
        let ctarget = |rel: i32| -> Result<DynamicLabel, Unsupported> {
            let t = branch_target(instr, rel);
            u32::try_from(t)
                .ok()
                .and_then(|pc| clabels.get(&pc).copied())
                .ok_or(Unsupported::BranchTarget(t))
        };
        match instr.op {
            Op::LoadInt32 => {
                let dst = reg(ops_ref, 0)?;
                let v = imm32(ops_ref, 1)?;
                emit_load_u64(ops, 9, (TAG_INT32 << 48) | u64::from(v as u32));
                store_reg(ops, 9, dst)?;
            }
            Op::LoadLocal => {
                let dst = reg(ops_ref, 0)?;
                let idx = local_index(ops_ref, 1)?;
                load_reg(ops, 9, idx)?;
                store_reg(ops, 9, dst)?;
            }
            Op::LoadUndefined => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, TAG_SPECIAL << 48);
                store_reg(ops, 9, dst)?;
            }
            Op::LoadHole => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, (TAG_SPECIAL << 48) | SPECIAL_HOLE);
                store_reg(ops, 9, dst)?;
            }
            Op::LoadTrue => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, (TAG_SPECIAL << 48) | u64::from(SPECIAL_TRUE));
                store_reg(ops, 9, dst)?;
            }
            Op::LoadFalse => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, (TAG_SPECIAL << 48) | u64::from(SPECIAL_FALSE));
                store_reg(ops, 9, dst)?;
            }
            Op::StoreLocal => {
                let src = reg(ops_ref, 0)?;
                let idx = local_index(ops_ref, 1)?;
                load_reg(ops, 9, src)?;
                store_reg(ops, 9, idx)?;
            }
            Op::Add | Op::Sub | Op::Mul => emit_add_sub_mul(ops, ops_ref, bail, instr.op)?,
            Op::Div => emit_div(ops, ops_ref, bail)?,
            Op::BitwiseOr => emit_int_binop(ops, ops_ref, bail, IntBinOp::Or)?,
            Op::BitwiseAnd => emit_int_binop(ops, ops_ref, bail, IntBinOp::And)?,
            Op::BitwiseXor => emit_int_binop(ops, ops_ref, bail, IntBinOp::Xor)?,
            Op::Shl => emit_int_binop(ops, ops_ref, bail, IntBinOp::Shl)?,
            Op::Shr => emit_int_binop(ops, ops_ref, bail, IntBinOp::Shr)?,
            Op::LessThan => emit_cmp(ops, ops_ref, bail, Cmp::Lt)?,
            Op::LessEq => emit_cmp(ops, ops_ref, bail, Cmp::Le)?,
            Op::GreaterThan => emit_cmp(ops, ops_ref, bail, Cmp::Gt)?,
            Op::GreaterEq => emit_cmp(ops, ops_ref, bail, Cmp::Ge)?,
            Op::Equal => emit_cmp(ops, ops_ref, bail, Cmp::Eq)?,
            Op::NotEqual => emit_cmp(ops, ops_ref, bail, Cmp::Ne)?,
            Op::ToPrimitive | Op::ToNumeric => {
                let dst = reg(ops_ref, 0)?;
                let src = reg(ops_ref, 1)?;
                load_reg(ops, 9, src)?;
                guard_number!(ops, 9, bail);
                store_reg(ops, 9, dst)?;
            }
            Op::Jump => {
                let rel = imm32(ops_ref, 0)?;
                let tgt = ctarget(rel)?;
                dynasm!(ops ; .arch aarch64 ; b =>tgt);
            }
            Op::JumpIfFalse | Op::JumpIfTrue => {
                let rel = imm32(ops_ref, 0)?;
                let cond = reg(ops_ref, 1)?;
                let tgt = ctarget(rel)?;
                load_reg(ops, 9, cond)?;
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
            Op::Return | Op::ReturnValue => {
                let src = reg(ops_ref, 0)?;
                load_reg(ops, 9, src)?;
                dynasm!(ops ; .arch aarch64 ; b =>inline_done);
            }
            Op::ReturnUndefined => {
                emit_load_u64(ops, 9, TAG_SPECIAL << 48);
                dynasm!(ops ; .arch aarch64 ; b =>inline_done);
            }
            // Pre-scanned by `is_inline_pure_op`; unreachable in practice.
            _ => return Err(Unsupported::ArgCount(0)),
        }
        Ok(())
    }

    /// Try to splice `callee`'s body into the current `Op::Call` site instead of
    /// emitting the per-call bridge. Returns `Ok(true)` when inlined, `Ok(false)`
    /// when the callee fails the pure-leaf / size / arity test (the caller then
    /// emits the normal direct-call bridge).
    ///
    /// The body runs only after a guard confirms the callee register holds
    /// exactly the speculated closure-less function value. It runs in a fresh
    /// native-stack scratch window the frame-register base `x19` is repointed at;
    /// `x19` (from the ctx) and `sp` are restored on every exit, including the
    /// bail path. Because the body has no GC point and commits nothing
    /// observable before a possible bail — and never restamps `bail_pc` — a guard
    /// or body bail re-runs the whole call in the interpreter, idempotently.
    fn try_emit_inline_call(
        ops: &mut Assembler,
        callee: &JitInlineCallee,
        call_operands: &[Operand],
        bail: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        let dst = reg(call_operands, 0)?;
        let callee_reg = reg(call_operands, 1)?;
        let argc = const_index(call_operands, 2)? as usize;

        if argc != usize::from(callee.param_count)
            || argc > INLINE_MAX_ARGS
            || callee.register_count > INLINE_MAX_REGS
            || callee.instructions.len() > INLINE_MAX_INSTRS
            || !callee.instructions.iter().all(|i| is_inline_pure_op(i.op))
        {
            return Ok(false);
        }

        // One private label per callee byte-PC for internal branches.
        let mut clabels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
        for i in &callee.instructions {
            clabels.insert(i.byte_pc, ops.new_dynamic_label());
        }
        let inline_done = ops.new_dynamic_label();
        let inline_bail = ops.new_dynamic_label();
        let after = ops.new_dynamic_label();
        let scratch_bytes = (u32::from(callee.register_count) * 8).next_multiple_of(16);

        // Identity guard (x19 = caller frame base, sp not yet moved): the callee
        // register must be exactly the speculated function value, else bail.
        load_reg(ops, 9, callee_reg)?;
        emit_load_u64(
            ops,
            10,
            (TAG_FUNCTION_ID << 48) | u64::from(callee.function_id),
        );
        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>bail);

        // Reserve scratch, copy args into param slots (read via caller base x19),
        // zero the remaining slots to undefined (a fresh frame's register state),
        // then repoint x19 at the scratch base for the body.
        if scratch_bytes > 0 {
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
        }
        for slot in 0..argc {
            let areg = reg(call_operands, 3 + slot)?;
            load_reg(ops, 9, areg)?;
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        emit_load_u64(ops, 9, TAG_SPECIAL << 48);
        for slot in argc..usize::from(callee.register_count) {
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        dynasm!(ops ; .arch aarch64 ; add x19, sp, #0);

        for i in &callee.instructions {
            dynasm!(ops ; .arch aarch64 ; =>clabels[&i.byte_pc]);
            emit_inline_pure_op(ops, i, inline_bail, inline_done, &clabels)?;
        }

        // Normal completion: result in x9, unwind scratch, restore caller base,
        // store to dst.
        dynasm!(ops ; .arch aarch64 ; =>inline_done);
        if scratch_bytes > 0 {
            dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
        }
        dynasm!(ops
            ; .arch aarch64
            ; ldr x19, [x20]
        );
        store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>after);

        // Bail path: unwind scratch so the shared bail epilogue sees the frame
        // base sp (it reloads x19/x20 from the stack), then jump to it.
        dynasm!(ops ; .arch aarch64 ; =>inline_bail);
        if scratch_bytes > 0 {
            dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
        }
        dynasm!(ops ; .arch aarch64 ; b =>bail ; =>after);
        Ok(true)
    }

    /// Whether an op may appear in an inlined read-only method body: the pure
    /// leaf set plus `LoadThis` (reads the spliced receiver slot) and
    /// `LoadProperty` (a sealed load from the receiver at a baked offset). Any
    /// other op — notably a property/element store — aborts the inline attempt,
    /// so a method with a side effect keeps using the full method call.
    fn is_inline_method_op(op: Op) -> bool {
        is_inline_pure_op(op) || matches!(op, Op::LoadThis | Op::LoadProperty | Op::StoreProperty)
    }

    /// Ops that cannot bail once emitted, so they are safe to run *after* an
    /// inline `StoreProperty` has already mutated the receiver (a bail there
    /// would re-run the whole method in the interpreter and double-apply the
    /// store). Loads of immediates/locals and `Return*` qualify; anything that
    /// can guard-and-bail (property access, arithmetic, coercions) does not.
    fn is_nonbailing_after_store(op: Op) -> bool {
        matches!(
            op,
            Op::LoadThis
                | Op::LoadInt32
                | Op::LoadLocal
                | Op::LoadUndefined
                | Op::LoadHole
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::StoreLocal
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    }

    /// Emit one op of an inlined method body. `this_slot` is the scratch slot
    /// holding the receiver; `prop_offsets` maps a body `LoadProperty` /
    /// `StoreProperty` byte-PC to the baked value slab byte offset.
    /// `LoadThis`, `LoadProperty`, and `StoreProperty` are handled here; every
    /// other op routes to [`emit_inline_pure_op`].
    #[allow(clippy::too_many_arguments)]
    fn emit_inline_method_op(
        ops: &mut Assembler,
        instr: &otter_vm::JitInstrView,
        this_slot: u16,
        prop_offsets: &rustc_hash::FxHashMap<u32, u32>,
        cage_base: usize,
        recv_shape: u32,
        object_shape_byte: u32,
        object_values_ptr_byte: u32,
        bail: DynamicLabel,
        inline_done: DynamicLabel,
        clabels: &BTreeMap<u32, DynamicLabel>,
    ) -> Result<(), Unsupported> {
        let ops_ref = instr.operands.as_slice();
        match instr.op {
            Op::LoadThis => {
                let dst = reg(ops_ref, 0)?;
                load_reg(ops, 9, this_slot)?;
                store_reg(ops, 9, dst)?;
                Ok(())
            }
            Op::LoadProperty => {
                let dst = reg(ops_ref, 0)?;
                let obj = reg(ops_ref, 1)?;
                let off = *prop_offsets
                    .get(&instr.byte_pc)
                    .ok_or(Unsupported::ArgCount(0))?;
                load_reg(ops, 9, obj)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; lsr x10, x9, #48
                    ; movz x11, TAG_PTR_OBJECT as u32
                    ; cmp x10, x11
                    ; b.ne =>bail
                    ; mov w12, w9
                );
                emit_load_u64(ops, 13, cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x13, x13, x12
                    ; ldr x13, [x13, object_values_ptr_byte]
                    ; cbz x13, =>bail
                    ; ldr x9, [x13, off]
                );
                store_reg(ops, 9, dst)?;
                Ok(())
            }
            Op::StoreProperty => {
                // Sealed value-slab store `recv.<prop> = src`. The receiver shape
                // is re-guarded (the baked offset is only valid for it) and the
                // value is required to be a non-`Gc` primitive — a pointer value
                // would need a generational write barrier that cannot run in the
                // remapped scratch window, so it bails *before* writing and the
                // interpreter re-runs the store with the barrier. Every guard
                // here bails ahead of the `str`, so no mutation is lost on a
                // fallback; the site emitter forbids any later bailing op.
                let obj = reg(ops_ref, 0)?;
                let src = reg(ops_ref, 2)?;
                let off = *prop_offsets
                    .get(&instr.byte_pc)
                    .ok_or(Unsupported::ArgCount(0))?;
                load_reg(ops, 9, obj)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; lsr x10, x9, #48
                    ; movz x11, TAG_PTR_OBJECT as u32
                    ; cmp x10, x11
                    ; b.ne =>bail
                    ; mov w12, w9
                );
                emit_load_u64(ops, 13, cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x13, x13, x12
                    ; ldrb w14, [x13]
                    ; cmp w14, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>bail
                    ; ldr w14, [x13, object_shape_byte]
                    ; movz w15, recv_shape & 0xffff
                    ; movk w15, (recv_shape >> 16) & 0xffff, lsl #16
                    ; cmp w14, w15
                    ; b.ne =>bail
                );
                let store_ok = ops.new_dynamic_label();
                load_reg(ops, 9, src)?;
                dynasm!(ops
                    ; .arch aarch64
                    // Gc-pointer tags are exactly 0x7FFC..=0x7FFF; bail on any of
                    // them so only barrier-free primitive stores are inlined.
                    // Tags below 0x7FFC (doubles/int/special/function-id) and at
                    // or above 0x8000 (negative doubles) are non-`Gc`.
                    ; lsr x10, x9, #48
                    ; movz x11, TAG_PTR_OBJECT as u32
                    ; cmp x10, x11
                    ; b.lo =>store_ok
                    ; movz x11, 0x8000
                    ; cmp x10, x11
                    ; b.hs =>store_ok
                    ; b =>bail
                    ; =>store_ok
                    ; ldr x13, [x13, object_values_ptr_byte]
                    ; cbz x13, =>bail
                    ; str x9, [x13, off]
                );
                Ok(())
            }
            _ => emit_inline_pure_op(ops, instr, bail, inline_done, clabels),
        }
    }

    /// Try to splice `method`'s body into the current `Op::CallMethodValue` site
    /// instead of building a callee frame. Returns `Ok(true)` when inlined,
    /// `Ok(false)` when the method fails the op-allowlist / size / arity test (the
    /// caller then emits the normal method-call bridge).
    ///
    /// Soundness: the body runs only after the inline identity guard confirms (a)
    /// the receiver shape matches the baked one, and (b) the receiver's flat
    /// prototype still resolves the method slot to the baked `function_id` — both
    /// re-read every call, so a prototype-method reassignment falls back to the
    /// in-place full method call (not a bail). The body touches only the
    /// receiver and performs no allocation, so it has no GC point; it runs in a
    /// native-stack scratch window with `x19` repointed, restored on every exit.
    /// A body `StoreProperty` mutates the receiver in place: every store guard
    /// bails *before* the write, and the emitter rejects any bailing op after a
    /// store, so a fallback never double-applies a mutation.
    #[allow(clippy::too_many_arguments)]
    fn try_emit_inline_method_call(
        ops: &mut Assembler,
        method: &JitInlineMethod,
        call_operands: &[Operand],
        site: u64,
        cage_base: usize,
        object_shape_byte: u32,
        object_values_ptr_byte: u32,
        jit_proto_byte: u32,
        closure_fid_byte: u32,
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        let dst = reg(call_operands, 0)?;
        let recv_reg = reg(call_operands, 1)?;
        let argc = const_index(call_operands, 3)? as usize;

        if cage_base == 0
            || argc != usize::from(method.param_count)
            || argc > INLINE_MAX_ARGS
            || method.register_count >= INLINE_MAX_REGS
            || method.instructions.len() > INLINE_MAX_INSTRS
            || !method
                .instructions
                .iter()
                .all(|i| is_inline_method_op(i.op))
        {
            return Ok(false);
        }

        // An inline `StoreProperty` mutates the receiver in place; a later bail
        // would re-run the whole method in the interpreter and double-apply the
        // store. Refuse to inline any body where a bailing op can follow a store.
        let mut store_seen = false;
        for i in &method.instructions {
            if store_seen && !is_nonbailing_after_store(i.op) {
                return Ok(false);
            }
            if i.op == Op::StoreProperty {
                store_seen = true;
            }
        }

        let mut clabels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
        for i in &method.instructions {
            clabels.insert(i.byte_pc, ops.new_dynamic_label());
        }
        let inline_done = ops.new_dynamic_label();
        let inline_bail = ops.new_dynamic_label();
        let fallback = ops.new_dynamic_label();
        let after = ops.new_dynamic_label();
        let fid_immediate = ops.new_dynamic_label();
        let fid_compare = ops.new_dynamic_label();
        // One extra slot past the method register window holds `this`.
        let this_slot = method.register_count;
        let scratch_regs = u32::from(method.register_count) + 1;
        let scratch_bytes = (scratch_regs * 8).next_multiple_of(16);

        // Inline identity guard, no per-call resolve bridge. Decompress the
        // receiver (x19 = caller frame base), require its shape to match the
        // baked one, then chase its flat prototype, guard the prototype's shape,
        // read the method slot, and compare the resolved closure's `function_id`
        // to the baked method id. Re-reading the prototype slot every call keeps
        // this sound against prototype-method reassignment: any mismatch (shape,
        // tag, slot tag, or id) lands on the in-place full method call.
        let recv_off = reg_offset(recv_reg)?;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, recv_off]
            ; lsr x10, x9, #48
            ; movz x11, TAG_PTR_OBJECT as u32
            ; cmp x10, x11
            ; b.ne =>fallback
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>fallback
            ; ldr w14, [x13, object_shape_byte]
            ; movz w15, method.recv_shape & 0xffff
            ; movk w15, (method.recv_shape >> 16) & 0xffff, lsl #16
            ; cmp w14, w15
            ; b.ne =>fallback
            // Flat prototype: load the compressed handle, bail on null, then
            // decompress and guard the prototype object's shape.
            ; ldr w9, [x13, jit_proto_byte]
            ; cbz w9, =>fallback
        );
        emit_load_u64(ops, 12, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x12, x9
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>fallback
            ; ldr w14, [x13, object_shape_byte]
            ; movz w15, method.proto_shape & 0xffff
            ; movk w15, (method.proto_shape >> 16) & 0xffff, lsl #16
            ; cmp w14, w15
            ; b.ne =>fallback
            // Method slot: load the 64-bit Value from the prototype's value
            // slab. A resolved method is either a closure-less bytecode
            // reference (`TAG_FUNCTION_ID`, fid in the low 32 bits) or a
            // closure pointer (`TAG_PTR_FUNCTION` → `JsClosureBody`, fid read
            // from its body). Decode the function id into w14 either way, then
            // compare to the baked id; anything else falls back.
            ; ldr x13, [x13, object_values_ptr_byte]
            ; cbz x13, =>fallback
            ; ldr x9, [x13, method.method_value_byte]
            ; lsr x10, x9, #48
            ; movz x11, TAG_FUNCTION_ID as u32
            ; cmp x10, x11
            ; b.eq =>fid_immediate
            ; movz x11, TAG_PTR_FUNCTION as u32
            ; cmp x10, x11
            ; b.ne =>fallback
            ; mov w12, w9
        );
        emit_load_u64(ops, 11, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x11, x11, x12
            // Require a closure body (a native method shares TAG_PTR_FUNCTION but
            // has no bytecode id at this offset), then read `function_id`.
            ; ldrb w14, [x11]
            ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG
            ; b.ne =>fallback
            ; ldr w14, [x11, closure_fid_byte]
            ; b =>fid_compare
            ; =>fid_immediate
            ; mov w14, w9
            ; =>fid_compare
            ; movz w15, method.method_fid & 0xffff
            ; movk w15, (method.method_fid >> 16) & 0xffff, lsl #16
            ; cmp w14, w15
            ; b.ne =>fallback
        );

        // Reserve scratch, copy method args into param slots, the receiver into
        // the `this` slot (all read via caller base x19), zero remaining slots to
        // undefined, then repoint x19 at the scratch base for the body.
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
        for slot in 0..argc {
            let areg = reg(call_operands, 4 + slot)?;
            load_reg(ops, 9, areg)?;
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        load_reg(ops, 9, recv_reg)?;
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, u32::from(this_slot) * 8]);
        emit_load_u64(ops, 9, TAG_SPECIAL << 48);
        for slot in argc..usize::from(method.register_count) {
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        dynasm!(ops ; .arch aarch64 ; add x19, sp, #0);

        for i in &method.instructions {
            dynasm!(ops ; .arch aarch64 ; =>clabels[&i.byte_pc]);
            emit_inline_method_op(
                ops,
                i,
                this_slot,
                &method.prop_offsets,
                cage_base,
                method.recv_shape,
                object_shape_byte,
                object_values_ptr_byte,
                inline_bail,
                inline_done,
                &clabels,
            )?;
        }

        // Normal completion: result in x9, unwind scratch, restore caller base.
        dynasm!(ops ; .arch aarch64 ; =>inline_done);
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
        dynasm!(ops ; .arch aarch64 ; ldr x19, [x20]);
        store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>after);

        // Body bail: unwind scratch, then the shared interpreter bail.
        dynasm!(ops ; .arch aarch64 ; =>inline_bail);
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes ; b =>bail);

        // Ineligible at run time (method changed / shape mismatch): the full
        // in-place method call, which restores nothing (sp untouched here).
        dynasm!(ops ; .arch aarch64 ; =>fallback);
        emit_method_call(ops, call_operands, site, threw)?;
        dynasm!(ops ; .arch aarch64 ; =>after);
        Ok(true)
    }

    /// Emit a direct `Call`: ask the VM to publish an eligible callee frame,
    /// build the callee `JitCtx` on the native stack, branch to the compiled
    /// entry, then finish/pop/store through the narrow direct-call ABI. Cold or
    /// ineligible calls bail to the interpreter instead of using the generic
    /// runtime call bridge.
    fn emit_call(
        ops: &mut Assembler,
        operands: &[Operand],
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let dst = reg(operands, 0)?;
        let callee = reg(operands, 1)?;
        let argc = const_index(operands, 2)? as usize;
        if argc > MAX_INLINE_ARGS {
            return Err(Unsupported::ArgCount(argc));
        }
        let direct_done = ops.new_dynamic_label();

        // jit_prepare_direct_call_stub(ctx, callee, argc, a0..a3) -> status.
        // 0 = direct prepared, 1 = throw, 2 = cold/ineligible → interpreter.
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, x20
            ; movz x1, callee as u32
            ; movz x2, argc as u32
        );
        for slot in 0..MAX_INLINE_ARGS {
            let areg = if slot < argc {
                reg(operands, 3 + slot)?
            } else {
                0
            };
            // arg registers map to x3..x6.
            let xn = 3 + slot as u32;
            dynasm!(ops ; .arch aarch64 ; movz X(xn), areg as u32);
        }
        emit_load_u64(ops, 16, jit_prepare_direct_call_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x0, #1
            ; b.eq =>threw
            ; cmp x0, #2
            ; b.eq =>bail
        );

        // Direct prepared (status 0): build the callee ctx, branch, finish.
        emit_direct_call_tail(ops, dst, threw, direct_done);
        dynasm!(ops ; .arch aarch64 ; =>direct_done);
        Ok(())
    }

    /// Shared direct-call dispatch tail used after a prepare stub returned
    /// status 0 (callee frame published in `ctx.direct_*`). Builds the callee
    /// `JitCtx` on the native stack, branches to the compiled entry, and runs
    /// the returned / bailed / threw finish helpers, landing at `done`.
    fn emit_direct_call_tail(
        ops: &mut Assembler,
        dst: u16,
        threw: DynamicLabel,
        done: DynamicLabel,
    ) {
        let direct_returned = ops.new_dynamic_label();
        let direct_bailed = ops.new_dynamic_label();
        let direct_threw = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; sub sp, sp, JIT_CTX_STACK_SIZE
            ; ldr x9, [x20, DIRECT_REGS_OFFSET]
            ; str x9, [sp]
            ; ldr x9, [x20, DIRECT_SELF_OFFSET]
            ; str x9, [sp, #8]
            ; ldr x9, [x20, DIRECT_THIS_OFFSET]
            ; str x9, [sp, #16]
            ; str wzr, [sp, BAIL_PC_OFFSET]
            ; ldr x9, [x20, VM_OFFSET]
            ; str x9, [sp, VM_OFFSET]
            ; ldr x9, [x20, STACK_OFFSET]
            ; str x9, [sp, STACK_OFFSET]
            ; ldr x9, [x20, CONTEXT_OFFSET]
            ; str x9, [sp, CONTEXT_OFFSET]
            ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; str x9, [sp, FRAME_INDEX_OFFSET]
            ; ldr x9, [x20, ERROR_SLOT_OFFSET]
            ; str x9, [sp, ERROR_SLOT_OFFSET]
            // Copy the prepared callee upvalue-spine base so inline upvalue ops
            // in the direct callee read its cells without the stub.
            ; ldr x9, [x20, DIRECT_UPVALUES_OFFSET]
            ; str x9, [sp, UPVALUES_PTR_OFFSET]
            ; mov x0, sp
            ; ldr x16, [x20, DIRECT_ENTRY_OFFSET]
            ; blr x16
            ; cmp x1, STATUS_RETURNED as u32
            ; b.eq =>direct_returned
            ; cmp x1, STATUS_BAILED as u32
            ; b.eq =>direct_bailed
            ; b =>direct_threw
            ; =>direct_returned
            ; mov x3, x0
            ; ldr x2, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; mov x0, x20
            ; movz x1, dst as u32
        );
        emit_call_stub(
            ops,
            jit_finish_direct_call_returned_stub as *const () as usize,
            threw,
        );
        dynasm!(ops ; .arch aarch64 ; b =>done);

        dynasm!(ops
            ; .arch aarch64
            ; =>direct_bailed
            ; ldr w3, [sp, BAIL_PC_OFFSET]
            ; ldr x2, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; mov x0, x20
            ; movz x1, dst as u32
        );
        emit_call_stub(
            ops,
            jit_finish_direct_call_bailed_stub as *const () as usize,
            threw,
        );
        dynasm!(ops ; .arch aarch64 ; b =>done);

        dynasm!(ops
            ; .arch aarch64
            ; =>direct_threw
            ; ldr x1, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; mov x0, x20
        );
        emit_call_stub(ops, jit_abort_direct_call_stub as *const () as usize, threw);
        // The caller places `done` (once) after any trailing fallback code.
        dynasm!(ops ; .arch aarch64 ; b =>threw);
    }

    /// Emit a direct `CallMethodValue`: resolve the method through the call
    /// site's monomorphic IC and direct-branch to its compiled entry, exactly
    /// like [`emit_call`]; on an ineligible resolution fall back to the in-place
    /// full method-call stub (not a bail) so cold / native / polymorphic methods
    /// keep running compiled.
    fn emit_method_call(
        ops: &mut Assembler,
        operands: &[Operand],
        site: u64,
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        const MAX_METHOD_ARGS: usize = 3;
        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let name = const_index(operands, 2)?;
        let argc = const_index(operands, 3)? as usize;
        if argc > MAX_METHOD_ARGS {
            return Err(Unsupported::ArgCount(argc));
        }

        let fallback = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();

        // jit_prepare_direct_method_call_stub(ctx, recv, name, site, argc, a0..a2)
        // -> 0 = direct prepared, 1 = throw, 2 = ineligible → in-place fallback.
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, x20
            ; movz x1, recv as u32
        );
        emit_load_u64(ops, 2, u64::from(name));
        emit_load_u64(ops, 3, site);
        dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
        for slot in 0..MAX_METHOD_ARGS {
            let areg = if slot < argc {
                reg(operands, 4 + slot)?
            } else {
                0
            };
            let xn = 5 + slot as u32;
            dynasm!(ops ; .arch aarch64 ; movz X(xn), areg as u32);
        }
        emit_load_u64(
            ops,
            16,
            jit_prepare_direct_method_call_stub as *const () as u64,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x0, #1
            ; b.eq =>threw
            ; cmp x0, #2
            ; b.eq =>fallback
        );

        // Direct prepared (status 0): same dispatch tail as Op::Call.
        emit_direct_call_tail(ops, dst, threw, done);

        // Ineligible (status 2): in-place full method call, returns to compiled
        // code (the receiver method may be native / polymorphic / accessor).
        dynasm!(ops
            ; .arch aarch64
            ; =>fallback
            ; mov x0, x20
            ; movz x1, dst as u32
            ; movz x2, recv as u32
        );
        emit_load_u64(ops, 3, u64::from(name));
        dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
        for slot in 0..MAX_METHOD_ARGS {
            let areg = if slot < argc {
                reg(operands, 4 + slot)?
            } else {
                0
            };
            let xn = 5 + slot as u32;
            dynasm!(ops ; .arch aarch64 ; movz X(xn), areg as u32);
        }
        emit_call_stub(ops, jit_call_method_stub as *const () as usize, threw);
        dynasm!(ops ; .arch aarch64 ; =>done);
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

    /// Probe the `Vec<u8>` field layout — which std does **not** guarantee — by
    /// value-identity, returning `(data_pointer_byte_offset, length_byte_offset)`
    /// of the two words within a `Vec<u8>`. Computed once and cached. The inline
    /// typed-array element path reads the backing buffer's data pointer and its
    /// live byte length (the memory-safety bound) at these offsets.
    pub(super) fn vec_layout_offsets() -> (u32, u32) {
        use std::sync::OnceLock;
        static CACHE: OnceLock<(u32, u32)> = OnceLock::new();
        *CACHE.get_or_init(|| {
            // capacity 4, length 1: cap, len, and the (large) data pointer are
            // three distinct values, so each machine word is identified
            // unambiguously by equality.
            let mut v: Vec<u8> = Vec::with_capacity(4);
            v.push(0xA5);
            let ptr = v.as_ptr() as usize;
            let len = v.len();
            assert_eq!(
                std::mem::size_of::<Vec<u8>>(),
                24,
                "Vec<u8> is not three machine words"
            );
            // SAFETY: copy the three machine words of the Vec by value; they are
            // only compared to the public pointer/length, never dereferenced.
            let words: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
            let mut ptr_off = None;
            let mut len_off = None;
            for (i, &w) in words.iter().enumerate() {
                if w == ptr {
                    ptr_off = Some((i * 8) as u32);
                } else if w == len {
                    len_off = Some((i * 8) as u32);
                }
            }
            (
                ptr_off.expect("Vec<u8> data-pointer word not found"),
                len_off.expect("Vec<u8> length word not found"),
            )
        })
    }

    /// Shared element-access prelude: load the receiver `Value` from its frame
    /// slot, guard the pointer-object tag, decompress to its GC body pointer,
    /// and read its header type tag. Leaves `x9` = body pointer, `x11` =
    /// cage base, `w10` = header type tag. A non-pointer receiver misses to
    /// `el_miss`. No safepoint, so the pointer is recomputed from the rooted
    /// frame slot every time and never held across a move.
    fn emit_recv_decompress(
        ops: &mut Assembler,
        cage_base: usize,
        recv_off: u32,
        el_miss: DynamicLabel,
    ) {
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, recv_off]      // receiver Value
            ; lsr x14, x9, #48
            ; movz x15, TAG_PTR_OBJECT as u32
            ; cmp x14, x15
            ; b.ne =>el_miss
            ; mov w12, w9                  // low-32 Gc offset (zero-ext, scratch)
        );
        emit_load_u64(ops, 11, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x9, x11, x12             // x9 = body GcHeader ptr
            ; ldrb w10, [x9]               // w10 = header type tag
        );
    }

    /// Shared element-access prelude: load the index `Value`, guard it is an
    /// int32, and leave the zero-extended `u32` payload in `x12`. A non-int32
    /// index misses to `el_miss`.
    fn emit_idx_int32(ops: &mut Assembler, idx_off: u32, el_miss: DynamicLabel) {
        dynasm!(ops
            ; .arch aarch64
            ; ldr x12, [x19, idx_off]      // index Value
            ; lsr x14, x12, #48
            ; movz x15, TAG_INT32 as u32
            ; cmp x14, x15
            ; b.ne =>el_miss               // non-int32 index → stub
            ; and x12, x12, #0xffffffff    // index = zero-extended u32 payload
        );
    }

    /// Typed-array backing resolution. Assumes the prelude already set `x9` =
    /// typed-array body ptr, `x11` = cage base, `x12` = int32 index. Guards
    /// not-length-tracking → index in `[0, cached length)` → `Local`
    /// (non-shared) backing → local buffer body tag, then dispatches on element
    /// kind to `f64_path` / `i32_path` leaving `x13` = buffer data pointer,
    /// `x16` = view byte offset, `x17` = live `Vec<u8>` byte length (the
    /// detach/resize memory-safety bound). Any miss → `el_miss`.
    fn emit_ta_backing(
        ops: &mut Assembler,
        ta: &JitTypedArrayLayout,
        el_miss: DynamicLabel,
        f64_path: DynamicLabel,
        i32_path: DynamicLabel,
    ) {
        let local_buf_type_tag = u32::from(ta.local_buffer_type_tag);
        let local_tag = ta.buffer_local_tag;
        let kind_f64 = ta.kind_float64;
        let kind_i32 = ta.kind_int32;
        let length_tracking_byte = ta.ta_length_tracking_byte;
        let length_byte = ta.ta_length_byte;
        let byte_offset_byte = ta.ta_byte_offset_byte;
        let buffer_disc_byte = ta.buffer_disc_byte;
        let buffer_handle_byte = ta.buffer_handle_byte;
        // The std `Vec` field order is not guaranteed, so the buffer body
        // carries only the Vec base; add the probed data-pointer / length word
        // sub-offsets here.
        let (ptr_word, len_word) = vec_layout_offsets();
        let bytes_ptr_byte = ta.buf_bytes_byte + ptr_word;
        let bytes_len_byte = ta.buf_bytes_byte + len_word;
        let kind_byte = ta.ta_kind_byte;
        dynasm!(ops
            ; .arch aarch64
            ; ldrb w14, [x9, length_tracking_byte]
            ; cbnz w14, =>el_miss          // length-tracking view → stub
            ; ldr x14, [x9, length_byte]   // cached element length
            ; cmp x12, x14
            ; b.hs =>el_miss               // index >= length (unsigned) → stub
            ; ldr w14, [x9, buffer_disc_byte]
            ; cmp w14, local_tag
            ; b.ne =>el_miss               // Shared backing → stub
            ; ldr w15, [x9, buffer_handle_byte]
            ; add x10, x11, x15            // x10 = local buffer GcHeader ptr
            ; ldrb w14, [x10]
            ; cmp w14, local_buf_type_tag
            ; b.ne =>el_miss
            ; ldr x13, [x10, bytes_ptr_byte]   // Vec<u8> data pointer
            ; ldr x17, [x10, bytes_len_byte]   // live Vec<u8> byte length
            ; ldr x16, [x9, byte_offset_byte]  // view byte offset
            ; ldr w14, [x9, kind_byte]         // element kind
            ; cmp w14, kind_f64
            ; b.eq =>f64_path
            ; cmp w14, kind_i32
            ; b.eq =>i32_path
            ; b =>el_miss                  // other kinds → stub
        );
    }

    /// Typed-array store guard chain (the store path is typed-array only for
    /// now): prelude + `Float64Array`/`Int32Array` backing dispatch.
    #[allow(clippy::too_many_arguments)]
    fn emit_ta_guard_chain(
        ops: &mut Assembler,
        ta: &JitTypedArrayLayout,
        cage_base: usize,
        recv_off: u32,
        idx_off: u32,
        el_miss: DynamicLabel,
        f64_path: DynamicLabel,
        i32_path: DynamicLabel,
    ) {
        let ta_type_tag = u32::from(ta.ta_type_tag);
        emit_recv_decompress(ops, cage_base, recv_off, el_miss);
        dynasm!(ops ; .arch aarch64 ; cmp w10, ta_type_tag ; b.ne =>el_miss);
        emit_idx_int32(ops, idx_off, el_miss);
        emit_ta_backing(ops, ta, el_miss, f64_path, i32_path);
    }

    /// Unified inline `LoadElement`: one receiver decompress + one index guard,
    /// then a header-type-tag dispatch to the dense-`Array` path (raw `Value`
    /// with a hole-sentinel guard) or the typed-array path (`Float64Array` /
    /// `Int32Array`, box/unbox). Anything else — other kinds, a hole, an
    /// out-of-bounds or non-int32 index, a non-array/typed-array receiver —
    /// misses to `el_miss` (the runtime stub, which owns the spec-correct
    /// prototype / sparse / accessor / string semantics). No safepoint.
    #[allow(clippy::too_many_arguments)]
    fn emit_element_load(
        ops: &mut Assembler,
        layout: &JitTypedArrayLayout,
        cage_base: usize,
        recv_off: u32,
        idx_off: u32,
        dst_off: u32,
        el_miss: DynamicLabel,
        el_done: DynamicLabel,
    ) {
        let array_tag = u32::from(layout.array_type_tag);
        let ta_tag = u32::from(layout.ta_type_tag);
        let (ptr_word, len_word) = vec_layout_offsets();
        let arr_ptr_byte = layout.array_elements_byte + ptr_word;
        let arr_len_byte = layout.array_elements_byte + len_word;
        let hole_bits = (TAG_SPECIAL << 48) | SPECIAL_HOLE;
        let array_path = ops.new_dynamic_label();
        let ta_path = ops.new_dynamic_label();
        let f64_path = ops.new_dynamic_label();
        let i32_path = ops.new_dynamic_label();

        emit_recv_decompress(ops, cage_base, recv_off, el_miss);
        emit_idx_int32(ops, idx_off, el_miss);
        dynasm!(ops
            ; .arch aarch64
            ; cmp w10, array_tag
            ; b.eq =>array_path
            ; cmp w10, ta_tag
            ; b.eq =>ta_path
            ; b =>el_miss
        );

        // Dense Array: element is a raw 8-byte Value. Bounds-check against the
        // live `elements` Vec length, then a hole sentinel → stub (the stub
        // walks the prototype / sparse / accessor, all spec-owned there).
        dynasm!(ops
            ; .arch aarch64
            ; =>array_path
            ; ldr x17, [x9, arr_len_byte]      // elements Vec length
            ; cmp x12, x17
            ; b.hs =>el_miss                   // index >= length → stub
            ; ldr x13, [x9, arr_ptr_byte]      // elements Vec data pointer
            ; lsl x14, x12, #3                 // index * sizeof(Value)
            ; add x14, x13, x14                // element address
            ; ldr x13, [x14]                   // the Value
        );
        emit_load_u64(ops, 15, hole_bits);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x13, x15
            ; b.eq =>el_miss                   // hole → stub
            ; str x13, [x19, dst_off]
            ; b =>el_done
        );

        // Typed array: resolve backing, then per-kind load + box.
        dynasm!(ops ; .arch aarch64 ; =>ta_path);
        emit_ta_backing(ops, layout, el_miss, f64_path, i32_path);
        dynasm!(ops
            ; .arch aarch64
            ; =>f64_path
            ; lsl x14, x12, #3                 // index * 8
            ; add x14, x14, x16                // + byte_offset
            ; add x15, x14, #8                 // + element size (bound)
            ; cmp x15, x17
            ; b.hi =>el_miss
            ; add x14, x13, x14                // element address
            ; ldr d0, [x14]
        );
        emit_box_double(ops, 0, 15);
        dynasm!(ops
            ; .arch aarch64
            ; str x15, [x19, dst_off]
            ; b =>el_done
            ; =>i32_path
            ; lsl x14, x12, #2                 // index * 4
            ; add x14, x14, x16                // + byte_offset
            ; add x15, x14, #4                 // + element size (bound)
            ; cmp x15, x17
            ; b.hi =>el_miss
            ; add x14, x13, x14                // element address
            ; ldr w13, [x14]                   // signed int32 (low-32)
        );
        box_low32!(ops, 13, 15, TAG_INT32);
        dynasm!(ops
            ; .arch aarch64
            ; str x13, [x19, dst_off]
            ; b =>el_done
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
                load_array_length: false,
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
            cage_base: 0,
            ta_layout: otter_vm::JitTypedArrayLayout::default(),
            object_shape_byte: 8,
            object_values_ptr_byte: 16,
            jit_proto_byte: 12,
            closure_fid_byte: 8,
            instructions,
            inline_callees: Default::default(),
            inline_methods: Default::default(),
        }
    }

    /// The inline typed-array element path locates the backing buffer's data
    /// pointer and live length inside a `Vec<u8>` via `vec_layout_offsets`
    /// (std does not guarantee the field order). Verify the probe lands on the
    /// real pointer and length words for an independent Vec.
    #[test]
    fn vec_layout_probe_finds_ptr_and_len() {
        let (ptr_off, len_off) = super::arm64::vec_layout_offsets();
        assert_ne!(ptr_off, len_off, "ptr and len must be distinct words");
        assert!(
            ptr_off < 24 && len_off < 24,
            "offsets within the 3-word Vec"
        );
        let mut v: Vec<u8> = Vec::with_capacity(16);
        v.extend_from_slice(&[1, 2, 3, 4, 5]);
        // SAFETY: read one machine word at each probed offset and compare to the
        // public pointer/length; never dereferenced beyond the read itself.
        let base = std::ptr::addr_of!(v).cast::<u8>();
        let read_word = |off: u32| unsafe { base.add(off as usize).cast::<usize>().read() };
        assert_eq!(read_word(ptr_off), v.as_ptr() as usize, "probe ptr word");
        assert_eq!(read_word(len_off), v.len(), "probe len word");
    }

    // Branch encoding: target = branch_byte_pc + 1 + rel (see `branch_target`),
    // with branch_byte_pc = from*STRIDE and target = to*STRIDE.
    fn rel(from: usize, to: usize) -> i32 {
        (to as i32 - from as i32) * STRIDE as i32 - 1
    }

    fn run(view: &JitFunctionView, regs: &mut [u64]) -> Exit {
        let code = compile(view).expect("compiles");
        let mut error = None;
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            self_closure: 0,
            this_value: 0,
            vm: std::ptr::null_mut(),
            stack: std::ptr::null_mut(),
            context: std::ptr::null(),
            frame_index: 0,
            upvalues_ptr: 0,
            bail_pc: 0,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
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

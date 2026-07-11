//! Sparkplug-style baseline emitter (arm64).
//!
//! Lowers a [`otter_vm::JitCompileSnapshot`] to native arm64 with **no IR, no
//! register allocation, and no deopt** — one linear pass, one emit routine per
//! supported opcode, branch fixups via dynasm dynamic labels. Operands and
//! results flow through the executing frame's register window; compiled code
//! reaches the VM through named runtime stubs on [`otter_vm::Interpreter`] for
//! calls, allocation helpers, property fallbacks, and cooperative backedge
//! polling.
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
//! Framed entries use VM-frame registers; frameless JIT-to-JIT entries reserve
//! windows in the interpreter's fixed flat register stack. Both stores are
//! traced as precise roots for their full live extent. No movable JS pointer is
//! kept only in a machine register across a safepoint; allocating callees remain
//! framed and carry exact safepoint records.
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
pub(crate) use otter_vm::value::tag as value_tag;
use otter_vm::{
    HoltStack, Interpreter, JitCompileSnapshot, JitExecOutcome, JitFunctionCode,
    RuntimeStubAllocContext, SafepointRecord, Value, VmError, VmRuntimeActivation,
    native_abi::{
        CodeRegistryView, NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread,
    },
    runtime_stubs::{alloc_value_stub_trampoline_pair, leaf_no_alloc_stub2_trampoline_pair},
};

use crate::CompiledCode;

mod runtime_ops;
use runtime_ops::{
    jit_add_stub, jit_define_data_property_stub, jit_define_own_property_stub,
    jit_fresh_upvalue_stub, jit_load_builtin_error_stub, jit_load_string_stub,
    jit_make_closure_stub, jit_math_call_stub, jit_neg_stub, jit_new_array_stub,
    jit_store_upvalue_checked_stub,
};

// The compiled value encoding is single-sourced from the frozen
// `otter_vm::value::tag` constants: emitted box / unbox / tag-test code bakes
// these exact bit patterns, and the `debug_assert!`s below trip if the frozen
// layout is ever edited out from under the codegen.
//
// A `Value` is one tagged 64-bit word: an int32 carries `NUMBER_TAG` in its
// high bits with the payload in the low 32; a double is its IEEE bits plus
// `DOUBLE_ENCODE_OFFSET`; a heap cell is a bare 4 GiB-cage offset (top 16 bits
// zero, `OTHER_TAG` clear); the immediates `null` / `false` / `true` /
// `undefined` / hole and the closure-less function id are small `OTHER_TAG`
// patterns. Cell-class disambiguation (object vs closure vs string) is by the
// GC header type tag, never by the value word.

/// High 16 bits of [`value_tag::NUMBER_TAG`] — `movz Xd, NUMBER_TAG_HI16, lsl #48`
/// materializes the number tag in one instruction (its low 48 bits are zero).
pub(crate) const NUMBER_TAG_HI16: u32 = (value_tag::NUMBER_TAG >> 48) as u32;
/// High 16 bits of [`value_tag::DOUBLE_ENCODE_OFFSET`] (`0x0002`), added when
/// boxing a double and subtracted when unboxing.
pub(crate) const DOUBLE_OFFSET_HI16: u32 = (value_tag::DOUBLE_ENCODE_OFFSET >> 48) as u32;
/// High 16 bits of [`value_tag::CANONICAL_NAN`] (`0x7ff8`) — every boxed NaN
/// canonicalises to this before the double offset is applied.
pub(crate) const CANONICAL_NAN_HI16: u32 = (value_tag::CANONICAL_NAN >> 48) as u32;
/// `null` immediate (full value word).
pub(crate) const VALUE_NULL: u64 = value_tag::VALUE_NULL;
/// `false` immediate (full value word). A boolean boxes as
/// `VALUE_FALSE + (cond as 0|1)`, i.e. `VALUE_FALSE` / `VALUE_TRUE`.
pub(crate) const VALUE_FALSE: u64 = value_tag::VALUE_FALSE;
/// Low 32 bits of [`VALUE_FALSE`] — the bare-`u32` immediate a `cset`+`add`
/// boolean box folds in (an `add` immediate is not a logical-immediate cast).
pub(crate) const VALUE_FALSE_LOW: u32 = value_tag::VALUE_FALSE as u32;
/// `true` immediate (full value word).
pub(crate) const VALUE_TRUE: u64 = value_tag::VALUE_TRUE;
/// `undefined` immediate (full value word).
pub(crate) const VALUE_UNDEFINED: u64 = value_tag::VALUE_UNDEFINED;
/// Internal array / `this` hole sentinel (full value word).
pub(crate) const VALUE_HOLE: u64 = value_tag::VALUE_HOLE;
/// Low 16 bits selecting the closure-less function-id immediate; the function
/// id sits in bits `[16, 48)`, so a value boxes as `(fid << 16) | FUNCTION_ID_TAG`
/// and an inline call site guards identity by comparing the callee word to that
/// exact immediate.
pub(crate) const FUNCTION_ID_TAG: u64 = value_tag::FUNCTION_ID_TAG;

const _: () = assert!(value_tag::NUMBER_TAG == 0xfffe_0000_0000_0000);
const _: () = assert!(value_tag::DOUBLE_ENCODE_OFFSET == 0x0002_0000_0000_0000);
const _: () = assert!(value_tag::NOT_CELL_MASK == value_tag::NUMBER_TAG | value_tag::OTHER_TAG);
const _: () = assert!(value_tag::CANONICAL_NAN == 0x7ff8_0000_0000_0000);

/// GC header type tag for an ordinary `ObjectBody` (mirrors
/// `otter_vm::object::OBJECT_BODY_TYPE_TAG`). A heap cell is disambiguated by
/// this tag before an inline shape-slot read, since every cell value word is a
/// bare cage offset with no class tag of its own.
pub(crate) const OBJECT_BODY_TYPE_TAG: u32 = 0x11;
/// GC header type tag for a `JsClosureBody` (mirrors
/// `otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG`). Guarded before reading a
/// resolved method's `function_id` so a native callable cell is never misread
/// as a bytecode closure.
pub(crate) const JS_CLOSURE_BODY_TYPE_TAG: u32 = 0x23;
/// Largest argument count the `Call` emitter inlines (args passed in registers
/// to the call stub). Functions called with more args fall back.
const MAX_INLINE_ARGS: usize = 4;

/// Largest argument count a `CallMethodValue` site passes inline. The emitter
/// packs the argument *register indices* one per 16-bit lane of a single word,
/// so a full method-call stub needs only one register for all of them (leaving
/// room in the C ABI's eight argument registers); raising this past 4 needs a
/// second packed word.
pub(crate) const MAX_METHOD_ARGS: usize = 4;

/// Pack up to [`MAX_METHOD_ARGS`] argument register indices into one word (one
/// per 16-bit lane), the form the method-call stubs receive.
pub(crate) fn pack_method_arg_regs(arg_regs: &[u16]) -> u64 {
    let mut packed = 0u64;
    for (slot, &areg) in arg_regs.iter().take(MAX_METHOD_ARGS).enumerate() {
        packed |= u64::from(areg) << (16 * slot);
    }
    packed
}

/// Unpack the [`MAX_METHOD_ARGS`] argument register indices a method-call stub
/// received (one per 16-bit lane).
fn unpack_method_arg_regs(packed: u64) -> [u16; MAX_METHOD_ARGS] {
    [
        (packed & 0xffff) as u16,
        ((packed >> 16) & 0xffff) as u16,
        ((packed >> 32) & 0xffff) as u16,
        ((packed >> 48) & 0xffff) as u16,
    ]
}

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
    /// Sole machine-visible VM state pointer.
    thread: *mut VmThread,
    /// Published authoritative activation.
    native_frame: *mut NativeFrame,
    /// Index of the executing frame within `stack`.
    frame_index: usize,
    /// Base of this frame's upvalue spine (`Box<[UpvalueCell]>` data; each a
    /// 4-byte compressed cell handle), or `0` when the frame captures nothing
    /// or the function captures nothing. Inline `LoadUpvalue` /
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
    /// Base of the interpreter's flat JIT register stack
    /// (`reg_stack[0]`). Compiled code builds a self-recursive callee window at
    /// `reg_stack_base + reg_top*8` without a Rust frame-build bridge.
    reg_stack_base: *mut u64,
    /// Address of the interpreter's `reg_top` (live extent of the flat register
    /// stack, in slots). Compiled code loads it, reserves a callee window by
    /// adding the callee register count, and stores it back; the matching pop on
    /// return restores it.
    reg_top_ptr: *mut usize,
    /// Shared synchronous native-reentry depth counter.
    sync_reentry_depth_ptr: *mut u32,
    /// Effective limit checked before a frameless native call mutates state.
    sync_reentry_limit: u32,
    /// Address of the live array-index accessor protector. Dense array stores
    /// read through this pointer at the store site, not at entry, because a
    /// re-entered VM call can invalidate the protector before later stores.
    array_index_accessor_protector_ptr: *const bool,
    /// Base of the VM-published live collection method IC slots.
    collection_method_ics: *const otter_vm::JitCollectionMethodIcSlot,
    /// Number of live collection method IC slots.
    collection_method_ic_count: u32,
    /// Base of the VM-published flat direct-method inline-link table (indexed by
    /// IC site). Baseline code reads a slot to build the callee window and
    /// branch to a compiled method with no Rust bridge.
    direct_method_inline: *const otter_vm::JitDirectMethodInline,
    /// Opaque heap pointer for native leaf runtime stubs.
    gc_heap: *const std::ffi::c_void,
    /// Address of the cooperative interrupt flag's backing byte. Compiled code
    /// polls this inline at every back-edge and re-enters only when it is set.
    interrupt_flag: *const u8,
    /// Address of the VM's back-edge fuel counter. Compiled code decrements it
    /// inline per back-edge and re-enters the poll stub when it reaches zero,
    /// batching the budget checkpoint across the whole run of iterations.
    backedge_fuel: *mut u64,
}

impl JitCtx {
    /// VM-owned activation published through the sole machine-visible thread
    /// pointer. Runtime stubs use this explicitly; emitted code never observes
    /// its Rust pointers or container types.
    fn activation(&self) -> &VmRuntimeActivation {
        // SAFETY: runtime-capable contexts point at the VmThread built for the
        // current entry, whose runtime_context retains VmRuntimeActivation.
        unsafe { &*((*self.thread).runtime_context as *const VmRuntimeActivation) }
    }
}

/// Two-word return of compiled code (`x0`/`x1` on arm64).
#[repr(C)]
pub(crate) struct JitRet {
    value: u64,
    status: u64,
}

/// `status` discriminants in [`JitRet`].
pub(crate) const STATUS_RETURNED: u64 = 0;
pub(crate) const STATUS_BAILED: u64 = 1;
pub(crate) const STATUS_THREW: u64 = 2;

/// Byte offset of [`JitCtx::bail_pc`] — where compiled code stamps the current
/// instruction's byte-PC before each op so a bail resumes at the exact site.
pub(crate) const BAIL_PC_OFFSET: u32 = std::mem::offset_of!(JitCtx, bail_pc) as u32;
/// Byte offset of [`JitCtx::error`] for nested direct-call context construction.
#[allow(dead_code)]
pub(crate) const ERROR_SLOT_OFFSET: u32 = std::mem::offset_of!(JitCtx, error) as u32;
pub(crate) const THREAD_OFFSET: u32 = std::mem::offset_of!(JitCtx, thread) as u32;
pub(crate) const NATIVE_FRAME_OFFSET: u32 = std::mem::offset_of!(JitCtx, native_frame) as u32;
pub(crate) const NATIVE_FRAME_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, code_object_id) as u32;
/// Byte offset of [`JitCtx::interrupt_flag`] — the inline back-edge interrupt poll.
pub(crate) const INTERRUPT_FLAG_OFFSET: u32 = std::mem::offset_of!(JitCtx, interrupt_flag) as u32;
/// Byte offset of [`JitCtx::backedge_fuel`] — the inline back-edge fuel counter.
pub(crate) const BACKEDGE_FUEL_OFFSET: u32 = std::mem::offset_of!(JitCtx, backedge_fuel) as u32;
pub(crate) const FRAME_INDEX_OFFSET: u32 = std::mem::offset_of!(JitCtx, frame_index) as u32;
/// Byte offset of [`JitCtx::upvalues_ptr`] for inline upvalue access.
pub(crate) const UPVALUES_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, upvalues_ptr) as u32;
/// Byte offset of [`JitCtx::reg_stack_base`] — the flat JIT register stack base
/// used to build a self-recursive callee window inline.
pub(crate) const REG_STACK_BASE_OFFSET: u32 = std::mem::offset_of!(JitCtx, reg_stack_base) as u32;
/// Byte offset of [`JitCtx::reg_top_ptr`] — the address of the interpreter's
/// `reg_top`, bumped to reserve a callee window and restored on return.
pub(crate) const REG_TOP_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, reg_top_ptr) as u32;
pub(crate) const SYNC_REENTRY_DEPTH_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, sync_reentry_depth_ptr) as u32;
pub(crate) const SYNC_REENTRY_LIMIT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, sync_reentry_limit) as u32;
pub(crate) const ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, array_index_accessor_protector_ptr) as u32;
pub(crate) const COLLECTION_METHOD_ICS_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, collection_method_ics) as u32;
pub(crate) const COLLECTION_METHOD_IC_COUNT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, collection_method_ic_count) as u32;
pub(crate) const COLLECTION_METHOD_IC_SLOT_SIZE: u32 =
    std::mem::size_of::<otter_vm::JitCollectionMethodIcSlot>() as u32;
/// Byte offset of [`JitCtx::direct_method_inline`] (the direct-method link table
/// base) and the flat slot layout baseline code reads.
pub(crate) const DIRECT_METHOD_INLINE_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_method_inline) as u32;
pub(crate) const DIRECT_METHOD_INLINE_SLOT_SIZE: u32 =
    std::mem::size_of::<otter_vm::JitDirectMethodInline>() as u32;
pub(crate) const DIRECT_METHOD_ENTRY_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, entry_addr) as u32;
pub(crate) const DIRECT_METHOD_REGISTER_COUNT_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, register_count) as u32;
pub(crate) const DIRECT_METHOD_RECV_SHAPE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, recv_shape_offset) as u32;
pub(crate) const DIRECT_METHOD_PROTO_SHAPE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, proto_shape_offset) as u32;
pub(crate) const DIRECT_METHOD_ON_RECEIVER_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, method_on_receiver) as u32;
pub(crate) const DIRECT_METHOD_VALUE_BYTE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, method_value_byte) as u32;
pub(crate) const DIRECT_METHOD_FID_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitDirectMethodInline, method_fid) as u32;
pub(crate) const COLLECTION_METHOD_IC_STATE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, state) as u32;
pub(crate) const COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, receiver_type_tag) as u32;
pub(crate) const COLLECTION_METHOD_IC_PROTO_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, proto_offset) as u32;
pub(crate) const COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, proto_shape) as u32;
pub(crate) const COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, method_value_byte) as u32;
pub(crate) const COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, leaf_stub_id) as u32;
pub(crate) const COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, alloc_stub_id) as u32;
pub(crate) const COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitCollectionMethodIcSlot, builtin_fn_addr) as u32;
#[allow(dead_code)]
pub(crate) const GC_HEAP_OFFSET: u32 = std::mem::offset_of!(JitCtx, gc_heap) as u32;
pub(crate) const ALLOC_CTX_THREAD_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, thread) as u32;
pub(crate) const ALLOC_CTX_FRAME_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, frame) as u32;
pub(crate) const ALLOC_CTX_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, code_object_id) as u32;
pub(crate) const ALLOC_CTX_SAFEPOINT_ID_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, safepoint_id) as u32;
pub(crate) const ALLOC_CTX_RESERVED0_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, reserved0) as u32;
pub(crate) const ALLOC_CTX_RESERVED1_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, reserved1) as u32;
pub(crate) const ALLOC_CTX_SPILL_SLOTS_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, spill_slots) as u32;
pub(crate) const ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, spill_slot_count) as u32;
pub(crate) const ALLOC_CTX_STACK_SIZE: u32 =
    ((std::mem::size_of::<RuntimeStubAllocContext>() + 15) & !15) as u32;
/// Size of one `UpvalueCell` (a 4-byte compressed `Gc<UpvalueCellBody>`).
pub(crate) const UPVALUE_CELL_SIZE: u32 = 4;
/// Byte offset of the single `Value` inside an `UpvalueCellBody` from its
/// decompressed pointer (just past the 8-byte `GcHeader`).
pub(crate) const UPVALUE_VALUE_OFFSET: u32 = 8;
pub(crate) const DIRECT_ENTRY_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_entry_addr) as u32;
pub(crate) const DIRECT_REGS_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_regs) as u32;
pub(crate) const DIRECT_SELF_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_self_closure) as u32;
pub(crate) const DIRECT_THIS_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_this_value) as u32;
/// Byte offset of the precomputed `this` bits in [`JitCtx`], for inline
/// `LoadThis` in baseline entries.
pub(crate) const THIS_VALUE_OFFSET: u32 = std::mem::offset_of!(JitCtx, this_value) as u32;
pub(crate) const DIRECT_FRAME_INDEX_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_frame_index) as u32;
pub(crate) const DIRECT_UPVALUES_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_upvalues_ptr) as u32;
pub(crate) const JIT_CTX_STACK_SIZE: u32 = ((std::mem::size_of::<JitCtx>() + 15) & !15) as u32;

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

/// Publish one machine-constructed [`JitCtx`] before its compiled entry can
/// reach an allocating/reentrant safepoint. Returns `0` on success and parks a
/// stack-overflow error in the shared slot on failure.
pub(crate) extern "C" fn jit_push_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the caller has fully initialized `ctx` on its native stack and
    // keeps it live until the matching pop stub.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    // SAFETY: both fields live inside `ctx`, whose native allocation remains
    // live across the compiled callee's dynamic extent.
    match unsafe {
        vm.jit_push_native_activation(
            std::ptr::addr_of_mut!(ctx.self_closure),
            std::ptr::addr_of_mut!(ctx.this_value),
        )
    } {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Release the topmost native JIT activation before its `JitCtx` stack record
/// is discarded.
pub(crate) extern "C" fn jit_pop_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the active context and its interpreter pointer are live by ABI.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    vm.jit_pop_native_activation();
    0
}

/// Validate a closure callee for scratch-frame inlining and return its captured
/// upvalue-spine base, or `0` when the site must take the normal call path.
pub(crate) extern "C" fn jit_inline_closure_upvalues_stub(
    ctx: *mut JitCtx,
    callee_reg: u64,
    expected_fid: u64,
) -> usize {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    // SAFETY: `callee_reg` comes from a bytecode register operand inside the
    // active frame window.
    let callee_bits = unsafe { *ctx.regs.add(callee_reg as usize) };
    vm.jit_inline_closure_upvalues(Value::from_bits(callee_bits), expected_fid as u32)
        .unwrap_or(0)
}

/// Prepare a direct compiled **method** call (`recv.name(args…)`). Same
/// `ctx.direct_*` / status contract as [`jit_prepare_direct_call_stub`], but
/// status `2` means "ineligible — use the in-place full method-call stub"
/// rather than "bail to the interpreter" (a native/polymorphic method in a hot
/// loop must keep running compiled).
#[allow(clippy::too_many_arguments)]
pub(crate) extern "C" fn jit_prepare_direct_method_call_stub(
    ctx: *mut JitCtx,
    recv: u64,
    name_idx: u64,
    site: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    let status = match vm.jit_prepare_direct_method_call(
        context,
        stack,
        ctx.frame_index,
        recv as u16,
        name_idx as u32,
        site as usize,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>().cast_const(),
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
    };
    refresh_jit_collection_method_ics(ctx, vm);
    status
}

pub(crate) extern "C" fn jit_finish_direct_call_returned_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    value: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
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

pub(crate) extern "C" fn jit_finish_direct_call_bailed_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    bail_pc: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
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

pub(crate) extern "C" fn jit_abort_direct_call_stub(
    ctx: *mut JitCtx,
    callee_frame_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    vm.jit_abort_direct_call(stack, callee_frame_index as usize);
    0
}

/// Bridge stub for a *frameless* self-recursive callee that bailed: rebuild an
/// interpreter frame from the live register-stack window and run it to
/// completion. Returns the callee's value in `x0` with `STATUS_RETURNED`, or
/// `STATUS_THREW` (error parked in `ctx`) on an uncaught throw.
pub(crate) extern "C" fn jit_self_call_bail_stub(
    ctx: *mut JitCtx,
    bail_pc: u64,
    regcount: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_self_call_bail(
        context,
        stack,
        ctx.frame_index,
        bail_pc as u32,
        regcount as usize,
    ) {
        Ok(value) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(err) => {
            park_jit_error(ctx, err);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
        }
    }
}

/// Complete a frameless direct-method callee after its compiled entry bailed.
/// The VM rebuilds the callee frame from the rooted flat register window and
/// the already-resolved method value; no bytecode instruction is decoded here.
pub(crate) extern "C" fn jit_direct_method_call_bail_stub(
    ctx: *mut JitCtx,
    bail_pc: u64,
    regcount: u64,
    callee: u64,
    this: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_direct_method_call_bail(
        context,
        stack,
        bail_pc as u32,
        regcount as usize,
        Value::from_bits(callee),
        Value::from_bits(this),
    ) {
        Ok(value) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(err) => {
            park_jit_error(ctx, err);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
        }
    }
}

/// Bridge stub: build a `MakeFunction` closure from compiled code. Returns `0`
/// on success, `1` when construction threw (error parked in `ctx`).
extern "C" fn jit_make_fn_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_make_function(context, stack, ctx.frame_index, dst as u16, idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Poll VM interrupts and runtime budget on compiled back-edges. Mirrors the
/// interpreter's cooperative checkpoint so watchdogs and budget rejection apply
/// equally after a loop tiers up through OSR.
pub(crate) extern "C" fn jit_backedge_poll_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    if ctx.activation().vm_ptr().is_null() {
        return 0;
    }
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    match vm.jit_backedge_poll() {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Number of shapes a WhiskerIC site caches inline before it is megamorphic and
/// always misses to the stub. Four matches the polymorphism most real sites
/// reach (V8 / JSC use the same width); a bimorphic site (e.g. two object
/// layouts alternating through one loop) then stays fully inline instead of
/// thrashing a single cell.
const IC_WAYS: usize = 4;

/// One cached `(shape → slot)` mapping in a [`WhiskerIcCell`].
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WhiskerIcWay {
    /// Cached receiver shape-handle compressed offset; `0` == empty.
    shape: u32,
    /// Byte offset from the value slab pointer to the value slot.
    value_byte: u32,
}

/// WhiskerIC self-patching cell for one named-property site (one per
/// `LoadProperty` / `StoreProperty` op in the compiled function). Emitted code
/// walks the [`IC_WAYS`] ways comparing each `shape` (a `0` shape never matches
/// a live receiver, so empty ways are skipped for free); on a hit it reads the
/// matched way's `value_byte`. On a monomorphic own-data inline-slot miss the
/// stub fills the next empty way, so a poly site caches every shape it sees up
/// to the width. The cell holds only compressed offsets (no GC pointers), so it
/// needs no tracing, and a shape offset is a stable token (shapes are immortal
/// and pinned in old space).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WhiskerIcCell {
    ways: [WhiskerIcWay; IC_WAYS],
}

/// Self-patch one IC cell with a resolved `(shape, value_byte)` mapping: fill
/// the first empty way, or evict way 0 when all are full (the site is more
/// polymorphic than the cache is wide). Writes `value_byte` before `shape` so a
/// concurrent inline guard never reads a live shape against a stale offset.
///
/// # Safety
/// `cell` must be a valid, stable [`WhiskerIcCell`] pointer (a site's cell from
/// the owning `BaselineCode` backing slice).
unsafe fn whisker_ic_fill(cell: *mut WhiskerIcCell, shape: u32, value_byte: u32) {
    unsafe {
        let ways = &mut (*cell).ways;
        let slot = ways
            .iter()
            .position(|w| w.shape == 0 || w.shape == shape)
            .unwrap_or(0);
        ways[slot].value_byte = value_byte;
        ways[slot].shape = shape;
    }
}

/// Frameless `LoadProperty` miss handler for a self-recursive callee running on
/// the flat register window (no `HoltStack` frame). Resolves the own-data IC
/// directly against `ctx.regs`; returns `0` on an inline-eligible hit (value
/// written, cell self-patched), `2` when the load needs the full `[[Get]]`
/// ladder (caller bails to the interpreter), `1` on throw. `function_id` is
/// baked by the emitter (the window has no frame to read it from).
extern "C" fn jit_load_prop_window_stub(
    ctx: *mut JitCtx,
    dst: u64,
    obj: u64,
    name_idx: u64,
    site: u64,
    cell: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract; `ctx.regs` is the GC-traced
    // register window of the executing (framed or frameless) callee.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match unsafe {
        vm.jit_runtime_load_property_window(
            context,
            ctx.regs,
            function_id as u32,
            dst as u16,
            obj as u16,
            name_idx as u32,
            site as usize,
        )
    } {
        Ok(Some(fill)) => {
            if cell != 0 && fill != 0 {
                let cell = cell as *mut WhiskerIcCell;
                // SAFETY: stable per-site cell address baked into this code.
                unsafe {
                    whisker_ic_fill(cell, fill as u32, (fill >> 32) as u32);
                }
            }
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Frameless `StoreProperty` miss handler — the [`jit_load_prop_window_stub`]
/// counterpart. Resolves an existing-own-data store (with barrier) against
/// `ctx.regs` and runs ordinary data-property transitions from decoded values.
/// Returns `0` when handled (self-patching the cell when eligible), `2` for
/// accessor/exotic/non-object semantics, and `1` on throw.
extern "C" fn jit_store_prop_window_stub(
    ctx: *mut JitCtx,
    obj: u64,
    name_idx: u64,
    src: u64,
    site: u64,
    cell: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: as `jit_load_prop_window_stub`.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match unsafe {
        vm.jit_runtime_store_property_window(
            context,
            ctx.regs,
            function_id as u32,
            obj as u16,
            name_idx as u32,
            src as u16,
            site as usize,
        )
    } {
        Ok(Some(fill)) => {
            if cell != 0 && fill != 0 {
                let cell = cell as *mut WhiskerIcCell;
                // SAFETY: stable per-site cell address baked into this code.
                unsafe {
                    whisker_ic_fill(cell, fill as u32, (fill >> 32) as u32);
                }
            }
            0
        }
        Ok(None) => 2,
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    vm.jit_runtime_write_barrier(stack, ctx.frame_index, obj as u16, src as u16);
    0
}

/// Frameless write barrier — reads the parent/child from `ctx.regs` so an
/// inline `StoreProperty` of a pointer value works without a `HoltStack` frame
/// (used by frameless-eligible bodies, framed or frameless).
extern "C" fn jit_write_barrier_window_stub(ctx: *mut JitCtx, obj: u64, src: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract; `ctx.regs` is GC-traced.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    unsafe { vm.jit_runtime_write_barrier_window(ctx.regs, obj as u16, src as u16) };
    0
}

/// Bridge stub: perform a computed `LoadElement` (`recv[idx]`) from compiled
/// code, delegating to the safe [`Interpreter::jit_runtime_load_element`].
/// Returns `0` on success, `1` when the read threw (error parked in `ctx`).
extern "C" fn jit_load_element_stub(ctx: *mut JitCtx, dst: u64, recv: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
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
extern "C" fn jit_load_global_stub(
    ctx: *mut JitCtx,
    dst: u64,
    name_idx: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_load_global(
        context,
        stack,
        ctx.frame_index,
        function_id as u32,
        dst as u16,
        name_idx as u32,
    ) {
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    match vm.jit_runtime_store_upvalue(stack, ctx.frame_index, src as u16, idx as i32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Bridge stub: allocate an ordinary object for `NewObject` from compiled code.
/// Uses the VM's stack-rooted allocator so moving young-GC semantics match the
/// interpreter path.
extern "C" fn jit_new_object_stub(ctx: *mut JitCtx, dst: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    match vm.jit_runtime_new_object(stack, ctx.frame_index, dst as u16) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

extern "C" fn otter_jit_math_random() -> u64 {
    Value::number(otter_vm::math::random_number()).to_bits()
}

/// Narrow collection-IC method bridge.
///
/// Return status: `0` = IC hit and `dst` written, `1` = throw parked in ctx,
/// `2` = miss, continue to the generic method path.
#[allow(clippy::too_many_arguments)]
pub(crate) extern "C" fn jit_call_collection_method_ic_stub(
    ctx: *mut JitCtx,
    dst: u64,
    recv: u64,
    site: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    let status = match vm.jit_runtime_try_collection_method_ic(
        stack,
        ctx.frame_index,
        dst as u16,
        recv as u16,
        site as usize,
        &all[..argc],
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    };
    refresh_jit_collection_method_ics(ctx, vm);
    status
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
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
    /// Installed code-object identity used for safepoint lookup.
    code_object_id: u64,
    /// Tagged register-window width published in the native frame.
    register_count: u16,
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
    /// `LoadProperty` op, self-patched by [`jit_load_prop_window_stub`]. Emitted code
    /// holds raw addresses into this slice, so it must never be moved out or
    /// cloned after `compile` returns (the code object is only ever shared by
    /// `Arc`, never cloned by value). Boxed so the buffer address is fixed.
    #[allow(dead_code)]
    load_ic_cells: Box<[WhiskerIcCell]>,
    /// Stable backing store for the WhiskerIC `StoreProperty` cells — one per
    /// `StoreProperty` op, self-patched by [`jit_store_prop_window_stub`]. Same
    /// ownership / stability contract as [`Self::load_ic_cells`].
    #[allow(dead_code)]
    store_ic_cells: Box<[WhiskerIcCell]>,
    /// Stable decoded source-register tables for `NewArray` sites. Emitted code
    /// passes pointers into these boxed slices to [`jit_new_array_stub`], so the
    /// tables must live exactly as long as the executable mapping.
    #[allow(dead_code)]
    array_literal_regs: Box<[Box<[u16]>]>,
    /// Stable decoded parent-upvalue index tables for `MakeClosure` sites.
    #[allow(dead_code)]
    closure_parent_indices: Box<[Box<[u32]>]>,
    /// Stable decoded argument-register tables for non-leaf `MathCall` sites.
    #[allow(dead_code)]
    math_argument_regs: Box<[Box<[u16]>]>,
    /// Stable backing store for code-object-owned allocating safepoints.
    safepoint_records: Box<[SafepointRecord]>,
    /// Every op in the body addresses registers through the window
    /// (`JitCtx.regs`), so the body is sound to enter frameless (see
    /// [`JitFunctionCode::frameless_entry_safe`]).
    frameless_entry_safe: bool,
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

    fn frameless_entry_safe(&self) -> bool {
        self.frameless_entry_safe
    }

    fn entry_addr(&self) -> Option<usize> {
        // SAFETY: the mapping is live for `self`; callers must keep the owning
        // code object installed while using this address.
        Some(unsafe { self.code.entry_ptr() as usize })
    }

    fn safepoint_count(&self) -> u32 {
        self.safepoint_records.len() as u32
    }

    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome {
        // SAFETY: the mapping is live and the main entry was emitted with the
        // `JitEntry` ABI.
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `activation` upholds the
        // reentry contract (valid, non-aliased for the call).
        unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.register_count,
                &self.safepoint_records,
            )
        }
    }

    fn osr_entry(&self, activation: VmRuntimeActivation, byte_pc: u32) -> Option<JitExecOutcome> {
        let offset = *self.osr_entries.get(&byte_pc)?;
        // SAFETY: `offset` is an assembler offset recorded for this buffer and
        // points at a prologue trampoline emitted with the `JitEntry` ABI.
        let entry = unsafe { self.code.ptr_at(offset) };
        // SAFETY: same reentry contract as `run_entry`.
        Some(unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.register_count,
                &self.safepoint_records,
            )
        })
    }
}

/// Build the `JitCtx` for `activation` and invoke compiled code at `entry`, mapping
/// the returned status to a [`JitExecOutcome`].
///
/// Shared across compiled tiers and entry kinds: the baseline function-entry
/// and loop-header OSR paths, and the optimizing tier — every compiled entry
/// uses the identical [`JitEntry`] ABI (`extern "C" fn(*mut JitCtx) -> JitRet`)
/// and the same `JitCtx` construction, differing only in which instruction the
/// prologue branches to. Lives free (it uses no compiled-code state) so any
/// [`JitFunctionCode`] implementation can reuse it.
///
/// # Safety
/// `entry` must point at a prologue emitted with the [`JitEntry`] ABI inside a
/// live executable mapping that outlives the call, and `activation` must uphold the
/// [`VmRuntimeActivation`](otter_vm::VmRuntimeActivation) contract.
#[repr(C)]
struct ActiveSafepoints {
    code_object_id: u64,
    records: *const SafepointRecord,
    count: u32,
}

unsafe extern "C" fn resolve_active_safepoint(
    context: u64,
    code_object_id: u64,
    safepoint_id: u32,
) -> *const SafepointRecord {
    if context == 0 {
        return std::ptr::null();
    }
    // SAFETY: enter_compiled retains the registry and record slice for the call.
    let active = unsafe { &*(context as *const ActiveSafepoints) };
    if active.code_object_id != code_object_id || active.records.is_null() {
        return std::ptr::null();
    }
    // SAFETY: publisher records the exact live boxed-slice extent.
    let records = unsafe { std::slice::from_raw_parts(active.records, active.count as usize) };
    records
        .binary_search_by_key(&safepoint_id, |record| record.id)
        .ok()
        .map_or(std::ptr::null(), |index| &raw const records[index])
}

pub(crate) unsafe fn enter_compiled(
    activation: VmRuntimeActivation,
    entry: *const u8,
    code_object_id: u64,
    register_count: u16,
    safepoint_records: &[SafepointRecord],
) -> JitExecOutcome {
    {
        let stack = activation.stack_ptr().cast::<HoltStack>();
        let vm = activation.vm_ptr().cast::<Interpreter>();
        // SAFETY: `activation.stack_ptr()` is a valid `*mut HoltStack` for this call.
        let regs =
            Interpreter::jit_frame_regs_ptr(unsafe { &mut *stack }, activation.frame_index());
        // SAFETY: `activation.vm_ptr()`/`activation.stack_ptr()` are valid for this call and not aliased
        // by a live `&mut` (the VM froze its borrows); read the self closure up
        // front so a `MakeFunction`-of-self needs no Rust round-trip.
        let self_closure =
            unsafe { (*vm).jit_frame_self_closure_bits(&*stack, activation.frame_index()) };
        // SAFETY: same validity/aliasing contract as `self_closure` above.
        let this_value = unsafe { (*vm).jit_frame_this_bits(&*stack, activation.frame_index()) };
        // SAFETY: same validity/aliasing contract; the spine `Box` outlives this
        // entry (frame-owned), and the cells it holds are old-space (immobile).
        let upvalues_ptr =
            Interpreter::jit_frame_upvalues_ptr(unsafe { &*stack }, activation.frame_index());
        // SAFETY: `vm` is a valid `*mut Interpreter` for this entry and not
        // aliased by a live `&mut` (the VM froze its borrows); these return the
        // stable base / `reg_top` address of the flat JIT register stack.
        let reg_stack_base = unsafe { (*vm).jit_reg_stack_base() };
        let reg_top_ptr = unsafe { (*vm).jit_reg_top_ptr() };
        let sync_reentry_depth_ptr = unsafe { (*vm).jit_sync_reentry_depth_ptr() };
        let sync_reentry_limit = unsafe { (*vm).jit_sync_reentry_limit() };
        let array_index_accessor_protector_ptr =
            unsafe { (*vm).jit_array_index_accessor_protector_ptr() };
        let collection_method_ics = unsafe { (*vm).jit_collection_method_ics_ptr() };
        let collection_method_ic_count = unsafe { (*vm).jit_collection_method_ics_len() };
        let direct_method_inline = unsafe { (*vm).jit_direct_method_inline_ptr() };
        let gc_heap = unsafe { (*vm).jit_gc_heap_ptr() };
        let interrupt_flag = unsafe { (*vm).jit_interrupt_flag_ptr() };
        let backedge_fuel = unsafe { (*vm).jit_backedge_fuel_ptr() };
        let active_safepoints = ActiveSafepoints {
            code_object_id,
            records: safepoint_records.as_ptr(),
            count: safepoint_records.len() as u32,
        };
        let registry = CodeRegistryView {
            context: std::ptr::addr_of!(active_safepoints) as u64,
            resolve_safepoint: resolve_active_safepoint as *const () as u64,
        };
        let flags = if safepoint_records.is_empty() {
            NativeFrameFlags::empty()
        } else {
            NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS)
        };
        let mut native_frame = NativeFrame {
            header: VmFrameHeader {
                function_id: code_object_id.saturating_sub(1) as u32,
                code_block_id: code_object_id.saturating_sub(1) as u32,
                pc: 0,
                register_count,
                kind: NativeFrameKind::Baseline,
                flags,
            },
            previous_frame: 0,
            register_base: regs as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id,
            this_value_bits: this_value,
            new_target_bits: Value::undefined().to_bits(),
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
            argument_count: 0,
            reserved0: 0,
            feedback_id: 0,
        };
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.runtime_context = std::ptr::addr_of!(activation) as u64;
        thread.code_registry = std::ptr::addr_of!(registry) as u64;
        thread.interrupt_cell = interrupt_flag as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            regs,
            self_closure,
            this_value,
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            frame_index: activation.frame_index(),
            upvalues_ptr,
            bail_pc: 0,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
            reg_stack_base,
            reg_top_ptr,
            sync_reentry_depth_ptr,
            sync_reentry_limit,
            array_index_accessor_protector_ptr,
            collection_method_ics,
            collection_method_ic_count,
            direct_method_inline,
            gc_heap,
            interrupt_flag,
            backedge_fuel,
        };
        // SAFETY: the mapping is live and `entry` was emitted with the
        // `JitEntry` ABI.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let activation_status = jit_push_native_activation_stub(&mut ctx);
        if activation_status != 0 {
            return JitExecOutcome::Threw(error.take().unwrap_or(VmError::InvalidOperand));
        }
        let ret = entry(&mut ctx);
        let _ = jit_pop_native_activation_stub(&mut ctx);
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

fn refresh_jit_collection_method_ics(ctx: &mut JitCtx, vm: &Interpreter) {
    ctx.collection_method_ics = vm.jit_collection_method_ics_ptr();
    ctx.collection_method_ic_count = vm.jit_collection_method_ics_len();
    // The direct-method inline table can reallocate too; refresh its base with the
    // collection ICs at every reentry so a bridge that grew it leaves the compiled
    // caller a valid pointer.
    ctx.direct_method_inline = vm.jit_direct_method_inline_ptr();
}

#[cfg(target_arch = "aarch64")]
pub(crate) mod arm64 {
    #![allow(unused_parens)]
    use super::{
        ALLOC_CTX_CODE_OBJECT_ID_OFFSET, ALLOC_CTX_FRAME_OFFSET, ALLOC_CTX_RESERVED0_OFFSET,
        ALLOC_CTX_RESERVED1_OFFSET, ALLOC_CTX_SAFEPOINT_ID_OFFSET,
        ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE,
        ALLOC_CTX_THREAD_OFFSET, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET, BACKEDGE_FUEL_OFFSET,
        BAIL_PC_OFFSET, BaselineCode, CANONICAL_NAN_HI16,
        COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET, COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET,
        COLLECTION_METHOD_IC_COUNT_OFFSET, COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET,
        COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET, COLLECTION_METHOD_IC_PROTO_OFFSET,
        COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET, COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET,
        COLLECTION_METHOD_IC_SLOT_SIZE, COLLECTION_METHOD_IC_STATE_OFFSET,
        COLLECTION_METHOD_ICS_OFFSET, DIRECT_ENTRY_OFFSET, DIRECT_FRAME_INDEX_OFFSET,
        DIRECT_METHOD_ENTRY_OFFSET, DIRECT_METHOD_FID_OFFSET, DIRECT_METHOD_INLINE_OFFSET,
        DIRECT_METHOD_INLINE_SLOT_SIZE, DIRECT_METHOD_ON_RECEIVER_OFFSET,
        DIRECT_METHOD_PROTO_SHAPE_OFFSET, DIRECT_METHOD_RECV_SHAPE_OFFSET,
        DIRECT_METHOD_REGISTER_COUNT_OFFSET, DIRECT_METHOD_VALUE_BYTE_OFFSET, DIRECT_REGS_OFFSET,
        DIRECT_SELF_OFFSET, DIRECT_THIS_OFFSET, DIRECT_UPVALUES_OFFSET, DOUBLE_OFFSET_HI16,
        ERROR_SLOT_OFFSET, FRAME_INDEX_OFFSET, FUNCTION_ID_TAG, GC_HEAP_OFFSET, IC_WAYS,
        INTERRUPT_FLAG_OFFSET, JIT_CTX_STACK_SIZE, JS_CLOSURE_BODY_TYPE_TAG, MAX_INLINE_ARGS,
        MAX_METHOD_ARGS, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET, NATIVE_FRAME_OFFSET, NUMBER_TAG_HI16,
        OBJECT_BODY_TYPE_TAG, Op, Operand, REG_STACK_BASE_OFFSET, REG_TOP_PTR_OFFSET,
        STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, SYNC_REENTRY_DEPTH_PTR_OFFSET,
        SYNC_REENTRY_LIMIT_OFFSET, THIS_VALUE_OFFSET, THREAD_OFFSET, UPVALUE_CELL_SIZE,
        UPVALUE_VALUE_OFFSET, UPVALUES_PTR_OFFSET, Unsupported, VALUE_FALSE, VALUE_FALSE_LOW,
        VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, WhiskerIcCell,
        alloc_value_stub_trampoline_pair, jit_abort_direct_call_stub, jit_add_stub,
        jit_backedge_poll_stub, jit_call_collection_method_ic_stub, jit_define_data_property_stub,
        jit_define_own_property_stub, jit_direct_method_call_bail_stub,
        jit_finish_direct_call_bailed_stub, jit_finish_direct_call_returned_stub,
        jit_fresh_upvalue_stub, jit_inline_closure_upvalues_stub, jit_load_builtin_error_stub,
        jit_load_element_stub, jit_load_global_stub, jit_load_prop_window_stub,
        jit_load_string_stub, jit_load_upvalue_stub, jit_make_closure_stub, jit_make_fn_stub,
        jit_math_call_stub, jit_neg_stub, jit_new_array_stub, jit_new_object_stub,
        jit_pop_native_activation_stub, jit_prepare_direct_method_call_stub,
        jit_push_native_activation_stub, jit_self_call_bail_stub, jit_store_element_stub,
        jit_store_prop_window_stub, jit_store_upvalue_checked_stub, jit_store_upvalue_stub,
        jit_write_barrier_stub, jit_write_barrier_window_stub, leaf_no_alloc_stub2_trampoline_pair,
        otter_jit_math_random, pack_method_arg_regs, reg_offset, value_tag,
    };
    use crate::CompiledCode;
    use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
    use otter_vm::Interpreter;
    use otter_vm::{
        JitArrayMethod, JitArrayMethodKind, JitCollectionAllocMethod, JitCollectionLeafMethod,
        JitCompileSnapshot, JitInlineCallee, JitInlineMethod, JitTypedArrayLayout, NO_FRAME_STATE,
        STUB_COLLECTION_SET_ADD_ALLOC, STUB_STRING_CONCAT_ALLOC, SafepointId, SafepointRecord,
        jit::{JIT_COLLECTION_METHOD_IC_COLLECTION, JIT_COLLECTION_METHOD_IC_NO_STUB},
        runtime_stubs::alloc_value_stub_by_id,
    };
    use std::collections::{BTreeMap, BTreeSet};

    /// Comparison flavors that emit a `cset` from integer `cmp` flags.
    enum Cmp {
        Lt,
        Le,
        Gt,
        Ge,
        Eq,
        Ne,
    }

    /// Box the int32 payload in the low 32 bits of `Xt` by setting `NUMBER_TAG`.
    /// The producing op wrote `Xt` through its `W` view, which on AArch64 zeroes
    /// bits [63:32], so a single `orr` with the tag completes the box.
    macro_rules! box_int32 {
        ($ops:expr, $t:literal, $scratch:literal) => {
            dynasm!($ops
                ; .arch aarch64
                ; movz X($scratch), NUMBER_TAG_HI16, lsl #48
                ; orr X($t), X($t), X($scratch)
            );
        };
    }

    /// Box a boolean: a preceding `cset` wrote `0`/`1` into `W(t)`; adding
    /// `VALUE_FALSE` yields the full `VALUE_FALSE` / `VALUE_TRUE` immediate word
    /// (the high bits are already zero from the `W` write).
    macro_rules! box_bool {
        ($ops:expr, $t:literal, $scratch:literal) => {
            dynasm!($ops
                ; .arch aarch64
                ; movz W($scratch), VALUE_FALSE_LOW
                ; add W($t), W($t), W($scratch)
            );
        };
    }

    /// Emit an int32 guard on x-register `r`: bail unless every `NUMBER_TAG` bit
    /// is set (`(r & NUMBER_TAG) == NUMBER_TAG`). Clobbers x14/x15.
    macro_rules! guard_int32 {
        ($ops:expr, $r:literal, $bail:expr) => {
            dynasm!($ops
                ; .arch aarch64
                ; movz x15, NUMBER_TAG_HI16, lsl #48
                ; and x14, X($r), x15
                ; cmp x14, x15
                ; b.ne =>$bail
            );
        };
    }

    /// Emit a "value is a Number" guard on x-register `r`: bail unless any
    /// `NUMBER_TAG` bit is set (an int32 sets all of them, a boxed double at
    /// least one). Cells / immediates carry none and bail. Clobbers x15.
    macro_rules! guard_number {
        ($ops:expr, $r:literal, $bail:expr) => {
            dynasm!($ops
                ; .arch aarch64
                ; movz x15, NUMBER_TAG_HI16, lsl #48
                ; tst X($r), x15
                ; b.eq =>$bail
            );
        };
    }

    /// Emit `dst = ToPrimitive(src)` for already-primitive values. Heap cells
    /// (objects, callables, strings) and bytecode-function references bail to
    /// the interpreter so observable `@@toPrimitive` / `valueOf` / `toString`
    /// hooks still run; numbers and the `null` / boolean / `undefined`
    /// immediates pass through unchanged. Clobbers x9/x14/x15.
    fn emit_to_primitive_identity(
        ops: &mut Assembler,
        dst: u16,
        src: u16,
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let keep = ops.new_dynamic_label();
        load_reg(ops, 9, src)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; tst x9, x15                 // number → already primitive
            ; b.ne =>keep
            ; orr x15, x15, #value_tag::OTHER_TAG   // NOT_CELL_MASK
            ; tst x9, x15
            ; b.eq =>bail                 // heap cell (object/string/callable)
            ; and x14, x9, #0xffff
            ; cmp x14, #(FUNCTION_ID_TAG as u32)
            ; b.eq =>bail                 // closure-less function reference
            ; =>keep
        );
        store_reg(ops, 9, dst)
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
        operands: impl WordOperands,
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
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, x9, x15
            ; cmp x14, x15
            ; b.ne =>float_path
            ; and x14, x10, x15
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
        box_int32!(ops, 13, 12);
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

    /// Emit `Add` with the same numeric inline path as [`emit_add_sub_mul`],
    /// but delegate non-number operands back to the VM instead of bailing out
    /// of compiled code. That keeps string/boolean/null `+` loops resident in
    /// baseline JIT while preserving the interpreter's full `+` semantics.
    fn emit_add_with_runtime_fallback(
        ops: &mut Assembler,
        operands: impl WordOperands,
        string_concat_safepoint: Option<SafepointId>,
        register_count: u16,
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        let float_path = ops.new_dynamic_label();
        let runtime_path = ops.new_dynamic_label();
        let delegate_path = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, x9, x15
            ; cmp x14, x15
            ; b.ne =>float_path
            ; and x14, x10, x15
            ; cmp x14, x15
            ; b.ne =>float_path
            ; adds w13, w9, w10
            ; b.vs =>float_path
        );
        box_int32!(ops, 13, 12);
        store_reg(ops, 13, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
        emit_num_to_double(ops, 9, 0, runtime_path);
        emit_num_to_double(ops, 10, 1, runtime_path);
        dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1);
        emit_box_double(ops, 2, 13);
        store_reg(ops, 13, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>runtime_path);
        if let Some(safepoint) = string_concat_safepoint {
            emit_string_concat_alloc_call(
                ops,
                dst,
                lhs,
                rhs,
                safepoint,
                register_count,
                delegate_path,
                done,
            )?;
        }
        dynasm!(ops
            ; .arch aarch64
            ; =>delegate_path
            ; mov x0, x20
            ; movz x1, dst as u32
            ; movz x2, lhs as u32
            ; movz x3, rhs as u32
        );
        emit_call_stub(ops, jit_add_stub as *const () as usize, threw);
        dynasm!(ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_string_concat_alloc_call(
        ops: &mut Assembler,
        dst: u16,
        lhs: u16,
        rhs: u16,
        safepoint: SafepointId,
        _register_count: u16,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let Some(stub_addr) =
            alloc_value_stub_by_id(STUB_STRING_CONCAT_ALLOC.id).and_then(|stub| stub.entry_addr())
        else {
            return Ok(());
        };
        let undefined_bits = VALUE_UNDEFINED;
        dynasm!(ops
            ; .arch aarch64
            ; sub sp, sp, ALLOC_CTX_STACK_SIZE
            ; ldr x9, [x20, THREAD_OFFSET]
            ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
            ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
            ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
            ; movz w9, safepoint
            ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
            ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
            ; movz w9, #0
            ; strh w9, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
            ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
            ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; mov x0, sp
        );
        emit_load_u64(ops, 1, u64::from(safepoint));
        load_reg(ops, 2, lhs)?;
        load_reg(ops, 3, rhs)?;
        emit_load_u64(ops, 4, undefined_bits);
        emit_load_u64(ops, 16, stub_addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; and x1, x1, #0xff
            ; mov x5, x1
            ; add sp, sp, ALLOC_CTX_STACK_SIZE
            ; cbnz x5, =>miss
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(())
    }

    /// Emit `Div`: division always yields a Number (f64) in ECMAScript — even
    /// `6 / 2` is the Number `3` — so there is no int fast path; decode both
    /// operands to f64 and `fdiv`. A non-number operand bails to `bail`.
    fn emit_div(
        ops: &mut Assembler,
        operands: impl WordOperands,
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

    /// Emit `Rem` (`%`): an int32 fast path that computes the truncating integer
    /// remainder with `sdiv`/`msub`. Cases the integer path cannot represent
    /// `bail` to the interpreter, which owns the full `f64`/`fmod` semantics:
    /// a non-int32 operand, a zero divisor (`NaN`), and a zero remainder from a
    /// negative dividend (JS yields `-0`, which int32 cannot encode). A zero
    /// remainder from a non-negative dividend is `+0` and stays on the int path.
    /// `i32::MIN % -1` needs no special case: AArch64 `sdiv` defines it as
    /// `i32::MIN`, so `msub` yields the correct `0` remainder.
    fn emit_rem(
        ops: &mut Assembler,
        operands: impl WordOperands,
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        guard_int32!(ops, 9, bail);
        guard_int32!(ops, 10, bail);
        let store = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; cbz w10, =>bail          // rhs == 0 → interpreter yields NaN
            ; sdiv w11, w9, w10        // truncating quotient
            ; msub w13, w11, w10, w9   // remainder = lhs - quotient * rhs
            ; cbnz w13, =>store        // nonzero remainder: sign already correct
            ; tbnz w9, #31, =>bail     // zero remainder, negative dividend → -0
            ; =>store
        );
        box_int32!(ops, 13, 12);
        store_reg(ops, 13, dst)?;
        Ok(())
    }

    /// First caller-saved FP register used to park decoded `f64` values.
    const FP_RESIDENCY_BASE: u32 = 3;
    /// Number of FP residency registers (`d3`..=`d7`). Caller-saved (`v8`–`v15`
    /// are callee-saved and would force a prologue spill on every call), and
    /// clobbered across calls — which is exactly where residency is cleared.
    const FP_RESIDENCY_REGS: usize = 5;

    /// Tracks which frame slots have their decoded `f64` currently parked in a
    /// caller-saved FP register, so a later float consumer reads the register
    /// instead of reloading + NaN-decoding the slot. This is the first
    /// optimizing-tier slice ([`OPTIMIZING_TIER.md`] S1): a write-through read
    /// cache over the linear emitter. Memory stays authoritative (every parked
    /// value is also boxed and stored to its slot), so the cache is advisory —
    /// dropping any entry is always sound. Unboxed numbers are not GC pointers,
    /// so holding one in a register across ops cannot dangle.
    #[derive(Default)]
    struct FloatResidency {
        /// `entries[i] == Some(slot)` means `d(FP_RESIDENCY_BASE + i)` holds the
        /// `f64` of frame slot `slot`.
        entries: [Option<u16>; FP_RESIDENCY_REGS],
        /// Round-robin victim for the next assignment.
        next: usize,
    }

    impl FloatResidency {
        /// Drop all residency — used at block boundaries (branch targets,
        /// safepoints, any op outside the modelled numeric set).
        fn clear(&mut self) {
            self.entries = [None; FP_RESIDENCY_REGS];
        }

        /// Drop any entry for `slot` (its value in memory/registers changed).
        fn invalidate(&mut self, slot: u16) {
            for e in self.entries.iter_mut() {
                if *e == Some(slot) {
                    *e = None;
                }
            }
        }

        /// FP register currently holding `slot`'s `f64`, if any.
        fn lookup(&self, slot: u16) -> Option<u32> {
            self.entries
                .iter()
                .position(|e| *e == Some(slot))
                .map(|i| FP_RESIDENCY_BASE + i as u32)
        }

        /// Reserve an FP register for `slot` (evicting round-robin) and return
        /// its number. The evicted slot is simply dropped from the cache — its
        /// authoritative value is still in memory.
        fn assign(&mut self, slot: u16) -> u32 {
            self.invalidate(slot);
            let i = self.next;
            self.next = (self.next + 1) % FP_RESIDENCY_REGS;
            self.entries[i] = Some(slot);
            FP_RESIDENCY_BASE + i as u32
        }
    }

    /// Materialize `slot` as an `f64` in `dst_d`. Reads the parked residency
    /// register when present (a plain `fmov`); otherwise loads the boxed Value
    /// into `scratch_x` and NaN-decodes it, bailing on a non-number.
    fn load_operand_f64(
        ops: &mut Assembler,
        fres: &FloatResidency,
        slot: u16,
        dst_d: u32,
        scratch_x: u32,
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        if let Some(src_d) = fres.lookup(slot) {
            if src_d != dst_d {
                dynasm!(ops ; .arch aarch64 ; fmov D(dst_d), D(src_d));
            }
        } else {
            load_reg(ops, scratch_x, slot)?;
            emit_num_to_double(ops, scratch_x, dst_d, bail);
        }
        Ok(())
    }

    /// Residency-aware `Add`/`Sub`/`Mul`/`Div`. Computes purely in `f64` (no
    /// int fast path), writes the boxed result through to the frame slot so
    /// memory stays authoritative for bails/safepoints, then parks the result's
    /// `f64` in a residency register for later consumers. Only used for
    /// float-natured functions (those containing `Op::Div`); for an all-integer
    /// result the `f64` box is the same Number as the int32 box and
    /// `int32 op int32` is exact in `f64` (operands are ≤ 32-bit).
    fn emit_float_binop_res(
        ops: &mut Assembler,
        operands: impl WordOperands,
        bail: DynamicLabel,
        op: Op,
        fres: &mut FloatResidency,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_operand_f64(ops, fres, lhs, 0, 9, bail)?;
        load_operand_f64(ops, fres, rhs, 1, 10, bail)?;
        match op {
            Op::Add => dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1),
            Op::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
            Op::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
            Op::Div => dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1),
            _ => return Err(Unsupported::ArgCount(0)),
        }
        emit_box_double(ops, 2, 13);
        store_reg(ops, 13, dst)?;
        let park = fres.assign(dst);
        dynasm!(ops ; .arch aarch64 ; fmov D(park), d2);
        Ok(())
    }

    /// Residency-aware comparison: decode both operands to `f64` (from residency
    /// or memory) and `fcmp`, matching the `f64` path of [`emit_cmp`]. The
    /// destination receives a boolean, so its residency is dropped.
    fn emit_cmp_res(
        ops: &mut Assembler,
        operands: impl WordOperands,
        bail: DynamicLabel,
        cmp: Cmp,
        fres: &mut FloatResidency,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_operand_f64(ops, fres, lhs, 0, 9, bail)?;
        load_operand_f64(ops, fres, rhs, 1, 10, bail)?;
        dynasm!(ops ; .arch aarch64 ; fcmp d0, d1);
        match cmp {
            Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, mi),
            Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, ls),
            Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
            Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
            Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
        }
        box_bool!(ops, 13, 12);
        store_reg(ops, 13, dst)?;
        fres.invalidate(dst);
        Ok(())
    }

    /// Fast-path `ToInt32` for bitwise operators.
    ///
    /// Int32-tagged values are unboxed directly. Any finite double is truncated
    /// toward zero and reduced modulo 2^32 — the full ECMAScript `ToInt32`, not
    /// just the already-in-range case — so an integer arithmetic result that
    /// overflowed int32 into a double (e.g. `(a + b) | 0`) stays in compiled
    /// code instead of bailing. Only NaN / infinity / `|x| >= 2^63` (which would
    /// saturate the 64-bit `fcvtzs`) and non-number tags (string / BigInt /
    /// object) bail to the interpreter for exact coercion.
    fn emit_to_int32_fast(ops: &mut Assembler, src_x: u32, dst_w: u32, bail: DynamicLabel) {
        let is_non_int = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, X(src_x), x15
            ; cmp x14, x15
            ; b.ne =>is_non_int
            ; mov W(dst_w), W(src_x)
            ; b =>done
            ; =>is_non_int
            // A boxed double carries at least one NUMBER_TAG bit; a cell or
            // tagged immediate carries none and bails for exact coercion. The
            // canonical NaN flows to the fcmp check below and bails as non-finite.
            ; tst X(src_x), x15           // any NUMBER_TAG bit → boxed double
            ; b.eq =>bail                 // cell / immediate → exact coercion
            ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
            ; sub x14, X(src_x), x14      // unbox double
            ; fmov d0, x14
            ; fcmp d0, d0
            ; b.vs =>bail
        );
        // A finite double with `|x| < 2^63` truncates toward zero into i64
        // exactly (`fcvtzs`, round-to-zero); its low 32 bits are `ToInt32(x)`
        // (the truncated integer mod 2^32 mapped to the signed range). Only
        // `|x| >= 2^63` / infinity would saturate `fcvtzs`, so those bail.
        emit_load_u64(ops, 14, 9_223_372_036_854_775_808.0f64.to_bits());
        dynasm!(ops
            ; .arch aarch64
            ; fabs d1, d0
            ; fmov d2, x14
            ; fcmp d1, d2
            ; b.ge =>bail
            ; fcvtzs X(dst_w), d0
            ; =>done
        );
    }

    /// Fast-path `ToUint32` for unsigned shifts.
    ///
    /// Int32-tagged values pass through as raw low-32 bits (a negative int32
    /// reinterprets to its `mod 2^32` value). Any finite double is truncated
    /// toward zero and reduced modulo 2^32 — the full ECMAScript `ToUint32`,
    /// including negatives (`-1 >>> 0 === 4294967295`) — so it stays compiled
    /// instead of bailing. Only NaN / infinity / `|x| >= 2^63` (which would
    /// saturate the 64-bit `fcvtzs`) and non-number tags bail.
    fn emit_to_uint32_fast(ops: &mut Assembler, src_x: u32, dst_w: u32, bail: DynamicLabel) {
        let is_non_int = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, X(src_x), x15
            ; cmp x14, x15
            ; b.ne =>is_non_int
            ; mov W(dst_w), W(src_x)
            ; b =>done
            ; =>is_non_int
            ; tst X(src_x), x15           // any NUMBER_TAG bit → boxed double
            ; b.eq =>bail                 // cell / immediate → exact coercion
            ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
            ; sub x14, X(src_x), x14      // unbox double
            ; fmov d0, x14
            ; fcmp d0, d0
            ; b.vs =>bail
        );
        // Truncate toward zero into i64 (`fcvtzs`); the low 32 bits are the
        // `mod 2^32` residue regardless of sign. Only `|x| >= 2^63` / infinity
        // would saturate, so those bail.
        emit_load_u64(ops, 14, 9_223_372_036_854_775_808.0f64.to_bits());
        dynasm!(ops
            ; .arch aarch64
            ; fabs d1, d0
            ; fmov d2, x14
            ; fcmp d1, d2
            ; b.ge =>bail
            ; fcvtzs X(dst_w), d0
            ; =>done
        );
    }

    /// Emit an int32 bitwise/shift op (`BitwiseOr`/`And`/`Xor`/`Shl`/`Shr`).
    ///
    /// Operands take the guarded `ToInt32` fast path above; misses bail to the
    /// interpreter. Result is int32, matching JS semantics for accepted inputs:
    /// the AArch64 32-bit `lsl`/`asr` mask the shift count to its low 5 bits
    /// exactly as JS masks the right operand to `& 31`.
    fn emit_int_binop(
        ops: &mut Assembler,
        operands: impl WordOperands,
        bail: DynamicLabel,
        kind: IntBinOp,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        emit_to_int32_fast(ops, 9, 11, bail);
        emit_to_int32_fast(ops, 10, 12, bail);
        match kind {
            IntBinOp::Or => dynasm!(ops ; .arch aarch64 ; orr w13, w11, w12),
            IntBinOp::And => dynasm!(ops ; .arch aarch64 ; and w13, w11, w12),
            IntBinOp::Xor => dynasm!(ops ; .arch aarch64 ; eor w13, w11, w12),
            IntBinOp::Shl => dynasm!(ops ; .arch aarch64 ; lsl w13, w11, w12),
            IntBinOp::Shr => dynasm!(ops ; .arch aarch64 ; asr w13, w11, w12),
        }
        box_int32!(ops, 13, 12);
        store_reg(ops, 13, dst)?;
        Ok(())
    }

    /// Emit unsigned right shift. The result is boxed as a double because JS
    /// `>>>` returns a uint32-valued Number and values above `i32::MAX` cannot
    /// be represented by Otter's int32 tag.
    fn emit_ushr(
        ops: &mut Assembler,
        operands: impl WordOperands,
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        emit_to_uint32_fast(ops, 9, 11, bail);
        emit_to_uint32_fast(ops, 10, 12, bail);
        dynasm!(ops
            ; .arch aarch64
            ; lsr w13, w11, w12
            ; ucvtf d0, w13
        );
        emit_box_double(ops, 0, 13);
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

    pub(super) fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
        let code_block = view.code_block.as_ref();
        if let Some(instr) = view.instructions.iter().find(|instr| {
            matches!(
                instr.op(code_block),
                Op::EnterTry | Op::LeaveTry | Op::Throw | Op::EndFinally
            )
        }) {
            return Err(Unsupported::Opcode(instr.op(code_block)));
        }
        if let Some(argc) = view
            .instructions
            .iter()
            .filter(|instr| instr.op(code_block) == Op::CallMethodValue)
            .filter_map(|instr| instr.const_index(code_block, 3))
            .find(|&argc| argc as usize > super::MAX_METHOD_ARGS)
        {
            return Err(Unsupported::ArgCount(argc as usize));
        }

        let mut ops = Assembler::new().expect("assembler alloc");
        let bail = ops.new_dynamic_label();
        let threw = ops.new_dynamic_label();

        // A dynamic label per canonical instruction PC. Byte PCs remain side
        // metadata for bailout/OSR records only.
        let mut labels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
        for instr in &view.instructions {
            labels.insert(instr.instruction_pc(code_block), ops.new_dynamic_label());
        }
        let target_label = |instruction_pc: i64| -> Result<DynamicLabel, Unsupported> {
            u32::try_from(instruction_pc)
                .ok()
                .and_then(|pc| labels.get(&pc).copied())
                .ok_or(Unsupported::BranchTarget(instruction_pc))
        };

        // Set when an unsupported opcode is emitted as a bail (see the catch-all
        // arm); such code is OSR-only.
        let mut osr_only = false;
        let mut array_literal_regs: Vec<Box<[u16]>> = Vec::new();
        let mut closure_parent_indices: Vec<Box<[u32]>> = Vec::new();
        let mut math_argument_regs: Vec<Box<[u16]>> = Vec::new();

        let mut safepoint_records: Vec<_> = view.safepoints.values().cloned().collect();
        let mut next_safepoint = safepoint_records
            .iter()
            .map(|record| record.id)
            .max()
            .map_or(1, |id| id.saturating_add(1))
            .max(1);
        let mut add_alloc_safepoints: BTreeMap<u32, SafepointId> = BTreeMap::new();
        let mut live_method_alloc_safepoints: BTreeMap<u32, SafepointId> = BTreeMap::new();
        for instr in &view.instructions {
            if instr.op(code_block) == Op::CallMethodValue {
                if let Some(alloc) = view.collection_alloc_methods.get(&instr.byte_pc) {
                    live_method_alloc_safepoints.insert(instr.byte_pc, alloc.safepoint_id);
                } else {
                    let safepoint = next_safepoint;
                    next_safepoint = next_safepoint.saturating_add(1);
                    live_method_alloc_safepoints.insert(instr.byte_pc, safepoint);
                    safepoint_records.push(SafepointRecord::frame_slot_window(
                        safepoint,
                        NO_FRAME_STATE,
                        view.code_block.register_count,
                    ));
                }
            }
            if instr.op(code_block) == Op::Add {
                let safepoint = next_safepoint;
                next_safepoint = next_safepoint.saturating_add(1);
                add_alloc_safepoints.insert(instr.byte_pc, safepoint);
                safepoint_records.push(SafepointRecord::frame_slot_window(
                    safepoint,
                    NO_FRAME_STATE,
                    view.code_block.register_count,
                ));
            }
        }

        // Loop headers = back-edge targets: the PCs an OSR entry can land on.
        // A branch whose resolved target sits at or before its own PC closes a
        // loop; that target is a basic-block boundary where the interpreter's
        // live registers match what compiled code expects (the baseline keeps
        // all live values in the frame array between ops). Collect them here so
        // a trampoline is emitted for each after the body.
        let mut loop_headers: BTreeMap<u32, u32> = BTreeMap::new();
        for instr in &view.instructions {
            if matches!(
                instr.op(code_block),
                Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue
            ) {
                let rel = instr
                    .imm32(code_block, 0)
                    .ok_or(Unsupported::OperandShape("branch offset"))?;
                let target = branch_target(code_block, instr, rel);
                if target >= 0
                    && target < i64::from(instr.instruction_pc(code_block))
                    && let Ok(target_pc) = u32::try_from(target)
                    && labels.contains_key(&target_pc)
                {
                    let byte_pc = view.instructions[target_pc as usize].byte_pc;
                    loop_headers.insert(byte_pc, target_pc);
                }
            }
        }

        // Every instruction reachable by a branch is a basic-block boundary: the
        // incoming register state is unknown, so FP residency is cleared on
        // entry. Includes forward targets, unlike `loop_headers`.
        let mut branch_targets: BTreeSet<u32> = BTreeSet::new();
        for instr in &view.instructions {
            if matches!(
                instr.op(code_block),
                Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue
            ) {
                let rel = instr
                    .imm32(code_block, 0)
                    .ok_or(Unsupported::OperandShape("branch offset"))?;
                let target = branch_target(code_block, instr, rel);
                if let Ok(pc) = u32::try_from(target) {
                    branch_targets.insert(pc);
                }
            }
        }

        // FP-residency read cache (OPTIMIZING_TIER.md S1) is enabled only for
        // float-natured functions — those that divide. Integer-heavy code (no
        // `Op::Div`) keeps the byte-identical int-fast-path emit, so this can
        // never slow a non-dividing function. `Op::Div` always produces a
        // Number via `f64`, so a function that contains one already runs its
        // arithmetic through the double path on the hot values.
        let enable_fres = view
            .instructions
            .iter()
            .any(|i| i.op(code_block) == Op::Div);
        let mut fres = FloatResidency::default();

        // One self-patching WhiskerIC cell per `LoadProperty` op. Allocated up
        // front (stable boxed buffer) so each site can bake its cell address;
        // filled at runtime by `jit_load_prop_window_stub` on a monomorphic own-data
        // inline-slot hit. `as_mut_ptr` gives a write-provenance base that
        // outlives every execution (the buffer is owned by the returned
        // `BaselineCode` and never re-formed as a `&[_]` slice).
        let load_property_count = view
            .instructions
            .iter()
            .filter(|i| i.op(code_block) == Op::LoadProperty)
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
            .filter(|i| i.op(code_block) == Op::StoreProperty)
            .count();
        let mut store_ic_cells: Box<[WhiskerIcCell]> =
            vec![WhiskerIcCell::default(); store_property_count].into_boxed_slice();
        let store_cell_base = store_ic_cells.as_mut_ptr() as usize;
        let mut store_ic_idx: usize = 0;

        let entry = ops.offset();
        // Self-recursion target: a direct `Op::Call` to the running closure
        // re-enters here (a fresh callee `JitCtx` in `x0`) without a Rust
        // frame-build bridge. Only used when the body is frame-index-free.
        let self_call_safe = is_self_call_safe(view);
        let self_entry = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; =>self_entry);
        emit_prologue(&mut ops);

        // Stable GC cage base, baked for inline property-load decompression.
        let cage_base = view.cage_base;
        // Static typed-array body offsets for inline element access. Only used
        // when `cage_base != 0` (i.e. baked by the real compile path).
        let ta_layout = view.ta_layout;

        for instr in &view.instructions {
            let instruction_pc = instr.instruction_pc(code_block);
            dynasm!(ops ; .arch aarch64 ; =>labels[&instruction_pc]);
            // A branch target is a block boundary: control can arrive here from
            // elsewhere with unknown register state (and OSR enters loop headers
            // with values freshly loaded from memory), so no FP register can be
            // assumed to hold a slot's value.
            if enable_fres && branch_targets.contains(&instruction_pc) {
                fres.clear();
            }
            // Stamp this op's byte-PC into the context so any bail (guard
            // failure or unsupported opcode) resumes the interpreter at the
            // exact instruction, preserving committed side effects.
            emit_load_u64(&mut ops, 9, u64::from(instr.byte_pc));
            dynasm!(ops ; .arch aarch64 ; str w9, [x20, BAIL_PC_OFFSET]);
            let ops_ref = instr.operand_view(code_block);
            match instr.op(code_block) {
                Op::LoadInt32 => {
                    let dst = reg(ops_ref, 0)?;
                    let v = imm32(ops_ref, 1)?;
                    let boxed = value_tag::NUMBER_TAG | u64::from(v as u32);
                    emit_load_u64(&mut ops, 9, boxed);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadNumber => {
                    let dst = reg(ops_ref, 0)?;
                    let Some(value) = instr.load_number else {
                        return Err(Unsupported::OperandShape("load-number constant"));
                    };
                    // Materialize the boxed `Value` (int32 or offset-double) inline
                    // instead of re-running the constant load through the delegate
                    // bridge: a float literal in a numeric loop otherwise pays a VM
                    // round-trip on every execution.
                    emit_load_u64(&mut ops, 9, otter_vm::Value::number_f64(value).to_bits());
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
                    emit_load_u64(&mut ops, 9, VALUE_UNDEFINED);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadNull => {
                    let dst = reg(ops_ref, 0)?;
                    emit_load_u64(&mut ops, 9, VALUE_NULL);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadHole => {
                    let dst = reg(ops_ref, 0)?;
                    // SPECIAL payload `SPECIAL_HOLE` == the TDZ/uninitialized hole.
                    emit_load_u64(&mut ops, 9, VALUE_HOLE);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadTrue => {
                    let dst = reg(ops_ref, 0)?;
                    emit_load_u64(&mut ops, 9, VALUE_TRUE);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadFalse => {
                    let dst = reg(ops_ref, 0)?;
                    emit_load_u64(&mut ops, 9, VALUE_FALSE);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::StoreLocal => {
                    let src = reg(ops_ref, 0)?;
                    let idx = local_index(ops_ref, 1)?;
                    load_reg(&mut ops, 9, src)?;
                    store_reg(&mut ops, 9, idx)?;
                }
                Op::Add => emit_add_with_runtime_fallback(
                    &mut ops,
                    ops_ref,
                    add_alloc_safepoints.get(&instr.byte_pc).copied(),
                    view.code_block.register_count,
                    threw,
                )?,
                Op::Sub | Op::Mul | Op::Div if enable_fres => {
                    emit_float_binop_res(&mut ops, ops_ref, bail, instr.op(code_block), &mut fres)?;
                }
                Op::Sub | Op::Mul => {
                    emit_add_sub_mul(&mut ops, ops_ref, bail, instr.op(code_block))?;
                }
                Op::Div => emit_div(&mut ops, ops_ref, bail)?,
                Op::Rem => emit_rem(&mut ops, ops_ref, bail)?,
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                    if enable_fres =>
                {
                    let cmp = match instr.op(code_block) {
                        Op::LessThan => Cmp::Lt,
                        Op::LessEq => Cmp::Le,
                        Op::GreaterThan => Cmp::Gt,
                        Op::GreaterEq => Cmp::Ge,
                        Op::Equal => Cmp::Eq,
                        _ => Cmp::Ne,
                    };
                    emit_cmp_res(&mut ops, ops_ref, bail, cmp, &mut fres)?;
                }
                Op::LessThan => emit_cmp(&mut ops, ops_ref, bail, Cmp::Lt)?,
                Op::LessEq => emit_cmp(&mut ops, ops_ref, bail, Cmp::Le)?,
                Op::GreaterThan => emit_cmp(&mut ops, ops_ref, bail, Cmp::Gt)?,
                Op::GreaterEq => emit_cmp(&mut ops, ops_ref, bail, Cmp::Ge)?,
                Op::Equal => emit_cmp(&mut ops, ops_ref, bail, Cmp::Eq)?,
                Op::NotEqual => emit_cmp(&mut ops, ops_ref, bail, Cmp::Ne)?,
                // `ToPrimitive` is identity on primitives. Object/function
                // families bail so observable coercion hooks run in the VM.
                Op::ToPrimitive => {
                    let dst = reg(ops_ref, 0)?;
                    let src = reg(ops_ref, 1)?;
                    emit_to_primitive_identity(&mut ops, dst, src, bail)?;
                }
                // `ToNumeric` is identity on a number (int32 or double); emit
                // a guarded move. Other primitives/objects need the VM path.
                Op::ToNumeric => {
                    let dst = reg(ops_ref, 0)?;
                    let src = reg(ops_ref, 1)?;
                    load_reg(&mut ops, 9, src)?;
                    guard_number!(ops, 9, bail);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::Jump => {
                    let rel = imm32(ops_ref, 0)?;
                    let target = branch_target(code_block, instr, rel);
                    let tgt = target_label(target)?;
                    if target <= i64::from(instruction_pc) {
                        emit_backedge_interrupt_check(&mut ops, threw);
                    }
                    dynasm!(ops ; .arch aarch64 ; b =>tgt);
                }
                Op::JumpIfFalse | Op::JumpIfTrue => {
                    let rel = imm32(ops_ref, 0)?;
                    let cond = reg(ops_ref, 1)?;
                    let target = branch_target(code_block, instr, rel);
                    let tgt = target_label(target)?;
                    load_reg(&mut ops, 9, cond)?;
                    // Only boolean conditions are supported in this subset.
                    dynasm!(ops
                        ; .arch aarch64
                        ; sub x14, x9, #(VALUE_FALSE as u32)          // bail unless boolean
                        ; cmp x14, #1
                        ; b.hi =>bail
                        ; cmp x9, #(VALUE_TRUE as u32)                // eq iff true
                    );
                    if target <= i64::from(instruction_pc) {
                        let taken = ops.new_dynamic_label();
                        let fallthrough = ops.new_dynamic_label();
                        if matches!(instr.op(code_block), Op::JumpIfFalse) {
                            dynasm!(ops ; .arch aarch64 ; b.ne =>taken);
                        } else {
                            dynasm!(ops ; .arch aarch64 ; b.eq =>taken);
                        }
                        dynasm!(ops ; .arch aarch64 ; b =>fallthrough ; =>taken);
                        emit_backedge_interrupt_check(&mut ops, threw);
                        dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
                    } else if matches!(instr.op(code_block), Op::JumpIfFalse) {
                        dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
                    }
                }
                Op::MakeFunction | Op::MakeClosure if instr.make_self => {
                    // SELF binding: the closure value is precomputed in
                    // `JitCtx.self_closure` (offset 8 from x20), so read it
                    // straight into `dst` — no Rust round-trip through
                    // the function/closure builder.
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
                Op::NewObject => {
                    let dst = reg(ops_ref, 0)?;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_call_stub(&mut ops, jit_new_object_stub as *const () as usize, threw);
                }
                Op::NewArray => {
                    let dst = reg(ops_ref, 0)?;
                    let count = const_index(ops_ref, 1)? as usize;
                    if ops_ref.len() != count + 2 {
                        return Err(Unsupported::OperandShape("NewArray register tail"));
                    }
                    let source_regs = (0..count)
                        .map(|slot| reg(ops_ref, slot + 2))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_boxed_slice();
                    let source_regs_ptr = source_regs.as_ptr();
                    array_literal_regs.push(source_regs);
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(&mut ops, 2, source_regs_ptr as u64);
                    emit_load_u64(&mut ops, 3, count as u64);
                    emit_call_stub(&mut ops, jit_new_array_stub as *const () as usize, threw);
                }
                Op::Call => {
                    // Splice a tiny monomorphic leaf callee inline under an
                    // identity guard (no per-call bridge); fall back to the
                    // direct-call bridge for absent / ineligible sites.
                    let inlined = match view.inline_callees.get(&instr.byte_pc) {
                        Some(callee) => {
                            try_emit_inline_call(&mut ops, callee, ops_ref, cage_base, bail)?
                        }
                        None => false,
                    };
                    if !inlined {
                        // A frame-index-free function re-enters self-recursive
                        // calls inline (no Rust frame-build bridge), bailing on a
                        // guard miss; any other function takes the direct-call
                        // bridge.
                        if self_call_safe {
                            emit_self_recursive_call(
                                &mut ops,
                                ops_ref,
                                view.code_block.register_count,
                                self_entry,
                                bail,
                                threw,
                            )?;
                        } else {
                            emit_call(&mut ops, ops_ref, bail, threw)?;
                        }
                    }
                }
                // `recv.name(args…)` — IC-resolve the method + direct-branch to
                // its compiled entry (WhiskerIC method call), falling back to the
                // in-place full method-call stub when ineligible.
                Op::CallMethodValue => {
                    let site = instr.property_ic_site(code_block).unwrap_or(usize::MAX) as u64;
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
                        // A polymorphic site (no single monomorphic entry) emits a
                        // most-frequent-first guard chain over its observed
                        // receiver shapes, bridging only when none match.
                        None => match view.inline_poly_methods.get(&instr.byte_pc) {
                            Some(methods) => try_emit_poly_inline_method_call(
                                &mut ops,
                                methods,
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
                        },
                    };
                    if !inlined {
                        // Splice an inline dense-array `pop` / `push` fast path
                        // ahead of the method bridge; a guard miss falls through to
                        // the bridge, a hit jumps past it.
                        let array_done = ops.new_dynamic_label();
                        let mut spliced_array = false;
                        if let Some(am) = view.array_methods.get(&instr.byte_pc).copied() {
                            let array_miss = ops.new_dynamic_label();
                            let emitted = match am.kind {
                                JitArrayMethodKind::Pop => emit_array_pop_inline(
                                    &mut ops, ops_ref, &am, view, array_miss, array_done,
                                )?,
                                JitArrayMethodKind::Push => emit_array_push_inline(
                                    &mut ops, ops_ref, &am, view, array_miss, array_done, threw,
                                )?,
                            };
                            if emitted {
                                dynasm!(ops ; .arch aarch64 ; =>array_miss);
                                spliced_array = true;
                            }
                        }
                        emit_method_call(
                            &mut ops,
                            ops_ref,
                            site,
                            view.collection_leaf_methods.get(&instr.byte_pc),
                            view.collection_alloc_methods.get(&instr.byte_pc),
                            Some(view),
                            live_method_alloc_safepoints.get(&instr.byte_pc).copied(),
                            bail,
                            threw,
                        )?;
                        if spliced_array {
                            dynasm!(ops ; .arch aarch64 ; =>array_done);
                        }
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
                // `recv[idx] = src` — inline plain dense `Array` stores and
                // `Float64Array`/`Int32Array` element stores (guarded, no
                // safepoint); every other case misses to the safe element-store
                // bridge. Operands: recv, idx, src, scratch.
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
                        let array_miss = ops.new_dynamic_label();
                        emit_array_store(
                            &mut ops, &ta_layout, cage_base, recv_off, idx_off, src_off,
                            array_miss, el_done, threw, recv, src,
                        );
                        dynasm!(ops ; .arch aarch64 ; =>array_miss);

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
                    emit_load_u64(&mut ops, 3, u64::from(view.code_block.id));
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
                        emit_load_u64(&mut ops, 11, VALUE_HOLE);
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
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x12, x11
                            ; b.eq =>up_miss                     // pointer -> barriered stub
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
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x12, x11
                            ; b.eq =>up_miss                     // pointer -> barriered bridge
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(&mut ops, 13, cage_base as u64);
                        emit_load_u64(&mut ops, 11, VALUE_HOLE);
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

                    dynasm!(ops
                        ; .arch aarch64
                        ; =>up_miss
                        ; mov x0, x20
                        ; movz x1, src as u32
                    );
                    emit_load_u64(&mut ops, 2, u64::from(idx as u32));
                    emit_call_stub(
                        &mut ops,
                        jit_store_upvalue_checked_stub as *const () as usize,
                        threw,
                    );
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
                        ; movz x15, NUMBER_TAG_HI16, lsl #48
                        ; and x14, x9, x15
                        ; cmp x14, x15
                        ; b.ne =>float_path
                        ; adds w13, w9, w12
                        ; b.vs =>float_path
                    );
                    box_int32!(ops, 13, 11);
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
                    let hole = VALUE_HOLE;
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, THIS_VALUE_OFFSET]);
                    emit_load_u64(&mut ops, 12, hole);
                    dynasm!(ops ; .arch aarch64 ; cmp x9, x12 ; b.eq =>bail);
                    store_reg(&mut ops, 9, dst)?;
                }
                Op::LoadProperty => {
                    // jit_load_prop_window_stub(ctx=x20, dst, obj, name_idx, site, cell).
                    // `site` is the dense IC index from the snapshot, used by
                    // the bridge for the monomorphic fast path (PC-keyed lookup
                    // is unavailable at PC 0); `usize::MAX` means "no site".
                    // `cell` is this site's self-patching WhiskerIC cell.
                    let dst = reg(ops_ref, 0)?;
                    let obj = reg(ops_ref, 1)?;
                    let name = const_index(ops_ref, 2)?;
                    let site = instr.property_ic_site(code_block).unwrap_or(usize::MAX) as u64;

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
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
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
                        box_int32!(ops, 9, 12);
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
                    // Shape `0` is reserved as the empty-cell sentinel. Some
                    // live shapes can currently have offset 0, so those shapes
                    // deliberately miss to the stub until the cell grows an
                    // explicit valid bit.
                    if cage_base != 0 {
                        let obj_off = reg_offset(obj)?;
                        let dst_off = reg_offset(dst)?;
                        let shape_byte = view.object_shape_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
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
                            ; cbz w14, =>miss
                        );
                        emit_load_u64(&mut ops, 15, cell_addr as u64);
                        // Walk the IC ways. The `cbz` above prevents empty ways
                        // (`shape == 0`) from matching a live shape-0 object.
                        // A hit loads that way's value byte into w17 and shares
                        // the slab read.
                        let do_load = ops.new_dynamic_label();
                        for way in 0..IC_WAYS as u32 {
                            let shape_off = way * 8;
                            let vbyte_off = shape_off + 4;
                            let next = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr w16, [x15, shape_off]
                                ; cmp w14, w16
                                ; b.ne =>next
                                ; ldr w17, [x15, vbyte_off]
                                ; b =>do_load
                                ; =>next
                            );
                        }
                        dynasm!(ops ; .arch aarch64 ; b =>miss ; =>do_load);
                        // Slab base from the fresh header (inline) or stable
                        // out-of-line `values_ptr` — never the cached body pointer.
                        emit_slab_base(&mut ops, view, 13, 14);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x13, =>miss
                            ; ldr w9, [x13, x17]       // 4-byte compressed slot
                        );
                        emit_decompress_slot(&mut ops, cage_base as u64, miss);
                        dynasm!(ops
                            ; .arch aarch64
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
                    emit_load_u64(&mut ops, 6, u64::from(view.code_block.id));
                    // The typed window operation handles only own-data IC
                    // resolution and self-patching. Full `[[Get]]` semantics
                    // bail to normal dispatch instead of re-entering one
                    // interpreter opcode through a framed bridge.
                    emit_load_u64(&mut ops, 16, jit_load_prop_window_stub as *const () as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cmp x0, #1
                        ; b.eq =>threw
                        ; cmp x0, #2
                        ; b.eq =>bail
                    );
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::StoreProperty => {
                    // Operands: obj, name_const, src, scratch_dst.
                    // jit_store_prop_window_stub(ctx=x20, obj, name_idx, src, site, cell).
                    let obj = reg(ops_ref, 0)?;
                    let name = const_index(ops_ref, 1)?;
                    let src = reg(ops_ref, 2)?;
                    let site = instr.property_ic_site(code_block).unwrap_or(usize::MAX) as u64;

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
                    // across one. Shape-0 receiver / empty cell / guard miss →
                    // shared stub.
                    if cage_base != 0 {
                        let obj_off = reg_offset(obj)?;
                        let src_off = reg_offset(src)?;
                        let shape_byte = view.object_shape_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
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
                            ; cbz w14, =>miss
                        );
                        emit_load_u64(&mut ops, 15, cell_addr as u64);
                        // N-way IC walk (see `LoadProperty`): match a way's shape,
                        // load its value byte into w17, then share the slab write.
                        let do_store = ops.new_dynamic_label();
                        for way in 0..IC_WAYS as u32 {
                            let shape_off = way * 8;
                            let vbyte_off = shape_off + 4;
                            let next = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr w16, [x15, shape_off]
                                ; cmp w14, w16
                                ; b.ne =>next
                                ; ldr w17, [x15, vbyte_off]
                                ; b =>do_store
                                ; =>next
                            );
                        }
                        let store_prim = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; b =>miss
                            ; =>do_store
                            ; ldr x9, [x19, src_off]   // value to store
                        );
                        // Slab base from the fresh header (inline) or stable
                        // out-of-line `values_ptr` — never the cached body pointer.
                        emit_slab_base(&mut ops, view, 13, 14);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x13, =>miss
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>store_prim        // primitive → compress, no barrier
                            // Cell: the compressed ref is the low-32 8-aligned
                            // offset (low-3 tag 000), i.e. the value's low word.
                            ; str w9, [x13, x17]
                        );
                        // Pointer value: card-mark the parent header. A
                        // frameless-eligible body uses the window barrier (reads
                        // the parent/child from the register window) so it is
                        // sound with no `HoltStack` frame.
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov x0, x20
                            ; movz x1, obj as u32
                            ; movz x2, src as u32
                        );
                        let barrier = if self_call_safe {
                            jit_write_barrier_window_stub as *const () as usize
                        } else {
                            jit_write_barrier_stub as *const () as usize
                        };
                        emit_call_stub(&mut ops, barrier, threw);
                        dynasm!(ops ; .arch aarch64 ; b =>done ; =>store_prim);
                        // A wide int / double / function id cannot inline-compress
                        // (a boxed number allocates); the runtime store handles it.
                        emit_compress_slot_or_bail(&mut ops, miss);
                        dynasm!(ops ; .arch aarch64 ; str w10, [x13, x17] ; b =>done);
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
                    emit_load_u64(&mut ops, 6, u64::from(view.code_block.id));
                    emit_load_u64(&mut ops, 16, jit_store_prop_window_stub as *const () as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cmp x0, #1
                        ; b.eq =>threw
                        ; cmp x0, #2
                        ; b.eq =>bail
                    );
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::BitwiseOr => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Or)?,
                Op::BitwiseAnd => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::And)?,
                Op::BitwiseXor => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Xor)?,
                Op::Shl => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Shl)?,
                Op::Shr => emit_int_binop(&mut ops, ops_ref, bail, IntBinOp::Shr)?,
                Op::Ushr => emit_ushr(&mut ops, ops_ref, bail)?,
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
                    let undef = VALUE_UNDEFINED; // SPECIAL_UNDEFINED == 0
                    emit_load_u64(&mut ops, 0, undef);
                    dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_RETURNED as u32);
                    emit_epilogue(&mut ops);
                }
                // Variadic operations still using compile-owned decoded operand
                // metadata. Fixed-operand slow paths below use typed ABI stubs.
                Op::MathCall => {
                    let dst = reg(ops_ref, 0)?;
                    let method_id = const_index(ops_ref, 1)?;
                    let argc = const_index(ops_ref, 2)? as usize;
                    if argc == 0
                        && otter_bytecode::method_id::MathMethod::from_u32(method_id)
                            == Some(otter_bytecode::method_id::MathMethod::Random)
                    {
                        emit_load_u64(&mut ops, 16, otter_jit_math_random as *const () as u64);
                        dynasm!(ops ; .arch aarch64 ; blr x16);
                        store_reg(&mut ops, 0, dst)?;
                    } else {
                        if ops_ref.len() != argc + 3 {
                            return Err(Unsupported::OperandShape("MathCall register tail"));
                        }
                        let argument_regs = (0..argc)
                            .map(|slot| reg(ops_ref, slot + 3))
                            .collect::<Result<Vec<_>, _>>()?
                            .into_boxed_slice();
                        let argument_regs_ptr = argument_regs.as_ptr();
                        math_argument_regs.push(argument_regs);
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov x0, x20
                            ; movz x1, dst as u32
                        );
                        emit_load_u64(&mut ops, 2, u64::from(method_id));
                        emit_load_u64(&mut ops, 3, argument_regs_ptr as u64);
                        emit_load_u64(&mut ops, 4, argc as u64);
                        emit_call_stub(&mut ops, jit_math_call_stub as *const () as usize, threw);
                    }
                }
                Op::MakeClosure => {
                    let dst = reg(ops_ref, 0)?;
                    let function_index = const_index(ops_ref, 1)?;
                    let count = const_index(ops_ref, 2)? as usize;
                    if ops_ref.len() != count + 3 {
                        return Err(Unsupported::OperandShape("MakeClosure upvalue tail"));
                    }
                    let parent_indices = (0..count)
                        .map(|slot| {
                            let index = imm32(ops_ref, slot + 3)?;
                            u32::try_from(index).map_err(|_| {
                                Unsupported::OperandShape("MakeClosure parent upvalue")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?
                        .into_boxed_slice();
                    let parent_indices_ptr = parent_indices.as_ptr();
                    closure_parent_indices.push(parent_indices);
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(&mut ops, 1, u64::from(view.code_block.id));
                    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
                    emit_load_u64(&mut ops, 3, u64::from(function_index));
                    emit_load_u64(&mut ops, 4, parent_indices_ptr as u64);
                    emit_load_u64(&mut ops, 5, count as u64);
                    emit_call_stub(&mut ops, jit_make_closure_stub as *const () as usize, threw);
                }
                Op::LoadString => {
                    let dst = reg(ops_ref, 0)?;
                    let constant_index = const_index(ops_ref, 1)?;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(&mut ops, 1, u64::from(view.code_block.id));
                    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
                    emit_load_u64(&mut ops, 3, u64::from(constant_index));
                    emit_call_stub(&mut ops, jit_load_string_stub as *const () as usize, threw);
                }
                Op::DefineDataProperty => {
                    let (object, key, value) = reg3(ops_ref)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, object as u32
                        ; movz x2, key as u32
                        ; movz x3, value as u32
                    );
                    emit_call_stub(
                        &mut ops,
                        jit_define_data_property_stub as *const () as usize,
                        threw,
                    );
                }
                Op::FreshUpvalue => {
                    let idx = imm32(ops_ref, 0)?;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(&mut ops, 1, u64::from(idx as u32));
                    emit_call_stub(
                        &mut ops,
                        jit_fresh_upvalue_stub as *const () as usize,
                        threw,
                    );
                }
                Op::LoadBuiltinError => {
                    let dst = reg(ops_ref, 0)?;
                    let kind_index = const_index(ops_ref, 1)?;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(&mut ops, 2, u64::from(kind_index));
                    emit_call_stub(
                        &mut ops,
                        jit_load_builtin_error_stub as *const () as usize,
                        threw,
                    );
                }
                Op::Neg => {
                    let dst = reg(ops_ref, 0)?;
                    let src = reg(ops_ref, 1)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, src as u32
                    );
                    emit_call_stub(&mut ops, jit_neg_stub as *const () as usize, threw);
                }
                Op::LooseEqual | Op::LooseNotEqual => {
                    emit_loose_cmp(
                        &mut ops,
                        ops_ref,
                        instr.op(code_block) == Op::LooseNotEqual,
                        bail,
                    )?;
                }
                Op::DefineOwnProperty => {
                    let (target, key, descriptor) = reg3(ops_ref)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, target as u32
                        ; movz x2, key as u32
                        ; movz x3, descriptor as u32
                    );
                    emit_call_stub(
                        &mut ops,
                        jit_define_own_property_stub as *const () as usize,
                        threw,
                    );
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
            // Maintain FP residency after the op. The arithmetic/compare arms
            // managed it themselves above; a load only overwrites its own
            // destination slot (so just drop that slot, preserving residency of
            // values around it in a numeric cluster); anything else is a
            // boundary or writes a slot the cache cannot track, so drop all.
            if enable_fres {
                match instr.op(code_block) {
                    Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual => {}
                    Op::LoadInt32
                    | Op::LoadLocal
                    | Op::LoadNumber
                    | Op::LoadString
                    | Op::LoadTrue
                    | Op::LoadFalse
                    | Op::LoadUndefined
                    | Op::LoadHole
                    | Op::LoadBigInt => {
                        if let Ok(dst) = reg(ops_ref, 0) {
                            fres.invalidate(dst);
                        }
                    }
                    _ => fres.clear(),
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
        for (&pc, &instruction_pc) in &loop_headers {
            let off = ops.offset().0;
            emit_prologue(&mut ops);
            let tgt = labels[&instruction_pc];
            dynasm!(ops ; .arch aarch64 ; b =>tgt);
            osr_entries.insert(pc, off);
        }

        safepoint_records.sort_by_key(|record| record.id);
        let safepoint_records = safepoint_records.into_boxed_slice();
        let buf = ops.finalize().expect("finalize");
        Ok(BaselineCode {
            code: CompiledCode::new(buf, entry),
            code_object_id: u64::from(view.code_block.id) + 1,
            register_count: view.code_block.register_count,
            osr_entries,
            osr_only,
            load_ic_cells,
            store_ic_cells,
            array_literal_regs: array_literal_regs.into_boxed_slice(),
            closure_parent_indices: closure_parent_indices.into_boxed_slice(),
            math_argument_regs: math_argument_regs.into_boxed_slice(),
            safepoint_records,
            frameless_entry_safe: self_call_safe,
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

    /// Compute the value-slab base for a shape-matched receiver into `reg`, which
    /// holds the decompressed `GcHeader` pointer on entry (`scratch` is
    /// clobbered). A small object (`slab_len <= INLINE_SLOT_CAP`) carries its slab
    /// inline in the body, so the base is `header + object_inline_values_byte`,
    /// derived fresh from the receiver's header every access. This deliberately
    /// never reads the cached `values_ptr`: that pointer aims into the body and
    /// dangles the instant the moving collector relocates the object — a stale
    /// base the collector only re-caches lazily, so a compiled load/store that
    /// trusted it wrote through a freed slab. A spilled object's slab is a stable
    /// out-of-line allocation, so its base is loaded from `values_ptr`.
    pub(crate) fn emit_slab_base(
        ops: &mut Assembler,
        view: &JitCompileSnapshot,
        reg: u32,
        scratch: u32,
    ) {
        // Frozen ABI (a `dynasm` immediate must be a compile-time constant): the
        // inline slab capacity and the header-relative offset of the in-body
        // inline slab. Pinned to `INLINE_SLOT_CAP` and
        // `HEADER_SIZE + OBJECT_BODY_INLINE_VALUES_OFFSET`, `debug_assert`ed
        // against the values otter-vm baked from the live `#[repr(C)]` layout so a
        // field reorder trips in tests rather than baking a wild offset.
        const INLINE_SLOT_CAP: u32 = 2;
        const INLINE_VALUES_BYTE: u32 = 80;
        debug_assert_eq!(INLINE_SLOT_CAP, view.object_inline_slot_cap);
        debug_assert_eq!(INLINE_VALUES_BYTE, view.object_inline_values_byte);
        let slab_len_off = view.object_slab_len_byte;
        let values_ptr_off = view.object_values_ptr_byte;
        let spilled = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        // A `dynasm` `cmp` / `add` immediate is only accepted with a static
        // register operand, so emit the fixed-register form for each register
        // pair the two emitters call this with (baseline x13/x14, optimizing
        // x16/x17).
        match (reg, scratch) {
            (13, 14) => dynasm!(ops
                ; .arch aarch64
                ; ldrh w14, [x13, slab_len_off]
                ; cmp w14, INLINE_SLOT_CAP
                ; b.hi =>spilled
                ; add x13, x13, INLINE_VALUES_BYTE
                ; b =>done
                ; =>spilled
                ; ldr x13, [x13, values_ptr_off]
                ; =>done
            ),
            (16, 17) => dynasm!(ops
                ; .arch aarch64
                ; ldrh w17, [x16, slab_len_off]
                ; cmp w17, INLINE_SLOT_CAP
                ; b.hi =>spilled
                ; add x16, x16, INLINE_VALUES_BYTE
                ; b =>done
                ; =>spilled
                ; ldr x16, [x16, values_ptr_off]
                ; =>done
            ),
            _ => unreachable!("emit_slab_base register pair"),
        }
    }

    fn emit_backedge_interrupt_check(ops: &mut Assembler, threw: DynamicLabel) {
        let slow = ops.new_dynamic_label();
        let cont = ops.new_dynamic_label();
        // Inline cooperative poll: read the interrupt byte and decrement the fuel
        // counter, re-entering the poll stub only when the interrupt is set or the
        // counter reaches zero. x9/x10 are transient scratch (no value is live
        // across a block boundary in the baseline register-window model).
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x20, INTERRUPT_FLAG_OFFSET]
            ; ldrb w9, [x9]
            ; cbnz w9, =>slow
            ; ldr x9, [x20, BACKEDGE_FUEL_OFFSET]
            ; ldr x10, [x9]
            ; subs x10, x10, #1
            ; str x10, [x9]
            ; b.gt =>cont
            ; =>slow
            ; mov x0, x20
        );
        emit_call_stub(ops, jit_backedge_poll_stub as *const () as usize, threw);
        dynasm!(ops ; .arch aarch64 ; =>cont);
    }

    /// Largest callee register window the inliner accepts. Bounds the per-site
    /// scratch reservation and keeps a spliced body "tiny".
    const INLINE_MAX_REGS: u16 = 24;
    /// Largest callee instruction count the inliner accepts.
    const INLINE_MAX_INSTRS: usize = 48;
    /// Largest argument count an inlined call accepts.
    const INLINE_MAX_ARGS: usize = 8;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InlineCallKind {
        Plain,
        ClosureUpvalues,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum InlineKnown {
        Unknown,
        Number,
        Bool,
    }

    /// Whether an op may appear in an inlined leaf callee: a pure, non-allocating
    /// operation with no `this`/upvalue/global/heap access and no further call,
    /// so the spliced body has no GC point and commits nothing observable before
    /// it can bail. Any op outside this set aborts the inline attempt.
    fn is_inline_pure_op(op: Op) -> bool {
        matches!(
            op,
            Op::LoadInt32
                | Op::LoadNumber
                | Op::LoadLocal
                | Op::LoadUndefined
                | Op::LoadNull
                | Op::LoadHole
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::StoreLocal
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::BitwiseOr
                | Op::BitwiseAnd
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
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

    fn inline_plain_op_allowed(
        code_block: &otter_vm::CodeBlock,
        instr: &otter_vm::JitInstructionMetadata,
    ) -> bool {
        is_inline_pure_op(instr.op(code_block))
            || (matches!(instr.op(code_block), Op::MakeFunction | Op::MakeClosure)
                && instr.make_self)
    }

    fn self_bindings_are_dead(callee: &JitInlineCallee) -> bool {
        let mut pending = Vec::<u16>::new();
        let code_block = callee.code_block.as_ref();

        for instr in &callee.instructions {
            let operands = instr.operand_view(code_block);
            let mut ok = true;
            match instr.op(code_block) {
                Op::LoadLocal | Op::StoreLocal => {}
                Op::ToPrimitive | Op::ToNumeric => {
                    ok &= reg(operands, 1)
                        .ok()
                        .is_some_and(|regn| !pending.contains(&regn));
                }
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::BitwiseOr
                | Op::BitwiseAnd
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    ok &= reg(operands, 1)
                        .ok()
                        .is_some_and(|regn| !pending.contains(&regn));
                    ok &= reg(operands, 2)
                        .ok()
                        .is_some_and(|regn| !pending.contains(&regn));
                }
                Op::Return | Op::ReturnValue => {
                    ok &= reg(operands, 0)
                        .ok()
                        .is_some_and(|regn| !pending.contains(&regn));
                }
                Op::JumpIfFalse | Op::JumpIfTrue => {
                    ok &= reg(operands, 1)
                        .ok()
                        .is_some_and(|regn| !pending.contains(&regn));
                }
                Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                    ok &= reg(operands, 0)
                        .ok()
                        .is_some_and(|regn| !pending.contains(&regn));
                }
                Op::MakeFunction | Op::MakeClosure if instr.make_self => {}
                Op::LoadUpvalue => {}
                op if is_inline_pure_op(op) => {}
                _ => {
                    if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                        eprintln!(
                            "[otter-jit] dead-self skip callee {} pc {} op {:?} make_self={} pending={pending:?}",
                            callee.function_id,
                            instr.byte_pc,
                            instr.op(code_block),
                            instr.make_self,
                        );
                    }
                    return false;
                }
            }
            if !ok {
                if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                    eprintln!(
                        "[otter-jit] dead-self read callee {} pc {} op {:?} pending={pending:?}",
                        callee.function_id,
                        instr.byte_pc,
                        instr.op(code_block),
                    );
                }
                return false;
            }

            match instr.op(code_block) {
                Op::LoadInt32
                | Op::LoadNumber
                | Op::LoadUndefined
                | Op::LoadNull
                | Op::LoadHole
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::BitwiseOr
                | Op::BitwiseAnd
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                | Op::ToPrimitive
                | Op::ToNumeric => {
                    if let Ok(dst) = reg(operands, 0) {
                        pending.retain(|&seen| seen != dst);
                    }
                }
                Op::LoadLocal => {
                    let Ok(dst) = reg(operands, 0) else {
                        return false;
                    };
                    let Ok(src) = local_index(operands, 1) else {
                        return false;
                    };
                    let src_is_self = pending.contains(&src);
                    pending.retain(|&seen| seen != dst);
                    if src_is_self {
                        pending.push(dst);
                    }
                }
                Op::StoreLocal => {
                    let Ok(src) = reg(operands, 0) else {
                        return false;
                    };
                    let Ok(dst) = local_index(operands, 1) else {
                        return false;
                    };
                    let src_is_self = pending.contains(&src);
                    pending.retain(|&seen| seen != dst);
                    if src_is_self {
                        pending.push(dst);
                    }
                }
                Op::LoadUpvalue => {
                    if let Ok(dst) = reg(operands, 0) {
                        pending.retain(|&seen| seen != dst);
                    }
                }
                Op::MakeFunction | Op::MakeClosure if instr.make_self => {
                    let Ok(dst) = reg(operands, 0) else {
                        return false;
                    };
                    pending.retain(|&seen| seen != dst);
                    pending.push(dst);
                }
                _ => {}
            }
        }
        true
    }

    fn classify_inline_call(callee: &JitInlineCallee) -> Option<InlineCallKind> {
        let code_block = callee.code_block.as_ref();
        let has_upvalue_op = callee.instructions.iter().any(|instr| {
            matches!(
                instr.op(code_block),
                Op::LoadUpvalue | Op::StoreUpvalue | Op::StoreUpvalueChecked
            )
        });
        if !has_upvalue_op {
            let ops_ok = callee
                .instructions
                .iter()
                .all(|instr| inline_plain_op_allowed(code_block, instr));
            let dead_self = self_bindings_are_dead(callee);
            if std::env::var_os("OTTER_JIT_TRACE").is_some() && (!ops_ok || !dead_self) {
                let bad_op = callee
                    .instructions
                    .iter()
                    .find(|instr| !inline_plain_op_allowed(code_block, instr))
                    .map(|instr| (instr.byte_pc, instr.op(code_block)));
                eprintln!(
                    "[otter-jit] inline call classify skip callee {}: ops_ok={ops_ok} dead_self={dead_self} bad_op={bad_op:?}",
                    callee.function_id
                );
            }
            return (ops_ok && dead_self).then_some(InlineCallKind::Plain);
        }
        if !self_bindings_are_dead(callee) {
            if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                eprintln!(
                    "[otter-jit] inline call classify skip callee {}: live self binding",
                    callee.function_id
                );
            }
            return None;
        }

        let mut regs = vec![InlineKnown::Unknown; usize::from(callee.register_count)];
        let mut store_seen = false;
        for instr in &callee.instructions {
            let operands = instr.operand_view(code_block);
            let read = |regs: &[InlineKnown], regn: u16| -> Option<InlineKnown> {
                regs.get(regn as usize).copied()
            };
            let write = |regs: &mut [InlineKnown], regn: u16, kind: InlineKnown| -> Option<()> {
                let slot = regs.get_mut(regn as usize)?;
                *slot = kind;
                Some(())
            };

            match instr.op(code_block) {
                Op::LoadInt32 | Op::LoadNumber => {
                    write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Number)?;
                }
                Op::LoadTrue | Op::LoadFalse => {
                    write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Bool)?;
                }
                Op::LoadUndefined | Op::LoadHole => {
                    write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Unknown)?;
                }
                Op::LoadLocal => {
                    let dst = reg(operands, 0).ok()?;
                    let src = local_index(operands, 1).ok()?;
                    let kind = read(&regs, src)?;
                    write(&mut regs, dst, kind)?;
                }
                Op::StoreLocal => {
                    let src = reg(operands, 0).ok()?;
                    let dst = local_index(operands, 1).ok()?;
                    let kind = read(&regs, src)?;
                    write(&mut regs, dst, kind)?;
                }
                Op::LoadUpvalue => {
                    write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Unknown)?;
                }
                Op::ToPrimitive => {
                    let dst = reg(operands, 0).ok()?;
                    let src = reg(operands, 1).ok()?;
                    let kind = read(&regs, src)?;
                    if store_seen && kind != InlineKnown::Number {
                        return None;
                    }
                    write(&mut regs, dst, kind)?;
                }
                Op::ToNumeric => {
                    let dst = reg(operands, 0).ok()?;
                    let src = reg(operands, 1).ok()?;
                    let kind = read(&regs, src)?;
                    if store_seen && kind != InlineKnown::Number {
                        return None;
                    }
                    write(&mut regs, dst, InlineKnown::Number)?;
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
                    let dst = reg(operands, 0).ok()?;
                    let lhs = read(&regs, reg(operands, 1).ok()?)?;
                    let rhs = read(&regs, reg(operands, 2).ok()?)?;
                    if store_seen && (lhs != InlineKnown::Number || rhs != InlineKnown::Number) {
                        return None;
                    }
                    write(&mut regs, dst, InlineKnown::Number)?;
                }
                Op::BitwiseOr
                | Op::BitwiseAnd
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    let dst = reg(operands, 0).ok()?;
                    let lhs = read(&regs, reg(operands, 1).ok()?)?;
                    let rhs = read(&regs, reg(operands, 2).ok()?)?;
                    if store_seen {
                        return None;
                    }
                    let result = if matches!(
                        instr.op(code_block),
                        Op::LessThan
                            | Op::LessEq
                            | Op::GreaterThan
                            | Op::GreaterEq
                            | Op::Equal
                            | Op::NotEqual
                    ) {
                        InlineKnown::Bool
                    } else {
                        let _ = (lhs, rhs);
                        InlineKnown::Number
                    };
                    write(&mut regs, dst, result)?;
                }
                Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                    let src = reg(operands, 0).ok()?;
                    if read(&regs, src)? != InlineKnown::Number {
                        return None;
                    }
                    store_seen = true;
                }
                Op::Return | Op::ReturnValue | Op::ReturnUndefined => {}
                // Keep upvalue inlining straight-line. The existing plain
                // inliner still owns branchy pure callees.
                _ => return None,
            }
        }
        Some(InlineCallKind::ClosureUpvalues)
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
        code_block: &otter_vm::CodeBlock,
        instr: &otter_vm::JitInstructionMetadata,
        bail: DynamicLabel,
        inline_done: DynamicLabel,
        clabels: &BTreeMap<u32, DynamicLabel>,
        cage_base: usize,
    ) -> Result<(), Unsupported> {
        let ops_ref = instr.operand_view(code_block);
        let ctarget = |rel: i32| -> Result<DynamicLabel, Unsupported> {
            let t = branch_target(code_block, instr, rel);
            u32::try_from(t)
                .ok()
                .and_then(|pc| clabels.get(&pc).copied())
                .ok_or(Unsupported::BranchTarget(t))
        };
        match instr.op(code_block) {
            Op::LoadInt32 => {
                let dst = reg(ops_ref, 0)?;
                let v = imm32(ops_ref, 1)?;
                emit_load_u64(ops, 9, value_tag::NUMBER_TAG | u64::from(v as u32));
                store_reg(ops, 9, dst)?;
            }
            Op::MakeFunction | Op::MakeClosure if instr.make_self => {}
            Op::LoadNumber => {
                let dst = reg(ops_ref, 0)?;
                let Some(value) = instr.load_number else {
                    return Err(Unsupported::OperandShape("load-number constant"));
                };
                // Materialize the boxed `Value` (int32 or offset-double), not the
                // raw f64 bits.
                emit_load_u64(ops, 9, otter_vm::Value::number_f64(value).to_bits());
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
                emit_load_u64(ops, 9, VALUE_UNDEFINED);
                store_reg(ops, 9, dst)?;
            }
            Op::LoadHole => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, VALUE_HOLE);
                store_reg(ops, 9, dst)?;
            }
            Op::LoadTrue => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, VALUE_TRUE);
                store_reg(ops, 9, dst)?;
            }
            Op::LoadFalse => {
                let dst = reg(ops_ref, 0)?;
                emit_load_u64(ops, 9, VALUE_FALSE);
                store_reg(ops, 9, dst)?;
            }
            Op::StoreLocal => {
                let src = reg(ops_ref, 0)?;
                let idx = local_index(ops_ref, 1)?;
                load_reg(ops, 9, src)?;
                store_reg(ops, 9, idx)?;
            }
            Op::LoadUpvalue => {
                if cage_base == 0 {
                    return Err(Unsupported::OperandShape("inline upvalue without cage"));
                }
                let dst = reg(ops_ref, 0)?;
                let idx = imm32(ops_ref, 1)?;
                if idx < 0 {
                    return Err(Unsupported::OperandShape("upvalue index"));
                }
                let idx_off = u32::try_from(idx)
                    .ok()
                    .and_then(|idx| idx.checked_mul(UPVALUE_CELL_SIZE))
                    .ok_or(Unsupported::OperandShape("upvalue index"))?;
                if idx_off > 32760 {
                    return Err(Unsupported::OperandShape("upvalue index"));
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x9, [x20, UPVALUES_PTR_OFFSET]
                    ; cbz x9, =>bail
                    ; ldr w10, [x9, idx_off]
                );
                emit_load_u64(ops, 11, cage_base as u64);
                emit_load_u64(ops, 12, VALUE_HOLE);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x11, x11, x10
                    ; ldr x9, [x11, UPVALUE_VALUE_OFFSET]
                    ; cmp x9, x12
                    ; b.eq =>bail
                );
                store_reg(ops, 9, dst)?;
            }
            Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                if cage_base == 0 {
                    return Err(Unsupported::OperandShape("inline upvalue without cage"));
                }
                let src = reg(ops_ref, 0)?;
                let idx = imm32(ops_ref, 1)?;
                if idx < 0 {
                    return Err(Unsupported::OperandShape("upvalue index"));
                }
                let idx_off = u32::try_from(idx)
                    .ok()
                    .and_then(|idx| idx.checked_mul(UPVALUE_CELL_SIZE))
                    .ok_or(Unsupported::OperandShape("upvalue index"))?;
                if idx_off > 32760 {
                    return Err(Unsupported::OperandShape("upvalue index"));
                }
                load_reg(ops, 12, src)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x11, NUMBER_TAG_HI16, lsl #48
                    ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst x12, x11
                    ; b.eq =>bail
                    ; ldr x9, [x20, UPVALUES_PTR_OFFSET]
                    ; cbz x9, =>bail
                    ; ldr w10, [x9, idx_off]
                );
                emit_load_u64(ops, 13, cage_base as u64);
                dynasm!(ops ; .arch aarch64 ; add x13, x13, x10);
                if instr.op(code_block) == Op::StoreUpvalueChecked {
                    emit_load_u64(ops, 11, VALUE_HOLE);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x14, [x13, UPVALUE_VALUE_OFFSET]
                        ; cmp x14, x11
                        ; b.eq =>bail
                    );
                }
                dynasm!(ops ; .arch aarch64 ; str x12, [x13, UPVALUE_VALUE_OFFSET]);
            }
            Op::Add | Op::Sub | Op::Mul => {
                emit_add_sub_mul(ops, ops_ref, bail, instr.op(code_block))?
            }
            Op::Div => emit_div(ops, ops_ref, bail)?,
            Op::Rem => emit_rem(ops, ops_ref, bail)?,
            Op::BitwiseOr => emit_int_binop(ops, ops_ref, bail, IntBinOp::Or)?,
            Op::BitwiseAnd => emit_int_binop(ops, ops_ref, bail, IntBinOp::And)?,
            Op::BitwiseXor => emit_int_binop(ops, ops_ref, bail, IntBinOp::Xor)?,
            Op::Shl => emit_int_binop(ops, ops_ref, bail, IntBinOp::Shl)?,
            Op::Shr => emit_int_binop(ops, ops_ref, bail, IntBinOp::Shr)?,
            Op::Ushr => emit_ushr(ops, ops_ref, bail)?,
            Op::LessThan => emit_cmp(ops, ops_ref, bail, Cmp::Lt)?,
            Op::LessEq => emit_cmp(ops, ops_ref, bail, Cmp::Le)?,
            Op::GreaterThan => emit_cmp(ops, ops_ref, bail, Cmp::Gt)?,
            Op::GreaterEq => emit_cmp(ops, ops_ref, bail, Cmp::Ge)?,
            Op::Equal => emit_cmp(ops, ops_ref, bail, Cmp::Eq)?,
            Op::NotEqual => emit_cmp(ops, ops_ref, bail, Cmp::Ne)?,
            Op::ToPrimitive => {
                let dst = reg(ops_ref, 0)?;
                let src = reg(ops_ref, 1)?;
                emit_to_primitive_identity(ops, dst, src, bail)?;
            }
            Op::ToNumeric => {
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
                    ; sub x14, x9, #(VALUE_FALSE as u32)          // bail unless boolean
                    ; cmp x14, #1
                    ; b.hi =>bail
                    ; cmp x9, #(VALUE_TRUE as u32)                // eq iff true
                );
                if matches!(instr.op(code_block), Op::JumpIfFalse) {
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
                emit_load_u64(ops, 9, VALUE_UNDEFINED);
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
        call_operands: impl WordOperands,
        cage_base: usize,
        bail: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        let dst = reg(call_operands, 0)?;
        let callee_reg = reg(call_operands, 1)?;
        let argc = const_index(call_operands, 2)? as usize;
        let Some(kind) = classify_inline_call(callee) else {
            if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                eprintln!(
                    "[otter-jit] inline call skip callee {}: classify",
                    callee.function_id
                );
            }
            return Ok(false);
        };

        if argc != usize::from(callee.param_count)
            || argc > INLINE_MAX_ARGS
            || callee.register_count > INLINE_MAX_REGS
            || callee.instructions.len() > INLINE_MAX_INSTRS
            || (kind == InlineCallKind::ClosureUpvalues && cage_base == 0)
        {
            if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                eprintln!(
                    "[otter-jit] inline call skip callee {}: shape argc={argc} params={} regs={} instrs={} kind={kind:?} cage_base={}",
                    callee.function_id,
                    callee.param_count,
                    callee.register_count,
                    callee.instructions.len(),
                    cage_base,
                );
            }
            return Ok(false);
        }

        // One private label per callee byte-PC for internal branches.
        let mut clabels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
        for i in &callee.instructions {
            clabels.insert(
                i.instruction_pc(&callee.code_block),
                ops.new_dynamic_label(),
            );
        }
        let inline_done = ops.new_dynamic_label();
        let inline_bail = ops.new_dynamic_label();
        let after = ops.new_dynamic_label();
        let saved_upvalues_slot = u32::from(callee.register_count);
        let scratch_regs =
            u32::from(callee.register_count) + u32::from(kind == InlineCallKind::ClosureUpvalues);
        let scratch_bytes = (scratch_regs * 8).next_multiple_of(16);

        // Identity guard (x19 = caller frame base, sp not yet moved). Plain
        // function values compare directly. Closure-upvalue inlines ask the VM
        // to validate the current closure's function id and unsupported closure
        // metadata, returning the immutable upvalue-spine base on success.
        if kind == InlineCallKind::Plain {
            load_reg(ops, 9, callee_reg)?;
            emit_load_u64(
                ops,
                10,
                value_tag::FUNCTION_ID_TAG | (u64::from(callee.function_id) << 16),
            );
            dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>bail);
        } else {
            dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, callee_reg as u32);
            emit_load_u64(ops, 2, u64::from(callee.function_id));
            emit_load_u64(
                ops,
                16,
                jit_inline_closure_upvalues_stub as *const () as u64,
            );
            dynasm!(ops
                ; .arch aarch64
                ; blr x16
                ; cbz x0, =>bail
                ; mov x15, x0
            );
        }

        // Reserve scratch, copy args into param slots (read via caller base x19),
        // zero the remaining slots to undefined (a fresh frame's register state),
        // then repoint x19 at the scratch base for the body.
        if scratch_bytes > 0 {
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
        }
        if kind == InlineCallKind::ClosureUpvalues {
            let saved_off = saved_upvalues_slot * 8;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [x20, UPVALUES_PTR_OFFSET]
                ; str x14, [sp, saved_off]
                ; str x15, [x20, UPVALUES_PTR_OFFSET]
            );
        }
        for slot in 0..argc {
            let areg = reg(call_operands, 3 + slot)?;
            load_reg(ops, 9, areg)?;
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        emit_load_u64(ops, 9, VALUE_UNDEFINED);
        for slot in argc..usize::from(callee.register_count) {
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        dynasm!(ops ; .arch aarch64 ; add x19, sp, #0);

        for i in &callee.instructions {
            let instruction_pc = i.instruction_pc(&callee.code_block);
            dynasm!(ops ; .arch aarch64 ; =>clabels[&instruction_pc]);
            emit_inline_pure_op(
                ops,
                &callee.code_block,
                i,
                inline_bail,
                inline_done,
                &clabels,
                cage_base,
            )?;
        }

        // Normal completion: result in x9, unwind scratch, restore caller base,
        // store to dst.
        dynasm!(ops ; .arch aarch64 ; =>inline_done);
        if kind == InlineCallKind::ClosureUpvalues {
            let saved_off = saved_upvalues_slot * 8;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [sp, saved_off]
                ; str x14, [x20, UPVALUES_PTR_OFFSET]
            );
        }
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
        if kind == InlineCallKind::ClosureUpvalues {
            let saved_off = saved_upvalues_slot * 8;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [sp, saved_off]
                ; str x14, [x20, UPVALUES_PTR_OFFSET]
            );
        }
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
        code_block: &otter_vm::CodeBlock,
        instr: &otter_vm::JitInstructionMetadata,
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
        let ops_ref = instr.operand_view(code_block);
        match instr.op(code_block) {
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
                    ; movz x11, NUMBER_TAG_HI16, lsl #48
                    ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst x9, x11
                    ; b.ne =>bail
                    ; mov w12, w9
                );
                emit_load_u64(ops, 13, cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x13, x13, x12
                    ; ldr x13, [x13, object_values_ptr_byte]
                    ; cbz x13, =>bail
                    ; ldr w9, [x13, off]                // 4-byte compressed slot
                );
                emit_decompress_slot(ops, cage_base as u64, bail);
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
                    ; movz x11, NUMBER_TAG_HI16, lsl #48
                    ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst x9, x11
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
                load_reg(ops, 9, src)?;
                dynasm!(ops
                    ; .arch aarch64
                    // Only a barrier-free primitive is inlined; a heap cell needs
                    // the generational write barrier and bails to the interpreter.
                    ; movz x11, NUMBER_TAG_HI16, lsl #48
                    ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst x9, x11
                    ; b.eq =>bail                          // heap cell → interpreter
                    ; ldr x13, [x13, object_values_ptr_byte]
                    ; cbz x13, =>bail
                );
                emit_compress_slot_or_bail(ops, bail);
                dynasm!(ops ; .arch aarch64 ; str w10, [x13, off]);
                Ok(())
            }
            _ => emit_inline_pure_op(
                ops,
                code_block,
                instr,
                bail,
                inline_done,
                clabels,
                cage_base,
            ),
        }
    }

    /// Whether `method`'s baked body can be spliced inline for a call of `argc`
    /// arguments. Mirrors the emit-time constraints the inline body relies on:
    /// arity match, register/instruction/arg budgets, an all-inlinable op set,
    /// and no bailing op after an in-place `StoreProperty` (a post-store bail
    /// would re-run the whole method and double-apply the mutation).
    fn inline_method_emit_eligible(method: &JitInlineMethod, argc: usize) -> bool {
        let code_block = method.code_block.as_ref();
        if argc != usize::from(method.param_count)
            || argc > INLINE_MAX_ARGS
            || method.register_count >= INLINE_MAX_REGS
            || method.instructions.len() > INLINE_MAX_INSTRS
            || !method
                .instructions
                .iter()
                .all(|i| is_inline_method_op(i.op(code_block)))
        {
            return false;
        }
        let mut store_seen = false;
        for i in &method.instructions {
            if store_seen && !is_nonbailing_after_store(i.op(code_block)) {
                return false;
            }
            if i.op(code_block) == Op::StoreProperty {
                store_seen = true;
            }
        }
        true
    }

    /// Emit one inline method attempt: the inline identity guard followed by the
    /// spliced body. On any guard mismatch (receiver tag/shape, prototype
    /// tag/shape, method-slot tag, or resolved `function_id`) control branches to
    /// `miss` — for a monomorphic site that is the in-place method bridge; for a
    /// polymorphic chain it is the next target's guard. On normal completion the
    /// result is written to the call's `dst` and control branches to `after`. A
    /// body store-bail unwinds the scratch window and branches to the shared
    /// `bail`. The caller must have checked [`inline_method_emit_eligible`].
    ///
    /// Soundness: the guard re-reads the receiver shape and re-resolves the
    /// prototype method slot every call, so a prototype-method reassignment or a
    /// receiver of a different shape lands on `miss` (no stale dispatch). All
    /// guards run *before* the scratch window is reserved and *before* any
    /// in-place store, so routing `miss` to a sibling attempt mutates no state.
    #[allow(clippy::too_many_arguments)]
    fn emit_inline_method_attempt(
        ops: &mut Assembler,
        method: &JitInlineMethod,
        call_operands: impl WordOperands,
        argc: usize,
        cage_base: usize,
        object_shape_byte: u32,
        object_values_ptr_byte: u32,
        jit_proto_byte: u32,
        closure_fid_byte: u32,
        miss: DynamicLabel,
        after: DynamicLabel,
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let dst = reg(call_operands, 0)?;
        let recv_reg = reg(call_operands, 1)?;

        let mut clabels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
        for i in &method.instructions {
            clabels.insert(
                i.instruction_pc(&method.code_block),
                ops.new_dynamic_label(),
            );
        }
        let inline_done = ops.new_dynamic_label();
        let inline_bail = ops.new_dynamic_label();
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
        // tag, slot tag, or id) lands on `miss`.
        let recv_off = reg_offset(recv_reg)?;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, recv_off]
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x13, object_shape_byte]
            ; movz w15, method.recv_shape & 0xffff
            ; movk w15, (method.recv_shape >> 16) & 0xffff, lsl #16
            ; cmp w14, w15
            ; b.ne =>miss
        );
        for &hop_shape in &method.proto_chain {
            dynasm!(ops
                ; .arch aarch64
                // Flat prototype: load the compressed handle, bail on null,
                // then decompress and guard the hopped object's shape. After
                // the final hop x13 holds the method holder's header.
                ; ldr w9, [x13, jit_proto_byte]
                ; cbz w9, =>miss
            );
            emit_load_u64(ops, 12, cage_base as u64);
            dynasm!(ops
                ; .arch aarch64
                ; add x13, x12, x9
                ; ldrb w14, [x13]
                ; cmp w14, OBJECT_BODY_TYPE_TAG
                ; b.ne =>miss
                ; ldr w14, [x13, object_shape_byte]
                ; movz w15, hop_shape & 0xffff
                ; movk w15, (hop_shape >> 16) & 0xffff, lsl #16
                ; cmp w14, w15
                ; b.ne =>miss
            );
        }
        dynasm!(ops
            ; .arch aarch64
            // Method slot: load the 64-bit Value from the receiver's or
            // prototype's value slab. A resolved method is either a closure-less
            // bytecode reference (function-id immediate, fid in bits [16, 48)) or
            // a closure cell (`JsClosureBody`, fid read from its body). Decode
            // the function id into w14 either way, then compare to the baked id;
            // a number or any non-closure cell misses.
            ; ldr x13, [x13, object_values_ptr_byte]
            ; cbz x13, =>miss
            ; ldr w9, [x13, method.method_value_byte]   // 4-byte compressed slot
        );
        emit_decompress_slot(ops, cage_base as u64, miss);
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; tst x9, x11
            ; b.ne =>miss                 // a number is not a callable method
            ; and x10, x9, #0xffff
            ; cmp x10, #(FUNCTION_ID_TAG as u32)
            ; b.eq =>fid_immediate
            ; mov w12, w9                 // otherwise a cell: low32 = gc offset
        );
        emit_load_u64(ops, 11, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x11, x11, x12
            // Require a closure body (a non-closure cell has a different header
            // tag at this offset), then read `function_id`.
            ; ldrb w14, [x11]
            ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x11, closure_fid_byte]
            ; b =>fid_compare
            ; =>fid_immediate
            ; lsr x14, x9, #16            // function id in bits [16, 48)
            ; =>fid_compare
            ; movz w15, method.method_fid & 0xffff
            ; movk w15, (method.method_fid >> 16) & 0xffff, lsl #16
            ; cmp w14, w15
            ; b.ne =>miss
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
        emit_load_u64(ops, 9, VALUE_UNDEFINED);
        for slot in argc..usize::from(method.register_count) {
            dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
        }
        dynasm!(ops ; .arch aarch64 ; add x19, sp, #0);

        for i in &method.instructions {
            let instruction_pc = i.instruction_pc(&method.code_block);
            dynasm!(ops ; .arch aarch64 ; =>clabels[&instruction_pc]);
            emit_inline_method_op(
                ops,
                &method.code_block,
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
        Ok(())
    }

    /// Splice a tiny monomorphic read-only / sealed-write method body into the
    /// current `Op::CallMethodValue` site instead of building a callee frame.
    /// Returns `Ok(true)` when inlined, `Ok(false)` when the method fails the
    /// op-allowlist / size / arity test (the caller then emits the normal
    /// method-call bridge). See [`emit_inline_method_attempt`] for the
    /// guard/body/soundness details; here a guard miss takes the in-place call.
    #[allow(clippy::too_many_arguments)]
    fn try_emit_inline_method_call(
        ops: &mut Assembler,
        method: &JitInlineMethod,
        call_operands: impl WordOperands,
        site: u64,
        cage_base: usize,
        object_shape_byte: u32,
        object_values_ptr_byte: u32,
        jit_proto_byte: u32,
        closure_fid_byte: u32,
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        let argc = const_index(call_operands, 3)? as usize;
        if cage_base == 0 || !inline_method_emit_eligible(method, argc) {
            return Ok(false);
        }
        let fallback = ops.new_dynamic_label();
        let after = ops.new_dynamic_label();
        emit_inline_method_attempt(
            ops,
            method,
            call_operands,
            argc,
            cage_base,
            object_shape_byte,
            object_values_ptr_byte,
            jit_proto_byte,
            closure_fid_byte,
            fallback,
            after,
            bail,
        )?;
        // Ineligible at run time (method changed / shape mismatch): the full
        // in-place method call, which restores nothing (sp untouched here).
        dynasm!(ops ; .arch aarch64 ; =>fallback);
        emit_method_call(
            ops,
            call_operands,
            site,
            None,
            None,
            None,
            None,
            bail,
            threw,
        )?;
        dynasm!(ops ; .arch aarch64 ; =>after);
        Ok(true)
    }

    /// Splice a most-frequent-first chain of inline method attempts for a
    /// *polymorphic* `Op::CallMethodValue` site. Each attempt guards its own
    /// receiver shape + prototype-method identity; a miss falls through to the
    /// next attempt, and a receiver matching none of them takes the in-place
    /// method bridge. Returns `Ok(false)` (no inline emitted) when no target is
    /// emit-eligible, so the caller emits the normal bridge.
    ///
    /// Soundness is identical to the monomorphic path: every attempt's guards run
    /// before it reserves a scratch window or performs any in-place store, so a
    /// guard miss that routes control to a sibling attempt has mutated nothing.
    #[allow(clippy::too_many_arguments)]
    fn try_emit_poly_inline_method_call(
        ops: &mut Assembler,
        methods: &[JitInlineMethod],
        call_operands: impl WordOperands,
        site: u64,
        cage_base: usize,
        object_shape_byte: u32,
        object_values_ptr_byte: u32,
        jit_proto_byte: u32,
        closure_fid_byte: u32,
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        let argc = const_index(call_operands, 3)? as usize;
        if cage_base == 0 {
            return Ok(false);
        }
        let eligible: Vec<&JitInlineMethod> = methods
            .iter()
            .filter(|m| inline_method_emit_eligible(m, argc))
            .collect();
        if eligible.is_empty() {
            return Ok(false);
        }
        let after = ops.new_dynamic_label();
        let fallback = ops.new_dynamic_label();
        // One entry label per attempt so each attempt's guard miss can branch to
        // the next attempt; the final attempt's miss branches to `fallback`.
        let entries: Vec<DynamicLabel> = (0..eligible.len())
            .map(|_| ops.new_dynamic_label())
            .collect();
        for (i, method) in eligible.iter().enumerate() {
            dynasm!(ops ; .arch aarch64 ; =>entries[i]);
            let miss = if i + 1 < eligible.len() {
                entries[i + 1]
            } else {
                fallback
            };
            emit_inline_method_attempt(
                ops,
                method,
                call_operands,
                argc,
                cage_base,
                object_shape_byte,
                object_values_ptr_byte,
                jit_proto_byte,
                closure_fid_byte,
                miss,
                after,
                bail,
            )?;
        }
        // No guard matched: the full in-place method call (sp untouched here).
        dynasm!(ops ; .arch aarch64 ; =>fallback);
        emit_method_call(
            ops,
            call_operands,
            site,
            None,
            None,
            None,
            None,
            bail,
            threw,
        )?;
        dynasm!(ops ; .arch aarch64 ; =>after);
        Ok(true)
    }

    /// Copy isolate- and execution-owned fields shared by every nested `JitCtx`.
    /// Callee registers, bindings, frame/upvalues, and safepoints are initialized
    /// separately by each native calling convention.
    fn emit_copy_shared_execution_context(ops: &mut Assembler) {
        for off in [
            THREAD_OFFSET,
            NATIVE_FRAME_OFFSET,
            ERROR_SLOT_OFFSET,
            REG_STACK_BASE_OFFSET,
            REG_TOP_PTR_OFFSET,
            SYNC_REENTRY_DEPTH_PTR_OFFSET,
            ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET,
            COLLECTION_METHOD_ICS_OFFSET,
            DIRECT_METHOD_INLINE_OFFSET,
            GC_HEAP_OFFSET,
            INTERRUPT_FLAG_OFFSET,
            BACKEDGE_FUEL_OFFSET,
        ] {
            dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, off] ; str x9, [sp, off]);
        }
        for off in [SYNC_REENTRY_LIMIT_OFFSET, COLLECTION_METHOD_IC_COUNT_OFFSET] {
            dynasm!(ops ; .arch aarch64 ; ldr w9, [x20, off] ; str w9, [sp, off]);
        }
    }

    /// Emit a self-recursive `Op::Call` inline, with no Rust frame-build bridge:
    /// guard the callee is the running closure, reserve a callee window on the
    /// interpreter's flat register stack, bind args, build the callee `JitCtx`,
    /// and re-enter the function's own entry. A guard miss or a register-stack
    /// overflow falls through to the general direct-call bridge (`emit_call`,
    /// emitted at `bridge`). The callee's compiled completion writes its value
    /// straight to `dst`; a callee bail rebuilds an interpreter frame from the
    /// window and runs it to completion ([`jit_self_call_bail_stub`]).
    ///
    /// Only emitted for a frame-index-free function (see [`is_self_call_safe`]):
    /// its body uses no stub that addresses registers through
    /// `JitCtx.frame_index`, so a frameless callee window is sound. A guard miss
    /// (the call is not self-recursive) or a register-stack overflow bails to the
    /// interpreter at the call (`bail`), which reconstructs a real frame.
    fn emit_self_recursive_call(
        ops: &mut Assembler,
        operands: impl WordOperands,
        regcount: u16,
        self_entry: DynamicLabel,
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let dst = reg(operands, 0)?;
        let callee = reg(operands, 1)?;
        let argc = const_index(operands, 2)? as usize;
        if argc > MAX_INLINE_ARGS {
            return Err(Unsupported::ArgCount(argc));
        }
        let rc = u32::from(regcount);
        let done = ops.new_dynamic_label();
        let returned = ops.new_dynamic_label();
        let bailed = ops.new_dynamic_label();
        let fill = ops.new_dynamic_label();
        let fill_done = ops.new_dynamic_label();
        let undef_bits: u64 = VALUE_UNDEFINED;

        // Guard the callee is the running closure (`ctx.self_closure` @ +8).
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, callee as u32 * 8]
            ; ldr x10, [x20, #8]
            ; cmp x9, x10
            ; b.ne =>bail
        );
        // Reserve the window: x12 = &reg_top, x11 = old top, x14 = window ptr,
        // x13 = new top. Overflow → bridge.
        dynasm!(ops
            ; .arch aarch64
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x11, [x12]
            ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
            ; add x14, x9, x11, lsl #3
        );
        emit_load_u64(ops, 13, u64::from(rc));
        dynasm!(ops ; .arch aarch64 ; add x13, x11, x13);
        emit_load_u64(ops, 9, Interpreter::jit_reg_stack_cap() as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x13, x9
            ; b.hi =>bail
            ; ldr x17, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w9, [x17]
            ; ldr w10, [x20, SYNC_REENTRY_LIMIT_OFFSET]
            ; cmp w9, w10
            ; b.hs =>bail
            ; add w9, w9, #1
            ; str w9, [x17]
            ; str x13, [x12]
        );
        // Zero-fill the window to `undefined`.
        emit_load_u64(ops, 10, undef_bits);
        emit_load_u64(ops, 15, u64::from(rc));
        dynasm!(ops
            ; .arch aarch64
            ; movz x9, 0
            ; =>fill
            ; cmp x9, x15
            ; b.hs =>fill_done
            ; str x10, [x14, x9, lsl #3]
            ; add x9, x9, #1
            ; b =>fill
            ; =>fill_done
        );
        // Bind args into the window's leading slots.
        for slot in 0..argc {
            let areg = reg(operands, 3 + slot)?;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x9, [x19, areg as u32 * 8]
                ; str x9, [x14, slot as u32 * 8]
            );
        }
        // Build the callee `JitCtx` on the native stack and re-enter `self_entry`.
        // regs = window; self_closure / upvalues / vm / stack / context /
        // frame_index / error / reg-stack pointers copy from the caller ctx
        // (self-recursion shares them); this = undefined; bail_pc = 0.
        dynasm!(ops
            ; .arch aarch64
            ; sub sp, sp, JIT_CTX_STACK_SIZE
            ; str x14, [sp]
            ; ldr x9, [x20, #8] ; str x9, [sp, #8]
        );
        emit_load_u64(ops, 9, undef_bits);
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, #16] ; str wzr, [sp, BAIL_PC_OFFSET]);
        emit_copy_shared_execution_context(ops);
        for off in [FRAME_INDEX_OFFSET, UPVALUES_PTR_OFFSET] {
            dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, off] ; str x9, [sp, off]);
        }
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, sp
            ; bl =>self_entry
            ; cmp x1, STATUS_BAILED as u32
            ; b.eq =>bailed
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; cmp x1, STATUS_RETURNED as u32
            ; b.eq =>returned
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x13, [x12]
        );
        emit_load_u64(ops, 9, u64::from(rc));
        dynasm!(ops
            ; .arch aarch64
            ; sub x13, x13, x9
            ; str x13, [x12]
            ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w13, [x12]
            ; sub w13, w13, #1
            ; str w13, [x12]
            ; b =>threw
        );
        // Returned: pop the window, store the value into `dst`.
        dynasm!(ops
            ; .arch aarch64
            ; =>returned
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x13, [x12]
        );
        emit_load_u64(ops, 9, u64::from(rc));
        dynasm!(ops
            ; .arch aarch64
            ; sub x13, x13, x9
            ; str x13, [x12]
            ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w13, [x12]
            ; sub w13, w13, #1
            ; str w13, [x12]
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        // Bailed: read the callee's resume PC, drop the native ctx, and run the
        // bailed callee to completion through the bail helper (which rebuilds an
        // interpreter frame from the live window and pops it). Helper returns the
        // value in x0 and status in x1.
        dynasm!(ops
            ; .arch aarch64
            ; =>bailed
            ; ldr w2, [sp, BAIL_PC_OFFSET]
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; mov x0, x20
            ; mov w1, w2
        );
        emit_load_u64(ops, 2, u64::from(rc));
        emit_load_u64(ops, 16, jit_self_call_bail_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w13, [x12]
            ; sub w13, w13, #1
            ; str w13, [x12]
            ; cmp x1, STATUS_THREW as u32
            ; b.eq =>threw
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// Whether `view`'s body is safe to run as a frameless self-recursive callee:
    /// every op either runs inline against the register window (`x19`) or is a
    /// `Call` (self-recursive — resolved by the inline guard — or a guard miss
    /// that bails) or the self-binding `MakeFunction`. Every allowed op is
    /// safepoint-free. A property/element/runtime operation may allocate or
    /// re-enter even when it addresses the flat register window, so it needs a
    /// published native activation and disqualifies the frameless path.
    fn is_self_call_safe(view: &JitCompileSnapshot) -> bool {
        let code_block = view.code_block.as_ref();
        view.instructions.iter().all(|instr| {
            is_inline_pure_op(instr.op(code_block))
                || instr.op(code_block) == Op::LoadThis
                || instr.op(code_block) == Op::Call
                || (matches!(instr.op(code_block), Op::MakeFunction | Op::MakeClosure)
                    && instr.make_self)
        })
    }

    /// Probe the VM-published polymorphic direct-method link table and enter a
    /// bytecode method through a rooted flat register window.
    /// Every guard precedes the window reservation, so a miss falls through to
    /// the normal typed method path without observable state.
    fn emit_direct_method_inline(
        ops: &mut Assembler,
        operands: impl WordOperands,
        site: u64,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        use otter_vm::jit::JIT_DIRECT_METHOD_WAYS;

        let argc = const_index(operands, 3)? as usize;
        if argc > MAX_METHOD_ARGS || view.cage_base == 0 {
            return Ok(false);
        }
        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let recv_off = reg_offset(recv)?;
        let returned = ops.new_dynamic_label();
        let bailed = ops.new_dynamic_label();
        let direct_threw = ops.new_dynamic_label();
        let hit = ops.new_dynamic_label();
        let table_byte = site
            .saturating_mul(JIT_DIRECT_METHOD_WAYS as u64)
            .saturating_mul(u64::from(DIRECT_METHOD_INLINE_SLOT_SIZE));

        // Common receiver guard. x8 retains the compressed object offset and
        // x7 the first link slot while each way may chase a prototype.
        dynasm!(ops
            ; .arch aarch64
            ; ldr x7, [x20, DIRECT_METHOD_INLINE_OFFSET]
            ; cbz x7, =>miss
        );
        emit_load_u64(ops, 12, table_byte);
        dynasm!(ops
            ; .arch aarch64
            ; add x7, x7, x12
            // Dense ways: first empty entry means whole site has no asm link.
            // Take cold fallback before receiver decoding or the large guard
            // chain, keeping non-eligible sites to one pointer + entry load.
            ; ldr x16, [x7, DIRECT_METHOD_ENTRY_OFFSET]
            ; cbz x16, =>miss
            ; ldr x9, [x19, recv_off]
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w8, w9
        );

        for way in 0..JIT_DIRECT_METHOD_WAYS {
            let next = if way + 1 == JIT_DIRECT_METHOD_WAYS {
                miss
            } else {
                ops.new_dynamic_label()
            };
            let way_byte = way as u32 * DIRECT_METHOD_INLINE_SLOT_SIZE;
            dynasm!(ops
                ; .arch aarch64
                ; add x17, x7, way_byte
                ; ldr x16, [x17, DIRECT_METHOD_ENTRY_OFFSET]
                // Ways are appended densely and cleared as a whole. An empty
                // entry therefore terminates the chain; no later way can hit.
                ; cbz x16, =>miss
            );
            emit_load_u64(ops, 12, view.cage_base as u64);
            dynasm!(ops
                ; .arch aarch64
                ; add x13, x12, x8
                ; ldrb w14, [x13]
                ; cmp w14, OBJECT_BODY_TYPE_TAG
                ; b.ne =>next
                ; ldr w14, [x13, view.object_shape_byte]
                ; ldr w15, [x17, DIRECT_METHOD_RECV_SHAPE_OFFSET]
                ; cmp w14, w15
                ; b.ne =>next
                ; ldr w15, [x17, DIRECT_METHOD_ON_RECEIVER_OFFSET]
                ; cbnz w15, >holder
                ; ldr w9, [x13, view.jit_proto_byte]
                ; cbz w9, =>next
            );
            emit_load_u64(ops, 12, view.cage_base as u64);
            dynasm!(ops
                ; .arch aarch64
                ; add x13, x12, x9
                ; ldrb w14, [x13]
                ; cmp w14, OBJECT_BODY_TYPE_TAG
                ; b.ne =>next
                ; ldr w14, [x13, view.object_shape_byte]
                ; ldr w15, [x17, DIRECT_METHOD_PROTO_SHAPE_OFFSET]
                ; cmp w14, w15
                ; b.ne =>next
                ; holder:
            );
            emit_slab_base(ops, view, 13, 14);
            dynasm!(ops
                ; .arch aarch64
                ; ldr w12, [x17, DIRECT_METHOD_VALUE_BYTE_OFFSET]
                ; ldr w9, [x13, x12]
            );
            emit_decompress_slot(ops, view.cage_base as u64, next);

            let immediate = ops.new_dynamic_label();
            let compare = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; tst x9, x11
                ; b.ne =>next
                ; and x10, x9, #0xffff
                ; cmp x10, #(FUNCTION_ID_TAG as u32)
                ; b.eq =>immediate
                ; mov w12, w9
            );
            emit_load_u64(ops, 11, view.cage_base as u64);
            dynasm!(ops
                ; .arch aarch64
                ; add x11, x11, x12
                ; ldrb w14, [x11]
                ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG
                ; b.ne =>next
                ; ldr w14, [x11, view.closure_fid_byte]
                ; ldr x10, [x11, view.closure_upvalues_ptr_byte]
                ; b =>compare
                ; =>immediate
                ; lsr x14, x9, #16
                ; movz x10, #0
                ; =>compare
                ; ldr w15, [x17, DIRECT_METHOD_FID_OFFSET]
                ; cmp w14, w15
                ; b.eq =>hit
            );
            if way + 1 != JIT_DIRECT_METHOD_WAYS {
                dynasm!(ops ; .arch aarch64 ; =>next);
            }
        }

        // x17 = selected link, x9 = live method SELF, x10 = live captured
        // upvalue spine. Keep those plus entry/window size in a native metadata
        // record while the callee context occupies the stack below it.
        dynasm!(ops
            ; .arch aarch64
            ; =>hit
            ; ldr x16, [x17, DIRECT_METHOD_ENTRY_OFFSET]
            ; ldr w15, [x17, DIRECT_METHOD_REGISTER_COUNT_OFFSET]
            ; sub sp, sp, #32
            ; str x16, [sp]
            ; str x15, [sp, #8]
            ; str x9, [sp, #16]
            ; str x10, [sp, #24]
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x11, [x12]
            ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
            ; add x14, x9, x11, lsl #3
            ; add x13, x11, x15
        );
        emit_load_u64(ops, 9, Interpreter::jit_reg_stack_cap() as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x13, x9
            ; b.hi >overflow
            ; ldr x17, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w9, [x17]
            ; ldr w10, [x20, SYNC_REENTRY_LIMIT_OFFSET]
            ; cmp w9, w10
            ; b.hs >overflow
            ; add w9, w9, #1
            ; str w9, [x17]
            ; str x13, [x12]
        );
        emit_load_u64(ops, 10, VALUE_UNDEFINED);
        let fill = ops.new_dynamic_label();
        let fill_done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x9, #0
            ; =>fill
            ; cmp x9, x15
            ; b.hs =>fill_done
            ; str x10, [x14, x9, lsl #3]
            ; add x9, x9, #1
            ; b =>fill
            ; =>fill_done

            // Bind supplied arguments directly into the callee window. A
            // frameless link is restricted to bodies without `arguments`, so
            // slots beyond the formal/register window are semantically dead;
            // missing slots remain the undefined values written above.
        );
        for slot in 0..argc {
            let arg = reg(operands, 4 + slot)?;
            let skip_arg = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; cmp x15, slot as u32
                ; b.ls =>skip_arg
                ; ldr x9, [x19, arg as u32 * 8]
                ; str x9, [x14, slot as u32 * 8]
                ; =>skip_arg
            );
        }
        dynasm!(ops
            ; .arch aarch64

            ; sub sp, sp, JIT_CTX_STACK_SIZE
            ; str x14, [sp]
            ; ldr x9, [sp, JIT_CTX_STACK_SIZE + 16]
            ; str x9, [sp, #8]
            ; ldr x9, [x19, recv_off]
            ; str x9, [sp, #16]
            ; str wzr, [sp, BAIL_PC_OFFSET]
        );
        emit_copy_shared_execution_context(ops);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x20, FRAME_INDEX_OFFSET]
            ; str x9, [sp, FRAME_INDEX_OFFSET]
        );
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [sp, JIT_CTX_STACK_SIZE + 24]
            ; str x9, [sp, UPVALUES_PTR_OFFSET]
            ; str xzr, [sp, DIRECT_ENTRY_OFFSET]
            ; str xzr, [sp, DIRECT_REGS_OFFSET]
            ; str xzr, [sp, DIRECT_SELF_OFFSET]
            ; str xzr, [sp, DIRECT_THIS_OFFSET]
            ; str xzr, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; str xzr, [sp, DIRECT_UPVALUES_OFFSET]
            ; mov x0, sp
            ; ldr x16, [sp, JIT_CTX_STACK_SIZE]
            ; blr x16
            ; cmp x1, STATUS_RETURNED as u32
            ; b.eq =>returned
            ; cmp x1, STATUS_BAILED as u32
            ; b.eq =>bailed
            ; b =>direct_threw

            ; =>returned
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; ldr x15, [sp, #8]
            ; add sp, sp, #32
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x13, [x12]
            ; sub x13, x13, x15
            ; str x13, [x12]
            ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w13, [x12]
            ; sub w13, w13, #1
            ; str w13, [x12]
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);

        dynasm!(ops
            ; .arch aarch64
            ; =>direct_threw
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; ldr x15, [sp, #8]
            ; add sp, sp, #32
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x13, [x12]
            ; sub x13, x13, x15
            ; str x13, [x12]
            ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w13, [x12]
            ; sub w13, w13, #1
            ; str w13, [x12]
            ; b =>threw

            ; =>bailed
            ; ldr w1, [sp, BAIL_PC_OFFSET]
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; ldr x2, [sp, #8]
            ; ldr x3, [sp, #16]
            ; ldr x4, [x19, recv_off]
            ; add sp, sp, #32
            ; mov x0, x20
        );
        emit_load_u64(
            ops,
            16,
            jit_direct_method_call_bail_stub as *const () as u64,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w13, [x12]
            ; sub w13, w13, #1
            ; str w13, [x12]
            ; cmp x1, STATUS_THREW as u32
            ; b.eq =>threw
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done ; overflow: ; add sp, sp, #32 ; b =>miss);
        Ok(true)
    }

    /// Emit a direct `Call`: ask the VM to publish an eligible callee frame,
    /// build the callee `JitCtx` on the native stack, branch to the compiled
    /// entry, then finish/pop/store through the narrow direct-call ABI. Cold or
    /// ineligible calls bail to the interpreter instead of using the generic
    /// runtime call bridge.
    fn emit_call(
        ops: &mut Assembler,
        _operands: impl WordOperands,
        bail: DynamicLabel,
        _threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        // The former direct-call ABI asked the interpreter to materialize a
        // HoltStack frame, then re-entered native code. That is neither a
        // native calling convention nor a useful boundary: plain calls bail
        // until they have a frameless native link.
        dynasm!(ops ; .arch aarch64 ; b =>bail);
        Ok(())
    }

    /// Shared direct-call dispatch tail used after a prepare stub returned
    /// status 0 (callee frame published in `ctx.direct_*`). Builds the callee
    /// `JitCtx` on the native stack, branches to the compiled entry, and runs
    /// the returned / bailed / threw finish helpers, landing at `done`.
    ///
    /// Both the baseline and the optimizing emitter enter compiled callees
    /// through this one tail, so the callee `JitCtx` is constructed from a
    /// single source: the isolate-boundary fields (`gc_heap`, safepoint table,
    /// collection ICs, array-index protector) propagate from the caller ctx and
    /// the per-call `direct_*` fields are copied verbatim. A second, hand-copied
    /// tail in either tier would be free to drift on which fields it initializes
    /// — the drift that left optimizing callees reading uninitialized safepoint
    /// and heap slots — so there is deliberately only this one.
    pub(crate) fn emit_direct_call_tail(
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
            ; ldr x9, [x20, THREAD_OFFSET]
            ; str x9, [sp, THREAD_OFFSET]
            ; ldr x9, [x20, NATIVE_FRAME_OFFSET]
            ; str x9, [sp, NATIVE_FRAME_OFFSET]
            ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; str x9, [sp, FRAME_INDEX_OFFSET]
            ; ldr x9, [x20, ERROR_SLOT_OFFSET]
            ; str x9, [sp, ERROR_SLOT_OFFSET]
            // Copy the prepared callee upvalue-spine base so inline upvalue ops
            // in the direct callee read its cells without the stub.
            ; ldr x9, [x20, DIRECT_UPVALUES_OFFSET]
            ; str x9, [sp, UPVALUES_PTR_OFFSET]
            // Propagate the flat register-stack pointers so the direct callee can
            // build its own self-recursive call windows inline.
            ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
            ; str x9, [sp, REG_STACK_BASE_OFFSET]
            ; ldr x9, [x20, REG_TOP_PTR_OFFSET]
            ; str x9, [sp, REG_TOP_PTR_OFFSET]
            ; ldr x9, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; str x9, [sp, SYNC_REENTRY_DEPTH_PTR_OFFSET]
            ; ldr w9, [x20, SYNC_REENTRY_LIMIT_OFFSET]
            ; str w9, [sp, SYNC_REENTRY_LIMIT_OFFSET]
            ; ldr x9, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
            ; str x9, [sp, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
            ; ldr x9, [x20, COLLECTION_METHOD_ICS_OFFSET]
            ; str x9, [sp, COLLECTION_METHOD_ICS_OFFSET]
            ; ldr w9, [x20, COLLECTION_METHOD_IC_COUNT_OFFSET]
            ; str w9, [sp, COLLECTION_METHOD_IC_COUNT_OFFSET]
            // Propagate the direct-method inline-link table base so a direct
            // callee can itself take the bridge-free method-call fast path.
            ; ldr x9, [x20, DIRECT_METHOD_INLINE_OFFSET]
            ; str x9, [sp, DIRECT_METHOD_INLINE_OFFSET]
            ; ldr x9, [x20, GC_HEAP_OFFSET]
            ; str x9, [sp, GC_HEAP_OFFSET]
            ; ldr x9, [x20, INTERRUPT_FLAG_OFFSET]
            ; str x9, [sp, INTERRUPT_FLAG_OFFSET]
            ; ldr x9, [x20, BACKEDGE_FUEL_OFFSET]
            ; str x9, [sp, BACKEDGE_FUEL_OFFSET]
            ; mov x0, sp
        );
        emit_load_u64(ops, 16, jit_push_native_activation_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x0, =>threw
            ; mov x0, sp
            ; ldr x16, [x20, DIRECT_ENTRY_OFFSET]
            ; blr x16
            ; cmp x1, STATUS_RETURNED as u32
            ; b.eq =>direct_returned
            ; cmp x1, STATUS_BAILED as u32
            ; b.eq =>direct_bailed
            ; b =>direct_threw
            ; =>direct_returned
            ; str x0, [sp, DIRECT_ENTRY_OFFSET]
            ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; mov x0, sp
        );
        emit_load_u64(ops, 16, jit_pop_native_activation_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; ldr x2, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; ldr x3, [sp, DIRECT_ENTRY_OFFSET]
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
            ; ldr w9, [sp, BAIL_PC_OFFSET]
            ; str w9, [sp, DIRECT_ENTRY_OFFSET]
            ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; mov x0, sp
        );
        emit_load_u64(ops, 16, jit_pop_native_activation_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; ldr x2, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; ldr w3, [sp, DIRECT_ENTRY_OFFSET]
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
            ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
            ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; mov x0, sp
        );
        emit_load_u64(ops, 16, jit_pop_native_activation_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; ldr x1, [sp, DIRECT_FRAME_INDEX_OFFSET]
            ; add sp, sp, JIT_CTX_STACK_SIZE
            ; mov x0, x20
        );
        emit_call_stub(ops, jit_abort_direct_call_stub as *const () as usize, threw);
        // The caller places `done` (once) after any trailing fallback code.
        dynasm!(ops ; .arch aarch64 ; b =>threw);
    }

    /// Emit the reusable baseline ABI call sequence for
    /// `leaf_no_alloc_stub2_trampoline_pair`.
    ///
    /// Inputs are the current `JitCtx` in `x20`, frame register window in
    /// `x19`, and a previously resolved nonzero `RuntimeStubId` in
    /// `stub_id_x`. The helper reads the opaque GC heap pointer from `JitCtx`,
    /// passes raw boxed receiver/key bits from the frame window, writes `dst`
    /// on `Ok`, and branches to `miss` for every non-`Ok` status.
    fn emit_leaf_no_alloc_stub2_pair_call(
        ops: &mut Assembler,
        stub_id_x: u32,
        dst: u16,
        recv: u16,
        key: Option<u16>,
        miss: DynamicLabel,
    ) -> Result<(), Unsupported> {
        dynasm!(ops
            ; .arch aarch64
            ; ldr x0, [x20, GC_HEAP_OFFSET]
            ; mov x1, X(stub_id_x)
        );
        load_reg(ops, 2, recv)?;
        if let Some(key) = key {
            load_reg(ops, 3, key)?;
        } else {
            emit_load_u64(ops, 3, VALUE_UNDEFINED);
        }
        emit_load_u64(
            ops,
            16,
            leaf_no_alloc_stub2_trampoline_pair as *const () as u64,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; and x1, x1, #0xff
            ; cbnz x1, =>miss
        );
        store_reg(ops, 0, dst)
    }

    fn emit_collection_leaf_method_guarded_call(
        ops: &mut Assembler,
        operands: impl WordOperands,
        leaf: &JitCollectionLeafMethod,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }

        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let argc = const_index(operands, 3)? as usize;
        let key = if argc == 0 {
            None
        } else {
            Some(reg(operands, 4)?)
        };
        let guard_flags_byte = view.collection_layout.guard_flags_byte;
        let object_shape_byte = view.object_shape_byte;
        let object_values_ptr_byte = view.object_values_ptr_byte;
        let native_static_fn_byte = view.native_static_fn_byte;
        let method_value_byte = leaf.method_value_byte;
        let receiver_type_tag = u32::from(leaf.receiver_type_tag);
        let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);

        load_reg(ops, 9, recv)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, receiver_type_tag
            ; b.ne =>miss
            ; ldr w14, [x13, guard_flags_byte]
            ; cbnz w14, =>miss
        );

        emit_load_u64(ops, 15, view.cage_base as u64);
        emit_load_u64(ops, 12, u64::from(leaf.proto_offset));
        dynasm!(ops
            ; .arch aarch64
            ; add x15, x15, x12
            ; ldrb w14, [x15]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x15, object_shape_byte]
        );
        emit_load_u64(ops, 12, u64::from(leaf.proto_shape));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w14, w12
            ; b.ne =>miss
            ; ldr x15, [x15, object_values_ptr_byte]
            ; cbz x15, =>miss
            ; ldr w17, [x15, method_value_byte]
        );
        emit_decompress_slot(ops, view.cage_base as u64, miss);
        dynasm!(ops
            ; .arch aarch64
            ; mov x9, x17
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, native_function_type_tag
            ; b.ne =>miss
            ; ldr x14, [x13, native_static_fn_byte]
        );
        emit_load_u64(ops, 15, leaf.builtin_fn_addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x14, x15
            ; b.ne =>miss
        );
        emit_load_u64(ops, 11, u64::from(leaf.leaf_stub_id));
        emit_leaf_no_alloc_stub2_pair_call(ops, 11, dst, recv, key, miss)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    /// Emit the shared receiver + prototype-builtin guard for an inline
    /// dense-array method. Leaves the dense-array body pointer in `x13`; any
    /// guard failure branches to `miss`. The receiver must be a pointer-tagged
    /// ordinary dense `Array` (array type tag, no exotic sidecar) and
    /// `%Array.prototype%` must still carry the original builtin at the cached
    /// shape + slot, so the resolved method can only be that builtin. The body
    /// pointer is recomputed from the rooted receiver slot at the end (the
    /// prototype guard clobbers `x13`); nothing on this path can move the heap.
    fn emit_array_dense_proto_guard(
        ops: &mut Assembler,
        recv: u16,
        am: &JitArrayMethod,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let cage_base = view.cage_base as u64;
        let array_tag = u32::from(view.ta_layout.array_type_tag);
        let exotic_byte = view.ta_layout.array_exotic_byte;
        let object_shape_byte = view.object_shape_byte;
        let object_values_ptr_byte = view.object_values_ptr_byte;
        let native_static_fn_byte = view.native_static_fn_byte;
        let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
        let method_value_byte = am.method_value_byte;

        load_reg(ops, 9, recv)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, cage_base);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, array_tag
            ; b.ne =>miss
            ; ldr x14, [x13, exotic_byte]
            ; cbnz x14, =>miss
        );

        emit_load_u64(ops, 15, cage_base);
        emit_load_u64(ops, 12, u64::from(am.proto_offset));
        dynasm!(ops
            ; .arch aarch64
            ; add x15, x15, x12
            ; ldrb w14, [x15]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x15, object_shape_byte]
        );
        emit_load_u64(ops, 12, u64::from(am.proto_shape));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w14, w12
            ; b.ne =>miss
            ; ldr x15, [x15, object_values_ptr_byte]
            ; cbz x15, =>miss
            // The value slab holds 4-byte compressed slots, so the method value is
            // a 32-bit load (the byte offset is `slot * 4` and need not be
            // 8-aligned). The method is expected to be a cell (a native function
            // object): its low-3 tag is `000` and its zero-extended offset is the
            // bare cage offset. Any non-cell (smi / immediate / function id / boxed
            // number) or the empty slot misses to the runtime method bridge.
            ; ldr w9, [x15, method_value_byte]
            ; ands w11, w9, #0x7
            ; b.ne =>miss
            ; cbz w9, =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, cage_base);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, native_function_type_tag
            ; b.ne =>miss
            ; ldr x14, [x13, native_static_fn_byte]
        );
        emit_load_u64(ops, 15, am.builtin_fn_addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x14, x15
            ; b.ne =>miss
        );

        // Recompute the dense-array body pointer into x13 (the prototype guard
        // clobbered it). The receiver tag is already verified.
        load_reg(ops, 9, recv)?;
        dynasm!(ops ; .arch aarch64 ; mov w12, w9);
        emit_load_u64(ops, 13, cage_base);
        dynasm!(ops ; .arch aarch64 ; add x13, x13, x12);
        Ok(())
    }

    /// Splice an inline `Array.prototype.pop` fast path under the shared
    /// dense-array guard. On a hit it removes and returns the last dense element
    /// with no call or allocation; on any guard miss it branches to `miss` (the
    /// caller continues to the runtime method bridge) and on a hit it branches to
    /// `done` (past the bridge). Returns `Ok(false)` (nothing emitted) when the
    /// site can't be served inline: no baked cage base, or `pop` called with
    /// arguments (only the canonical zero-arg form is modeled).
    ///
    /// GC: the only mutation is shrinking the dense `Vec` length, so the dropped
    /// slot falls outside the traced `[0, len)` range and the returned value is
    /// rooted in the destination frame slot. No write barrier or safepoint.
    fn emit_array_pop_inline(
        ops: &mut Assembler,
        operands: impl WordOperands,
        am: &JitArrayMethod,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }
        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let argc = const_index(operands, 3)? as usize;
        if argc != 0 {
            return Ok(false);
        }
        let length_byte = view.ta_layout.array_length_byte;
        let (ptr_word, len_word) = vec_layout_offsets();
        let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
        let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
        let undef = VALUE_UNDEFINED;

        emit_array_dense_proto_guard(ops, recv, am, view, miss)?;

        // pop body: require the dense invariant (Vec length == logical length);
        // an empty array returns undefined without mutating, otherwise drop and
        // return the last slot.
        let empty = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x13, arr_len_byte]
            ; ldr x11, [x13, length_byte]
            ; cmp x10, x11
            ; b.ne =>miss
            ; cbz x10, =>empty
            ; sub x10, x10, #1
            ; ldr x12, [x13, arr_ptr_byte]
            ; lsl x15, x10, #3
            ; add x12, x12, x15
            ; ldr x14, [x12]
            ; str x10, [x13, arr_len_byte]
            ; str x10, [x13, length_byte]
        );
        store_reg(ops, 14, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>empty);
        emit_load_u64(ops, 14, undef);
        store_reg(ops, 14, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    /// Splice an inline `Array.prototype.push(x)` fast path under the shared
    /// dense-array guard. The fast path serves the single-argument, has-spare-
    /// capacity case: it writes the value into the next dense slot, bumps the Vec
    /// and logical lengths, returns the new length, and marks the receiver's card
    /// when the value is a heap pointer (old→young barrier, mirroring the inline
    /// dense `StoreElement`). Growth (length == capacity), multi-argument pushes,
    /// and any guard miss branch to `miss`, where the runtime method bridge owns
    /// the spec-correct reallocation and rooting. A hit branches to `done`.
    ///
    /// Returns `Ok(false)` (nothing emitted) when the site can't be served
    /// inline: no baked cage base, or `push` with other than one argument.
    fn emit_array_push_inline(
        ops: &mut Assembler,
        operands: impl WordOperands,
        am: &JitArrayMethod,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }
        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let argc = const_index(operands, 3)? as usize;
        if argc != 1 {
            return Ok(false);
        }
        let value = reg(operands, 4)?;
        let length_byte = view.ta_layout.array_length_byte;
        let (ptr_word, len_word) = vec_layout_offsets();
        let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
        let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
        // The third Vec machine word is the capacity (the std `Vec` is three
        // words: data pointer, capacity, length).
        let cap_word = 24 - ptr_word - len_word;
        let arr_cap_byte = view.ta_layout.array_elements_byte + cap_word;

        emit_array_dense_proto_guard(ops, recv, am, view, miss)?;

        // push body: require the dense invariant and spare capacity; bound the
        // new length to the int32 fast path; an indexed accessor/proto hazard
        // (protector tripped) misses so the bridge applies the spec semantics.
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x13, arr_len_byte]     // veclen
            ; ldr x11, [x13, length_byte]      // logical length
            ; cmp x10, x11
            ; b.ne =>miss
            ; ldr x14, [x13, arr_cap_byte]     // capacity
            ; cmp x10, x14
            ; b.hs =>miss                      // no spare capacity → bridge grows
            ; add x11, x10, #1                 // new length
        );
        emit_load_u64(ops, 14, i32::MAX as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x11, x14
            ; b.hi =>miss                      // new length out of int32 fast path
            ; ldr x14, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
            ; ldrb w14, [x14]
            ; cbnz w14, =>miss                 // indexed proto/accessor hazard
            ; ldr x12, [x13, arr_ptr_byte]     // elements Vec data pointer
            ; lsl x15, x10, #3
            ; add x12, x12, x15                // &elements[veclen]
        );
        load_reg(ops, 9, value)?;
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [x12]                    // store value into the new slot
            ; str x11, [x13, arr_len_byte]     // Vec length++
            ; str x11, [x13, length_byte]      // logical length++
            ; movz x14, NUMBER_TAG_HI16, lsl #48
            ; orr x14, x11, x14                // box new length as int32
        );
        store_reg(ops, 14, dst)?;
        // Old→young card barrier when the stored value is a heap pointer,
        // matching the inline dense `StoreElement`. Primitives skip it.
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>done
            ; mov x0, x20
            ; movz x1, recv as u32
            ; movz x2, value as u32
        );
        emit_call_stub(ops, jit_write_barrier_stub as *const () as usize, threw);
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    fn emit_live_collection_leaf_method_guarded_call(
        ops: &mut Assembler,
        operands: impl WordOperands,
        site: u64,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }

        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let argc = const_index(operands, 3)? as usize;
        let key = if argc == 0 {
            None
        } else {
            Some(reg(operands, 4)?)
        };
        let guard_flags_byte = view.collection_layout.guard_flags_byte;
        let object_shape_byte = view.object_shape_byte;
        let object_values_ptr_byte = view.object_values_ptr_byte;
        let native_static_fn_byte = view.native_static_fn_byte;
        let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);

        dynasm!(ops
            ; .arch aarch64
            ; ldr x17, [x20, COLLECTION_METHOD_ICS_OFFSET]
            ; cbz x17, =>miss
            ; ldr w10, [x20, COLLECTION_METHOD_IC_COUNT_OFFSET]
        );
        emit_load_u64(ops, 11, site);
        dynasm!(ops ; .arch aarch64 ; cmp x11, x10 ; b.hs =>miss);
        emit_load_u64(
            ops,
            12,
            site.saturating_mul(u64::from(COLLECTION_METHOD_IC_SLOT_SIZE)),
        );
        dynasm!(ops
            ; .arch aarch64
            ; add x17, x17, x12
            ; ldrb w10, [x17, COLLECTION_METHOD_IC_STATE_OFFSET]
            ; cmp w10, JIT_COLLECTION_METHOD_IC_COLLECTION as u32
            ; b.ne =>miss
            ; ldr w11, [x17, COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET]
        );
        emit_load_u64(ops, 12, u64::from(JIT_COLLECTION_METHOD_IC_NO_STUB));
        dynasm!(ops ; .arch aarch64 ; cmp x11, x12 ; b.eq =>miss);

        load_reg(ops, 9, recv)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; ldrb w15, [x17, COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET]
            ; cmp w14, w15
            ; b.ne =>miss
            ; ldr w14, [x13, guard_flags_byte]
            ; cbnz w14, =>miss
        );

        emit_load_u64(ops, 15, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_OFFSET]
            ; add x15, x15, x12
            ; ldrb w14, [x15]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x15, object_shape_byte]
            ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET]
            ; cmp w14, w12
            ; b.ne =>miss
            ; ldr x15, [x15, object_values_ptr_byte]
            ; cbz x15, =>miss
            ; ldr w12, [x17, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET]
            ; ldr x9, [x15, x12]
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, native_function_type_tag
            ; b.ne =>miss
            ; ldr x14, [x13, native_static_fn_byte]
            ; ldr x15, [x17, COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET]
            ; cmp x14, x15
            ; b.ne =>miss
            ; ldr w11, [x17, COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET]
        );
        emit_leaf_no_alloc_stub2_pair_call(ops, 11, dst, recv, key, miss)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    fn emit_collection_alloc_method_guarded_call(
        ops: &mut Assembler,
        operands: impl WordOperands,
        alloc: &JitCollectionAllocMethod,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 || alloc.value_arg_count != 3 {
            return Ok(false);
        }
        let Some(stub_addr) =
            alloc_value_stub_by_id(alloc.alloc_stub_id).and_then(|stub| stub.entry_addr())
        else {
            return Ok(false);
        };

        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let argc = const_index(operands, 3)? as usize;
        let arg0 = if argc == 0 {
            None
        } else {
            Some(reg(operands, 4)?)
        };
        let arg1 = if argc <= 1 || alloc.alloc_stub_id == STUB_COLLECTION_SET_ADD_ALLOC.id {
            None
        } else {
            Some(reg(operands, 5)?)
        };
        let guard_flags_byte = view.collection_layout.guard_flags_byte;
        let object_shape_byte = view.object_shape_byte;
        let object_values_ptr_byte = view.object_values_ptr_byte;
        let native_static_fn_byte = view.native_static_fn_byte;
        let method_value_byte = alloc.method_value_byte;
        let receiver_type_tag = u32::from(alloc.receiver_type_tag);
        let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
        let undefined_bits = VALUE_UNDEFINED;

        load_reg(ops, 9, recv)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, receiver_type_tag
            ; b.ne =>miss
            ; ldr w14, [x13, guard_flags_byte]
            ; cbnz w14, =>miss
        );

        emit_load_u64(ops, 15, view.cage_base as u64);
        emit_load_u64(ops, 12, u64::from(alloc.proto_offset));
        dynasm!(ops
            ; .arch aarch64
            ; add x15, x15, x12
            ; ldrb w14, [x15]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x15, object_shape_byte]
        );
        emit_load_u64(ops, 12, u64::from(alloc.proto_shape));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w14, w12
            ; b.ne =>miss
            ; ldr x15, [x15, object_values_ptr_byte]
            ; cbz x15, =>miss
            // The value slab holds 4-byte compressed slots, so the method value is
            // a 32-bit load (the byte offset is `slot * 4` and need not be
            // 8-aligned). The method is expected to be a cell (a native function
            // object): its low-3 tag is `000` and its zero-extended offset is the
            // bare cage offset. Any non-cell (smi / immediate / function id / boxed
            // number) or the empty slot misses to the runtime method bridge.
            ; ldr w9, [x15, method_value_byte]
            ; ands w11, w9, #0x7
            ; b.ne =>miss
            ; cbz w9, =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, native_function_type_tag
            ; b.ne =>miss
            ; ldr x14, [x13, native_static_fn_byte]
        );
        emit_load_u64(ops, 15, alloc.builtin_fn_addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x14, x15
            ; b.ne =>miss

            ; sub sp, sp, ALLOC_CTX_STACK_SIZE
            ; ldr x9, [x20, THREAD_OFFSET]
            ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
            ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
            ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
            ; movz w9, alloc.safepoint_id
            ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
            ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
            ; movz w9, #0
            ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
            ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
            ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]

            ; mov x0, sp
        );
        emit_load_u64(ops, 1, u64::from(alloc.safepoint_id));
        load_reg(ops, 2, recv)?;
        if let Some(arg0) = arg0 {
            load_reg(ops, 3, arg0)?;
        } else {
            emit_load_u64(ops, 3, undefined_bits);
        }
        if let Some(arg1) = arg1 {
            load_reg(ops, 4, arg1)?;
        } else {
            emit_load_u64(ops, 4, undefined_bits);
        }
        emit_load_u64(ops, 16, stub_addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; and x1, x1, #0xff
            ; mov x5, x1
            ; add sp, sp, ALLOC_CTX_STACK_SIZE
            ; cbnz x5, =>miss
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    fn emit_live_collection_alloc_method_guarded_call(
        ops: &mut Assembler,
        operands: impl WordOperands,
        site: u64,
        safepoint: SafepointId,
        view: &JitCompileSnapshot,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }

        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let argc = const_index(operands, 3)? as usize;
        let arg0 = if argc == 0 {
            None
        } else {
            Some(reg(operands, 4)?)
        };
        let arg1 = if argc <= 1 {
            None
        } else {
            Some(reg(operands, 5)?)
        };
        let guard_flags_byte = view.collection_layout.guard_flags_byte;
        let object_shape_byte = view.object_shape_byte;
        let object_values_ptr_byte = view.object_values_ptr_byte;
        let native_static_fn_byte = view.native_static_fn_byte;
        let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
        let undefined_bits = VALUE_UNDEFINED;

        dynasm!(ops
            ; .arch aarch64
            ; ldr x17, [x20, COLLECTION_METHOD_ICS_OFFSET]
            ; cbz x17, =>miss
            ; ldr w10, [x20, COLLECTION_METHOD_IC_COUNT_OFFSET]
        );
        emit_load_u64(ops, 11, site);
        dynasm!(ops ; .arch aarch64 ; cmp x11, x10 ; b.hs =>miss);
        emit_load_u64(
            ops,
            12,
            site.saturating_mul(u64::from(COLLECTION_METHOD_IC_SLOT_SIZE)),
        );
        dynasm!(ops
            ; .arch aarch64
            ; add x17, x17, x12
            ; ldrb w10, [x17, COLLECTION_METHOD_IC_STATE_OFFSET]
            ; cmp w10, JIT_COLLECTION_METHOD_IC_COLLECTION as u32
            ; b.ne =>miss
            ; ldr w11, [x17, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET]
        );
        emit_load_u64(ops, 12, u64::from(JIT_COLLECTION_METHOD_IC_NO_STUB));
        dynasm!(ops ; .arch aarch64 ; cmp x11, x12 ; b.eq =>miss);

        load_reg(ops, 9, recv)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; ldrb w15, [x17, COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET]
            ; cmp w14, w15
            ; b.ne =>miss
            ; ldr w14, [x13, guard_flags_byte]
            ; cbnz w14, =>miss
        );

        emit_load_u64(ops, 15, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_OFFSET]
            ; add x15, x15, x12
            ; ldrb w14, [x15]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x15, object_shape_byte]
            ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET]
            ; cmp w14, w12
            ; b.ne =>miss
            ; ldr x15, [x15, object_values_ptr_byte]
            ; cbz x15, =>miss
            ; ldr w12, [x17, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET]
            ; ldr x9, [x15, x12]
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
        emit_load_u64(ops, 13, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12
            ; ldrb w14, [x13]
            ; cmp w14, native_function_type_tag
            ; b.ne =>miss
            ; ldr x14, [x13, native_static_fn_byte]
            ; ldr x15, [x17, COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET]
            ; cmp x14, x15
            ; b.ne =>miss
            ; ldr w1, [x17, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET]

            ; sub sp, sp, ALLOC_CTX_STACK_SIZE
            ; ldr x9, [x20, THREAD_OFFSET]
            ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
            ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
            ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
            ; movz w9, safepoint
            ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
            ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
            ; movz w9, #0
            ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
            ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
            ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]

            ; mov x0, sp
        );
        emit_load_u64(ops, 2, u64::from(safepoint));
        load_reg(ops, 3, recv)?;
        if let Some(arg0) = arg0 {
            load_reg(ops, 4, arg0)?;
        } else {
            emit_load_u64(ops, 4, undefined_bits);
        }
        if let Some(arg1) = arg1 {
            emit_load_u64(ops, 5, undefined_bits);
            let set_add = ops.new_dynamic_label();
            emit_load_u64(ops, 9, u64::from(STUB_COLLECTION_SET_ADD_ALLOC.id));
            dynasm!(ops ; .arch aarch64 ; cmp x1, x9 ; b.eq =>set_add);
            load_reg(ops, 5, arg1)?;
            dynasm!(ops ; .arch aarch64 ; =>set_add);
        } else {
            emit_load_u64(ops, 5, undefined_bits);
        }
        emit_load_u64(
            ops,
            16,
            alloc_value_stub_trampoline_pair as *const () as u64,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; and x1, x1, #0xff
            ; mov x5, x1
            ; add sp, sp, ALLOC_CTX_STACK_SIZE
            ; cbnz x5, =>miss
        );
        store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    /// Emit a direct `CallMethodValue`: resolve the method through the call
    /// site's monomorphic IC and direct-branch to its compiled entry, exactly
    /// like [`emit_call`]; on an ineligible resolution fall back to the in-place
    /// full method-call stub (not a bail) so cold / native / polymorphic methods
    /// keep running compiled.
    #[allow(clippy::too_many_arguments)]
    fn emit_method_call(
        ops: &mut Assembler,
        operands: impl WordOperands,
        site: u64,
        leaf: Option<&JitCollectionLeafMethod>,
        alloc: Option<&JitCollectionAllocMethod>,
        view: Option<&JitCompileSnapshot>,
        live_alloc_safepoint: Option<SafepointId>,
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let dst = reg(operands, 0)?;
        let recv = reg(operands, 1)?;
        let name = const_index(operands, 2)?;
        let argc = const_index(operands, 3)? as usize;
        if argc > MAX_METHOD_ARGS {
            return Err(Unsupported::ArgCount(argc));
        }
        // The argument register indices, packed one per 16-bit lane, are handed
        // to every method-call stub in a single register.
        let mut method_arg_regs: Vec<u16> = Vec::with_capacity(argc);
        for slot in 0..argc {
            method_arg_regs.push(reg(operands, 4 + slot)?);
        }
        let packed_args = pack_method_arg_regs(&method_arg_regs);

        let fallback = ops.new_dynamic_label();
        let after_leaf = ops.new_dynamic_label();
        let after_alloc = ops.new_dynamic_label();
        let after_live_leaf = ops.new_dynamic_label();
        let after_live_alloc = ops.new_dynamic_label();
        let after_direct_inline = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();

        if let Some(view) = view
            && emit_direct_method_inline(
                ops,
                operands,
                site,
                view,
                after_direct_inline,
                done,
                threw,
            )?
        {
            dynasm!(ops ; .arch aarch64 ; =>after_direct_inline);
        }

        if let (Some(leaf), Some(view)) = (leaf, view)
            && emit_collection_leaf_method_guarded_call(
                ops, operands, leaf, view, after_leaf, done,
            )?
        {
            dynasm!(ops ; .arch aarch64 ; =>after_leaf);
        }
        if let (Some(alloc), Some(view)) = (alloc, view)
            && emit_collection_alloc_method_guarded_call(
                ops,
                operands,
                alloc,
                view,
                after_alloc,
                done,
            )?
        {
            dynasm!(ops ; .arch aarch64 ; =>after_alloc);
        }
        if let Some(view) = view
            && emit_live_collection_leaf_method_guarded_call(
                ops,
                operands,
                site,
                view,
                after_live_leaf,
                done,
            )?
        {
            dynasm!(ops ; .arch aarch64 ; =>after_live_leaf);
        }
        if let (Some(view), Some(safepoint)) = (view, live_alloc_safepoint)
            && emit_live_collection_alloc_method_guarded_call(
                ops,
                operands,
                site,
                safepoint,
                view,
                after_live_alloc,
                done,
            )?
        {
            dynasm!(ops ; .arch aarch64 ; =>after_live_alloc);
        }

        dynasm!(
            ops
            ; .arch aarch64
            ; mov x0, x20
            ; movz x1, dst as u32
            ; movz x2, recv as u32
        );
        emit_load_u64(ops, 3, site);
        dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
        emit_load_u64(ops, 5, packed_args);
        emit_load_u64(
            ops,
            16,
            jit_call_collection_method_ic_stub as *const () as u64,
        );
        dynasm!(
            ops
            ; .arch aarch64
            ; blr x16
            ; cmp x0, #1
            ; b.eq =>threw
            ; cbz x0, =>done
        );

        if leaf.is_some() || alloc.is_some() {
            dynasm!(ops ; .arch aarch64 ; b =>fallback);
        }

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
        emit_load_u64(ops, 5, packed_args);
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

        // Ineligible resolution bails to normal dispatch. Native code never
        // re-enters one interpreter opcode through a bespoke method bridge.
        dynasm!(ops ; .arch aarch64 ; =>fallback ; b =>bail ; =>done);
        Ok(())
    }

    fn emit_cmp(
        ops: &mut Assembler,
        operands: impl WordOperands,
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
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, x9, x15
            ; cmp x14, x15
            ; b.ne =>float_path
            ; and x14, x10, x15
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
        if matches!(cmp, Cmp::Eq | Cmp::Ne) {
            let lhs_non_number = ops.new_dynamic_label();
            let number_path = ops.new_dynamic_label();
            let raw_identity = ops.new_dynamic_label();
            let strict_false = ops.new_dynamic_label();
            // Strict equality on non-number immediates (null / undefined /
            // boolean / hole / function id) decides by raw bit identity. Any
            // heap cell (object, string, BigInt, …) bails to the interpreter,
            // which owns object identity and string / BigInt content equality.
            dynasm!(ops
                ; .arch aarch64
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; tst x9, x11
                ; b.eq =>lhs_non_number
                ; tst x10, x11
                ; b.eq =>strict_false        // number !== non-number
                ; b =>number_path
                ; =>lhs_non_number
                ; tst x10, x11
                ; b.ne =>strict_false        // non-number !== number
                ; orr x11, x11, #value_tag::OTHER_TAG   // NOT_CELL_MASK
                ; tst x9, x11
                ; b.eq =>bail                // lhs heap cell → interpreter
                ; tst x10, x11
                ; b.eq =>bail                // rhs heap cell → interpreter
                ; =>raw_identity
                ; cmp x9, x10
            );
            match cmp {
                Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
                Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
                _ => unreachable!(),
            }
            let false_value = match cmp {
                Cmp::Eq => 0,
                Cmp::Ne => 1,
                _ => unreachable!(),
            };
            dynasm!(ops
                ; .arch aarch64
                ; b =>have_bool
                ; =>strict_false
                ; movz w13, false_value
                ; b =>have_bool
                ; =>number_path
            );
        }
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
        box_bool!(ops, 13, 12);
        store_reg(ops, 13, dst)?;
        Ok(())
    }

    /// Inline abstract equality for numbers and the null/undefined equivalence
    /// class. String/object/coercive cases bail before observable work to the
    /// exact interpreter instruction.
    fn emit_loose_cmp(
        ops: &mut Assembler,
        operands: impl WordOperands,
        negate: bool,
        bail: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let (dst, lhs, rhs) = reg3(operands)?;
        load_reg(ops, 9, lhs)?;
        load_reg(ops, 10, rhs)?;
        let lhs_nullish = ops.new_dynamic_label();
        let rhs_nullish = ops.new_dynamic_label();
        let have_bool = ops.new_dynamic_label();
        emit_load_u64(ops, 11, VALUE_NULL);
        dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>lhs_nullish);
        emit_load_u64(ops, 11, VALUE_UNDEFINED);
        dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>lhs_nullish);
        emit_load_u64(ops, 11, VALUE_NULL);
        dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq =>rhs_nullish);
        emit_load_u64(ops, 11, VALUE_UNDEFINED);
        dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq =>rhs_nullish);

        emit_num_to_double(ops, 9, 0, bail);
        emit_num_to_double(ops, 10, 1, bail);
        dynasm!(ops ; .arch aarch64 ; fcmp d0, d1 ; cset w13, eq ; b =>have_bool);

        dynasm!(ops ; .arch aarch64 ; =>lhs_nullish);
        emit_load_u64(ops, 11, VALUE_NULL);
        dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq >both_nullish);
        emit_load_u64(ops, 11, VALUE_UNDEFINED);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x10, x11
            ; cset w13, eq
            ; b =>have_bool
            ; both_nullish:
            ; movz w13, #1
            ; b =>have_bool
            ; =>rhs_nullish
            ; movz w13, #0
            ; =>have_bool
        );
        if negate {
            dynasm!(ops ; .arch aarch64 ; eor w13, w13, #1);
        }
        box_bool!(ops, 13, 12);
        store_reg(ops, 13, dst)
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
    /// `int32` payloads sign-convert (`scvtf`); a boxed double has the encode
    /// offset subtracted before `fmov`; a cell or non-number immediate (no
    /// `NUMBER_TAG` bit) bails to the interpreter. Uses scratch GPRs x14/x15.
    fn emit_num_to_double(ops: &mut Assembler, src_x: u32, dst_d: u32, bail: DynamicLabel) {
        let is_non_int = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, X(src_x), x15
            ; cmp x14, x15
            ; b.ne =>is_non_int
            ; scvtf D(dst_d), W(src_x)          // int32: signed 32-bit → f64
            ; b =>done
            ; =>is_non_int
            // A boxed double carries at least one NUMBER_TAG bit; a cell or
            // tagged immediate carries none and bails for exact coercion.
            ; tst X(src_x), x15
            ; b.eq =>bail
            ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
            ; sub x14, X(src_x), x14
            ; fmov D(dst_d), x14
            ; =>done
        );
    }

    /// Box the f64 in register `src_d` into x-register `dst_x` as a `Value`.
    ///
    /// A NaN result is first canonicalised to the single quiet-NaN pattern;
    /// then the encode offset is added so the bits land in the number space.
    /// Uses scratch GPR x14 in addition to `dst_x`.
    fn emit_box_double(ops: &mut Assembler, src_d: u32, dst_x: u32) {
        let ready = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; fmov X(dst_x), D(src_d)
            ; fcmp D(src_d), D(src_d)
            ; b.vc =>ready                       // ordered (not NaN) → keep bits
            ; movz X(dst_x), CANONICAL_NAN_HI16, lsl #48
            ; =>ready
            ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
            ; add X(dst_x), X(dst_x), x14        // purify into the number space
        );
    }

    /// Decompress a 4-byte object property slot (already zero-extended into
    /// `x9`) into a full tagged `Value`, in place in `x9`.
    ///
    /// A small-int, cell-ref, immediate, or function-id slot decodes inline; a
    /// `TAG_BOXED` slot (a heap-boxed double / wide int) branches to
    /// `boxed_bail`, where the interpreter reads the box. Fixed registers (the
    /// `#imm` forms below require literal registers): `x9` is the slot in/out,
    /// `x10` is scratch.
    fn emit_decompress_slot(ops: &mut Assembler, cage_base: u64, boxed_bail: DynamicLabel) {
        use otter_vm::value::compressed as cslot;
        // The literal slot tags below are the frozen `compressed` layout.
        debug_assert_eq!(cslot::TAG_MASK, 0b111);
        debug_assert_eq!(cslot::TAG_IMMEDIATE, 0b100);
        debug_assert_eq!(cslot::TAG_FUNCTION_ID, 0b110);
        debug_assert_eq!(
            (
                cslot::IMM_NULL,
                cslot::IMM_TRUE,
                cslot::IMM_FALSE,
                cslot::IMM_HOLE
            ),
            (1, 2, 3, 4)
        );
        let l_smi = ops.new_dynamic_label();
        let l_cell = ops.new_dynamic_label();
        let l_imm = ops.new_dynamic_label();
        let l_fid = ops.new_dynamic_label();
        let l_undef = ops.new_dynamic_label();
        let l_null = ops.new_dynamic_label();
        let l_true = ops.new_dynamic_label();
        let l_false = ops.new_dynamic_label();
        let l_hole = ops.new_dynamic_label();
        let l_done = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; tbnz w9, #0, =>l_smi                      // bit0 set → small int
            ; and w10, w9, #0x7                         // low-3-bit slot tag
            ; cbz w10, =>l_cell                         // 000 → cell ref
            ; cmp w10, #0x4                             // 100 → immediate
            ; b.eq =>l_imm
            ; cmp w10, #0x6                             // 110 → function id
            ; b.eq =>l_fid
            ; b =>boxed_bail                            // 010 → boxed number
            ; =>l_cell
            // A cell ref widens to the canonical heap-cell `Value` bits:
            // `cage_base | offset` (`Value::from_cell_offset`). A bare offset
            // would still dereference (consumers rebuild the address from the
            // low 32 bits) but would never bit-compare equal to a canonically
            // boxed handle of the same object, breaking strict/loose equality
            // on any value that flowed through a compiled slot load. The empty
            // slot (0) decodes to `undefined`.
            ; cbz x9, =>l_undef
        );
        emit_load_u64(ops, 10, cage_base);
        dynasm!(ops
            ; .arch aarch64
            ; orr x9, x9, x10
            ; b =>l_done
            ; =>l_smi
            ; asr w9, w9, #1                            // int32 = (i31 << 1 | 1) >> 1
            ; mov w9, w9                                // zero-extend the payload
            ; movz x10, NUMBER_TAG_HI16, lsl #48
            ; orr x9, x9, x10
            ; b =>l_done
            ; =>l_fid
            ; lsr w9, w9, #3                            // function id
            ; lsl x9, x9, #16
            ; movz x10, FUNCTION_ID_TAG as u32          // 0x22, fits a single movz
            ; orr x9, x9, x10
            ; b =>l_done
            ; =>l_imm
            ; lsr w10, w9, #3                           // immediate kind
            ; cmp w10, #1                               // IMM_NULL
            ; b.eq =>l_null
            ; cmp w10, #2                               // IMM_TRUE
            ; b.eq =>l_true
            ; cmp w10, #3                               // IMM_FALSE
            ; b.eq =>l_false
            ; cmp w10, #4                               // IMM_HOLE
            ; b.eq =>l_hole
            ; =>l_undef
            ; movz x9, VALUE_UNDEFINED as u32
            ; b =>l_done
            ; =>l_null
            ; movz x9, VALUE_NULL as u32
            ; b =>l_done
            ; =>l_true
            ; movz x9, VALUE_TRUE as u32
            ; b =>l_done
            ; =>l_false
            ; movz x9, VALUE_FALSE as u32
            ; b =>l_done
            ; =>l_hole
            ; movz x9, VALUE_HOLE as u32
            ; =>l_done
        );
    }

    /// Compress the tagged `Value` in `X(value)` into a 4-byte object slot in
    /// `W(out)`. Handles the barrier-free, non-allocating cases: a small int in
    /// `[-2^30, 2^30)` and the `undefined` / `null` / boolean / hole immediates.
    /// A wide int, double, function id, or heap cell branches to `bail` (the
    /// interpreter re-runs the store — a boxed number allocates, a cell needs the
    /// write barrier). The caller has already excluded cells. Clobbers `X(sc)`.
    /// Fixed registers (the `#imm` forms require literal registers): `x9` is the
    /// value in, `w10` the compressed slot out, `x11` scratch.
    fn emit_compress_slot_or_bail(ops: &mut Assembler, bail: DynamicLabel) {
        use otter_vm::value::compressed as cslot;
        // The literal compressed-immediate words below are `(kind << 3) | 0b100`.
        debug_assert_eq!(cslot::TAG_IMMEDIATE, 0b100);
        debug_assert_eq!(cslot::IMM_UNDEFINED, 0);
        let not_int = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        let imm_undef = ops.new_dynamic_label();
        let imm_null = ops.new_dynamic_label();
        let imm_true = ops.new_dynamic_label();
        let imm_false = ops.new_dynamic_label();
        let imm_hole = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; and x10, x9, x11
            ; cmp x10, x11
            ; b.ne =>not_int                            // not an int32
            // int32: keep only a small int in [-2^30, 2^30); wider ints box.
            ; movz w11, #0x4000, lsl #16                // 2^30
            ; add w10, w9, w11
            ; tbnz w10, #31, =>bail                     // out of small-int range
            ; lsl w10, w9, #1
            ; orr w10, w10, #1                          // (i << 1) | 1
            ; b =>done
            ; =>not_int
            ; cmp x9, #(VALUE_UNDEFINED as u32)
            ; b.eq =>imm_undef
            ; cmp x9, #(VALUE_NULL as u32)
            ; b.eq =>imm_null
            ; cmp x9, #(VALUE_TRUE as u32)
            ; b.eq =>imm_true
            ; cmp x9, #(VALUE_FALSE as u32)
            ; b.eq =>imm_false
            ; cmp x9, #(VALUE_HOLE as u32)
            ; b.eq =>imm_hole
            ; b =>bail                                  // double / function id → interpreter
            ; =>imm_undef
            ; movz w10, #0x4                            // (0 << 3) | 0b100
            ; b =>done
            ; =>imm_null
            ; movz w10, #0xc                            // (1 << 3) | 0b100
            ; b =>done
            ; =>imm_true
            ; movz w10, #0x14                           // (2 << 3) | 0b100
            ; b =>done
            ; =>imm_false
            ; movz w10, #0x1c                           // (3 << 3) | 0b100
            ; b =>done
            ; =>imm_hole
            ; movz w10, #0x24                           // (4 << 3) | 0b100
            ; =>done
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
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; orr x15, x15, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x15
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
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, x12, x15
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

    /// Typed-array store guard chain: prelude + `Float64Array`/`Int32Array`
    /// backing dispatch.
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

    /// Inline dense `Array` element store for the narrow non-observable case:
    /// default prototype, no exotic sidecar, intact array-index accessor
    /// protector, int32 index inside both logical `length` and the dense
    /// elements vector. Misses route to the existing typed-array/runtime path.
    #[allow(clippy::too_many_arguments)]
    fn emit_array_store(
        ops: &mut Assembler,
        layout: &JitTypedArrayLayout,
        cage_base: usize,
        recv_off: u32,
        idx_off: u32,
        src_off: u32,
        el_miss: DynamicLabel,
        el_done: DynamicLabel,
        threw: DynamicLabel,
        recv_reg: u16,
        src_reg: u16,
    ) {
        let array_tag = u32::from(layout.array_type_tag);
        let (ptr_word, len_word) = vec_layout_offsets();
        let arr_ptr_byte = layout.array_elements_byte + ptr_word;
        let arr_len_byte = layout.array_elements_byte + len_word;
        let length_byte = layout.array_length_byte;
        let exotic_byte = layout.array_exotic_byte;

        emit_recv_decompress(ops, cage_base, recv_off, el_miss);
        emit_idx_int32(ops, idx_off, el_miss);
        dynasm!(ops
            ; .arch aarch64
            ; cmp w10, array_tag
            ; b.ne =>el_miss
            ; ldr x14, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
            ; ldrb w14, [x14]
            ; cbnz w14, =>el_miss              // indexed proto/accessor hazard
            ; ldr x14, [x9, exotic_byte]
            ; cbnz x14, =>el_miss              // custom proto/accessor/flags/source
            ; ldr x17, [x9, arr_len_byte]      // elements Vec length
            ; cmp x12, x17
            ; b.hs =>el_miss
            ; ldr x16, [x9, length_byte]       // logical length
            ; cmp x12, x16
            ; b.hs =>el_miss                   // would need length update
            ; ldr x13, [x9, arr_ptr_byte]      // elements Vec data pointer
            ; lsl x14, x12, #3
            ; add x14, x13, x14                // element address
            ; ldr x9, [x19, src_off]
            ; str x9, [x14]
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>el_done                   // primitive value, no barrier
            ; mov x0, x20
            ; movz x1, recv_reg as u32
            ; movz x2, src_reg as u32
        );
        emit_call_stub(ops, jit_write_barrier_stub as *const () as usize, threw);
        dynasm!(ops ; .arch aarch64 ; b =>el_done);
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
        let hole_bits = VALUE_HOLE;
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
        box_int32!(ops, 13, 15);
        dynasm!(ops
            ; .arch aarch64
            ; str x13, [x19, dst_off]
            ; b =>el_done
        );
    }

    /// Target canonical instruction PC of a relative branch.
    fn branch_target(
        code_block: &otter_vm::CodeBlock,
        instr: &otter_vm::JitInstructionMetadata,
        rel: i32,
    ) -> i64 {
        i64::from(instr.instruction_pc(code_block)) + 1 + i64::from(rel)
    }

    trait WordOperands: Copy {
        fn get(self, index: usize) -> Option<Operand>;
    }

    impl WordOperands for otter_vm::OperandView<'_> {
        fn get(self, index: usize) -> Option<Operand> {
            self.get(index)
        }
    }

    impl WordOperands for &[Operand] {
        fn get(self, index: usize) -> Option<Operand> {
            <[Operand]>::get(self, index).copied()
        }
    }

    impl<const N: usize> WordOperands for &[Operand; N] {
        fn get(self, index: usize) -> Option<Operand> {
            self.as_slice().get(index).copied()
        }
    }

    fn reg(operands: impl WordOperands, i: usize) -> Result<u16, Unsupported> {
        match operands.get(i) {
            Some(Operand::Register(r)) => Ok(r),
            _ => Err(Unsupported::OperandShape("expected register")),
        }
    }

    fn imm32(operands: impl WordOperands, i: usize) -> Result<i32, Unsupported> {
        match operands.get(i) {
            Some(Operand::Imm32(v)) => Ok(v),
            _ => Err(Unsupported::OperandShape("expected imm32")),
        }
    }

    /// A local index encoded as an inline immediate (`LoadLocal`/`StoreLocal`).
    fn local_index(operands: impl WordOperands, i: usize) -> Result<u16, Unsupported> {
        u16::try_from(imm32(operands, i)?).map_err(|_| Unsupported::OperandShape("local index"))
    }

    /// A constant-pool index operand (`MakeFunction` body id, `Call` argc).
    fn const_index(operands: impl WordOperands, i: usize) -> Result<u32, Unsupported> {
        match operands.get(i) {
            Some(Operand::ConstIndex(n)) => Ok(n),
            _ => Err(Unsupported::OperandShape("expected const index")),
        }
    }

    fn reg3(operands: impl WordOperands) -> Result<(u16, u16, u16), Unsupported> {
        Ok((reg(operands, 0)?, reg(operands, 1)?, reg(operands, 2)?))
    }
}

/// Compile a function view to baseline arm64 code, or report why not.
#[cfg(target_arch = "aarch64")]
pub fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
    arm64::compile(view)
}

/// Non-arm64 stub: the emitter is arm64-only for now.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
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

    use super::{
        JitCtx, JitEntry, JitRet, STATUS_RETURNED, VALUE_FALSE, VALUE_NULL, VALUE_TRUE,
        VALUE_UNDEFINED, compile, value_tag,
    };
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, JitFunctionCode, jit::JitTestInstruction};

    const STRIDE: u32 = 4;

    enum Exit {
        Returned(u64),
        Bailed,
    }

    fn box_i32(v: i32) -> u64 {
        value_tag::NUMBER_TAG | u64::from(v as u32)
    }
    fn unbox_i32(bits: u64) -> i32 {
        bits as u32 as i32
    }

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(
                    *op,
                    idx as u32,
                    idx as u32 * STRIDE,
                    STRIDE,
                    operands.clone(),
                )
            })
            .collect();
        let mut view = JitCompileSnapshot::without_feedback(0, 1, 8, instructions);
        view.object_shape_byte = 8;
        view.object_values_ptr_byte = 16;
        view.object_inline_values_byte = 80;
        view.object_slab_len_byte = 88;
        view.object_inline_slot_cap = 2;
        view.jit_proto_byte = 12;
        view.heap_number_type_tag = 0x30;
        view.heap_number_bits_byte = 8;
        view.closure_fid_byte = 8;
        view.closure_upvalues_ptr_byte = 16;
        view
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

    #[test]
    fn frameless_entry_gate_accepts_only_window_owned_bodies() {
        let window_only = view(&[
            (Op::LoadThis, vec![Operand::Register(0)]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&window_only)
                .expect("window-only body compiles")
                .frameless_entry_safe()
        );

        let frame_reentry = view(&[
            (
                Op::LooseEqual,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        assert!(
            !compile(&frame_reentry)
                .expect("runtime-operation body compiles")
                .frameless_entry_safe()
        );
    }

    #[test]
    fn loose_equality_inlines_numeric_and_nullish_cases() {
        let nullish = view(&[
            (
                Op::LooseEqual,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [VALUE_NULL, VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0];
        match run(&nullish, &mut regs) {
            Exit::Returned(bits) => assert_eq!(bits, VALUE_TRUE),
            Exit::Bailed => panic!("nullish loose equality bailed"),
        }

        let numeric = view(&[
            (
                Op::LooseNotEqual,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(1), box_i32(2), 0, 0, 0, 0, 0, 0];
        match run(&numeric, &mut regs) {
            Exit::Returned(bits) => assert_eq!(bits, VALUE_TRUE),
            Exit::Bailed => panic!("numeric loose inequality bailed"),
        }
    }

    // CodeBlock branch encoding: target instruction = current + 1 + rel.
    fn rel(from: usize, to: usize) -> i32 {
        to as i32 - from as i32 - 1
    }

    fn run(view: &JitCompileSnapshot, regs: &mut [u64]) -> Exit {
        let code = compile(view).expect("compiles");
        let mut error = None;
        let array_index_accessor_protector = false;
        // Probe storage for the inline back-edge poll: an unset interrupt byte
        // and a fuel counter high enough that these small test loops never reach
        // the (null-`vm`) re-entry stub.
        let interrupt_probe: u8 = 0;
        let mut backedge_fuel_probe: u64 = 1 << 30;
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            self_closure: 0,
            this_value: 0,
            thread: std::ptr::null_mut(),
            native_frame: std::ptr::null_mut(),
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
            reg_stack_base: std::ptr::null_mut(),
            reg_top_ptr: std::ptr::null_mut(),
            sync_reentry_depth_ptr: std::ptr::null_mut(),
            sync_reentry_limit: 0,
            array_index_accessor_protector_ptr: &array_index_accessor_protector,
            collection_method_ics: std::ptr::null(),
            collection_method_ic_count: 0,
            direct_method_inline: std::ptr::null(),
            gc_heap: std::ptr::null(),
            interrupt_flag: &interrupt_probe,
            backedge_fuel: &mut backedge_fuel_probe,
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

    fn expect_int(view: &JitCompileSnapshot, regs: &mut [u64], expected: i32) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_i32(bits), expected),
            Exit::Bailed => panic!("expected Returned({expected}), got Bailed"),
        }
    }

    fn expect_f64(view: &JitCompileSnapshot, regs: &mut [u64], expected: f64) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), expected),
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
    fn bitwise_or_truncates_in_range_double() {
        let v = view(&[
            (
                Op::BitwiseOr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(123.9), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 123);
    }

    #[test]
    fn bitwise_or_wraps_out_of_range_double_mod_pow2_32() {
        // A finite double past the signed-32-bit range is the full ECMAScript
        // `ToInt32`: truncate toward zero, reduce mod 2^32 into the signed
        // range. `2^31 | 0 == -2^31`, `2^32 + 5 | 0 == 5`. These come up
        // whenever an int arithmetic result overflows int32 into a double and
        // is then masked with `| 0`, so they must stay compiled, not bail.
        let v = view(&[
            (
                Op::BitwiseOr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(2_147_483_648.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, -2_147_483_648);
        let mut regs = [box_f64(4_294_967_301.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 5);
        let mut regs = [box_f64(-2_147_483_649.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 2_147_483_647);
    }

    #[test]
    fn bitwise_or_bails_on_non_finite_double() {
        // Infinity / NaN / `|x| >= 2^63` would saturate the 64-bit `fcvtzs`, so
        // they bail to the interpreter for exact coercion (`ToInt32` of each is
        // `0`).
        let v = view(&[
            (
                Op::BitwiseOr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(f64::INFINITY), box_i32(0), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
        let mut regs = [
            box_f64(9_223_372_036_854_775_808.0),
            box_i32(0),
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn ushr_boxes_unsigned_int32_result_as_double() {
        let v = view(&[
            (
                Op::Ushr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(-1), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_f64(&v, &mut regs, 4_294_967_295.0);
    }

    #[test]
    fn ushr_truncates_positive_double_mod_uint32() {
        let v = view(&[
            (
                Op::Ushr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(4_294_967_301.9), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_f64(&v, &mut regs, 5.0);
    }

    #[test]
    fn ushr_wraps_negative_double_mod_uint32() {
        // `ToUint32` of a negative finite double wraps mod 2^32: `-1 >>> 0`
        // is `4294967295`, not a bail.
        let v = view(&[
            (
                Op::Ushr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(-1.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_f64(&v, &mut regs, 4_294_967_295.0);
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
        let true_bits = VALUE_TRUE;
        let false_bits = VALUE_FALSE;
        let mut regs = [box_i32(3), box_i32(9), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Returned(b) if b == true_bits));
        let mut regs = [box_i32(9), box_i32(3), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Returned(b) if b == false_bits));
    }

    fn box_f64(v: f64) -> u64 {
        let bits = if v.is_nan() {
            value_tag::CANONICAL_NAN
        } else {
            v.to_bits()
        };
        value_tag::box_double(bits)
    }
    fn unbox_f64(bits: u64) -> f64 {
        f64::from_bits(value_tag::unbox_double(bits))
    }
    fn add_view() -> JitCompileSnapshot {
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
        let t = VALUE_TRUE;
        let f = VALUE_FALSE;
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
    fn strict_non_number_identity_comparisons() {
        let t = VALUE_TRUE;
        let f = VALUE_FALSE;
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
                Exit::Returned(bits) => Some(bits),
                Exit::Bailed => None,
            }
        };
        // Non-number immediates (here, booleans) decide identity inline by raw
        // bit comparison.
        assert_eq!(run_cmp(Op::Equal, t, t), Some(t));
        assert_eq!(run_cmp(Op::Equal, t, f), Some(f));
        assert_eq!(run_cmp(Op::NotEqual, t, f), Some(t));
        // Heap cells (objects, strings, BigInts) bail to the interpreter, which
        // owns object identity and string / BigInt content equality.
        let obj_a = 0x1234; // bare cage offset = heap cell
        let obj_b = 0x5678;
        assert_eq!(run_cmp(Op::Equal, obj_a, obj_b), None);
        assert_eq!(run_cmp(Op::Equal, obj_a, VALUE_NULL), None);
    }

    #[test]
    fn bails_on_non_number_operand() {
        // A tagged non-number (undefined = TAG_SPECIAL, payload 0) must bail to
        // the interpreter for numeric-only operators; only int32 and doubles
        // take the compiled arith path. `Add` has a runtime fallback for JS
        // string/primitive concatenation semantics.
        let v = view(&[
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
        let mut regs = [box_i32(10), VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn to_primitive_bails_on_heap_cell() {
        // A heap cell (object, callable, string) bails to the interpreter so any
        // observable `@@toPrimitive` / `valueOf` / `toString` still runs; the
        // value word alone cannot tell an already-primitive string from an
        // object that needs coercion.
        let v = view(&[
            (
                Op::ToPrimitive,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let cell = 0x1234;
        let mut regs = [cell, 0, 0, 0, 0, 0, 0, 0];
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
        let mut regs = [VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0, 0];
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
                Operand::Register(3),
                Operand::Register(4),
                Operand::Register(5),
                Operand::Register(6),
                Operand::Register(7),
                Operand::Register(8),
                Operand::Register(9),
            ],
        )]);
        assert!(compile(&v).is_err());
    }

    #[test]
    fn method_call_uses_full_packed_argument_abi() {
        let four_args = view(&[
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(4),
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::Register(4),
                    Operand::Register(5),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&four_args).is_ok(),
            "the baseline must accept every argument representable by the shared packed ABI"
        );

        let five_args = view(&[
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(5),
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::Register(4),
                    Operand::Register(5),
                    Operand::Register(6),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(compile(&five_args).is_err());
    }

    #[test]
    fn store_element_is_part_of_the_baseline_subset() {
        let store = view(&[
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&store).is_ok(),
            "the emitted dense/typed-array fast path and typed runtime miss must be reachable"
        );
    }
}

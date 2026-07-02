//! arm64 machine-code emitter for the optimizing tier.
//!
//! Lowers a register-allocated typed SSA [`Graph`] to native arm64. Unlike the
//! baseline template emitter, this consumes a real backend pipeline — typed SSA,
//! SSA liveness, linear-scan register allocation (GP + FP classes), and per-guard
//! deopt frame states — and emits multi-block code with unboxed int32 and f64
//! islands, control flow over allocated registers, parallel edge moves at
//! control-flow merges, and exact-PC deoptimization to the interpreter at every
//! speculation guard.
//!
//! # ABI
//! Compiled functions are `extern "C" fn(*mut JitCtx) -> JitRet` — the identical
//! [`crate::baseline`] entry signature, run through the shared
//! [`crate::baseline::enter_compiled`]. The entry loads the frame register
//! window base (`ctx.regs`) into `x19` and keeps `ctx` in `x20`. A `Return`
//! yields `JitRet{value, status: 0}`; a failed guard / int32 overflow restores
//! the live interpreter registers, stamps the resume byte-PC into `ctx.bail_pc`,
//! and yields `status: 1` (the VM resumes the interpreter at that PC).
//!
//! Abstract GP allocator registers `Reg(0..7)` map to physical `x9..x15`; `x16`
//! and `x17` are GP emit scratch (loads, boxing, parallel-move temp). FP
//! allocator registers `Reg(0..6)` map to physical `d0..d5`, with `d6`/`d7` as FP
//! scratch. All are caller-saved and the numeric subset makes no calls, so none
//! need a prologue save. Spill slot `s` lives at `[sp, #s*8]` in the JIT spill
//! area reserved below the saved frame; the parallel-move cycle-break scratch is
//! the one extra slot `Spill(spill_slots)`.
//!
//! # GC contract
//! The numeric subset has no `Call` and allocates nothing, so it has no
//! safepoints. The frame register window `[x19]` is the GC root array and must
//! always hold valid NaN-boxed `Value`s: it arrives holding the boxed
//! parameters, and the emitted body never writes an unboxed number into a
//! `[x19]` slot. Computed values live unboxed in `x9..x15`, `d0..d7`, and in the
//! `[sp]` spill area, which hold non-pointers (a boxed double is its bits
//! verbatim, also a non-pointer) and so need no stack maps. `[x19]` slots are
//! written only on a deopt restore, where each live value is re-boxed to a
//! tagged `Value` first. The result is returned in `x0` boxed; it is never
//! written to the frame array.
//!
//! # Invariants
//! - **Whole-function correctness gate.** Any value the emitter must read (an
//!   operand, return value, phi edge input, or deopt frame-state value) that has
//!   no register-allocation home aborts the whole compile with
//!   [`Unsupported::Unallocated`]; the VM falls back to the baseline. A value
//!   that is dead (no home and never read) is simply skipped. No emitted function
//!   ever performs a wild access.
//! - **Deopt restores exactly the live set.** Each deopt exit re-boxes and stores
//!   only the registers live at the guard's byte-PC (per the deopt frame state),
//!   reconstructing precisely the interpreter frame the resumed bytecode reads.
//! - **Edge moves are critical-edge correct.** A control transfer runs that
//!   specific predecessor→successor edge's parallel moves (phi reconciliation)
//!   before branching to the successor, so a value entering a merge from two
//!   edges is placed correctly on each.
//!
//! # See also
//! - [`crate::baseline`] — the shared entry/ABI and the deopt target tier.
//! - [`super::regalloc`] — the allocation + edge moves consumed here.
//! - [`super::deopt`] — the per-guard frame states consumed here.

use super::Unsupported;
use super::deopt::{DeoptPoint, OsrEntry};
use super::ir::{BlockId, CmpOp, Float64UnaryOp, Graph, NodeId, NodeKind, Repr, Terminator};
use super::liveness::Liveness;
use super::regalloc::{Allocation, EdgeMoves, Location};
use crate::CompiledCode;
use otter_bytecode::Op;
use otter_vm::{JitFunctionView, NO_FRAME_STATE, SafepointId, SafepointRecord};

/// Caller-saved (volatile) GP registers in the allocator pool: abstract
/// `Reg(0..7)` → physical `x9..x15`. Clobbered across any call.
pub const CALLER_SAVED_GP: u32 = 7;

/// Callee-saved (non-volatile) GP registers in the allocator pool: abstract
/// `Reg(7..15)` → physical `x21..x28`. Preserved across a call by the callee's
/// prologue/epilogue (and by the C ABI for the runtime stubs), so a value live
/// across a call survives here without a frame round-trip.
pub const CALLEE_SAVED_GP: u32 = 8;

/// Number of abstract GP registers handed to the allocator (`Reg(0..15)`),
/// caller-saved `x9..x15` then callee-saved `x21..x28`.
pub const GP_REGS: u32 = CALLER_SAVED_GP + CALLEE_SAVED_GP;

/// Caller-saved FP registers: abstract `Reg(0..6)` of the `Fp` class → `d0..d5`.
/// `d6`/`d7` stay reserved as FP emit scratch (load staging, box/unbox,
/// arithmetic temporaries), mirroring the `x16`/`x17` GP scratch pair.
pub const CALLER_SAVED_FP: u32 = 6;

/// Callee-saved FP registers: abstract `Reg(6..14)` → `d8..d15`. The arm64 ABI
/// preserves only the low 64 bits of `d8..d15`; the allocator stores f64s, so
/// the full register is preserved.
pub const CALLEE_SAVED_FP: u32 = 8;

/// Number of abstract FP registers handed to the allocator (`Reg(0..14)`),
/// caller-saved `d0..d5` then callee-saved `d8..d15`.
pub const FP_REGS: u32 = CALLER_SAVED_FP + CALLEE_SAVED_FP;

/// Finalized optimizing-tier machine code for one function. Wraps a
/// [`CompiledCode`] and runs through the shared baseline entry, so it inherits
/// the exact reentry ABI and deopt-resume handling.
pub struct OptimizedCode {
    code: CompiledCode,
    /// Loop-header byte-PC → byte offset of that header's OSR-entry trampoline
    /// within `code`. Empty when the function has no eligible loop header.
    osr_offsets: rustc_hash::FxHashMap<u32, usize>,
    /// The function contains a `Deopt` terminator (an un-compilable
    /// prologue / epilogue around a hot loop). Such a function is entered ONLY
    /// through an OSR loop header; a function-entry call runs the interpreter
    /// from the top (returns a bail at PC 0). This keeps the un-compilable parts
    /// — and their side-effect ordering — exactly as the interpreter runs them,
    /// while the hot loop still tiers up via OSR.
    entry_via_osr_only: bool,
    /// Safepoint table published through the shared `JitCtx` ABI. The dynasm
    /// optimizing emitter can materialize the interpreter frame around calls,
    /// so VM-native allocating stubs can publish frame-slot roots without
    /// re-entering the method bridge.
    safepoint_records: Box<[SafepointRecord]>,
}

impl std::fmt::Debug for OptimizedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptimizedCode")
            .field("code_len", &self.code.len())
            .finish()
    }
}

impl otter_vm::JitFunctionCode for OptimizedCode {
    fn code_len(&self) -> usize {
        self.code.len()
    }

    fn osr_only(&self) -> bool {
        self.entry_via_osr_only
    }

    fn entry_addr(&self) -> Option<usize> {
        // A whole-function optimized body shares the baseline's `JitEntry` ABI,
        // so it can be a direct-call target: a caller's `Op::Call` enters it at
        // PC 0 without the generic call bridge. An `entry_via_osr_only` body has
        // an un-runnable prologue (it only exists for its loop OSR trampolines),
        // so it has no callable entry.
        if self.entry_via_osr_only {
            return None;
        }
        // SAFETY: the mapping is live for `self`; callers keep the owning code
        // object installed while using this address.
        Some(unsafe { self.code.entry_ptr() as usize })
    }

    fn run_entry(&self, _ptrs: otter_vm::JitReentryPtrs) -> otter_vm::JitExecOutcome {
        if self.entry_via_osr_only {
            // The compiled code has an un-compilable prologue / epilogue: never
            // run it from the top. Bail at PC 0 so the interpreter runs the
            // function (it will OSR the hot loop on a backedge). The frame is
            // untouched, so resuming the interpreter at PC 0 is exact.
            return otter_vm::JitExecOutcome::Bailed(0);
        }
        // SAFETY: the mapping is live for `self`, and the entry was emitted with
        // the shared `JitEntry` ABI (`extern "C" fn(*mut JitCtx) -> JitRet`).
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `_ptrs` upholds the
        // reentry contract.
        unsafe {
            crate::baseline::enter_compiled(
                _ptrs,
                entry,
                self.safepoint_records.as_ptr(),
                self.safepoint_records.len() as u32,
            )
        }
    }

    fn osr_entry(
        &self,
        ptrs: otter_vm::JitReentryPtrs,
        byte_pc: u32,
    ) -> Option<otter_vm::JitExecOutcome> {
        let offset = *self.osr_offsets.get(&byte_pc)?;
        // SAFETY: `offset` is an assembler offset recorded for this buffer at a
        // loop-header OSR trampoline emitted with the `JitEntry` ABI.
        let entry = unsafe { self.code.ptr_at(offset) };
        // SAFETY: same reentry contract as `run_entry`; the trampoline reloads
        // the live interpreter registers before branching to the loop header.
        Some(unsafe {
            crate::baseline::enter_compiled(
                ptrs,
                entry,
                self.safepoint_records.as_ptr(),
                self.safepoint_records.len() as u32,
            )
        })
    }
}

fn optimizing_safepoint_records(view: &JitFunctionView) -> Box<[SafepointRecord]> {
    let mut records: Vec<_> = view.safepoints.values().cloned().collect();
    let mut next_safepoint = records
        .iter()
        .map(|record| record.id)
        .max()
        .map_or(1, |id| id.saturating_add(1))
        .max(1);
    for instr in &view.instructions {
        if !matches!(instr.op, Op::CallMethodValue | Op::NewArray) {
            continue;
        }
        if instr.op == Op::CallMethodValue
            && view.collection_alloc_methods.contains_key(&instr.byte_pc)
        {
            continue;
        }
        let safepoint = next_safepoint;
        next_safepoint = next_safepoint.saturating_add(1);
        records.push(SafepointRecord::frame_slot_window(
            safepoint,
            NO_FRAME_STATE,
            view.register_count,
        ));
    }
    records.sort_by_key(|record| record.id);
    records.into_boxed_slice()
}

fn optimizing_call_method_safepoint_id(
    view: &JitFunctionView,
    byte_pc: u32,
) -> Option<SafepointId> {
    if let Some(alloc) = view.collection_alloc_methods.get(&byte_pc) {
        return Some(alloc.safepoint_id);
    }
    let mut next_safepoint = view
        .safepoints
        .values()
        .map(|record| record.id)
        .max()
        .map_or(1, |id| id.saturating_add(1))
        .max(1);
    for instr in &view.instructions {
        if instr.op != Op::CallMethodValue {
            continue;
        }
        if view.collection_alloc_methods.contains_key(&instr.byte_pc) {
            continue;
        }
        let safepoint = next_safepoint;
        next_safepoint = next_safepoint.saturating_add(1);
        if instr.byte_pc == byte_pc {
            return Some(safepoint);
        }
    }
    None
}

#[cfg(target_arch = "aarch64")]
pub(super) use arm64::emit;

#[cfg(not(target_arch = "aarch64"))]
pub(super) fn emit(
    _view: &otter_vm::JitFunctionView,
    _graph: &Graph,
    _liveness: &Liveness,
    _alloc: &Allocation,
    _frames: &rustc_hash::FxHashMap<NodeId, DeoptPoint>,
    _call_resume_frames: &rustc_hash::FxHashMap<NodeId, DeoptPoint>,
    _block_deopts: &rustc_hash::FxHashMap<BlockId, DeoptPoint>,
    _osr_entries: &[OsrEntry],
) -> Result<OptimizedCode, Unsupported> {
    Err(Unsupported::Unlowered("optimizing emit: non-aarch64 host"))
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    #![allow(unused_parens)]
    use super::super::builder::graph_allows_frameless_self_call;
    use super::{
        Allocation, BlockId, CmpOp, DeoptPoint, EdgeMoves, Float64UnaryOp, GP_REGS, Graph,
        Liveness, Location, NodeId, NodeKind, OptimizedCode, OsrEntry, Repr, Terminator,
        Unsupported,
    };
    use crate::CompiledCode;
    use crate::baseline::{
        ALLOC_CTX_CONTEXT_OFFSET, ALLOC_CTX_FRAME_INDEX_OFFSET, ALLOC_CTX_FRAME_SLOT_COUNT_OFFSET,
        ALLOC_CTX_FRAME_SLOTS_OFFSET, ALLOC_CTX_RESERVED0_OFFSET, ALLOC_CTX_RESERVED1_OFFSET,
        ALLOC_CTX_SAFEPOINT_COUNT_OFFSET, ALLOC_CTX_SAFEPOINT_RECORDS_OFFSET,
        ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_OFFSET,
        ALLOC_CTX_STACK_SIZE, ALLOC_CTX_VM_OFFSET, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET,
        BAIL_PC_OFFSET, CANONICAL_NAN_HI16, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET,
        COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET, COLLECTION_METHOD_IC_COUNT_OFFSET,
        COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET,
        COLLECTION_METHOD_IC_PROTO_OFFSET, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET,
        COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET, COLLECTION_METHOD_IC_SLOT_SIZE,
        COLLECTION_METHOD_IC_STATE_OFFSET, COLLECTION_METHOD_ICS_OFFSET, CONTEXT_OFFSET,
        DOUBLE_OFFSET_HI16, FRAME_INDEX_OFFSET, FUNCTION_ID_TAG, GC_HEAP_OFFSET,
        JS_CLOSURE_BODY_TYPE_TAG, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, REG_STACK_BASE_OFFSET,
        REG_TOP_PTR_OFFSET, SAFEPOINT_COUNT_OFFSET, SAFEPOINT_RECORDS_OFFSET, STACK_OFFSET,
        STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, THIS_VALUE_OFFSET, UPVALUE_CELL_SIZE,
        UPVALUE_VALUE_OFFSET, UPVALUES_PTR_OFFSET, VALUE_FALSE, VALUE_HOLE, VALUE_NULL, VALUE_TRUE,
        VALUE_UNDEFINED, VM_OFFSET, jit_alloc_object_literal_stub, jit_array_push_optimizing_stub,
        jit_backedge_poll_stub, jit_call_collection_method_ic_stub,
        jit_call_method_stub_optimizing, jit_new_array_stub, jit_prepare_direct_call_stub,
        jit_prepare_direct_method_call_stub, jit_self_call_bail_stub, value_tag,
    };
    use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
    use otter_vm::{
        Interpreter, JitFunctionView, STUB_COLLECTION_SET_ADD_ALLOC, SafepointId,
        jit::{JIT_COLLECTION_METHOD_IC_COLLECTION, JIT_COLLECTION_METHOD_IC_NO_STUB},
        runtime_stubs::{alloc_value_stub_trampoline_pair, leaf_no_alloc_stub2_trampoline_pair},
    };
    use rustc_hash::{FxHashMap, FxHashSet};

    /// Emit scratch register for parallel-move spill→spill staging and tag
    /// immediates (`x16`). Never an allocatable home (those are `x9..x15`).
    const MOVE_SCRATCH: u32 = 16;
    /// Emit scratch register for loaded values being boxed / tested (`x17`).
    const BOX_SCRATCH: u32 = 17;

    /// Physical register holding abstract allocator `Reg(i)`: caller-saved
    /// `Reg(0..7)` → `x9..x15`, callee-saved `Reg(7..15)` → `x21..x28`. Scratch
    /// `x16`/`x17` and reserved `x19`/`x20` are never returned here.
    fn phys(i: u32) -> u32 {
        debug_assert!(i < GP_REGS);
        if i < super::CALLER_SAVED_GP {
            9 + i
        } else {
            21 + (i - super::CALLER_SAVED_GP)
        }
    }

    /// Physical FP register holding `Fp`-class allocator `Reg(i)`: caller-saved
    /// `Reg(0..6)` → `d0..d5`, callee-saved `Reg(6..14)` → `d8..d15`. FP scratch
    /// `d6`/`d7` are reserved and never returned here.
    fn phys_fp(i: u32) -> u32 {
        debug_assert!(i < super::FP_REGS);
        if i < super::CALLER_SAVED_FP {
            i
        } else {
            8 + (i - super::CALLER_SAVED_FP)
        }
    }

    /// FP scratch register for load staging / verbatim boxing (`d6`).
    const FP_LOAD_SCRATCH: u32 = 6;
    /// FP scratch register for the second arithmetic operand (`d7`).
    const FP_ARITH_SCRATCH: u32 = 7;

    /// Frame byte offset of spill slot `s` within the JIT spill area (`[sp]`).
    fn spill_off(s: u32) -> u32 {
        s * 8
    }

    /// Round a byte count up to a 16-byte multiple (arm64 sp alignment).
    fn align16(n: u32) -> u32 {
        (n + 15) & !15
    }

    /// Materialize the 64-bit constant `v` into x-register `xr` (movz + up to
    /// three movk for the non-zero 16-bit lanes).
    fn emit_load_u64(ops: &mut Assembler, xr: u32, v: u64) {
        let l0 = (v & 0xFFFF) as u32;
        let l1 = ((v >> 16) & 0xFFFF) as u32;
        let l2 = ((v >> 32) & 0xFFFF) as u32;
        let l3 = ((v >> 48) & 0xFFFF) as u32;
        dynasm!(ops ; .arch aarch64 ; movz X(xr), l0);
        if l1 != 0 {
            dynasm!(ops ; .arch aarch64 ; movk X(xr), l1, lsl #16);
        }
        if l2 != 0 {
            dynasm!(ops ; .arch aarch64 ; movk X(xr), l2, lsl #32);
        }
        if l3 != 0 {
            dynasm!(ops ; .arch aarch64 ; movk X(xr), l3, lsl #48);
        }
    }

    fn is_backedge(graph: &Graph, from: BlockId, to: BlockId) -> bool {
        graph.blocks[to as usize].start_pc <= graph.blocks[from as usize].start_pc
    }

    fn emit_backedge_poll(ops: &mut Assembler, threw: DynamicLabel, save_base: u32) {
        // The poll stub follows the C ABI and preserves the callee-saved
        // allocator registers (`x21..x28`, `d8..d15`), so only the caller-saved
        // pool needs a round-trip here.
        for i in 0..super::CALLER_SAVED_GP {
            let reg = phys(i);
            let off = save_base + i * 8;
            dynasm!(ops ; .arch aarch64 ; str X(reg), [sp, off]);
        }
        for i in 0..super::CALLER_SAVED_FP {
            let off = save_base + (super::CALLER_SAVED_GP + i) * 8;
            dynasm!(ops ; .arch aarch64 ; str D(phys_fp(i)), [sp, off]);
        }
        dynasm!(ops ; .arch aarch64 ; mov x0, x20);
        emit_load_u64(ops, 16, jit_backedge_poll_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x0, =>threw
        );
        for i in 0..super::CALLER_SAVED_FP {
            let off = save_base + (super::CALLER_SAVED_GP + i) * 8;
            dynasm!(ops ; .arch aarch64 ; ldr D(phys_fp(i)), [sp, off]);
        }
        for i in 0..super::CALLER_SAVED_GP {
            let reg = phys(i);
            let off = save_base + i * 8;
            dynasm!(ops ; .arch aarch64 ; ldr X(reg), [sp, off]);
        }
    }

    /// Probe the `Vec<T>` field layout by value identity, returning the byte
    /// offsets of its data-pointer and length words. The standard library does
    /// not promise field order, while the JIT must read `Vec<Value>` /
    /// `Vec<u8>` backing storage without naming the VM-side Rust type here.
    fn vec_layout_offsets() -> (u32, u32) {
        static CACHE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| {
            let mut v: Vec<u8> = Vec::with_capacity(4);
            v.push(0xA5);
            let ptr = v.as_ptr() as usize;
            let len = v.len();
            assert_eq!(std::mem::size_of::<Vec<u8>>(), 24);
            // SAFETY: copy the Vec's three machine words by value. They are
            // compared to known pointer/length values, never dereferenced.
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
                ptr_off.expect("Vec data-pointer word not found"),
                len_off.expect("Vec length word not found"),
            )
        })
    }

    /// Box the f64 in `src_d` into x-register `dst_x` as a tagged `Value`.
    /// NaN is canonicalized first, then the encode offset is added so the bits
    /// land in the number space and never alias an immediate. Uses the move
    /// scratch (`x16`); `dst_x` must be an allocatable home, not the scratch.
    fn emit_box_double(ops: &mut Assembler, src_d: u32, dst_x: u32) {
        let ready = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; fmov X(dst_x), D(src_d)
            ; fcmp D(src_d), D(src_d)
            ; b.vc =>ready
            ; movz X(dst_x), CANONICAL_NAN_HI16, lsl #48
            ; =>ready
            ; movz X(MOVE_SCRATCH), DOUBLE_OFFSET_HI16, lsl #48
            ; add X(dst_x), X(dst_x), X(MOVE_SCRATCH)
        );
    }

    /// Load a value's home `loc` into physical x-register `dst`.
    fn load_loc(ops: &mut Assembler, dst: u32, loc: Location) {
        match loc {
            Location::Reg(r) => {
                let src = phys(r);
                if src != dst {
                    dynasm!(ops ; .arch aarch64 ; mov X(dst), X(src));
                }
            }
            Location::Spill(s) => {
                let off = spill_off(s);
                dynasm!(ops ; .arch aarch64 ; ldr X(dst), [sp, off]);
            }
        }
    }

    /// Store physical x-register `src` into a value's home `loc`.
    fn store_loc(ops: &mut Assembler, loc: Location, src: u32) {
        match loc {
            Location::Reg(r) => {
                let dst = phys(r);
                if dst != src {
                    dynasm!(ops ; .arch aarch64 ; mov X(dst), X(src));
                }
            }
            Location::Spill(s) => {
                let off = spill_off(s);
                dynasm!(ops ; .arch aarch64 ; str X(src), [sp, off]);
            }
        }
    }

    /// Load a `Float64` value's home `loc` into physical d-register `dst`.
    fn load_fp_loc(ops: &mut Assembler, dst: u32, loc: Location) {
        match loc {
            Location::Reg(r) => {
                let src = phys_fp(r);
                if src != dst {
                    dynasm!(ops ; .arch aarch64 ; fmov D(dst), D(src));
                }
            }
            Location::Spill(s) => {
                let off = spill_off(s);
                dynasm!(ops ; .arch aarch64 ; ldr D(dst), [sp, off]);
            }
        }
    }

    /// Store physical d-register `src` into a `Float64` value's home `loc`.
    fn store_fp_loc(ops: &mut Assembler, loc: Location, src: u32) {
        match loc {
            Location::Reg(r) => {
                let dst = phys_fp(r);
                if dst != src {
                    dynasm!(ops ; .arch aarch64 ; fmov D(dst), D(src));
                }
            }
            Location::Spill(s) => {
                let off = spill_off(s);
                dynasm!(ops ; .arch aarch64 ; str D(src), [sp, off]);
            }
        }
    }

    /// Materialize the boxed (tagged `Value`) form of an SSA value held at home
    /// `loc` into GP register `gp_dst`, regardless of its `repr`. A `Float64`
    /// value lives in an FP home: its boxed form is its bits verbatim, an `fmov`
    /// from the FP home into `gp_dst` (no NaN canonicalization — the producer
    /// already canonicalized any `NaN`). `Int32` / `Bool` / `Tagged` values load
    /// into `gp_dst` and box in place via `box_value` (`tag_scratch` carries the
    /// tag immediate and must differ from `gp_dst`).
    fn box_into_gp(ops: &mut Assembler, gp_dst: u32, repr: Repr, loc: Location, tag_scratch: u32) {
        if repr == Repr::Float64 {
            load_fp_loc(ops, FP_LOAD_SCRATCH, loc);
            // The producer already canonicalized any NaN; apply the encode offset
            // so the boxed bits land in the number space.
            dynasm!(ops
                ; .arch aarch64
                ; fmov X(gp_dst), D(FP_LOAD_SCRATCH)
                ; movz X(tag_scratch), DOUBLE_OFFSET_HI16, lsl #48
                ; add X(gp_dst), X(gp_dst), X(tag_scratch)
            );
        } else {
            load_loc(ops, gp_dst, loc);
            box_value(ops, gp_dst, repr, tag_scratch);
        }
    }

    /// Decompress a 4-byte object property slot (zero-extended in `x17`) into a
    /// full tagged `Value`, in place in `x17`. Small-int / cell-ref / immediate /
    /// function-id decode inline; a `TAG_BOXED` slot reads the heap-number box's
    /// raw value bits. Fixed registers (the `#imm` forms need literal
    /// registers): `x17` slot in/out, `x16` scratch.
    fn emit_decompress_slot(ops: &mut Assembler, view: &JitFunctionView, deopt: DynamicLabel) {
        use otter_vm::value::compressed as cslot;
        debug_assert_eq!(cslot::TAG_MASK, 0b111);
        debug_assert_eq!(cslot::TAG_BOXED, 0b010);
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
        let l_boxed = ops.new_dynamic_label();
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
            ; tbnz w17, #0, =>l_smi                     // bit0 set → small int
            ; and w16, w17, #0x7                        // low-3-bit slot tag
            ; cbz w16, =>l_cell                         // 000 → cell ref
            ; cmp w16, #0x4                             // 100 → immediate
            ; b.eq =>l_imm
            ; cmp w16, #0x6                             // 110 → function id
            ; b.eq =>l_fid
            ; cmp w16, #0x2                             // 010 → boxed number
            ; b.eq =>l_boxed
            ; b =>deopt
            ; =>l_cell
            ; cbz x17, =>l_undef                        // empty slot → undefined
            ; b =>l_done                                // cell ref = zero-extended offset
            ; =>l_boxed
            ; and w17, w17, #0xfffffff8
            ; mov w17, w17
        );
        emit_load_u64(ops, 16, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x16, x16, x17
            ; ldrb w17, [x16]
            ; cmp w17, u32::from(view.heap_number_type_tag)
            ; b.ne =>deopt
            ; ldr x17, [x16, view.heap_number_bits_byte]
            ; b =>l_done
            ; =>l_smi
            ; asr w17, w17, #1
            ; mov w17, w17
            ; movz x16, NUMBER_TAG_HI16, lsl #48
            ; orr x17, x17, x16
            ; b =>l_done
            ; =>l_fid
            ; lsr w17, w17, #3
            ; lsl x17, x17, #16
            ; movz x16, FUNCTION_ID_TAG as u32
            ; orr x17, x17, x16
            ; b =>l_done
            ; =>l_imm
            ; lsr w16, w17, #3
            ; cmp w16, #1
            ; b.eq =>l_null
            ; cmp w16, #2
            ; b.eq =>l_true
            ; cmp w16, #3
            ; b.eq =>l_false
            ; cmp w16, #4
            ; b.eq =>l_hole
            ; =>l_undef
            ; movz x17, VALUE_UNDEFINED as u32
            ; b =>l_done
            ; =>l_null
            ; movz x17, VALUE_NULL as u32
            ; b =>l_done
            ; =>l_true
            ; movz x17, VALUE_TRUE as u32
            ; b =>l_done
            ; =>l_false
            ; movz x17, VALUE_FALSE as u32
            ; b =>l_done
            ; =>l_hole
            ; movz x17, VALUE_HOLE as u32
            ; =>l_done
        );
    }

    /// Compress the full tagged `Value` in `x17` into a 4-byte object slot in
    /// `w17`, in place. A small int, cell ref, or immediate encodes inline; a
    /// wide int, double, or function id (a boxed-number slot allocates) side
    /// exits to `deopt`. Fixed registers: `x17` value in / slot out, `x16` scratch.
    fn emit_compress_slot(ops: &mut Assembler, deopt: DynamicLabel) {
        let not_int = ops.new_dynamic_label();
        let check_imm = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        let imm_undef = ops.new_dynamic_label();
        let imm_null = ops.new_dynamic_label();
        let imm_true = ops.new_dynamic_label();
        let imm_false = ops.new_dynamic_label();
        let imm_hole = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; and x16, x17, #value_tag::NUMBER_TAG
            ; eor x16, x16, #value_tag::NUMBER_TAG
            ; cbnz x16, =>not_int                       // not an int32
            // int32: keep only a small int in [-2^30, 2^30); wider ints box.
            ; movz w16, #0x4000, lsl #16                // 2^30
            ; add w16, w17, w16
            ; tbnz w16, #31, =>deopt                    // out of small-int range
            ; lsl w17, w17, #1
            ; orr w17, w17, #1                          // (i << 1) | 1
            ; b =>done
            ; =>not_int
            ; and x16, x17, #value_tag::NUMBER_TAG
            ; cbnz x16, =>deopt                         // a double boxes on the heap
            ; tst x17, #value_tag::OTHER_TAG
            ; b.ne =>check_imm                          // immediate or function id
            // Cell: the low-32 8-aligned offset (low-3 tag 000) is the slot.
            ; b =>done
            ; =>check_imm
            ; cmp x17, #(VALUE_UNDEFINED as u32)
            ; b.eq =>imm_undef
            ; cmp x17, #(VALUE_NULL as u32)
            ; b.eq =>imm_null
            ; cmp x17, #(VALUE_TRUE as u32)
            ; b.eq =>imm_true
            ; cmp x17, #(VALUE_FALSE as u32)
            ; b.eq =>imm_false
            ; cmp x17, #(VALUE_HOLE as u32)
            ; b.eq =>imm_hole
            ; b =>deopt                                 // function id → interpreter
            ; =>imm_undef
            ; movz w17, #0x4                            // (0 << 3) | 0b100
            ; b =>done
            ; =>imm_null
            ; movz w17, #0xc                            // (1 << 3) | 0b100
            ; b =>done
            ; =>imm_true
            ; movz w17, #0x14                           // (2 << 3) | 0b100
            ; b =>done
            ; =>imm_false
            ; movz w17, #0x1c                           // (3 << 3) | 0b100
            ; b =>done
            ; =>imm_hole
            ; movz w17, #0x24                           // (4 << 3) | 0b100
            ; =>done
        );
    }

    /// Emit the inline generational write barrier for a pointer-valued slot
    /// store: mark the parent object's card dirty when an old parent gains a
    /// young child (the remembered set the scavenger reads on a young
    /// collection). Mirrors `otter_gc::barrier::write_barrier`'s generational
    /// arm. The insertion (marking) barrier is dormant under the Phase-1 STW
    /// collector, so only the card-mark is emitted; it allocates nothing and
    /// never moves GC, hence needs no safepoint.
    ///
    /// Clobbers only the reserved scratch (`x16`/`x17`, `d6`/`d7`); reads the
    /// boxed parent/child from their SSA locations. All control flow funnels to
    /// a single `done` label, so a non-pointer / young-parent / old-child store
    /// falls straight through.
    fn emit_generational_card_mark(
        ops: &mut Assembler,
        view: &JitFunctionView,
        parent_loc: Location,
        child_loc: Location,
    ) -> Result<(), Unsupported> {
        // The card-mark uses dynasm immediates, which require literal registers
        // (`x16`/`x17` ≡ `MOVE_SCRATCH`/`BOX_SCRATCH`) and compile-time-const
        // immediate operands. The stable GcHeader/card ABI bits are otter-jit
        // consts; `debug_assert` pins them against the values otter-vm baked from
        // the live `#[repr(C)]` layout so a layout change is caught in tests. The
        // genuinely layout-variable offsets (cage base, page mask, card-bitmap
        // offset) are loaded into registers.
        const GC_FLAGS_BYTE: u32 = 1;
        const GC_YOUNG_BIT: u32 = 2;
        const GC_CARD_SHIFT: u32 = 9;
        debug_assert_eq!(view.gc_barrier.header_flags_byte, GC_FLAGS_BYTE);
        debug_assert_eq!(view.gc_barrier.young_flag.trailing_zeros(), GC_YOUNG_BIT);
        debug_assert_eq!(view.gc_barrier.card_shift, GC_CARD_SHIFT);
        let cage = view.cage_base as u64;
        let page_mask = view.gc_barrier.page_mask;
        let bitmap_off = view.gc_barrier.card_bitmap_byte as u64;
        let done = ops.new_dynamic_label();

        // (1) Child cell test: a heap cell carries neither a `NUMBER_TAG` bit
        // nor `OTHER_TAG`; a number or immediate child needs no barrier.
        load_loc(ops, MOVE_SCRATCH, child_loc);
        dynasm!(ops
            ; .arch aarch64
            ; tst x16, #value_tag::NUMBER_TAG
            ; b.ne =>done
            ; tst x16, #value_tag::OTHER_TAG
            ; b.ne =>done
        );

        // (2) Child young? child_hdr = cage_base + low32(child). An old child
        // needs no remembered-set entry, so a clear young bit skips.
        load_loc(ops, BOX_SCRATCH, child_loc);
        dynasm!(ops ; .arch aarch64 ; mov w16, w17);
        emit_load_u64(ops, BOX_SCRATCH, cage);
        dynasm!(ops
            ; .arch aarch64
            ; add x16, x16, x17
            ; ldrb w17, [x16, #GC_FLAGS_BYTE]
            ; tbz w17, #GC_YOUNG_BIT, =>done
        );

        // (3) Parent young? parent_hdr = cage_base + low32(parent). Young parents
        // are evacuated wholesale by the scavenger, so only an old parent records
        // a card. x16 = parent_hdr afterwards.
        load_loc(ops, BOX_SCRATCH, parent_loc);
        dynasm!(ops ; .arch aarch64 ; mov w16, w17);
        emit_load_u64(ops, BOX_SCRATCH, cage);
        dynasm!(ops
            ; .arch aarch64
            ; add x16, x16, x17
            ; ldrb w17, [x16, #GC_FLAGS_BYTE]
            ; tbnz w17, #GC_YOUNG_BIT, =>done
        );

        // (4) Mark the parent's card. page_base = parent_hdr & page_mask; card =
        // (parent_hdr - page_base) >> card_shift; set bit (card & 7) in the byte
        // at page_base + card_bitmap_off + (card >> 3). x16 = parent_hdr.
        emit_load_u64(ops, BOX_SCRATCH, page_mask);
        dynasm!(ops
            ; .arch aarch64
            ; and x17, x16, x17                  // x17 = page_base
            ; sub x16, x16, x17                  // x16 = byte_off
            ; lsr x16, x16, #GC_CARD_SHIFT       // x16 = card
            ; fmov d7, x16                       // park card
        );
        emit_load_u64(ops, MOVE_SCRATCH, bitmap_off);
        dynasm!(ops
            ; .arch aarch64
            ; add x17, x17, x16                  // x17 = bitmap base (page_base + off)
            ; fmov x16, d7                       // x16 = card
            ; add x17, x17, x16, lsr #3          // x17 = byte addr
            ; and x16, x16, #7                   // x16 = bit index
            // mask = 1 << bit, parking the byte address in d7 across the shift.
            ; fmov d7, x17
            ; movz w17, #1
            ; lslv w17, w17, w16                 // x17 = mask
            ; fmov x16, d7                       // x16 = byte addr
            // [x16] |= mask, parking mask/addr in d6/d7 across the load.
            ; fmov d7, x17                       // park mask
            ; ldrb w17, [x16]                    // x17 = byte
            ; fmov d6, x16                       // park addr
            ; fmov x16, d7                       // x16 = mask
            ; orr w17, w17, w16                  // x17 = byte | mask
            ; fmov x16, d6                       // x16 = addr
            ; strb w17, [x16]
            ; =>done
        );
        Ok(())
    }

    fn emit_rematerialized_boxed(
        ops: &mut Assembler,
        kind: &NodeKind,
        gp_dst: u32,
        tag_scratch: u32,
    ) -> bool {
        match kind {
            NodeKind::ConstUndefined => {
                emit_load_u64(ops, gp_dst, VALUE_UNDEFINED);
                true
            }
            NodeKind::ConstNull => {
                emit_load_u64(ops, gp_dst, VALUE_NULL);
                true
            }
            NodeKind::ConstBool(value) => {
                let special_bits = if *value { VALUE_TRUE } else { VALUE_FALSE };
                emit_load_u64(ops, gp_dst, special_bits);
                true
            }
            NodeKind::ConstInt32(value) => {
                emit_load_u64(ops, gp_dst, u64::from(*value as u32));
                box_value(ops, gp_dst, Repr::Int32, tag_scratch);
                true
            }
            NodeKind::ConstF64(value) => {
                let bits = if value.is_nan() {
                    value_tag::CANONICAL_NAN
                } else {
                    value.to_bits()
                };
                emit_load_u64(ops, gp_dst, value_tag::box_double(bits));
                true
            }
            NodeKind::SelfClosure => {
                dynasm!(ops ; .arch aarch64 ; ldr X(gp_dst), [x20, #8]);
                true
            }
            _ => false,
        }
    }

    /// Emit one parallel-move `from → to`. A register/register move is a `mov`; a
    /// spill on either end goes through `scratch_x`; a spill→spill move routes the
    /// load and store through `scratch_x` as well.
    fn emit_move(ops: &mut Assembler, from: Location, to: Location, scratch_x: u32) {
        if from == to {
            return;
        }
        match (from, to) {
            (Location::Reg(a), Location::Reg(b)) => {
                dynasm!(ops ; .arch aarch64 ; mov X(phys(b)), X(phys(a)));
            }
            (Location::Reg(a), Location::Spill(b)) => {
                let off = spill_off(b);
                dynasm!(ops ; .arch aarch64 ; str X(phys(a)), [sp, off]);
            }
            (Location::Spill(a), Location::Reg(b)) => {
                let off = spill_off(a);
                dynasm!(ops ; .arch aarch64 ; ldr X(phys(b)), [sp, off]);
            }
            (Location::Spill(a), Location::Spill(b)) => {
                let off_a = spill_off(a);
                let off_b = spill_off(b);
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr X(scratch_x), [sp, off_a]
                    ; str X(scratch_x), [sp, off_b]
                );
            }
        }
    }

    /// Emit a control-flow `pred → succ` edge: first the allocator's ordered,
    /// cycle-safe location moves (phi reconciliation + cross-edge relocation),
    /// then **representation reconciliation** — a phi is always `Tagged`, but a
    /// typed input flows in unboxed, so its boxed bits are placed in the phi's
    /// home here. An `Int32` / `Bool` input was already copied (raw) into the phi
    /// home by the GP parallel moves above, so it is re-boxed in place. A
    /// `Float64` input lives in an FP register that the GP parallel moves cannot
    /// reach (the allocator omits its phi move precisely for this), so its bits
    /// are read from the FP home and `fmov`-ed verbatim into the phi's GP home
    /// (the verbatim bits *are* the boxed double). Boxing into the phi home is
    /// sound because the phi is read only as a `Tagged` value (typed consumers go
    /// through a `Check*` unbox).
    fn emit_edge(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        moves: &[(Location, Location)],
        pred: BlockId,
        succ: BlockId,
    ) -> Result<(), Unsupported> {
        for &(from, to) in moves {
            emit_move(ops, from, to, MOVE_SCRATCH);
        }
        // Box typed phi inputs in their phi homes.
        let Some(pred_idx) = graph.block(succ).preds.iter().position(|&p| p == pred) else {
            return Ok(());
        };
        for &phi in &graph.block(succ).phis {
            let inputs = graph.node(phi).kind.inputs();
            let Some(&input) = inputs.get(pred_idx) else {
                continue;
            };
            let input_repr = graph.node(input).repr;
            if input_repr == Repr::Tagged {
                continue; // already boxed.
            }
            // A typed phi input that is itself dead (no home) cannot have been
            // moved in; the phi would then have no value — abort to the baseline.
            let phi_home = require_loc(alloc, phi)?;
            let input_home = require_loc(alloc, input)?;
            if input_repr == Repr::Float64 {
                // The FP-resident input had no GP parallel move; read its FP home,
                // box the double (offset applied), and store into the phi's GP home.
                load_fp_loc(ops, FP_LOAD_SCRATCH, input_home);
                emit_box_double(ops, FP_LOAD_SCRATCH, BOX_SCRATCH);
                store_loc(ops, phi_home, BOX_SCRATCH);
            } else {
                // Int32 / Bool: the raw bits are already in the phi home (GP
                // parallel move); re-box in place.
                load_loc(ops, BOX_SCRATCH, phi_home);
                box_value(ops, BOX_SCRATCH, input_repr, MOVE_SCRATCH);
                store_loc(ops, phi_home, BOX_SCRATCH);
            }
        }
        Ok(())
    }

    /// Box the unboxed value in `xr` (per its SSA `repr`) into a tagged `Value`
    /// in `xr`, using `scratch_x` for the tag immediate. `Tagged` values are
    /// already boxed and left unchanged.
    fn box_value(ops: &mut Assembler, xr: u32, repr: Repr, scratch_x: u32) {
        match repr {
            Repr::Int32 => {
                // The producer wrote `xr` through its W view, zeroing bits 63:32;
                // OR in the int32 tag.
                dynasm!(ops
                    ; .arch aarch64
                    ; movz X(scratch_x), NUMBER_TAG_HI16, lsl #48
                    ; orr X(xr), X(xr), X(scratch_x)
                );
            }
            Repr::Bool => {
                // 0/1 predicate → the `VALUE_FALSE` / `VALUE_TRUE` immediate
                // word: add the false base (through `scratch_x`, since a
                // dynamic-register immediate add is not expressible here). The W
                // write zeroes bits 63:32, so the result is the full value word.
                dynasm!(ops
                    ; .arch aarch64
                    ; movz W(scratch_x), VALUE_FALSE as u32
                    ; add W(xr), W(xr), W(scratch_x)
                );
            }
            Repr::Tagged => {}
            // A `Float64` value lives in an FP register, not in `xr`; its boxed
            // form is produced by `box_into_gp` (verbatim `fmov`), which never
            // routes through here.
            Repr::Float64 => unreachable!("float boxing goes through box_into_gp"),
        }
    }

    /// The home of `value`, or an `Unallocated` error when it is needed but the
    /// allocator gave it no home (a wild access would be unsound — abort the
    /// compile so the VM falls back to the baseline).
    fn require_loc(alloc: &Allocation, value: NodeId) -> Result<Location, Unsupported> {
        alloc
            .location
            .get(&value)
            .copied()
            .ok_or(Unsupported::Unallocated)
    }

    /// Load and guard a tagged object receiver. Leaves:
    /// - `x0`: decompressed body pointer
    /// - `x1`: cage base
    /// - `w2`: body type tag
    fn emit_recv_body(
        ops: &mut Assembler,
        view: &JitFunctionView,
        recv_loc: Location,
        exit: DynamicLabel,
    ) {
        load_loc(ops, 0, recv_loc);
        dynasm!(ops
            ; .arch aarch64
            ; movz x4, NUMBER_TAG_HI16, lsl #48
            ; orr x4, x4, #value_tag::OTHER_TAG   // NOT_CELL_MASK
            ; tst x0, x4
            ; b.ne =>exit                         // not a heap cell
            ; mov w0, w0
        );
        emit_load_u64(ops, 1, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x0, x1, x0
            ; ldrb w2, [x0]
        );
    }

    /// Lower speculative Array `.length` to an int32 result, deoptimizing on
    /// non-Array receiver or lengths outside the int32 fast path.
    fn emit_array_length_load(
        ops: &mut Assembler,
        view: &JitFunctionView,
        recv_loc: Location,
        dst: Option<Location>,
        exit: DynamicLabel,
    ) {
        emit_recv_body(ops, view, recv_loc, exit);
        let array_tag = u32::from(view.ta_layout.array_type_tag);
        let length_byte = view.ta_layout.array_length_byte;
        dynasm!(ops
            ; .arch aarch64
            ; cmp w2, array_tag
            ; b.ne =>exit
            ; ldr x3, [x0, length_byte]
        );
        emit_load_u64(ops, 4, i32::MAX as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x3, x4
            ; b.hi =>exit
            ; mov w3, w3
        );
        if let Some(loc) = dst {
            store_loc(ops, loc, 3);
        }
    }

    /// Lower speculative dense-array / typed-array `recv[idx]` fast paths.
    /// Every miss deopts to the interpreter at the `LoadElement` byte-PC.
    fn emit_element_load(
        ops: &mut Assembler,
        view: &JitFunctionView,
        recv_loc: Location,
        idx_loc: Location,
        dst: Option<Location>,
        exit: DynamicLabel,
    ) {
        let array_tag = u32::from(view.ta_layout.array_type_tag);
        let ta_tag = u32::from(view.ta_layout.ta_type_tag);
        let local_buf_type_tag = u32::from(view.ta_layout.local_buffer_type_tag);
        let kind_float64 = view.ta_layout.kind_float64;
        let kind_int32 = view.ta_layout.kind_int32;
        let buffer_local_tag = view.ta_layout.buffer_local_tag;
        let ta_kind_byte = view.ta_layout.ta_kind_byte;
        let ta_byte_offset_byte = view.ta_layout.ta_byte_offset_byte;
        let ta_length_byte = view.ta_layout.ta_length_byte;
        let ta_length_tracking_byte = view.ta_layout.ta_length_tracking_byte;
        let buffer_disc_byte = view.ta_layout.buffer_disc_byte;
        let buffer_handle_byte = view.ta_layout.buffer_handle_byte;
        let (ptr_word, len_word) = vec_layout_offsets();
        let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
        let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
        let bytes_ptr_byte = view.ta_layout.buf_bytes_byte + ptr_word;
        let bytes_len_byte = view.ta_layout.buf_bytes_byte + len_word;
        let hole_bits = VALUE_HOLE;
        let array_path = ops.new_dynamic_label();
        let ta_path = ops.new_dynamic_label();
        let f64_path = ops.new_dynamic_label();
        let i32_path = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();

        emit_recv_body(ops, view, recv_loc, exit);
        load_loc(ops, 5, idx_loc);
        dynasm!(ops
            ; .arch aarch64
            ; mov w5, w5
            ; cmp w2, array_tag
            ; b.eq =>array_path
            ; cmp w2, ta_tag
            ; b.eq =>ta_path
            ; b =>exit
        );

        let arr_exotic_byte = view.ta_layout.array_exotic_byte;
        dynasm!(ops
            ; .arch aarch64
            ; =>array_path
            ; ldr x3, [x0, arr_exotic_byte]
            ; cbnz x3, =>exit                 // exotic sidecar → not ordinary dense
            ; ldr x3, [x0, arr_len_byte]
            ; cmp x5, x3
            ; b.hs =>exit
            ; ldr x3, [x0, arr_ptr_byte]
            ; lsl x4, x5, #3
            ; add x4, x3, x4
            ; ldr x6, [x4]
        );
        emit_load_u64(ops, 7, hole_bits);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x6, x7
            ; b.eq =>exit
        );
        if let Some(loc) = dst {
            store_loc(ops, loc, 6);
        }
        dynasm!(ops ; .arch aarch64 ; b =>done);

        dynasm!(ops
            ; .arch aarch64
            ; =>ta_path
            ; ldrb w3, [x0, ta_length_tracking_byte]
            ; cbnz w3, =>exit
            ; ldr x3, [x0, ta_length_byte]
            ; cmp x5, x3
            ; b.hs =>exit
            ; ldr w3, [x0, buffer_disc_byte]
            ; movz w4, buffer_local_tag
            ; cmp w3, w4
            ; b.ne =>exit
            ; ldr w3, [x0, buffer_handle_byte]
            ; add x3, x1, x3
            ; ldrb w4, [x3]
            ; cmp w4, local_buf_type_tag
            ; b.ne =>exit
            ; ldr x6, [x3, bytes_ptr_byte]
            ; ldr x7, [x3, bytes_len_byte]
            ; ldr x3, [x0, ta_byte_offset_byte]
            ; ldr w4, [x0, ta_kind_byte]
            ; cmp w4, kind_float64
            ; b.eq =>f64_path
            ; cmp w4, kind_int32
            ; b.eq =>i32_path
            ; b =>exit
            ; =>f64_path
            ; lsl x4, x5, #3
            ; add x4, x4, x3
            ; add x0, x4, #8
            ; cmp x0, x7
            ; b.hi =>exit
            ; add x4, x6, x4
            ; ldr D(FP_LOAD_SCRATCH), [x4]
        );
        emit_box_double(ops, FP_LOAD_SCRATCH, 6);
        if let Some(loc) = dst {
            store_loc(ops, loc, 6);
        }
        dynasm!(ops
            ; .arch aarch64
            ; b =>done
            ; =>i32_path
            ; lsl x4, x5, #2
            ; add x4, x4, x3
            ; add x0, x4, #4
            ; cmp x0, x7
            ; b.hi =>exit
            ; add x4, x6, x4
            ; ldr w6, [x4]
            ; movz x7, NUMBER_TAG_HI16, lsl #48
            ; orr x6, x6, x7
        );
        if let Some(loc) = dst {
            store_loc(ops, loc, 6);
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
    }

    /// Lower speculative dense-array / typed-array `recv[idx] = value` fast
    /// paths. The optimizing tier has no safepoints, so every miss deopts
    /// instead of calling the VM store stub.
    fn emit_element_store(
        ops: &mut Assembler,
        view: &JitFunctionView,
        recv_loc: Location,
        idx_loc: Location,
        value_loc: Location,
        value_repr: Repr,
        exit: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let array_tag = u32::from(view.ta_layout.array_type_tag);
        let ta_tag = u32::from(view.ta_layout.ta_type_tag);
        let local_buf_type_tag = u32::from(view.ta_layout.local_buffer_type_tag);
        let kind_float64 = view.ta_layout.kind_float64;
        let kind_int32 = view.ta_layout.kind_int32;
        let buffer_local_tag = view.ta_layout.buffer_local_tag;
        let ta_kind_byte = view.ta_layout.ta_kind_byte;
        let ta_byte_offset_byte = view.ta_layout.ta_byte_offset_byte;
        let ta_length_byte = view.ta_layout.ta_length_byte;
        let ta_length_tracking_byte = view.ta_layout.ta_length_tracking_byte;
        let buffer_disc_byte = view.ta_layout.buffer_disc_byte;
        let buffer_handle_byte = view.ta_layout.buffer_handle_byte;
        let (ptr_word, len_word) = vec_layout_offsets();
        let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
        let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
        let arr_length_byte = view.ta_layout.array_length_byte;
        let arr_exotic_byte = view.ta_layout.array_exotic_byte;
        let bytes_ptr_byte = view.ta_layout.buf_bytes_byte + ptr_word;
        let bytes_len_byte = view.ta_layout.buf_bytes_byte + len_word;
        let array_path = ops.new_dynamic_label();
        let f64_path = ops.new_dynamic_label();
        let i32_path = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();

        emit_recv_body(ops, view, recv_loc, exit);
        load_loc(ops, 5, idx_loc);
        dynasm!(ops
            ; .arch aarch64
            ; mov w5, w5
            ; cmp w2, array_tag
            ; b.eq =>array_path
            ; cmp w2, ta_tag
            ; b.ne =>exit
            ; ldrb w3, [x0, ta_length_tracking_byte]
            ; cbnz w3, =>exit
            ; ldr x3, [x0, ta_length_byte]
            ; cmp x5, x3
            ; b.hs =>exit
            ; ldr w3, [x0, buffer_disc_byte]
            ; movz w4, buffer_local_tag
            ; cmp w3, w4
            ; b.ne =>exit
            ; ldr w3, [x0, buffer_handle_byte]
            ; add x3, x1, x3
            ; ldrb w4, [x3]
            ; cmp w4, local_buf_type_tag
            ; b.ne =>exit
            ; ldr x6, [x3, bytes_ptr_byte]
            ; ldr x7, [x3, bytes_len_byte]
            ; ldr x3, [x0, ta_byte_offset_byte]
            ; ldr w4, [x0, ta_kind_byte]
            ; cmp w4, kind_float64
            ; b.eq =>f64_path
            ; cmp w4, kind_int32
            ; b.eq =>i32_path
            ; b =>exit
            ; =>array_path
            ; ldr x3, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
            ; ldrb w3, [x3]
            ; cbnz w3, =>exit
            ; ldr x3, [x0, arr_exotic_byte]
            ; cbnz x3, =>exit
            ; ldr x3, [x0, arr_len_byte]
            ; cmp x5, x3
            ; b.hs =>exit
            ; ldr x3, [x0, arr_length_byte]
            ; cmp x5, x3
            ; b.hs =>exit
            ; ldr x4, [x0, arr_ptr_byte]
            ; lsl x3, x5, #3
            ; add x4, x4, x3
        );
        match value_repr {
            Repr::Int32 => {
                load_loc(ops, 6, value_loc);
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x7, NUMBER_TAG_HI16, lsl #48
                    ; orr x6, x6, x7
                    ; str x6, [x4]
                    ; b =>done
                );
            }
            Repr::Float64 => {
                load_fp_loc(ops, FP_LOAD_SCRATCH, value_loc);
                emit_box_double(ops, FP_LOAD_SCRATCH, 6);
                dynasm!(ops
                    ; .arch aarch64
                    ; str x6, [x4]
                    ; b =>done
                );
            }
            _ => return Err(Unsupported::Unlowered("store-element value not int32/f64")),
        }
        dynasm!(ops
            ; .arch aarch64
            ; =>f64_path
            ; lsl x4, x5, #3
            ; add x4, x4, x3
            ; add x0, x4, #8
            ; cmp x0, x7
            ; b.hi =>exit
            ; add x4, x6, x4
        );
        match value_repr {
            Repr::Float64 => load_fp_loc(ops, FP_LOAD_SCRATCH, value_loc),
            Repr::Int32 => {
                load_loc(ops, 0, value_loc);
                dynasm!(ops ; .arch aarch64 ; scvtf D(FP_LOAD_SCRATCH), w0);
            }
            _ => return Err(Unsupported::Unlowered("store-element value not int32/f64")),
        }
        dynasm!(ops
            ; .arch aarch64
            ; str D(FP_LOAD_SCRATCH), [x4]
            ; b =>done
            ; =>i32_path
            ; lsl x4, x5, #2
            ; add x4, x4, x3
            ; add x0, x4, #4
            ; cmp x0, x7
            ; b.hi =>exit
            ; add x4, x6, x4
        );
        match value_repr {
            Repr::Int32 => {
                load_loc(ops, 0, value_loc);
                dynasm!(ops ; .arch aarch64 ; str w0, [x4]);
            }
            Repr::Float64 => {
                dynasm!(ops ; .arch aarch64 ; b =>exit);
            }
            _ => return Err(Unsupported::Unlowered("store-element value not int32/f64")),
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    fn emit_frame_materialize(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        point: &DeoptPoint,
        box_scratch: u32,
    ) -> Result<(), Unsupported> {
        emit_frame_materialize_where(ops, graph, alloc, point, box_scratch, |_, _| true)
    }

    /// Materialize only the live pointer (`Tagged`) values of `point` into
    /// `[x19]`. A GC point (an allocating op or a call that can scavenge) roots
    /// the caller's live pointers through the wholesale window scan, so only
    /// tagged slots must be refreshed to their current value there; a live
    /// non-pointer (`Int32` / `Float64` / `Bool`) that crosses the call already
    /// survives unboxed in its spill/callee-saved home, and its stale `[x19]`
    /// slot is GC-safe (a non-pointer is never traced, and a stale pointer left
    /// in the slot is still rooted by that very slot, so it is relocated in place
    /// and never dangles). The full frame — non-pointers included — is
    /// reconstructed from homes only at the cold deopt/bail exit. Mirrors the
    /// Maglev safepoint, which records only tagged registers/slots.
    fn emit_frame_materialize_tagged(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        point: &DeoptPoint,
        box_scratch: u32,
    ) -> Result<(), Unsupported> {
        emit_frame_materialize_where(ops, graph, alloc, point, box_scratch, |_, value| {
            graph.node(value).repr == Repr::Tagged
        })
    }

    /// Box and store into the interpreter window `[x19]` exactly the live
    /// registers of `point` for which `keep(regn)` holds. A frameless
    /// self-recursive call splits its frame state: the register subset the
    /// recursion reads from `[x19]` (args + callee) is written on the hot path,
    /// and the complement — values live across the call but only read when the
    /// function bails to the interpreter — is written on the cold bail exit, so
    /// the fast recursion pays no box for a value it merely holds live. The
    /// frameless subset allocates nothing, so no GC observes `[x19]` between the
    /// two writes; the reconstructed frame is identical on every path that reads
    /// it (a guard-miss bail, a stack overflow, or a later op's frame state).
    fn emit_frame_materialize_where(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        point: &DeoptPoint,
        box_scratch: u32,
        keep: impl Fn(u16, NodeId) -> bool,
    ) -> Result<(), Unsupported> {
        for &(regn, value) in &point.registers {
            if !keep(regn, value) {
                continue;
            }
            let node = graph.node(value);
            if !emit_rematerialized_boxed(ops, &node.kind, box_scratch, MOVE_SCRATCH) {
                let loc = require_loc(alloc, value)?;
                box_into_gp(ops, box_scratch, node.repr, loc, MOVE_SCRATCH);
            }
            let off = u32::from(regn) * 8;
            dynasm!(ops ; .arch aarch64 ; str X(box_scratch), [x19, off]);
        }
        Ok(())
    }

    fn emit_frame_reload(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        point: &DeoptPoint,
        skip_reg: Option<u16>,
        box_scratch: u32,
    ) -> Result<(), Unsupported> {
        for &(regn, value) in &point.registers {
            if Some(regn) == skip_reg {
                continue;
            }
            let Some(&loc) = alloc.location.get(&value) else {
                continue;
            };
            let off = u32::from(regn) * 8;
            dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
            match graph.node(value).repr {
                Repr::Tagged => store_loc(ops, loc, box_scratch),
                Repr::Int32 => {
                    dynasm!(ops ; .arch aarch64 ; mov W(box_scratch), W(box_scratch));
                    store_loc(ops, loc, box_scratch);
                }
                Repr::Float64 => {
                    // Unbox: `emit_frame_materialize` stored the boxed double
                    // (bits + encode offset), so subtract the offset before the
                    // value re-enters its FP home.
                    dynasm!(ops
                        ; .arch aarch64
                        ; movz X(MOVE_SCRATCH), DOUBLE_OFFSET_HI16, lsl #48
                        ; sub X(box_scratch), X(box_scratch), X(MOVE_SCRATCH)
                        ; fmov D(FP_LOAD_SCRATCH), X(box_scratch)
                    );
                    store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                }
                Repr::Bool => {
                    let is_true = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    emit_load_u64(ops, MOVE_SCRATCH, VALUE_TRUE);
                    dynasm!(ops
                        ; .arch aarch64
                        ; cmp X(box_scratch), X(MOVE_SCRATCH)
                        ; b.eq =>is_true
                        ; movz W(box_scratch), #0
                        ; b =>done
                        ; =>is_true
                        ; movz W(box_scratch), #1
                        ; =>done
                    );
                    store_loc(ops, loc, box_scratch);
                }
            }
        }
        Ok(())
    }

    /// Reload only the live pointer (`Tagged`) values of `point` from `[x19]`
    /// into their homes after a call/allocation. Pairs with
    /// [`emit_frame_materialize_tagged`]: a moving GC may have relocated a
    /// pointer, and the wholesale window scan rewrote the tagged `[x19]` slot to
    /// the new address, so the pointer must be reloaded from there. A non-pointer
    /// value is untouched by the collector and survived the call unboxed in its
    /// spill/callee-saved home, so it is left in place rather than round-tripped
    /// through the tagged window.
    fn emit_frame_reload_tagged(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        point: &DeoptPoint,
        skip_reg: Option<u16>,
        box_scratch: u32,
    ) -> Result<(), Unsupported> {
        for &(regn, value) in &point.registers {
            if Some(regn) == skip_reg || graph.node(value).repr != Repr::Tagged {
                continue;
            }
            let Some(&loc) = alloc.location.get(&value) else {
                continue;
            };
            let off = u32::from(regn) * 8;
            dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
            store_loc(ops, loc, box_scratch);
        }
        Ok(())
    }

    fn value_is_used_after(
        graph: &Graph,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        value: NodeId,
    ) -> bool {
        for block in &graph.blocks {
            for &phi in &block.phis {
                if graph.node(phi).kind.inputs().contains(&value) {
                    return true;
                }
            }
            for &nid in &block.body {
                if nid != value && graph.node(nid).kind.inputs().contains(&value) {
                    return true;
                }
            }
            match &block.term {
                Some(Terminator::Return(v)) if *v == value => return true,
                Some(Terminator::Branch { cond, .. }) if *cond == value => return true,
                _ => {}
            }
        }
        frames
            .values()
            .any(|point| point.registers.iter().any(|&(_, v)| v == value))
    }

    fn emit_status_stub_call(ops: &mut Assembler, addr: usize, threw: DynamicLabel) {
        emit_load_u64(ops, 16, addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x0, =>threw
        );
    }

    fn load_frame_reg(ops: &mut Assembler, target: u32, regn: u16) -> Result<(), Unsupported> {
        let off = frame_reg_offset(regn)?;
        dynasm!(ops ; .arch aarch64 ; ldr X(target), [x19, off]);
        Ok(())
    }

    fn store_frame_reg(ops: &mut Assembler, source: u32, regn: u16) -> Result<(), Unsupported> {
        let off = frame_reg_offset(regn)?;
        dynasm!(ops ; .arch aarch64 ; str X(source), [x19, off]);
        Ok(())
    }

    fn frame_reg_offset(regn: u16) -> Result<u32, Unsupported> {
        let off = u32::from(regn) * 8;
        if off > 32760 {
            return Err(Unsupported::OperandShape("frame register range"));
        }
        Ok(off)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_opt_live_collection_leaf_method_guarded_call(
        ops: &mut Assembler,
        view: &JitFunctionView,
        dst_reg: u16,
        recv_reg: u16,
        site: u64,
        arg_regs: &[u16],
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }

        let key = arg_regs.first().copied();
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

        load_frame_reg(ops, 9, recv_reg)?;
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
            ; ldr x0, [x20, GC_HEAP_OFFSET]
            ; mov x1, x11
        );
        load_frame_reg(ops, 2, recv_reg)?;
        if let Some(key) = key {
            load_frame_reg(ops, 3, key)?;
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
        store_frame_reg(ops, 0, dst_reg)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_opt_live_collection_alloc_method_guarded_call(
        ops: &mut Assembler,
        view: &JitFunctionView,
        dst_reg: u16,
        recv_reg: u16,
        site: u64,
        arg_regs: &[u16],
        safepoint: SafepointId,
        miss: DynamicLabel,
        done: DynamicLabel,
    ) -> Result<bool, Unsupported> {
        if view.cage_base == 0 {
            return Ok(false);
        }

        let arg0 = arg_regs.first().copied();
        let arg1 = arg_regs.get(1).copied();
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

        load_frame_reg(ops, 9, recv_reg)?;
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
            ; ldr x9, [x20, VM_OFFSET]
            ; str x9, [sp, ALLOC_CTX_VM_OFFSET]
            ; ldr x9, [x20, STACK_OFFSET]
            ; str x9, [sp, ALLOC_CTX_STACK_OFFSET]
            ; ldr x9, [x20, CONTEXT_OFFSET]
            ; str x9, [sp, ALLOC_CTX_CONTEXT_OFFSET]
            ; ldr x9, [x20, SAFEPOINT_RECORDS_OFFSET]
            ; str x9, [sp, ALLOC_CTX_SAFEPOINT_RECORDS_OFFSET]
            ; ldr w9, [x20, SAFEPOINT_COUNT_OFFSET]
            ; str w9, [sp, ALLOC_CTX_SAFEPOINT_COUNT_OFFSET]
            ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
            ; ldr x9, [x20, FRAME_INDEX_OFFSET]
            ; str x9, [sp, ALLOC_CTX_FRAME_INDEX_OFFSET]
            ; str x19, [sp, ALLOC_CTX_FRAME_SLOTS_OFFSET]
            ; movz w9, view.register_count as u32
            ; strh w9, [sp, ALLOC_CTX_FRAME_SLOT_COUNT_OFFSET]
            ; movz w9, #0
            ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
            ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]

            ; mov x0, sp
        );
        emit_load_u64(ops, 2, u64::from(safepoint));
        load_frame_reg(ops, 3, recv_reg)?;
        if let Some(arg0) = arg0 {
            load_frame_reg(ops, 4, arg0)?;
        } else {
            emit_load_u64(ops, 4, undefined_bits);
        }
        if let Some(arg1) = arg1 {
            emit_load_u64(ops, 5, undefined_bits);
            let set_add = ops.new_dynamic_label();
            emit_load_u64(ops, 9, u64::from(STUB_COLLECTION_SET_ADD_ALLOC.id));
            dynasm!(ops ; .arch aarch64 ; cmp x1, x9 ; b.eq =>set_add);
            load_frame_reg(ops, 5, arg1)?;
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
        store_frame_reg(ops, 0, dst_reg)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
        Ok(true)
    }

    /// Enter a prepared direct-call callee through the single shared tail in the
    /// baseline emitter (see `baseline::arm64::emit_direct_call_tail`). The
    /// optimizing tier deliberately keeps no copy of this sequence: the callee
    /// `JitCtx` — including the isolate-boundary `gc_heap` / safepoint /
    /// collection-IC / array-index-protector fields — is built from one source,
    /// so an optimizing callee can never enter compiled code with those slots
    /// left as stack garbage.
    fn emit_direct_call_tail(
        ops: &mut Assembler,
        dst_reg: u16,
        threw: DynamicLabel,
        done: DynamicLabel,
    ) {
        crate::baseline::arm64::emit_direct_call_tail(ops, dst_reg, threw, done);
    }

    /// Emit the function prologue (copied from the baseline) then reserve the
    /// spill area. Returns the byte size subtracted from `sp` (0 when no spill
    /// area is needed).
    /// One callee-saved allocator register to preserve across the function: its
    /// physical register number, whether it is FP, and the byte offset from `sp`
    /// of its save slot.
    #[derive(Clone, Copy)]
    struct CalleeSavedReg {
        phys: u32,
        is_fp: bool,
        off: u32,
    }

    fn emit_prologue(ops: &mut Assembler, spill_bytes: u32, callee_saved: &[CalleeSavedReg]) {
        dynasm!(ops
            ; .arch aarch64
            ; stp x29, x30, [sp, #-32]!
            ; stp x19, x20, [sp, #16]
            ; mov x29, sp
            ; mov x20, x0
            ; ldr x19, [x20]
        );
        if spill_bytes != 0 {
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, spill_bytes);
        }
        // Preserve the callee-saved allocator registers this body uses, at fixed
        // offsets above the spill / poll-save area.
        for cs in callee_saved {
            if cs.is_fp {
                dynasm!(ops ; .arch aarch64 ; str D(cs.phys), [sp, cs.off]);
            } else {
                dynasm!(ops ; .arch aarch64 ; str X(cs.phys), [sp, cs.off]);
            }
        }
    }

    /// Emit the function epilogue: restore callee-saved allocator registers, undo
    /// the spill reservation, restore the saved frame, and return. `x0` (value)
    /// and `x1` (status) must be set.
    fn emit_epilogue(ops: &mut Assembler, spill_bytes: u32, callee_saved: &[CalleeSavedReg]) {
        for cs in callee_saved {
            if cs.is_fp {
                dynasm!(ops ; .arch aarch64 ; ldr D(cs.phys), [sp, cs.off]);
            } else {
                dynasm!(ops ; .arch aarch64 ; ldr X(cs.phys), [sp, cs.off]);
            }
        }
        if spill_bytes != 0 {
            dynasm!(ops ; .arch aarch64 ; add sp, sp, spill_bytes);
        }
        dynasm!(ops
            ; .arch aarch64
            ; ldp x19, x20, [sp, #16]
            ; ldp x29, x30, [sp], #32
            ; ret
        );
    }

    fn emit_osr_type_miss(
        ops: &mut Assembler,
        fail: DynamicLabel,
        byte_pc: u32,
        spill_bytes: u32,
        box_scratch: u32,
        callee_saved: &[CalleeSavedReg],
    ) {
        dynasm!(ops ; .arch aarch64 ; =>fail);
        emit_load_u64(ops, box_scratch, u64::from(byte_pc));
        dynasm!(ops ; .arch aarch64 ; str W(box_scratch), [x20, BAIL_PC_OFFSET]);
        dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_BAILED as u32);
        emit_epilogue(ops, spill_bytes, callee_saved);
    }

    fn emit_osr_reload(
        ops: &mut Assembler,
        repr: Repr,
        home: Location,
        src_off: u32,
        fail: DynamicLabel,
    ) {
        dynasm!(ops ; .arch aarch64 ; ldr X(BOX_SCRATCH), [x19, src_off]);
        match repr {
            Repr::Tagged => store_loc(ops, home, BOX_SCRATCH),
            Repr::Int32 => {
                dynasm!(ops
                    ; .arch aarch64
                    ; and x16, X(BOX_SCRATCH), #value_tag::NUMBER_TAG
                    ; eor x16, x16, #value_tag::NUMBER_TAG
                    ; cbnz x16, =>fail
                    ; mov W(BOX_SCRATCH), W(BOX_SCRATCH)
                );
                store_loc(ops, home, BOX_SCRATCH);
            }
            Repr::Float64 => {
                let int32_path = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; and x16, X(BOX_SCRATCH), #value_tag::NUMBER_TAG
                    ; cbz x16, =>fail
                    ; eor x16, x16, #value_tag::NUMBER_TAG
                    ; cbz x16, =>int32_path
                    ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
                    ; sub x16, X(BOX_SCRATCH), x16
                    ; fmov D(FP_LOAD_SCRATCH), x16
                    ; b =>done
                    ; =>int32_path
                    ; scvtf D(FP_LOAD_SCRATCH), W(BOX_SCRATCH)
                    ; =>done
                );
                store_fp_loc(ops, home, FP_LOAD_SCRATCH);
            }
            Repr::Bool => {
                let is_true = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_FALSE);
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp X(BOX_SCRATCH), X(MOVE_SCRATCH)
                    ; b.ne =>is_true
                    ; movz W(BOX_SCRATCH), #0
                    ; b =>done
                    ; =>is_true
                );
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_TRUE);
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp X(BOX_SCRATCH), X(MOVE_SCRATCH)
                    ; b.ne =>fail
                    ; movz W(BOX_SCRATCH), #1
                    ; =>done
                );
                store_loc(ops, home, BOX_SCRATCH);
            }
        }
    }

    /// Lower a register-allocated graph to native arm64.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::optimizing) fn emit(
        view: &JitFunctionView,
        graph: &Graph,
        liveness: &Liveness,
        alloc: &Allocation,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        call_resume_frames: &FxHashMap<NodeId, DeoptPoint>,
        block_deopts: &FxHashMap<BlockId, DeoptPoint>,
        osr_entries: &[OsrEntry],
    ) -> Result<OptimizedCode, Unsupported> {
        let mut ops = Assembler::new().expect("assembler alloc");

        // Spill area: one frame slot per value spill slot plus one parallel-move
        // cycle-break scratch slot (`Spill(spill_slots)`). Backedge polls call a
        // Rust ABI leaf stub, so reserve an additional save area for every
        // *caller-saved* allocatable GP/FP register (callee-saved survive the
        // C-ABI poll stub untouched).
        let value_spill_bytes = align16((alloc.spill_slots + 1) * 8);
        let poll_save_base = value_spill_bytes;
        let poll_save_bytes = (super::CALLER_SAVED_GP + super::CALLER_SAVED_FP) * 8;
        // Callee-saved registers the allocator actually used. The function's
        // prologue must preserve them for its own caller (the Rust `JitEntry`
        // boundary) and, for a self-recursive `bl`, for the calling frame; a
        // value live across a call can then survive in one without a frame
        // round-trip. Save only the used ones so leaf functions pay nothing.
        let mut cs_gp: Vec<u32> = Vec::new();
        let mut cs_fp: Vec<u32> = Vec::new();
        for (&value, &loc) in &alloc.location {
            if let Location::Reg(i) = loc {
                if matches!(graph.node(value).repr, Repr::Float64) {
                    if i >= super::CALLER_SAVED_FP {
                        cs_fp.push(i);
                    }
                } else if i >= super::CALLER_SAVED_GP {
                    cs_gp.push(i);
                }
            }
        }
        cs_gp.sort_unstable();
        cs_gp.dedup();
        cs_fp.sort_unstable();
        cs_fp.dedup();
        let cs_base = value_spill_bytes + poll_save_bytes;
        let mut callee_saved: Vec<CalleeSavedReg> = Vec::new();
        for (slot, &i) in cs_gp.iter().chain(std::iter::empty()).enumerate() {
            callee_saved.push(CalleeSavedReg {
                phys: phys(i),
                is_fp: false,
                off: cs_base + slot as u32 * 8,
            });
        }
        let gp_slots = cs_gp.len() as u32;
        for (slot, &i) in cs_fp.iter().enumerate() {
            callee_saved.push(CalleeSavedReg {
                phys: phys_fp(i),
                is_fp: true,
                off: cs_base + (gp_slots + slot as u32) * 8,
            });
        }
        let cs_save_bytes = callee_saved.len() as u32 * 8;
        let spill_bytes = align16(value_spill_bytes + poll_save_bytes + cs_save_bytes);
        let callee_saved = callee_saved.as_slice();

        // One dynamic label per block, addressed by BlockId.
        let block_labels: Vec<DynamicLabel> = (0..graph.blocks.len())
            .map(|_| ops.new_dynamic_label())
            .collect();
        // One cold deopt-exit label per deopt-capable node (filled after the body).
        let mut deopt_labels: FxHashMap<NodeId, DynamicLabel> = FxHashMap::default();
        let threw = ops.new_dynamic_label();

        // Fast lookup: edge moves keyed by (pred, succ).
        let mut edge_index: FxHashMap<(BlockId, BlockId), &EdgeMoves> = FxHashMap::default();
        for em in &alloc.edge_moves {
            edge_index.insert((em.pred, em.succ), em);
        }
        let edge_moves_for = |pred: BlockId, succ: BlockId| -> &[(Location, Location)] {
            edge_index
                .get(&(pred, succ))
                .map(|em| em.moves.as_slice())
                .unwrap_or(&[])
        };

        let self_entry = ops.new_dynamic_label();
        let entry = ops.offset();
        dynasm!(&mut ops ; .arch aarch64 ; =>self_entry);
        emit_prologue(&mut ops, spill_bytes, callee_saved);

        // Entry param load: each per-register entry def that has a home is a
        // boxed Tagged value sitting in `[x19, r*8]`. Load it into its home so
        // later reads find it where the allocator placed it. ConstUndefined
        // entry defs likewise carry their tagged value; load the frame slot
        // (which holds `undefined` for an uninitialized local on entry).
        // Entry param load: the boxed parameters arrive in the frame register
        // window. Load each live `Param(r)` (the leading entry-body defs, in
        // register order) from `[x19, r*8]` into its allocated home; it stays a
        // boxed `Tagged` value there. `ConstUndefined` entry defs are not loaded
        // from the (possibly uninitialized) frame slot — they are materialized as
        // `undefined` by `lower_node` if live.
        let entry_block = graph.block(graph.entry);
        let rc = graph.register_count as usize;
        for (r, &nid) in entry_block.body.iter().take(rc).enumerate() {
            if !matches!(graph.node(nid).kind, NodeKind::Param(_)) {
                continue;
            }
            let Some(&loc) = alloc.location.get(&nid) else {
                continue; // dead: never read.
            };
            let off = (r as u32) * 8;
            dynasm!(&mut ops ; .arch aarch64 ; ldr X(BOX_SCRATCH), [x19, off]);
            store_loc(&mut ops, loc, BOX_SCRATCH);
        }

        // Walk blocks in liveness RPO. The entry block falls through from the
        // prologue; every other block is reached via a branch.
        for (ord_idx, &b) in liveness.rpo.iter().enumerate() {
            let next_block = liveness.rpo.get(ord_idx + 1).copied();
            let block = graph.block(b);
            dynasm!(&mut ops ; .arch aarch64 ; =>block_labels[b as usize]);

            // Phi heads carry no code (edge moves place their values). Lower the
            // body in order. Entry per-register `Param` defs lower to nothing
            // (their boxed value was loaded into its home above); an entry
            // `ConstUndefined` re-materializes the same `undefined` it already
            // holds — both are harmless to revisit here.
            for &nid in &block.body {
                lower_node(
                    &mut ops,
                    view,
                    graph,
                    alloc,
                    frames,
                    call_resume_frames,
                    &mut deopt_labels,
                    self_entry,
                    threw,
                    nid,
                    spill_bytes,
                    BOX_SCRATCH,
                    callee_saved,
                )?;
            }

            // Terminator.
            match block
                .term
                .as_ref()
                .ok_or(Unsupported::Unlowered("block without terminator"))?
            {
                Terminator::Return(v) => {
                    let loc = require_loc(alloc, *v)?;
                    let repr = graph.node(*v).repr;
                    box_into_gp(&mut ops, 0, repr, loc, BOX_SCRATCH);
                    dynasm!(&mut ops ; .arch aarch64 ; movz x1, STATUS_RETURNED as u32);
                    emit_epilogue(&mut ops, spill_bytes, callee_saved);
                }
                Terminator::Jump(target) => {
                    if is_backedge(graph, b, *target) {
                        emit_backedge_poll(&mut ops, threw, poll_save_base);
                    }
                    let moves = edge_moves_for(b, *target);
                    emit_edge(&mut ops, graph, alloc, moves, b, *target)?;
                    // Omit the branch only when the target is the very next block
                    // emitted.
                    if next_block != Some(*target) {
                        dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[*target as usize]);
                    }
                }
                Terminator::Branch {
                    cond,
                    on_true,
                    on_false,
                } => {
                    let cloc = require_loc(alloc, *cond)?;
                    load_loc(&mut ops, BOX_SCRATCH, cloc);
                    // The cond is an unboxed Bool (0/1) — the builder declines a
                    // non-boolean (Tagged) branch condition. Test it; route each
                    // edge through its own moves. The false setup is a cold
                    // trampoline so the true edge can fall straight through.
                    let false_setup = ops.new_dynamic_label();
                    let true_moves = edge_moves_for(b, *on_true);
                    let false_moves = edge_moves_for(b, *on_false);
                    if graph.node(*cond).repr == Repr::Bool {
                        dynasm!(&mut ops ; .arch aarch64 ; cbz W(BOX_SCRATCH), =>false_setup);
                    } else {
                        emit_load_u64(&mut ops, MOVE_SCRATCH, VALUE_FALSE);
                        dynasm!(&mut ops
                            ; .arch aarch64
                            ; cmp X(BOX_SCRATCH), X(MOVE_SCRATCH)
                            ; b.eq =>false_setup
                        );
                    }
                    // cond != 0 → true edge. The false trampoline is emitted
                    // immediately after, so the true edge always needs an
                    // explicit branch (it can never fall through). `cond` was
                    // already tested, so the edge boxing may clobber box_scratch.
                    let _ = next_block;
                    if is_backedge(graph, b, *on_true) {
                        emit_backedge_poll(&mut ops, threw, poll_save_base);
                    }
                    emit_edge(&mut ops, graph, alloc, true_moves, b, *on_true)?;
                    dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[*on_true as usize]);
                    // False trampoline: run the false edge's moves then branch.
                    dynasm!(&mut ops ; .arch aarch64 ; =>false_setup);
                    if is_backedge(graph, b, *on_false) {
                        emit_backedge_poll(&mut ops, threw, poll_save_base);
                    }
                    emit_edge(&mut ops, graph, alloc, false_moves, b, *on_false)?;
                    dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[*on_false as usize]);
                }
                Terminator::Deopt(_) => {
                    let point = block_deopts.get(&b).ok_or(Unsupported::Unlowered(
                        "deopt terminator without frame state",
                    ))?;
                    emit_frame_restore_and_bail(
                        &mut ops,
                        graph,
                        alloc,
                        point,
                        spill_bytes,
                        BOX_SCRATCH,
                        callee_saved,
                    )?;
                }
            }
        }

        // Cold deopt exits, after all block bodies. Each restores the live frame
        // registers (boxed), stamps the resume PC, and returns `STATUS_BAILED`.
        emit_deopt_exits(
            &mut ops,
            graph,
            alloc,
            frames,
            &deopt_labels,
            spill_bytes,
            BOX_SCRATCH,
            callee_saved,
        )?;
        dynasm!(&mut ops
            ; .arch aarch64
            ; =>threw
            ; movz x0, #0
            ; movz x1, STATUS_THREW as u32
        );
        emit_epilogue(&mut ops, spill_bytes, callee_saved);

        // OSR-entry trampolines, one per eligible loop header. Each sets up the
        // frame, reloads every live interpreter register from the frame window
        // `[x19, r*8]` into the representation-specific home the header expects,
        // then branches to the header block.
        let mut osr_offsets: rustc_hash::FxHashMap<u32, usize> = rustc_hash::FxHashMap::default();
        for osr in osr_entries {
            // Build the reload set: each register's header value that the header
            // actually reads — its own phis (loop-carried; defined at the header,
            // so absent from live-in) and the live-in invariants. Then require
            // pairwise-distinct homes: if two reloads target the same home (an
            // env/allocation node mismatch), one would clobber the other, so skip
            // the whole header rather than corrupt the frame. Correctness over
            // coverage — an un-OSR'd header still runs in the interpreter, and an
            // enclosing loop's header (cleaner set) can still tier up.
            let phis = &graph.block(osr.block).phis;
            let live_in = &liveness.live_in[osr.block as usize];
            let mut reloads: Vec<(u16, NodeId, Location, Repr)> = Vec::new();
            let mut seen_values: FxHashSet<NodeId> = FxHashSet::default();
            for &(r, v) in &osr.registers {
                if !phis.contains(&v) && !live_in.contains(&v) {
                    continue;
                }
                if !seen_values.insert(v) {
                    continue;
                }
                if let Some(&home) = alloc.location.get(&v) {
                    reloads.push((r, v, home, graph.node(v).repr));
                }
            }
            let mut homes: Vec<(u8, u32)> = reloads
                .iter()
                .map(|&(_, _, h, repr)| match h {
                    Location::Reg(i) if repr == Repr::Float64 => (1, i),
                    Location::Reg(i) => (0, i),
                    Location::Spill(i) => (2, i),
                })
                .collect();
            homes.sort_unstable();
            if homes.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }
            let off = ops.offset();
            let osr_fail = ops.new_dynamic_label();
            emit_prologue(&mut ops, spill_bytes, callee_saved);
            for (r, _, home, repr) in reloads {
                let src_off = u32::from(r) * 8;
                emit_osr_reload(&mut ops, repr, home, src_off, osr_fail);
            }
            dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[osr.block as usize]);
            emit_osr_type_miss(
                &mut ops,
                osr_fail,
                osr.byte_pc,
                spill_bytes,
                BOX_SCRATCH,
                callee_saved,
            );
            osr_offsets.insert(osr.byte_pc, off.0);
        }

        let buf = ops
            .finalize()
            .map_err(|_| Unsupported::Unlowered("assembler finalize failed"))?;
        Ok(OptimizedCode {
            code: CompiledCode::new(buf, entry),
            osr_offsets,
            entry_via_osr_only: !block_deopts.is_empty() || graph.entry != 0,
            safepoint_records: super::optimizing_safepoint_records(view),
        })
    }

    /// Guard a receiver is an ordinary dense `Array` whose `%Array.prototype%`
    /// still carries the original `push` / `pop` builtin at the cached shape +
    /// slot, deoptimizing to `exit` on any miss. Leaves the decompressed array
    /// body pointer in `x0` and the cage base in `x1`; uses `x2..x7` as scratch
    /// (the optimizing tier keeps allocated values in `x9..x15` / `x21..x28`, so
    /// the low registers are free here). Mirrors the baseline guard but with the
    /// optimizing register convention and a deopt exit instead of a bridge miss.
    fn emit_opt_array_dense_proto_guard(
        ops: &mut Assembler,
        view: &JitFunctionView,
        am: &otter_vm::JitArrayMethod,
        recv_loc: Location,
        exit: DynamicLabel,
    ) {
        let array_tag = u32::from(view.ta_layout.array_type_tag);
        let exotic_byte = view.ta_layout.array_exotic_byte;
        let object_shape_byte = view.object_shape_byte;
        let object_values_ptr_byte = view.object_values_ptr_byte;
        let native_static_fn_byte = view.native_static_fn_byte;
        let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
        let method_value_byte = am.method_value_byte;

        load_loc(ops, 0, recv_loc);
        dynasm!(ops
            ; .arch aarch64
            ; movz x4, NUMBER_TAG_HI16, lsl #48
            ; orr x4, x4, #value_tag::OTHER_TAG   // NOT_CELL_MASK
            ; tst x0, x4
            ; b.ne =>exit                         // not a heap cell
            ; mov w0, w0
        );
        emit_load_u64(ops, 1, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x0, x1, x0                  // x0 = array body ptr
            ; ldrb w2, [x0]
            ; cmp w2, array_tag
            ; b.ne =>exit
            ; ldr x3, [x0, exotic_byte]
            ; cbnz x3, =>exit                 // exotic sidecar → not ordinary dense
        );
        emit_load_u64(ops, 4, u64::from(am.proto_offset));
        dynasm!(ops
            ; .arch aarch64
            ; add x4, x1, x4                  // x4 = %Array.prototype% body ptr
            ; ldrb w5, [x4]
            ; cmp w5, OBJECT_BODY_TYPE_TAG
            ; b.ne =>exit
            ; ldr w5, [x4, object_shape_byte]
        );
        emit_load_u64(ops, 6, u64::from(am.proto_shape));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w5, w6
            ; b.ne =>exit
            ; ldr x4, [x4, object_values_ptr_byte]
            ; cbz x4, =>exit
            ; ldr w17, [x4, method_value_byte]   // 4-byte compressed slot
        );
        emit_decompress_slot(ops, view, exit);
        dynasm!(ops
            ; .arch aarch64
            ; mov x5, x17
            ; movz x7, NUMBER_TAG_HI16, lsl #48
            ; orr x7, x7, #value_tag::OTHER_TAG  // NOT_CELL_MASK
            ; tst x5, x7
            ; b.ne =>exit
            ; mov w6, w5
            ; add x6, x1, x6                  // x6 = method fn body ptr
            ; ldrb w7, [x6]
            ; cmp w7, native_function_type_tag
            ; b.ne =>exit
            ; ldr x7, [x6, native_static_fn_byte]
        );
        emit_load_u64(ops, 5, am.builtin_fn_addr as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x7, x5
            ; b.ne =>exit                     // prototype builtin overridden → deopt
        );
    }

    /// Lower one SSA body node. A node's result goes to its home; a node with no
    /// home is dead and emits only the guards that can deopt.
    #[allow(clippy::too_many_arguments)]
    fn lower_node(
        ops: &mut Assembler,
        view: &JitFunctionView,
        graph: &Graph,
        alloc: &Allocation,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        call_resume_frames: &FxHashMap<NodeId, DeoptPoint>,
        deopt_labels: &mut FxHashMap<NodeId, DynamicLabel>,
        self_entry: DynamicLabel,
        threw: DynamicLabel,
        nid: NodeId,
        spill_bytes: u32,
        box_scratch: u32,
        callee_saved: &[CalleeSavedReg],
    ) -> Result<(), Unsupported> {
        let node = graph.node(nid);
        let dst = alloc.location.get(&nid).copied();
        match &node.kind {
            // Entry per-register defs and phis carry no body code.
            NodeKind::Param(_) | NodeKind::Phi(_) => Ok(()),
            NodeKind::ConstUndefined => {
                if let Some(loc) = dst {
                    emit_load_u64(ops, box_scratch, VALUE_UNDEFINED);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::ConstNull => {
                if let Some(loc) = dst {
                    emit_load_u64(ops, box_scratch, VALUE_NULL);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::ConstBool(b) => {
                if let Some(loc) = dst {
                    let bits = if *b { VALUE_TRUE } else { VALUE_FALSE };
                    emit_load_u64(ops, box_scratch, bits);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::SelfClosure => {
                if let Some(loc) = dst {
                    // ctx.self_closure is a boxed Value at offset 8 from x20.
                    dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x20, #8]);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::ConstInt32(v) => {
                if let Some(loc) = dst {
                    // Unboxed int32 in the low 32 bits.
                    emit_load_u64(ops, box_scratch, u64::from(*v as u32));
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::CheckInt32(operand) => {
                // Guard a boxed operand is int32; an already-unboxed int32 input
                // can appear after SSA rewrites and passes through unchanged.
                let oloc = require_loc(alloc, *operand)?;
                if graph.node(*operand).repr == Repr::Int32 {
                    if let Some(loc) = dst {
                        load_loc(ops, box_scratch, oloc);
                        store_loc(ops, loc, box_scratch);
                    }
                    return Ok(());
                }
                if graph.node(*operand).repr != Repr::Tagged {
                    return Err(Unsupported::Unlowered(
                        "check-int32 operand not tagged/int32",
                    ));
                }
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                load_loc(ops, box_scratch, oloc);
                // int32 iff every `NUMBER_TAG` bit is set. The two logical
                // immediates need no extra register, so the value stays in
                // box_scratch and the allocatable file (x9..x15) is untouched.
                dynasm!(ops
                    ; .arch aarch64
                    ; and x16, X(box_scratch), #value_tag::NUMBER_TAG
                    ; eor x16, x16, #value_tag::NUMBER_TAG
                    ; cbnz x16, =>exit
                );
                if let Some(loc) = dst {
                    // The guarded low32 is the unboxed int value; mask off the tag.
                    dynasm!(ops ; .arch aarch64 ; mov W(box_scratch), W(box_scratch));
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::Int32Add(a, b) | NodeKind::Int32Sub(a, b) | NodeKind::Int32Mul(a, b) => {
                let aloc = require_loc(alloc, *a)?;
                let bloc = require_loc(alloc, *b)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                // Operands are unboxed Int32 in low32; operate on W views via the
                // scratch x-regs x16/x17.
                load_loc(ops, 16, aloc);
                load_loc(ops, 17, bloc);
                match &node.kind {
                    NodeKind::Int32Add(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; adds w16, w16, w17 ; b.vs =>exit);
                    }
                    NodeKind::Int32Sub(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; subs w16, w16, w17 ; b.vs =>exit);
                    }
                    NodeKind::Int32Mul(_, _) => {
                        dynasm!(ops
                            ; .arch aarch64
                            ; smull x16, w16, w17
                            ; cmp x16, w16, sxtw
                            ; b.ne =>exit
                        );
                    }
                    _ => unreachable!(),
                }
                if let Some(loc) = dst {
                    // w16 holds the unboxed int32 result (W write zeroed bits
                    // 63:32). Keep unboxed in the home.
                    dynasm!(ops ; .arch aarch64 ; mov W(box_scratch), w16);
                    store_loc(ops, loc, box_scratch);
                } else {
                    // Dead result but the overflow guard above still runs.
                }
                Ok(())
            }
            NodeKind::Int32BitOr(a, b)
            | NodeKind::Int32BitAnd(a, b)
            | NodeKind::Int32BitXor(a, b)
            | NodeKind::Int32Shl(a, b)
            | NodeKind::Int32Shr(a, b)
            | NodeKind::Int32UshrToFloat64(a, b) => {
                // Pure int32 bitwise / shift on W views; no overflow, no deopt.
                // arm64 32-bit `lslv`/`asrv` mask the shift amount mod 32 — the
                // JS `& 31` shift semantics.
                let aloc = require_loc(alloc, *a)?;
                let bloc = require_loc(alloc, *b)?;
                load_loc(ops, 16, aloc);
                load_loc(ops, 17, bloc);
                match &node.kind {
                    NodeKind::Int32BitOr(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; orr w16, w16, w17);
                    }
                    NodeKind::Int32BitAnd(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; and w16, w16, w17);
                    }
                    NodeKind::Int32BitXor(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; eor w16, w16, w17);
                    }
                    NodeKind::Int32Shl(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; lslv w16, w16, w17);
                    }
                    NodeKind::Int32Shr(_, _) => {
                        dynasm!(ops ; .arch aarch64 ; asrv w16, w16, w17);
                    }
                    NodeKind::Int32UshrToFloat64(_, _) => {
                        dynasm!(ops
                            ; .arch aarch64
                            ; lsrv w16, w16, w17
                            ; ucvtf D(FP_LOAD_SCRATCH), w16
                        );
                    }
                    _ => unreachable!(),
                }
                if let Some(loc) = dst {
                    if matches!(node.kind, NodeKind::Int32UshrToFloat64(_, _)) {
                        store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; mov W(box_scratch), w16);
                        store_loc(ops, loc, box_scratch);
                    }
                }
                Ok(())
            }
            NodeKind::Int32Compare(op, a, b) => {
                let aloc = require_loc(alloc, *a)?;
                let bloc = require_loc(alloc, *b)?;
                load_loc(ops, 16, aloc);
                load_loc(ops, 17, bloc);
                dynasm!(ops ; .arch aarch64 ; cmp w16, w17);
                if let Some(loc) = dst {
                    match op {
                        CmpOp::Lt => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), lt),
                        CmpOp::Le => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), le),
                        CmpOp::Gt => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), gt),
                        CmpOp::Ge => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), ge),
                        CmpOp::Eq => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), eq),
                        CmpOp::Ne => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), ne),
                    }
                    // Unboxed Bool (0/1) in the home.
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::ConstF64(v) => {
                if let Some(loc) = dst {
                    // Materialize the f64 bit pattern in a GP scratch, move it to
                    // the FP scratch, and store to the (FP) home.
                    emit_load_u64(ops, box_scratch, v.to_bits());
                    dynasm!(ops ; .arch aarch64 ; fmov D(FP_LOAD_SCRATCH), X(box_scratch));
                    store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                }
                Ok(())
            }
            NodeKind::CheckNumber(operand) => {
                // Guard a boxed operand is a number, unboxing it to f64. An
                // already-typed numeric input can appear after SSA rewrites and
                // is converted/copied without a tag check.
                let oloc = require_loc(alloc, *operand)?;
                match graph.node(*operand).repr {
                    Repr::Float64 => {
                        if let Some(loc) = dst {
                            load_fp_loc(ops, FP_LOAD_SCRATCH, oloc);
                            store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                        }
                        return Ok(());
                    }
                    Repr::Int32 => {
                        if let Some(loc) = dst {
                            load_loc(ops, box_scratch, oloc);
                            dynasm!(ops ; .arch aarch64 ; scvtf D(FP_LOAD_SCRATCH), W(box_scratch));
                            store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                        }
                        return Ok(());
                    }
                    Repr::Tagged => {}
                    Repr::Bool => {
                        return Err(Unsupported::Unlowered("check-number operand not numeric"));
                    }
                }
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                load_loc(ops, box_scratch, oloc);
                let int32_path = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                // `value & NUMBER_TAG`: zero ⇒ a cell or non-number immediate
                // (deopt); == NUMBER_TAG ⇒ int32 (widen the low-32 payload);
                // otherwise a boxed double (subtract the encode offset).
                dynasm!(ops
                    ; .arch aarch64
                    ; and x16, X(box_scratch), #value_tag::NUMBER_TAG
                    ; cbz x16, =>exit
                    ; eor x16, x16, #value_tag::NUMBER_TAG
                    ; cbz x16, =>int32_path
                    ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
                    ; sub x16, X(box_scratch), x16
                    ; fmov D(FP_LOAD_SCRATCH), x16
                    ; b =>done
                    ; =>int32_path
                    // Int32: widen the signed low-32 payload to f64.
                    ; scvtf D(FP_LOAD_SCRATCH), W(box_scratch)
                    ; =>done
                );
                if let Some(loc) = dst {
                    store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                }
                Ok(())
            }
            NodeKind::CheckBool(operand) => {
                // Guard a boxed operand is exactly `false` or `true`, producing
                // the unboxed 0/1 predicate. This is not general ToBoolean:
                // any non-boolean deopts so the interpreter runs the full
                // truthiness semantics at the branch bytecode.
                let oloc = require_loc(alloc, *operand)?;
                match graph.node(*operand).repr {
                    Repr::Bool => {
                        if let Some(loc) = dst {
                            load_loc(ops, box_scratch, oloc);
                            store_loc(ops, loc, box_scratch);
                        }
                        return Ok(());
                    }
                    Repr::Tagged => {}
                    Repr::Int32 | Repr::Float64 => {
                        let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                        dynasm!(ops ; .arch aarch64 ; b =>exit);
                        return Ok(());
                    }
                }
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let is_true = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_loc(ops, box_scratch, oloc);
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_FALSE);
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp X(box_scratch), X(MOVE_SCRATCH)
                    ; b.ne =>is_true
                    ; movz W(box_scratch), #0
                    ; b =>done
                    ; =>is_true
                );
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_TRUE);
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp X(box_scratch), X(MOVE_SCRATCH)
                    ; b.ne =>exit
                    ; movz W(box_scratch), #1
                    ; =>done
                );
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::Int32ToFloat64(operand) => {
                // Widen an already-unboxed int32 (low 32 bits of its GP home) to
                // f64. No guard: the input's int32-ness was established upstream.
                let oloc = require_loc(alloc, *operand)?;
                if let Some(loc) = dst {
                    load_loc(ops, box_scratch, oloc);
                    dynasm!(ops ; .arch aarch64 ; scvtf D(FP_LOAD_SCRATCH), W(box_scratch));
                    store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                }
                Ok(())
            }
            NodeKind::Float64ToInt32(operand) => {
                let oloc = require_loc(alloc, *operand)?;
                if let Some(loc) = dst {
                    load_fp_loc(ops, FP_LOAD_SCRATCH, oloc);
                    dynasm!(ops ; .arch aarch64 ; fjcvtzs W(box_scratch), D(FP_LOAD_SCRATCH));
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::Float64Add(a, b)
            | NodeKind::Float64Sub(a, b)
            | NodeKind::Float64Mul(a, b)
            | NodeKind::Float64Div(a, b) => {
                // IEEE arithmetic is total — no overflow guard / deopt. A dead
                // result has no observable effect, so skip it entirely.
                let Some(loc) = dst else { return Ok(()) };
                let aloc = require_loc(alloc, *a)?;
                let bloc = require_loc(alloc, *b)?;
                load_fp_loc(ops, FP_LOAD_SCRATCH, aloc);
                load_fp_loc(ops, FP_ARITH_SCRATCH, bloc);
                match &node.kind {
                    NodeKind::Float64Add(_, _) => dynasm!(ops ; .arch aarch64
                        ; fadd D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH), D(FP_ARITH_SCRATCH)),
                    NodeKind::Float64Sub(_, _) => dynasm!(ops ; .arch aarch64
                        ; fsub D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH), D(FP_ARITH_SCRATCH)),
                    NodeKind::Float64Mul(_, _) => dynasm!(ops ; .arch aarch64
                        ; fmul D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH), D(FP_ARITH_SCRATCH)),
                    NodeKind::Float64Div(_, _) => dynasm!(ops ; .arch aarch64
                        ; fdiv D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH), D(FP_ARITH_SCRATCH)),
                    _ => unreachable!(),
                }
                store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                Ok(())
            }
            NodeKind::Float64Unary(uop, a) => {
                // Single exact float instruction; total, so a dead result is a
                // no-op. The operand is already an unboxed `f64`.
                let Some(loc) = dst else { return Ok(()) };
                let aloc = require_loc(alloc, *a)?;
                load_fp_loc(ops, FP_LOAD_SCRATCH, aloc);
                match uop {
                    Float64UnaryOp::Sqrt => dynasm!(ops ; .arch aarch64
                        ; fsqrt D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH)),
                    Float64UnaryOp::Abs => dynasm!(ops ; .arch aarch64
                        ; fabs D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH)),
                    Float64UnaryOp::Floor => dynasm!(ops ; .arch aarch64
                        ; frintm D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH)),
                    Float64UnaryOp::Ceil => dynasm!(ops ; .arch aarch64
                        ; frintp D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH)),
                    Float64UnaryOp::Trunc => dynasm!(ops ; .arch aarch64
                        ; frintz D(FP_LOAD_SCRATCH), D(FP_LOAD_SCRATCH)),
                }
                store_fp_loc(ops, loc, FP_LOAD_SCRATCH);
                Ok(())
            }
            NodeKind::Float64Compare(op, a, b) => {
                let aloc = require_loc(alloc, *a)?;
                let bloc = require_loc(alloc, *b)?;
                load_fp_loc(ops, FP_LOAD_SCRATCH, aloc);
                load_fp_loc(ops, FP_ARITH_SCRATCH, bloc);
                dynasm!(ops ; .arch aarch64 ; fcmp D(FP_LOAD_SCRATCH), D(FP_ARITH_SCRATCH));
                if let Some(loc) = dst {
                    // Unordered-safe conditions: a NaN operand makes every
                    // relation but `Ne` false, matching JS number comparison.
                    match op {
                        CmpOp::Lt => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), mi),
                        CmpOp::Le => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), ls),
                        CmpOp::Gt => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), gt),
                        CmpOp::Ge => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), ge),
                        CmpOp::Eq => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), eq),
                        CmpOp::Ne => dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), ne),
                    }
                    // Unboxed Bool (0/1) in the home.
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::TaggedIsNull {
                value,
                negate,
                nullish,
            } => {
                let vloc = require_loc(alloc, *value)?;
                load_loc(ops, box_scratch, vloc);
                let flags_ready = ops.new_dynamic_label();
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_NULL);
                dynasm!(ops ; .arch aarch64 ; cmp X(box_scratch), X(MOVE_SCRATCH));
                if *nullish {
                    // A loose null test also matches `undefined`. On a `null` hit
                    // the flags already read equal; otherwise re-test against
                    // `undefined` so the flags read equal iff nullish.
                    dynasm!(ops ; .arch aarch64 ; b.eq =>flags_ready);
                    emit_load_u64(ops, MOVE_SCRATCH, VALUE_UNDEFINED);
                    dynasm!(ops ; .arch aarch64 ; cmp X(box_scratch), X(MOVE_SCRATCH));
                }
                dynasm!(ops ; .arch aarch64 ; =>flags_ready);
                if let Some(loc) = dst {
                    if *negate {
                        dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), ne);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; cset W(box_scratch), eq);
                    }
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::CheckShape(obj, shape_offset) => {
                // Guard the receiver is an ordinary object of the baked shape:
                // pointer tag, GC type tag, then receiver-shape == baked shape. A
                // miss deopts. The guarded (tagged) receiver is the result.
                let oloc = require_loc(alloc, *obj)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                load_loc(ops, box_scratch, oloc);
                // A heap cell carries neither a `NUMBER_TAG` bit nor `OTHER_TAG`;
                // a number or immediate receiver deopts. The GC type-tag check
                // below disambiguates an ordinary object from another cell class.
                dynasm!(ops
                    ; .arch aarch64
                    ; tst X(box_scratch), #value_tag::NUMBER_TAG
                    ; b.ne =>exit
                    ; tst X(box_scratch), #value_tag::OTHER_TAG
                    ; b.ne =>exit
                );
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                // Decompress: GcHeader ptr = cage_base + low32(value).
                dynasm!(ops ; .arch aarch64 ; mov W(MOVE_SCRATCH), W(box_scratch));
                emit_load_u64(ops, box_scratch, view.cage_base as u64);
                let shape_byte = view.object_shape_byte;
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, X(box_scratch)
                    ; ldrb w17, [x16]
                    ; cmp w17, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr w17, [x16, shape_byte]
                );
                emit_load_u64(ops, MOVE_SCRATCH, u64::from(*shape_offset));
                dynasm!(ops ; .arch aarch64 ; cmp w17, W(MOVE_SCRATCH) ; b.ne =>exit);
                Ok(())
            }
            NodeKind::CheckFunctionIdentity {
                callee,
                function_id,
            } => {
                let callee_loc = require_loc(alloc, *callee)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let fid_immediate = ops.new_dynamic_label();
                let fid_compare = ops.new_dynamic_label();
                load_loc(ops, box_scratch, callee_loc);
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x1, NUMBER_TAG_HI16, lsl #48
                    ; tst X(box_scratch), x1
                    ; b.ne =>exit                       // a number is not callable
                    ; and x0, X(box_scratch), #0xffff
                    ; cmp x0, #(FUNCTION_ID_TAG as u32)
                    ; b.eq =>fid_immediate
                    ; mov w2, W(box_scratch)            // otherwise a cell: low32 = gc offset
                );
                emit_load_u64(ops, 3, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x3, x3, x2
                    ; ldrb w0, [x3]
                    ; cmp w0, JS_CLOSURE_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr w4, [x3, view.closure_fid_byte]
                    ; b =>fid_compare
                    ; =>fid_immediate
                    ; lsr x4, X(box_scratch), #16        // function id in bits [16, 48)
                    ; =>fid_compare
                    ; movz w5, function_id & 0xffff
                    ; movk w5, (function_id >> 16) & 0xffff, lsl #16
                    ; cmp w4, w5
                    ; b.ne =>exit
                );
                if let Some(loc) = dst {
                    load_loc(ops, box_scratch, callee_loc);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::CheckMethodIdentity {
                recv,
                recv_shape,
                proto_shape,
                method_value_byte,
                method_on_receiver,
                method_fid,
            } => {
                let recv_loc = require_loc(alloc, *recv)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                load_loc(ops, box_scratch, recv_loc);
                let fid_immediate = ops.new_dynamic_label();
                let fid_compare = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x1, NUMBER_TAG_HI16, lsl #48
                    ; orr x1, x1, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst X(box_scratch), x1
                    ; b.ne =>exit
                    ; mov w2, W(box_scratch)
                );
                emit_load_u64(ops, 3, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x3, x3, x2
                    ; ldrb w4, [x3]
                    ; cmp w4, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr w4, [x3, view.object_shape_byte]
                    ; movz w5, recv_shape & 0xffff
                    ; movk w5, (recv_shape >> 16) & 0xffff, lsl #16
                    ; cmp w4, w5
                    ; b.ne =>exit
                );
                if !*method_on_receiver {
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w2, [x3, view.jit_proto_byte]
                        ; cbz w2, =>exit
                    );
                    emit_load_u64(ops, 3, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x3, x3, x2
                        ; ldrb w4, [x3]
                        ; cmp w4, OBJECT_BODY_TYPE_TAG
                        ; b.ne =>exit
                        ; ldr w4, [x3, view.object_shape_byte]
                        ; movz w5, proto_shape & 0xffff
                        ; movk w5, (proto_shape >> 16) & 0xffff, lsl #16
                        ; cmp w4, w5
                        ; b.ne =>exit
                    );
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x3, [x3, view.object_values_ptr_byte]
                    ; cbz x3, =>exit
                    ; ldr w17, [x3, *method_value_byte]  // 4-byte compressed slot
                );
                emit_decompress_slot(ops, view, exit);
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x4, x17
                    ; movz x1, NUMBER_TAG_HI16, lsl #48
                    ; tst x4, x1
                    ; b.ne =>exit                       // a number is not a method
                    ; and x0, x4, #0xffff
                    ; cmp x0, #(FUNCTION_ID_TAG as u32)
                    ; b.eq =>fid_immediate
                    ; mov w2, w4                        // otherwise a cell: low32 = gc offset
                );
                emit_load_u64(ops, 3, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x3, x3, x2
                    ; ldrb w0, [x3]
                    ; cmp w0, JS_CLOSURE_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr w4, [x3, view.closure_fid_byte]
                    ; b =>fid_compare
                    ; =>fid_immediate
                    ; lsr x4, x4, #16                   // function id in bits [16, 48)
                    ; =>fid_compare
                    ; movz w5, method_fid & 0xffff
                    ; movk w5, (method_fid >> 16) & 0xffff, lsl #16
                    ; cmp w4, w5
                    ; b.ne =>exit
                );
                if let Some(loc) = dst {
                    load_loc(ops, box_scratch, recv_loc);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::MethodIdentityMatches {
                recv,
                recv_shape,
                proto_shape,
                method_value_byte,
                method_on_receiver,
                method_fid,
            } => {
                // Same receiver-shape + prototype-method-identity probe as
                // `CheckMethodIdentity`, but a mismatch falls through to `miss`
                // (result `false`) instead of deoptimizing, so a polymorphic
                // dispatch chain can try the next candidate shape. The result is
                // the boolean a `Branch` consumes.
                let recv_loc = require_loc(alloc, *recv)?;
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let fid_immediate = ops.new_dynamic_label();
                let fid_compare = ops.new_dynamic_label();
                load_loc(ops, box_scratch, recv_loc);
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x1, NUMBER_TAG_HI16, lsl #48
                    ; orr x1, x1, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst X(box_scratch), x1
                    ; b.ne =>miss
                    ; mov w2, W(box_scratch)
                );
                emit_load_u64(ops, 3, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x3, x3, x2
                    ; ldrb w4, [x3]
                    ; cmp w4, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>miss
                    ; ldr w4, [x3, view.object_shape_byte]
                    ; movz w5, recv_shape & 0xffff
                    ; movk w5, (recv_shape >> 16) & 0xffff, lsl #16
                    ; cmp w4, w5
                    ; b.ne =>miss
                );
                if !*method_on_receiver {
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w2, [x3, view.jit_proto_byte]
                        ; cbz w2, =>miss
                    );
                    emit_load_u64(ops, 3, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x3, x3, x2
                        ; ldrb w4, [x3]
                        ; cmp w4, OBJECT_BODY_TYPE_TAG
                        ; b.ne =>miss
                        ; ldr w4, [x3, view.object_shape_byte]
                        ; movz w5, proto_shape & 0xffff
                        ; movk w5, (proto_shape >> 16) & 0xffff, lsl #16
                        ; cmp w4, w5
                        ; b.ne =>miss
                    );
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x3, [x3, view.object_values_ptr_byte]
                    ; cbz x3, =>miss
                    ; ldr w17, [x3, *method_value_byte]  // 4-byte compressed slot
                );
                emit_decompress_slot(ops, view, miss);
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x4, x17
                    ; movz x1, NUMBER_TAG_HI16, lsl #48
                    ; tst x4, x1
                    ; b.ne =>miss                        // a number is not a method
                    ; and x0, x4, #0xffff
                    ; cmp x0, #(FUNCTION_ID_TAG as u32)
                    ; b.eq =>fid_immediate
                    ; mov w2, w4                         // otherwise a cell: low32 = gc offset
                );
                emit_load_u64(ops, 3, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x3, x3, x2
                    ; ldrb w0, [x3]
                    ; cmp w0, JS_CLOSURE_BODY_TYPE_TAG
                    ; b.ne =>miss
                    ; ldr w4, [x3, view.closure_fid_byte]
                    ; b =>fid_compare
                    ; =>fid_immediate
                    ; lsr x4, x4, #16                    // function id in bits [16, 48)
                    ; =>fid_compare
                    ; movz w5, method_fid & 0xffff
                    ; movk w5, (method_fid >> 16) & 0xffff, lsl #16
                    ; cmp w4, w5
                    ; b.ne =>miss
                    ; movz W(box_scratch), #1
                    ; b =>done
                    ; =>miss
                    ; movz W(box_scratch), #0
                    ; =>done
                );
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadSlot(obj, value_byte) => {
                // Read the slot at the baked byte offset within the shape-guarded
                // receiver's value slab. No guard (CheckShape established it).
                if *value_byte > 32760 {
                    return Err(Unsupported::Unlowered(
                        "property slot offset out of ldr range",
                    ));
                }
                let oloc = require_loc(alloc, *obj)?;
                if let Some(loc) = dst {
                    debug_assert_eq!(box_scratch, BOX_SCRATCH);
                    let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                    let slot_byte = *value_byte;
                    load_loc(ops, box_scratch, oloc);
                    dynasm!(ops ; .arch aarch64 ; mov W(MOVE_SCRATCH), W(box_scratch));
                    emit_load_u64(ops, box_scratch, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x16, x16, X(box_scratch)     // header ptr
                    );
                    crate::baseline::arm64::emit_slab_base(ops, view, 16, 17);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w17, [x16, slot_byte]        // 4-byte compressed slot
                    );
                    emit_decompress_slot(ops, view, exit);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadProtoSlot {
                recv,
                recv_shape,
                proto_shape,
                slot_byte,
            } => {
                // Direct-prototype data load: guard the receiver shape, follow the
                // flattened prototype mirror, guard the prototype shape, then read
                // the baked prototype value slot. Any miss deopts before the load's
                // value is observed.
                if *slot_byte > 32760 {
                    return Err(Unsupported::Unlowered(
                        "prototype property slot offset out of ldr range",
                    ));
                }
                let recv_loc = require_loc(alloc, *recv)?;
                if let Some(loc) = dst {
                    debug_assert_eq!(box_scratch, BOX_SCRATCH);
                    let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                    let shape_byte = view.object_shape_byte;
                    let proto_byte = view.jit_proto_byte;
                    load_loc(ops, box_scratch, recv_loc);
                    dynasm!(ops
                        ; .arch aarch64
                        ; tst X(box_scratch), #value_tag::NUMBER_TAG
                        ; b.ne =>exit
                        ; tst X(box_scratch), #value_tag::OTHER_TAG
                        ; b.ne =>exit
                        ; mov W(MOVE_SCRATCH), W(box_scratch)
                    );
                    emit_load_u64(ops, box_scratch, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x16, x16, X(box_scratch)     // receiver header
                        ; ldrb w17, [x16]
                        ; cmp w17, OBJECT_BODY_TYPE_TAG
                        ; b.ne =>exit
                        ; ldr w17, [x16, shape_byte]
                        ; fmov d6, x16
                    );
                    emit_load_u64(ops, MOVE_SCRATCH, u64::from(*recv_shape));
                    dynasm!(ops
                        ; .arch aarch64
                        ; cmp w17, W(MOVE_SCRATCH)
                        ; b.ne =>exit
                        ; fmov x16, d6
                        ; ldr w17, [x16, proto_byte]       // compressed prototype
                        ; cbz w17, =>exit
                    );
                    emit_load_u64(ops, MOVE_SCRATCH, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x16, x17, X(MOVE_SCRATCH)    // prototype header
                        ; ldrb w17, [x16]
                        ; cmp w17, OBJECT_BODY_TYPE_TAG
                        ; b.ne =>exit
                        ; ldr w17, [x16, shape_byte]
                        ; fmov d6, x16
                    );
                    emit_load_u64(ops, MOVE_SCRATCH, u64::from(*proto_shape));
                    dynasm!(ops
                        ; .arch aarch64
                        ; cmp w17, W(MOVE_SCRATCH)
                        ; b.ne =>exit
                        ; fmov x16, d6
                    );
                    crate::baseline::arm64::emit_slab_base(ops, view, 16, 17);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w17, [x16, *slot_byte]       // 4-byte compressed slot
                    );
                    emit_decompress_slot(ops, view, exit);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadArrayLength(obj) => {
                let oloc = require_loc(alloc, *obj)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                emit_array_length_load(ops, view, oloc, dst, exit);
                Ok(())
            }
            NodeKind::LoadElement(recv, idx) => {
                let recv_loc = require_loc(alloc, *recv)?;
                let idx_loc = require_loc(alloc, *idx)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                emit_element_load(ops, view, recv_loc, idx_loc, dst, exit);
                Ok(())
            }
            NodeKind::StoreElement(recv, idx, value) => {
                let recv_loc = require_loc(alloc, *recv)?;
                let idx_loc = require_loc(alloc, *idx)?;
                let value_loc = require_loc(alloc, *value)?;
                let value_repr = graph.node(*value).kind.repr();
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                emit_element_store(ops, view, recv_loc, idx_loc, value_loc, value_repr, exit)
            }
            NodeKind::ArrayPop { recv } => {
                // Inline dense `pop()`: guard, then drop and return the last slot.
                // Leaf — no allocation/safepoint/barrier; the deopt exit owns every
                // non-fast case (non-dense receiver, prototype override, sparse
                // length). An empty array returns undefined without mutating.
                let recv_loc = require_loc(alloc, *recv)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let am = view
                    .array_methods
                    .get(&node.byte_pc)
                    .ok_or(Unsupported::Unlowered("array pop without feedback"))?;
                let length_byte = view.ta_layout.array_length_byte;
                let (ptr_word, len_word) = vec_layout_offsets();
                let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
                let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
                let empty = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                emit_opt_array_dense_proto_guard(ops, view, am, recv_loc, exit);
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x3, [x0, arr_len_byte]
                    ; ldr x4, [x0, length_byte]
                    ; cmp x3, x4
                    ; b.ne =>exit                 // sparse / length-detached → deopt
                    ; cbz x3, =>empty
                    ; sub x3, x3, #1
                    ; ldr x5, [x0, arr_ptr_byte]
                    ; lsl x6, x3, #3
                    ; add x5, x5, x6
                    ; ldr x6, [x5]                // popped value
                    ; str x3, [x0, arr_len_byte]
                    ; str x3, [x0, length_byte]
                );
                if let Some(loc) = dst {
                    store_loc(ops, loc, 6);
                }
                dynasm!(ops ; .arch aarch64 ; b =>done ; =>empty);
                emit_load_u64(ops, 6, VALUE_UNDEFINED);
                if let Some(loc) = dst {
                    store_loc(ops, loc, 6);
                }
                dynasm!(ops ; .arch aarch64 ; =>done);
                Ok(())
            }
            NodeKind::ArrayPush {
                recv,
                value,
                recv_reg,
            } => {
                // Dense `push(value)`: guard, then route the append through a
                // safepointed runtime stub that handles growth and the
                // generational barrier in Rust. The guard deopts on every
                // non-fast case before any frame change; the live frame is
                // materialized for the call safepoint, the value is passed boxed
                // by value (its source register may be reused), and the new length
                // is written back to the frame and reloaded.
                let point = frames
                    .get(&nid)
                    .ok_or(Unsupported::Unlowered("array push without safepoint state"))?;
                let dst_reg = node
                    .frame_dst
                    .ok_or(Unsupported::Unlowered("array push without frame dst"))?;
                let recv_loc = require_loc(alloc, *recv)?;
                let value_loc = require_loc(alloc, *value)?;
                let value_repr = graph.node(*value).kind.repr();
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let am = *view
                    .array_methods
                    .get(&node.byte_pc)
                    .ok_or(Unsupported::Unlowered("array push without feedback"))?;
                emit_opt_array_dense_proto_guard(ops, view, &am, recv_loc, exit);
                emit_frame_materialize(ops, graph, alloc, point, box_scratch)?;
                box_into_gp(ops, 3, value_repr, value_loc, box_scratch);
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, x20
                    ; movz x1, u32::from(dst_reg)
                    ; movz x2, u32::from(*recv_reg)
                );
                emit_load_u64(ops, 16, jit_array_push_optimizing_stub as *const () as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; blr x16
                    ; cmp x0, #1
                    ; b.eq =>threw
                );
                let resume = call_resume_frames.get(&nid).unwrap_or(point);
                emit_frame_reload(ops, graph, alloc, resume, None, box_scratch)?;
                if value_is_used_after(graph, call_resume_frames, nid)
                    && let Some(loc) = dst
                    && !resume.registers.iter().any(|&(r, _)| r == dst_reg)
                {
                    let off = u32::from(dst_reg) * 8;
                    dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::NewArray => {
                let point = frames
                    .get(&nid)
                    .ok_or(Unsupported::Unlowered("new array without safepoint state"))?;
                let dst_reg = node
                    .frame_dst
                    .ok_or(Unsupported::Unlowered("new array without frame dst"))?;
                emit_frame_materialize_tagged(ops, graph, alloc, point, box_scratch)?;
                dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                emit_load_u64(ops, 1, u64::from(node.byte_pc));
                emit_load_u64(ops, 16, jit_new_array_stub as *const () as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; blr x16
                    ; cmp x0, #1
                    ; b.eq =>threw
                );
                let resume = call_resume_frames.get(&nid).unwrap_or(point);
                emit_frame_reload_tagged(ops, graph, alloc, resume, None, box_scratch)?;
                if value_is_used_after(graph, call_resume_frames, nid)
                    && let Some(loc) = dst
                    && !resume.registers.iter().any(|&(r, _)| r == dst_reg)
                {
                    let off = u32::from(dst_reg) * 8;
                    dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::StoreSlot(obj, value_byte, value) => {
                // Write a value into the shape-guarded receiver's value slab. A
                // primitive (int32 / f64) needs no write barrier. A `Tagged` value
                // may be a heap pointer, so a generational card-mark is emitted
                // inline after the store (parent old + child young → mark the
                // parent's card). The insertion (marking) barrier is dormant under
                // the Phase-1 STW collector, so only the card-mark is needed; it
                // allocates nothing and never moves GC.
                if *value_byte > 32760 {
                    return Err(Unsupported::Unlowered(
                        "property slot offset out of str range",
                    ));
                }
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                let oloc = require_loc(alloc, *obj)?;
                let vloc = require_loc(alloc, *value)?;
                let vrepr = graph.node(*value).kind.repr();
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let slot_byte = *value_byte;
                if matches!(vrepr, Repr::Float64) {
                    box_into_gp(ops, box_scratch, Repr::Float64, vloc, MOVE_SCRATCH);
                    dynasm!(ops ; .arch aarch64 ; fmov d7, x17);
                    load_loc(ops, box_scratch, oloc);
                    dynasm!(ops ; .arch aarch64 ; mov W(MOVE_SCRATCH), W(box_scratch));
                    emit_load_u64(ops, box_scratch, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x16, x16, X(box_scratch)     // receiver header
                    );
                    crate::baseline::arm64::emit_slab_base(ops, view, 16, 17);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w17, [x16, slot_byte]        // existing compressed slot
                        ; and w16, w17, #0x7
                        ; cmp w16, #0x2                    // boxed number
                        ; b.ne =>exit
                        ; and w17, w17, #0xfffffff8
                        ; mov w17, w17
                    );
                    emit_load_u64(ops, 16, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x16, x16, x17                // heap-number header
                        ; ldrb w17, [x16]
                        ; cmp w17, u32::from(view.heap_number_type_tag)
                        ; b.ne =>exit
                        ; fmov x17, d7
                        ; str x17, [x16, view.heap_number_bits_byte]
                    );
                    return Ok(());
                }
                // Box the value into x17. Float boxing uses the FP load scratch +
                // x16; int32 boxing inserts the tag with `movk` (the producer
                // zeroed bits 63:32); a `Tagged` value is already boxed.
                match vrepr {
                    Repr::Float64 => {
                        box_into_gp(ops, box_scratch, Repr::Float64, vloc, MOVE_SCRATCH)
                    }
                    Repr::Int32 => {
                        load_loc(ops, box_scratch, vloc);
                        dynasm!(ops ; .arch aarch64 ; movk x17, NUMBER_TAG_HI16, lsl #48);
                    }
                    Repr::Tagged => {
                        load_loc(ops, box_scratch, vloc);
                    }
                    _ => {
                        return Err(Unsupported::Unlowered(
                            "store-slot value not int32/f64/tagged",
                        ));
                    }
                }
                // Compress to a 4-byte slot (a double / wide int / function id
                // deopts — it would allocate a heap box). Park the slot in d7
                // while the slab base is recomputed, since the compress and the
                // base computation both use x16.
                emit_compress_slot(ops, exit);
                dynasm!(ops ; .arch aarch64 ; fmov d7, x17);
                load_loc(ops, box_scratch, oloc);
                dynasm!(ops ; .arch aarch64 ; mov W(MOVE_SCRATCH), W(box_scratch));
                emit_load_u64(ops, box_scratch, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, X(box_scratch)     // header ptr
                );
                crate::baseline::arm64::emit_slab_base(ops, view, 16, 17);
                dynasm!(ops
                    ; .arch aarch64
                    ; fmov x17, d7                      // restore compressed slot
                    ; str w17, [x16, slot_byte]         // 4-byte slot write
                );
                if matches!(vrepr, Repr::Tagged) {
                    emit_generational_card_mark(ops, view, oloc, vloc)?;
                }
                Ok(())
            }
            NodeKind::LoadSlotPoly(obj, cases) => {
                // Inline structure-guard chain (a JSC `MultiGetByOffset`): read the
                // receiver shape once, then compare it to each baked case's shape;
                // the first match loads that case's slot and writes the result, and
                // the final miss deopts at the load's exact PC. Arms 0..n-1 branch
                // to the next case on a miss; the last arm's miss is the deopt.
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                let oloc = require_loc(alloc, *obj)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let done = ops.new_dynamic_label();
                let shape_byte = view.object_shape_byte;
                // Receiver → header, and read the shape id, once. `d6` parks the
                // header pointer across the compare chain.
                load_loc(ops, box_scratch, oloc);
                dynasm!(ops
                    ; .arch aarch64
                    ; tst X(box_scratch), #value_tag::NUMBER_TAG
                    ; b.ne =>exit
                    ; tst X(box_scratch), #value_tag::OTHER_TAG
                    ; b.ne =>exit
                    ; mov W(MOVE_SCRATCH), W(box_scratch)
                );
                emit_load_u64(ops, box_scratch, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, X(box_scratch)
                    ; ldrb w17, [x16]
                    ; cmp w17, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr w17, [x16, shape_byte]   // w17 = receiver shape id
                    ; fmov d6, x16                 // park header ptr
                );
                for (i, (shape, slot_byte)) in cases.iter().enumerate() {
                    let is_last = i + 1 == cases.len();
                    let miss = if is_last {
                        exit
                    } else {
                        ops.new_dynamic_label()
                    };
                    emit_load_u64(ops, MOVE_SCRATCH, u64::from(*shape));
                    dynasm!(ops ; .arch aarch64 ; cmp w17, W(MOVE_SCRATCH) ; b.ne =>miss);
                    dynasm!(ops
                        ; .arch aarch64
                        ; fmov x16, d6                     // header ptr
                    );
                    crate::baseline::arm64::emit_slab_base(ops, view, 16, 17);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w17, [x16, *slot_byte]       // 4-byte compressed slot
                    );
                    emit_decompress_slot(ops, view, exit);
                    if let Some(loc) = dst {
                        store_loc(ops, loc, box_scratch);
                    }
                    dynasm!(ops ; .arch aarch64 ; b =>done);
                    if !is_last {
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                    }
                }
                dynasm!(ops ; .arch aarch64 ; =>done);
                Ok(())
            }
            NodeKind::StoreSlotPoly(obj, cases, value) => {
                // Inline structure-guard chain (a JSC `MultiPutByOffset`): box and
                // compress the value once, read the receiver shape once, then store
                // into the first matching case's slot; the final miss deopts before
                // any write, so re-executing the store on the interpreter is
                // correct. A tagged value carries the inline generational card-mark.
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                let oloc = require_loc(alloc, *obj)?;
                let vloc = require_loc(alloc, *value)?;
                let vrepr = graph.node(*value).kind.repr();
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                let done = ops.new_dynamic_label();
                let shape_byte = view.object_shape_byte;
                // Box the value into x17, then compress to a 4-byte slot parked in
                // d7. A value that does not fit the compressed form deopts.
                match vrepr {
                    Repr::Float64 => {
                        box_into_gp(ops, box_scratch, Repr::Float64, vloc, MOVE_SCRATCH)
                    }
                    Repr::Int32 => {
                        load_loc(ops, box_scratch, vloc);
                        dynasm!(ops ; .arch aarch64 ; movk x17, NUMBER_TAG_HI16, lsl #48);
                    }
                    Repr::Tagged => {
                        load_loc(ops, box_scratch, vloc);
                    }
                    _ => {
                        return Err(Unsupported::Unlowered(
                            "store-slot-poly value not int32/f64/tagged",
                        ));
                    }
                }
                emit_compress_slot(ops, exit);
                dynasm!(ops ; .arch aarch64 ; fmov d7, x17);
                // Receiver → header + shape id, once; park header in d6.
                load_loc(ops, box_scratch, oloc);
                dynasm!(ops
                    ; .arch aarch64
                    ; tst X(box_scratch), #value_tag::NUMBER_TAG
                    ; b.ne =>exit
                    ; tst X(box_scratch), #value_tag::OTHER_TAG
                    ; b.ne =>exit
                    ; mov W(MOVE_SCRATCH), W(box_scratch)
                );
                emit_load_u64(ops, box_scratch, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, X(box_scratch)
                    ; ldrb w17, [x16]
                    ; cmp w17, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr w17, [x16, shape_byte]   // w17 = receiver shape id
                    ; fmov d6, x16                 // park header ptr
                );
                for (i, (shape, slot_byte)) in cases.iter().enumerate() {
                    let is_last = i + 1 == cases.len();
                    let miss = if is_last {
                        exit
                    } else {
                        ops.new_dynamic_label()
                    };
                    emit_load_u64(ops, MOVE_SCRATCH, u64::from(*shape));
                    dynasm!(ops ; .arch aarch64 ; cmp w17, W(MOVE_SCRATCH) ; b.ne =>miss);
                    dynasm!(ops
                        ; .arch aarch64
                        ; fmov x16, d6                     // header ptr
                    );
                    crate::baseline::arm64::emit_slab_base(ops, view, 16, 17);
                    dynasm!(ops
                        ; .arch aarch64
                        ; fmov x17, d7                     // compressed slot
                        ; str w17, [x16, *slot_byte]       // 4-byte slot write
                    );
                    if matches!(vrepr, Repr::Tagged) {
                        emit_generational_card_mark(ops, view, oloc, vloc)?;
                    }
                    dynasm!(ops ; .arch aarch64 ; b =>done);
                    if !is_last {
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                    }
                }
                dynasm!(ops ; .arch aarch64 ; =>done);
                Ok(())
            }
            NodeKind::AllocObjectLiteral {
                shape_offset,
                inputs,
            } => {
                // An object literal allocation: a call safepoint (the allocation
                // can scavenge), then a runtime helper that allocates the object
                // directly in its baked final hidden class and bulk-initializes
                // its slots with the boxed property values (write barriers in
                // Rust). The property values are passed by value in x4..x7, boxed
                // from their SSA locations — the bytecode registers they came from
                // may have been reused by later properties, so they cannot be read
                // back from the frame window.
                let point = frames.get(&nid).ok_or(Unsupported::Unlowered(
                    "object literal without safepoint state",
                ))?;
                let dst_reg = node
                    .frame_dst
                    .ok_or(Unsupported::Unlowered("object literal without frame dst"))?;
                if inputs.len() > 4 {
                    return Err(Unsupported::OperandShape("object literal property count"));
                }
                emit_frame_materialize_tagged(ops, graph, alloc, point, box_scratch)?;
                // Box each property value into x4..x7 before setting the leading
                // ABI argument registers (x0..x3), so boxing's scratch use cannot
                // clobber them.
                for (slot, &inp) in inputs.iter().enumerate() {
                    let loc = require_loc(alloc, inp)?;
                    let repr = graph.node(inp).kind.repr();
                    box_into_gp(ops, 4 + slot as u32, repr, loc, box_scratch);
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, x20
                    ; movz x1, u32::from(dst_reg)
                    ; movz x3, inputs.len() as u32
                );
                emit_load_u64(ops, 2, u64::from(*shape_offset));
                emit_load_u64(ops, 16, jit_alloc_object_literal_stub as *const () as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; blr x16
                    ; cmp x0, #1
                    ; b.eq =>threw
                );
                let resume = call_resume_frames.get(&nid).unwrap_or(point);
                emit_frame_reload_tagged(ops, graph, alloc, resume, None, box_scratch)?;
                if value_is_used_after(graph, call_resume_frames, nid)
                    && let Some(loc) = dst
                    && !resume.registers.iter().any(|&(r, _)| r == dst_reg)
                {
                    let off = u32::from(dst_reg) * 8;
                    dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::Call {
                callee_reg,
                arg_regs,
                inputs,
            } => {
                let point = frames
                    .get(&nid)
                    .ok_or(Unsupported::Unlowered("call without safepoint state"))?;
                let bail = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let after = ops.new_dynamic_label();
                let dst_reg = node
                    .frame_dst
                    .ok_or(Unsupported::Unlowered("call without frame dst"))?;
                if arg_regs.len() > 4 {
                    return Err(Unsupported::OperandShape("call arg count"));
                }
                if matches!(
                    inputs.first().map(|callee| &graph.node(*callee).kind),
                    Some(NodeKind::SelfClosure)
                ) && graph_allows_frameless_self_call(graph)
                {
                    // The recursion reads only the args and the self callee from
                    // `[x19]`; write just those on the hot path and defer the rest
                    // of the live set to the cold bail exit below.
                    let crosses = |r: u16, _v: NodeId| r == *callee_reg || arg_regs.contains(&r);
                    emit_frame_materialize_where(ops, graph, alloc, point, box_scratch, crosses)?;
                    emit_self_recursive_call(
                        ops,
                        graph.register_count,
                        dst_reg,
                        *callee_reg,
                        arg_regs,
                        self_entry,
                        bail,
                        threw,
                    )?;
                    dynasm!(ops ; .arch aarch64 ; =>done);
                    // No frame reload: every value live across the call was pinned
                    // to a callee-saved register (or spilled) by the allocator, so
                    // the recursion preserved it. Only the call result needs to be
                    // pulled from the callee's window slot into its home.
                    if value_is_used_after(graph, call_resume_frames, nid)
                        && let Some(loc) = dst
                    {
                        let off = u32::from(dst_reg) * 8;
                        dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
                        store_loc(ops, loc, box_scratch);
                    }
                    dynasm!(ops
                        ; .arch aarch64
                        ; b =>after
                        ; =>bail
                    );
                    // Cold: a guard-miss or reg-stack overflow bails to the
                    // interpreter, so complete `[x19]` with the registers the hot
                    // path skipped before stamping the resume PC.
                    emit_frame_materialize_where(ops, graph, alloc, point, box_scratch, |r, v| {
                        !crosses(r, v)
                    })?;
                    emit_load_u64(ops, box_scratch, u64::from(point.byte_pc));
                    dynasm!(ops
                        ; .arch aarch64
                        ; str W(box_scratch), [x20, BAIL_PC_OFFSET]
                        ; movz x1, STATUS_BAILED as u32
                    );
                    emit_epilogue(ops, spill_bytes, callee_saved);
                    dynasm!(ops ; .arch aarch64 ; =>after);
                    return Ok(());
                }
                emit_frame_materialize(ops, graph, alloc, point, box_scratch)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, x20
                    ; movz x1, *callee_reg as u32
                    ; movz x2, arg_regs.len() as u32
                );
                for slot in 0..4 {
                    let arg = arg_regs.get(slot).copied().unwrap_or(0);
                    let xn = 3 + slot as u32;
                    dynasm!(ops ; .arch aarch64 ; movz X(xn), arg as u32);
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
                emit_direct_call_tail(ops, dst_reg, threw, done);
                dynasm!(ops ; .arch aarch64 ; =>done);
                let resume = call_resume_frames.get(&nid).unwrap_or(point);
                emit_frame_reload(ops, graph, alloc, resume, None, box_scratch)?;
                if value_is_used_after(graph, call_resume_frames, nid)
                    && let Some(loc) = dst
                    && !resume.registers.iter().any(|&(r, _)| r == dst_reg)
                {
                    let off = u32::from(dst_reg) * 8;
                    dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
                    store_loc(ops, loc, box_scratch);
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; b =>after
                    ; =>bail
                );
                emit_load_u64(ops, box_scratch, u64::from(point.byte_pc));
                dynasm!(ops
                    ; .arch aarch64
                    ; str W(box_scratch), [x20, BAIL_PC_OFFSET]
                    ; movz x1, STATUS_BAILED as u32
                );
                emit_epilogue(ops, spill_bytes, callee_saved);
                dynasm!(ops ; .arch aarch64 ; =>after);
                Ok(())
            }
            NodeKind::CallMethod {
                recv_reg,
                name,
                site,
                arg_regs,
                ..
            } => {
                let point = frames.get(&nid).ok_or(Unsupported::Unlowered(
                    "method call without safepoint state",
                ))?;
                let fallback = ops.new_dynamic_label();
                let after_live_leaf = ops.new_dynamic_label();
                let after_live_alloc = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let dst_reg = node
                    .frame_dst
                    .ok_or(Unsupported::Unlowered("method call without frame dst"))?;
                if arg_regs.len() > 3 {
                    return Err(Unsupported::OperandShape("method call arg count"));
                }

                emit_frame_materialize(ops, graph, alloc, point, box_scratch)?;
                emit_opt_live_collection_leaf_method_guarded_call(
                    ops,
                    view,
                    dst_reg,
                    *recv_reg,
                    *site,
                    arg_regs,
                    after_live_leaf,
                    done,
                )?;
                dynasm!(ops ; .arch aarch64 ; =>after_live_leaf);
                if let Some(safepoint) =
                    super::optimizing_call_method_safepoint_id(view, node.byte_pc)
                {
                    emit_opt_live_collection_alloc_method_guarded_call(
                        ops,
                        view,
                        dst_reg,
                        *recv_reg,
                        *site,
                        arg_regs,
                        safepoint,
                        after_live_alloc,
                        done,
                    )?;
                    dynasm!(ops ; .arch aarch64 ; =>after_live_alloc);
                }
                // The argument register indices, packed one per 16-bit lane, are
                // handed to every method-call stub in a single register.
                let packed_args = crate::baseline::pack_method_arg_regs(arg_regs);
                dynasm!(
                    ops
                    ; .arch aarch64
                    ; mov x0, x20
                    ; movz x1, dst_reg as u32
                    ; movz x2, *recv_reg as u32
                );
                emit_load_u64(ops, 3, *site);
                dynasm!(ops ; .arch aarch64 ; movz x4, arg_regs.len() as u32);
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
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, x20
                    ; movz x1, *recv_reg as u32
                );
                emit_load_u64(ops, 2, u64::from(*name));
                emit_load_u64(ops, 3, *site);
                dynasm!(ops ; .arch aarch64 ; movz x4, arg_regs.len() as u32);
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

                emit_direct_call_tail(ops, dst_reg, threw, done);

                dynasm!(ops
                    ; .arch aarch64
                    ; =>fallback
                    ; mov x0, x20
                    ; movz x1, dst_reg as u32
                    ; movz x2, *recv_reg as u32
                );
                // Pack call-site IC id (high 32) with the name index (low 32).
                emit_load_u64(ops, 3, (*site << 32) | u64::from(*name));
                dynasm!(ops ; .arch aarch64 ; movz x4, arg_regs.len() as u32);
                emit_load_u64(ops, 5, packed_args);
                emit_status_stub_call(
                    ops,
                    jit_call_method_stub_optimizing as *const () as usize,
                    threw,
                );
                dynasm!(ops ; .arch aarch64 ; =>done);
                let resume = call_resume_frames.get(&nid).unwrap_or(point);
                emit_frame_reload(ops, graph, alloc, resume, None, box_scratch)?;
                if value_is_used_after(graph, call_resume_frames, nid)
                    && let Some(loc) = dst
                    && !resume.registers.iter().any(|&(r, _)| r == dst_reg)
                {
                    let off = u32::from(dst_reg) * 8;
                    dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x19, off]);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadThis => {
                // `this` bits from JitCtx; a TDZ hole (derived-ctor this before
                // super) deopts to the interpreter.
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x20, THIS_VALUE_OFFSET]);
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp X(box_scratch), X(MOVE_SCRATCH) ; b.eq =>exit);
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadHole => {
                if let Some(loc) = dst {
                    emit_load_u64(ops, box_scratch, VALUE_HOLE);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadUpvalue(idx) => {
                let idx =
                    u32::try_from(*idx).map_err(|_| Unsupported::OperandShape("upvalue index"))?;
                let idx_off = idx
                    .checked_mul(UPVALUE_CELL_SIZE)
                    .ok_or(Unsupported::OperandShape("upvalue index"))?;
                if idx_off > 32760 {
                    return Err(Unsupported::OperandShape("upvalue index"));
                }
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [x20, UPVALUES_PTR_OFFSET]
                    ; cbz x16, =>exit
                    ; ldr w17, [x16, idx_off]
                );
                emit_load_u64(ops, 16, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, x17
                    ; ldr X(box_scratch), [x16, UPVALUE_VALUE_OFFSET]
                );
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp X(box_scratch), X(MOVE_SCRATCH) ; b.eq =>exit);
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::InlineUpvalue { closure, index } => {
                // Read an inlined closure callee's own captured upvalue. Decode
                // the live closure body from the (fid-guarded) callee value, take
                // its spine pointer, then the per-index compressed cell handle —
                // the context-spine `LoadUpvalue` sequence with the spine sourced
                // from the closure instead of `JitCtx.upvalues_ptr`.
                let closure_loc = require_loc(alloc, *closure)?;
                let idx_off = index
                    .checked_mul(UPVALUE_CELL_SIZE)
                    .ok_or(Unsupported::OperandShape("upvalue index"))?;
                if idx_off > 32760 {
                    return Err(Unsupported::OperandShape("upvalue index"));
                }
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                load_loc(ops, box_scratch, closure_loc);
                // A heap closure (the only form that carries captures) is tagged
                // `TAG_PTR_FUNCTION`; a bare function-id immediate has no spine.
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x1, NUMBER_TAG_HI16, lsl #48
                    ; orr x1, x1, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                    ; tst X(box_scratch), x1
                    ; b.ne =>exit
                    ; mov w2, W(box_scratch)
                );
                emit_load_u64(ops, 3, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x3, x3, x2
                    ; ldrb w0, [x3]
                    ; cmp w0, JS_CLOSURE_BODY_TYPE_TAG
                    ; b.ne =>exit
                    ; ldr x16, [x3, view.closure_upvalues_ptr_byte]
                    ; cbz x16, =>exit
                    ; ldr w17, [x16, idx_off]
                );
                emit_load_u64(ops, 16, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, x17
                    ; ldr X(box_scratch), [x16, UPVALUE_VALUE_OFFSET]
                );
                emit_load_u64(ops, MOVE_SCRATCH, VALUE_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp X(box_scratch), X(MOVE_SCRATCH) ; b.eq =>exit);
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_self_recursive_call(
        ops: &mut Assembler,
        regcount: u16,
        dst_reg: u16,
        callee_reg: u16,
        arg_regs: &[u16],
        self_entry: DynamicLabel,
        bail: DynamicLabel,
        threw: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let rc = u32::from(regcount);
        let done = ops.new_dynamic_label();
        let returned = ops.new_dynamic_label();
        let bailed = ops.new_dynamic_label();
        let fill = ops.new_dynamic_label();
        let fill_done = ops.new_dynamic_label();
        let undef_bits = VALUE_UNDEFINED;

        let callee_off = u32::from(callee_reg) * 8;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, callee_off]
            ; ldr x10, [x20, #8]
            ; cmp x9, x10
            ; b.ne =>bail
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x11, [x12]
            ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
            ; add x14, x9, x11, lsl #3
        );
        emit_load_u64(ops, 13, u64::from(rc));
        dynasm!(ops ; .arch aarch64 ; add x13, x11, x13);
        emit_load_u64(ops, 9, Interpreter::jit_reg_stack_cap() as u64);
        dynasm!(ops ; .arch aarch64 ; cmp x13, x9 ; b.hi =>bail ; str x13, [x12]);

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
        for (slot, &areg) in arg_regs.iter().enumerate() {
            let off = u32::from(areg) * 8;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x9, [x19, off]
                ; str x9, [x14, slot as u32 * 8]
            );
        }

        // The recursive callee shares every invariant of this frame's `JitCtx` —
        // same function (`self_closure`), `vm` / `stack` / `context` /
        // `frame_index` (a frameless self-call pushes no HoltStack frame), error
        // slot, upvalue spine, and flat-reg-stack base/top. Only the register
        // window base differs, and the callee reads it exactly once (its prologue
        // captures `[ctx] -> x19`, callee-saved). So reuse this frame's ctx in
        // place: point its window base at the new window and reset `bail_pc` for
        // the callee, then restore the window base on return. This avoids
        // rebuilding the whole ctx struct on every recursive call.
        dynasm!(ops
            ; .arch aarch64
            ; str x14, [x20]
            ; str wzr, [x20, BAIL_PC_OFFSET]
            ; mov x0, x20
            ; bl =>self_entry
            ; str x19, [x20]
            ; cmp x1, STATUS_BAILED as u32
            ; b.eq =>bailed
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
            ; b =>threw
            ; =>returned
            ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
            ; ldr x13, [x12]
        );
        emit_load_u64(ops, 9, u64::from(rc));
        let dst_off = u32::from(dst_reg) * 8;
        dynasm!(ops
            ; .arch aarch64
            ; sub x13, x13, x9
            ; str x13, [x12]
            ; str x0, [x19, dst_off]
            ; b =>done
            ; =>bailed
            ; ldr w2, [x20, BAIL_PC_OFFSET]
            ; mov x0, x20
            ; mov w1, w2
        );
        emit_load_u64(ops, 2, u64::from(rc));
        emit_load_u64(ops, 16, jit_self_call_bail_stub as *const () as u64);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x1, STATUS_THREW as u32
            ; b.eq =>threw
            ; str x0, [x19, dst_off]
            ; =>done
        );
        Ok(())
    }

    /// The deopt-exit label for a deopt-capable node, creating it on first use.
    /// Errors when the node has no captured frame state (it must, by the deopt
    /// capture contract; a missing one would be a wild bail).
    fn deopt_exit_label(
        ops: &mut Assembler,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        deopt_labels: &mut FxHashMap<NodeId, DynamicLabel>,
        nid: NodeId,
    ) -> Result<DynamicLabel, Unsupported> {
        if !frames.contains_key(&nid) {
            return Err(Unsupported::Unlowered("guard without deopt frame state"));
        }
        Ok(*deopt_labels
            .entry(nid)
            .or_insert_with(|| ops.new_dynamic_label()))
    }

    /// Emit every cold deopt exit. Each reconstructs the live interpreter
    /// registers (re-boxed to tagged Values, stored into the frame array), stamps
    /// the resume byte-PC, and returns `STATUS_BAILED`.
    #[allow(clippy::too_many_arguments)]
    fn emit_deopt_exits(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        deopt_labels: &FxHashMap<NodeId, DynamicLabel>,
        spill_bytes: u32,
        box_scratch: u32,
        callee_saved: &[CalleeSavedReg],
    ) -> Result<(), Unsupported> {
        // Deterministic order for reproducible code.
        let mut nodes: Vec<NodeId> = deopt_labels.keys().copied().collect();
        nodes.sort_unstable();
        for nid in nodes {
            let label = deopt_labels[&nid];
            let point = frames
                .get(&nid)
                .ok_or(Unsupported::Unlowered("deopt exit without frame state"))?;
            dynasm!(ops ; .arch aarch64 ; =>label);
            emit_frame_restore_and_bail(
                ops,
                graph,
                alloc,
                point,
                spill_bytes,
                box_scratch,
                callee_saved,
            )?;
        }
        Ok(())
    }

    /// Restore the live interpreter registers from a deopt frame state and return
    /// `STATUS_BAILED` at `point.byte_pc`. Shared by per-guard cold deopt exits
    /// and the `Deopt` terminator.
    fn emit_frame_restore_and_bail(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        point: &DeoptPoint,
        spill_bytes: u32,
        box_scratch: u32,
        callee_saved: &[CalleeSavedReg],
    ) -> Result<(), Unsupported> {
        for &(regn, value) in &point.registers {
            let node = graph.node(value);
            if !emit_rematerialized_boxed(ops, &node.kind, box_scratch, MOVE_SCRATCH) {
                let loc = require_loc(alloc, value)?;
                box_into_gp(ops, box_scratch, node.repr, loc, MOVE_SCRATCH);
            }
            let off = u32::from(regn) * 8;
            dynasm!(ops ; .arch aarch64 ; str X(box_scratch), [x19, off]);
        }
        emit_load_u64(ops, box_scratch, u64::from(point.byte_pc));
        dynasm!(ops ; .arch aarch64 ; str W(box_scratch), [x20, BAIL_PC_OFFSET]);
        dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_BAILED as u32);
        emit_epilogue(ops, spill_bytes, callee_saved);
        Ok(())
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    //! End-to-end execution of emitted optimizing-tier code. Builds a fake frame
    //! register window and `JitCtx`-shaped buffer (offset 0 = regs pointer,
    //! offset 8 = self closure), calls the emitted entry with the shared
    //! `extern "C" fn(*mut JitCtx) -> JitRet` ABI, and checks the boxed result and
    //! return/bail status — the real correctness contract the VM relies on.

    use crate::optimizing::{build_graph, deopt, liveness, regalloc};
    use otter_bytecode::{Op, Operand};
    use otter_vm::{
        JitFunctionView,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32},
    };

    use otter_vm::value::tag as value_tag;

    fn r(n: u16) -> Operand {
        Operand::Register(n)
    }
    fn imm(n: i32) -> Operand {
        Operand::Imm32(n)
    }
    fn boxi(v: i32) -> u64 {
        value_tag::NUMBER_TAG | (v as u32 as u64)
    }
    fn unboxf(v: u64) -> f64 {
        f64::from_bits(value_tag::unbox_double(v))
    }
    fn unboxi(v: u64) -> i32 {
        v as u32 as i32
    }

    #[repr(C)]
    struct Ret {
        value: u64,
        status: u64,
    }

    /// Build a single-function view; `byte_pc == index` (`STRIDE == 1`) so the
    /// hand-written relative branch offsets match the compiler's `pc + 1 + rel`.
    fn view(
        param_count: u16,
        register_count: u16,
        instrs: &[(Op, Vec<Operand>, u8)],
    ) -> JitFunctionView {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, ops, fb))| otter_vm::JitInstrView {
                op: *op,
                byte_pc: idx as u32,
                byte_len: 1,
                property_ic_site: None,
                operands: ops.clone(),
                make_self: matches!(op, Op::MakeFunction),
                load_array_length: false,
                load_number: None,
                property_feedback: None,
                property_feedback_poly: Vec::new(),
                property_proto_feedback: None,
                object_literal: None,
                arith_feedback: *fb,
            })
            .collect();
        JitFunctionView {
            function_id: 0,
            param_count,
            register_count,
            code_byte_len: instrs.len() as u32,
            is_strict: true,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            cage_base: 0,
            ta_layout: Default::default(),
            object_shape_byte: 8,
            object_values_ptr_byte: 16,
            object_inline_values_byte: 80,
            object_slab_len_byte: 88,
            object_inline_slot_cap: 2,
            gc_barrier: Default::default(),
            jit_proto_byte: 12,
            heap_number_type_tag: 0x30,
            heap_number_bits_byte: 8,
            closure_fid_byte: 8,
            closure_upvalues_ptr_byte: 16,
            collection_layout: Default::default(),
            native_static_fn_byte: 0,
            instructions,
            inline_callees: Default::default(),
            inline_methods: Default::default(),
            inline_poly_methods: Default::default(),
            collection_leaf_methods: Default::default(),
            collection_alloc_methods: Default::default(),
            array_methods: Default::default(),
            safepoints: Default::default(),
        }
    }

    /// Compile `v`, run it with frame slot `r` preloaded from `params[r]`, and
    /// return `(status, boxed value)`.
    fn run(v: &JitFunctionView, params: &[u64]) -> (u64, u64) {
        let g = build_graph(v).expect("builds");
        let bcl = deopt::bytecode_liveness(v);
        let frames = deopt::capture_frame_states(&g, &bcl);
        let call_resume_frames = deopt::capture_call_resume_states(&g, v, &bcl);
        let live_uses = deopt::merge_frame_state_uses([&frames, &call_resume_frames]);
        let deopt_uses = deopt::deopt_value_uses(&live_uses);
        let block_deopts = deopt::capture_deopt_terminators(&g, &bcl);
        let live = liveness::analyze(&g, &deopt_uses, &block_deopts);
        let alloc = regalloc::allocate(
            &g,
            &live,
            super::GP_REGS,
            super::FP_REGS,
            super::CALLER_SAVED_GP,
            super::CALLER_SAVED_FP,
            &deopt_uses,
        );
        let osr = deopt::capture_osr_entries(&g, &bcl);
        let code = super::emit(
            v,
            &g,
            &live,
            &alloc,
            &frames,
            &call_resume_frames,
            &block_deopts,
            &osr,
        )
        .expect("emits");

        let mut regs = vec![0u64; 64];
        for (i, &p) in params.iter().enumerate() {
            regs[i] = p;
        }
        let mut ctx = vec![0u64; 64];
        ctx[0] = regs.as_mut_ptr() as u64;
        ctx[1] = boxi(0);
        // SAFETY: `entry` was emitted with the shared `extern "C" fn(*mut JitCtx)
        // -> JitRet` ABI; `ctx` is a JitCtx-shaped buffer whose offset-0 regs
        // pointer is a valid 64-slot window; `code` outlives the call.
        let entry = unsafe { code.code.entry_ptr() };
        let f: extern "C" fn(*mut u64) -> Ret = unsafe { std::mem::transmute(entry) };
        let ret = f(ctx.as_mut_ptr());
        (ret.status, ret.value)
    }

    /// `f(n){ let i=0; while(i<n){ i=i+1 } return n }` — a loop with a
    /// loop-carried phi that must preserve the param across the loop.
    fn loop_return_param() -> JitFunctionView {
        view(
            1,
            17,
            &[
                (Op::MakeFunction, vec![r(3), imm(3)], 0),
                (Op::StoreLocal, vec![r(3), imm(2)], 0),
                (Op::StoreLocal, vec![r(0), imm(1)], 0),
                (Op::LoadInt32, vec![r(5), imm(0)], 0),
                (Op::StoreLocal, vec![r(5), imm(4)], 0),
                (Op::LoadUndefined, vec![r(6)], 0),
                (Op::LoadLocal, vec![r(7), imm(4)], 0),
                (Op::LoadLocal, vec![r(8), imm(1)], 0),
                (Op::ToPrimitive, vec![r(9), r(7)], 0),
                (Op::ToPrimitive, vec![r(10), r(8)], 0),
                (Op::LessThan, vec![r(11), r(9), r(10)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(7), r(11)], 0),
                (Op::LoadLocal, vec![r(12), imm(4)], 0),
                (Op::LoadInt32, vec![r(13), imm(1)], 0),
                (Op::ToPrimitive, vec![r(14), r(12)], 0),
                (Op::Add, vec![r(15), r(14), r(13)], ARITH_INT32),
                (Op::StoreLocal, vec![r(15), imm(4)], 0),
                (Op::StoreLocal, vec![r(15), imm(6)], 0),
                (Op::Jump, vec![imm(-13)], 0),
                (Op::LoadLocal, vec![r(16), imm(1)], 0),
                (Op::ReturnValue, vec![r(16)], 0),
                (Op::ReturnUndefined, vec![], 0),
            ],
        )
    }

    #[test]
    fn loop_preserves_param() {
        let v = loop_return_param();
        let (status, value) = run(&v, &[boxi(5)]);
        assert_eq!(status, 0, "returns, no bail");
        assert_eq!(unboxi(value), 5, "f(5) == n == 5");
        let (s2, v2) = run(&v, &[boxi(1000)]);
        assert_eq!(s2, 0);
        assert_eq!(unboxi(v2), 1000);
    }

    #[test]
    fn non_int32_param_bails() {
        // A non-int32 param fails the CheckInt32 guard and bails to the
        // interpreter (status 1), which owns the spec-correct semantics.
        let v = loop_return_param();
        let double_bits = value_tag::box_double(3.5_f64.to_bits()); // boxed real double
        let (status, _value) = run(&v, &[double_bits]);
        assert_eq!(status, 1, "non-int32 param deopts to the interpreter");
    }

    const SPECIAL_UNDEFINED: u64 = value_tag::VALUE_UNDEFINED;

    /// `f(a){ return a / 2 }` — a float site (`Div` always lowers float): the
    /// param is `CheckNumber`-unboxed, the int32 const `2` is widened, and the
    /// `fdiv` result is returned as a boxed double.
    fn divide_by_two() -> JitFunctionView {
        view(
            1,
            4,
            &[
                (Op::LoadInt32, vec![r(1), imm(2)], 0),
                (Op::Div, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        )
    }

    #[test]
    fn float_divide_int_param_widens() {
        // a = 5 (boxed int32): CheckNumber widens via scvtf, 5/2 = 2.5.
        let (status, value) = run(&divide_by_two(), &[boxi(5)]);
        assert_eq!(status, 0, "returns, no bail");
        assert_eq!(unboxf(value), 2.5);
    }

    #[test]
    fn float_divide_double_param_verbatim() {
        // a = 7.0 (real double, bits verbatim): CheckNumber takes the double
        // path, 7.0/2 = 3.5.
        let (status, value) = run(
            &divide_by_two(),
            &[value_tag::box_double(7.0_f64.to_bits())],
        );
        assert_eq!(status, 0);
        assert_eq!(unboxf(value), 3.5);
    }

    #[test]
    fn float_divide_non_number_bails() {
        // a = undefined: CheckNumber sees a non-number tag and deopts.
        let (status, _value) = run(&divide_by_two(), &[SPECIAL_UNDEFINED]);
        assert_eq!(status, 1, "non-number operand deopts to the interpreter");
    }

    fn ushr_zero() -> JitFunctionView {
        view(
            1,
            4,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::Ushr, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        )
    }

    #[test]
    fn ushr_int32_result_widens_to_double() {
        let (status, value) = run(&ushr_zero(), &[boxi(-1)]);
        assert_eq!(status, 0, "returns, no bail");
        assert_eq!(unboxf(value), 4_294_967_295.0);
    }

    fn bit_or_zero() -> JitFunctionView {
        view(
            1,
            4,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (
                    Op::BitwiseOr,
                    vec![r(2), r(0), r(1)],
                    ARITH_INT32 | ARITH_FLOAT64,
                ),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        )
    }

    #[test]
    fn float64_to_int32_bitwise_or_returns_js_to_int32() {
        for (input, expected) in [
            (2_500_000.0, 2_500_000),
            (4_294_967_297.0, 1),
            (-1.5, -1),
            (f64::NAN, 0),
            (f64::INFINITY, 0),
        ] {
            let (status, value) = run(&bit_or_zero(), &[value_tag::box_double(input.to_bits())]);
            assert_eq!(status, 0, "{input:?} returns, no bail");
            assert_eq!(unboxi(value), expected, "ToInt32({input:?})");
        }
    }

    /// `f(n){ let x=0.0; let i=0; while(i<n){ x = x + 1.5; i = i+1 } return x }`
    /// — a loop carrying a `Float64` accumulator through a `Tagged` header phi
    /// (boxed on the back edge, `CheckNumber`-unboxed at the top each iteration),
    /// the canonical mandelbrot/nbody shape. Returns `n * 1.5`.
    fn float_accumulate_loop() -> JitFunctionView {
        // r0 = n (param). r1 = x, r2 = i, r3 = 1.5 const, r4 = i<n, r5 = 1 const.
        view(
            1,
            8,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0), // x = 0  (int, widened on first add)
                (Op::LoadInt32, vec![r(2), imm(0)], 0), // i = 0
                (Op::LessThan, vec![r(4), r(2), r(0)], ARITH_INT32), // header: i < n
                (Op::JumpIfFalse, vec![imm(5), r(4)], 0), // -> exit (idx 9)
                (Op::LoadNumber, vec![r(3), Operand::ConstIndex(0)], 0), // 1.5
                (Op::Add, vec![r(1), r(1), r(3)], ARITH_FLOAT64 | ARITH_INT32), // x += 1.5
                (Op::LoadInt32, vec![r(5), imm(1)], 0),
                (Op::Add, vec![r(2), r(2), r(5)], ARITH_INT32), // i += 1
                (Op::Jump, vec![imm(-7)], 0),
                (Op::ReturnValue, vec![r(1)], 0), // return x
            ],
        )
    }

    #[test]
    fn return_undefined_materializes_value() {
        // A function whose only terminator is `ReturnUndefined` must box and
        // return the `undefined` special, not a garbage register: the returned
        // value's SSA node has to be lowered before the return reads its home.
        let v = view(0, 1, &[(Op::ReturnUndefined, vec![], 0)]);
        let (status, value) = run(&v, &[]);
        assert_eq!(status, 0, "returns, no bail");
        assert_eq!(value, SPECIAL_UNDEFINED, "returns boxed undefined");
    }

    #[test]
    fn float_loop_accumulates_through_tagged_phi() {
        // The `LoadNumber` const must be resolved in the view (the test helper
        // leaves it None), so bake 1.5 onto that instruction.
        let mut v = float_accumulate_loop();
        v.instructions[4].load_number = Some(1.5);
        let (status, value) = run(&v, &[boxi(4)]);
        assert_eq!(status, 0, "returns, no bail");
        assert_eq!(unboxf(value), 6.0, "4 * 1.5");
        let (s2, v2) = run(&v, &[boxi(100)]);
        assert_eq!(s2, 0);
        assert_eq!(unboxf(v2), 150.0, "100 * 1.5");
    }
}

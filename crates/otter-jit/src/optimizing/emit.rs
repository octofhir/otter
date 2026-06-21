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
use super::ir::{BlockId, CmpOp, Graph, NodeId, NodeKind, Repr, Terminator};
use super::liveness::Liveness;
use super::regalloc::{Allocation, EdgeMoves, Location};
use crate::CompiledCode;

/// Number of abstract GP registers handed to the allocator (`Reg(0..7)`), mapped
/// to physical `x9..x15`.
pub const GP_REGS: u32 = 7;

/// Number of abstract FP registers handed to the allocator (`Reg(0..6)` of the
/// `Fp` class), mapped to physical `d0..d5`. `d6`/`d7` are reserved as FP emit
/// scratch (load staging, box/unbox, arithmetic temporaries), mirroring the
/// `x16`/`x17` GP scratch pair.
pub const FP_REGS: u32 = 6;

/// Finalized optimizing-tier machine code for one function. Wraps a
/// [`CompiledCode`] and runs through the shared baseline entry, so it inherits
/// the exact reentry ABI and deopt-resume handling.
pub struct OptimizedCode {
    code: CompiledCode,
    /// Loop-header byte-PC → byte offset of that header's OSR-entry trampoline
    /// within `code`. Empty when the function has no eligible loop header.
    osr_offsets: rustc_hash::FxHashMap<u32, usize>,
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

    fn run_entry(&self, ptrs: otter_vm::JitReentryPtrs) -> otter_vm::JitExecOutcome {
        // SAFETY: the mapping is live for `self`, and the entry was emitted with
        // the shared `JitEntry` ABI (`extern "C" fn(*mut JitCtx) -> JitRet`).
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `ptrs` upholds the
        // reentry contract.
        unsafe { crate::baseline::enter_compiled(ptrs, entry) }
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
        Some(unsafe { crate::baseline::enter_compiled(ptrs, entry) })
    }
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
    _osr_entries: &[OsrEntry],
) -> Result<OptimizedCode, Unsupported> {
    Err(Unsupported::Unlowered("optimizing emit: non-aarch64 host"))
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use super::{
        Allocation, BlockId, CmpOp, DeoptPoint, EdgeMoves, GP_REGS, Graph, Liveness, Location,
        NodeId, NodeKind, OptimizedCode, OsrEntry, Repr, Terminator, Unsupported,
    };
    use crate::CompiledCode;
    use crate::baseline::{
        BAIL_PC_OFFSET, OBJECT_BODY_TYPE_TAG, SPECIAL_FALSE, SPECIAL_HOLE, SPECIAL_TRUE,
        STATUS_BAILED, STATUS_RETURNED, TAG_INT32, TAG_PTR_OBJECT, TAG_SPECIAL, THIS_VALUE_OFFSET,
    };
    use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
    use otter_vm::JitFunctionView;
    use rustc_hash::FxHashMap;

    /// Emit scratch register for parallel-move spill→spill staging and tag
    /// immediates (`x16`). Never an allocatable home (those are `x9..x15`).
    const MOVE_SCRATCH: u32 = 16;
    /// Emit scratch register for loaded values being boxed / tested (`x17`).
    const BOX_SCRATCH: u32 = 17;

    /// Physical register holding abstract allocator `Reg(i)`. `Reg(0..GP_REGS)`
    /// → `x9..x15`. Scratch `x16`/`x17` are reserved and never returned here.
    fn phys(i: u32) -> u32 {
        debug_assert!(i < GP_REGS);
        9 + i
    }

    /// Physical FP register holding `Fp`-class allocator `Reg(i)`.
    /// `Reg(0..FP_REGS)` → `d0..d5`. FP scratch `d6`/`d7` are reserved and never
    /// returned here.
    fn phys_fp(i: u32) -> u32 {
        debug_assert!(i < super::FP_REGS);
        i
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
            dynasm!(ops ; .arch aarch64 ; fmov X(gp_dst), D(FP_LOAD_SCRATCH));
        } else {
            load_loc(ops, gp_dst, loc);
            box_value(ops, gp_dst, repr, tag_scratch);
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
                // The FP-resident input had no GP parallel move; read its FP home
                // and store the verbatim (boxed) bits into the phi's GP home.
                load_fp_loc(ops, FP_LOAD_SCRATCH, input_home);
                dynasm!(ops ; .arch aarch64 ; fmov X(BOX_SCRATCH), D(FP_LOAD_SCRATCH));
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
                    ; movz X(scratch_x), TAG_INT32 as u32, lsl #48
                    ; orr X(xr), X(xr), X(scratch_x)
                );
            }
            Repr::Bool => {
                // 0/1 predicate → SPECIAL false(3)/true(4): add the false base
                // (through `scratch_x`, since a dynamic-register immediate add is
                // not expressible in this assembler), then OR the special tag —
                // reusing the same scratch sequentially so only one is needed.
                dynasm!(ops
                    ; .arch aarch64
                    ; movz W(scratch_x), SPECIAL_FALSE
                    ; add W(xr), W(xr), W(scratch_x)
                    ; movz X(scratch_x), TAG_SPECIAL as u32, lsl #48
                    ; orr X(xr), X(xr), X(scratch_x)
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

    /// Emit the function prologue (copied from the baseline) then reserve the
    /// spill area. Returns the byte size subtracted from `sp` (0 when no spill
    /// area is needed).
    fn emit_prologue(ops: &mut Assembler, spill_bytes: u32) {
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
    }

    /// Emit the function epilogue: undo the spill reservation, restore the
    /// callee-saved frame, and return. `x0` (value) and `x1` (status) must be set.
    fn emit_epilogue(ops: &mut Assembler, spill_bytes: u32) {
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

    /// Lower a register-allocated graph to native arm64.
    pub(in crate::optimizing) fn emit(
        view: &JitFunctionView,
        graph: &Graph,
        liveness: &Liveness,
        alloc: &Allocation,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        osr_entries: &[OsrEntry],
    ) -> Result<OptimizedCode, Unsupported> {
        let mut ops = Assembler::new().expect("assembler alloc");

        // Spill area: one frame slot per value spill slot plus one parallel-move
        // cycle-break scratch slot (`Spill(spill_slots)`), rounded to 16 bytes.
        let spill_bytes = align16((alloc.spill_slots + 1) * 8);

        // One dynamic label per block, addressed by BlockId.
        let block_labels: Vec<DynamicLabel> = (0..graph.blocks.len())
            .map(|_| ops.new_dynamic_label())
            .collect();
        // One cold deopt-exit label per deopt-capable node (filled after the body).
        let mut deopt_labels: FxHashMap<NodeId, DynamicLabel> = FxHashMap::default();

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

        let entry = ops.offset();
        emit_prologue(&mut ops, spill_bytes);

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
                    &mut deopt_labels,
                    nid,
                    BOX_SCRATCH,
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
                    emit_epilogue(&mut ops, spill_bytes);
                }
                Terminator::Jump(target) => {
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
                    let false_setup = ops.new_dynamic_label();
                    let true_moves = edge_moves_for(b, *on_true);
                    let false_moves = edge_moves_for(b, *on_false);
                    // An unboxed `Bool` cond is 0/1 → `cbz`. A `Tagged` cond is a
                    // boxed boolean (a comparison result merged through a phi for
                    // `&&` / `||` / a ternary): boxed false is
                    // `TAG_SPECIAL<<48 | SPECIAL_FALSE`, so compare against it.
                    // `JumpIf*` operands are always boolean — the bytecode compiler
                    // emits an explicit truthiness test for non-boolean conditions.
                    if graph.node(*cond).repr == Repr::Bool {
                        dynasm!(&mut ops ; .arch aarch64 ; cbz W(BOX_SCRATCH), =>false_setup);
                    } else {
                        emit_load_u64(
                            &mut ops,
                            MOVE_SCRATCH,
                            (TAG_SPECIAL << 48) | SPECIAL_FALSE as u64,
                        );
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
                    emit_edge(&mut ops, graph, alloc, true_moves, b, *on_true)?;
                    dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[*on_true as usize]);
                    // False trampoline: run the false edge's moves then branch.
                    dynasm!(&mut ops ; .arch aarch64 ; =>false_setup);
                    emit_edge(&mut ops, graph, alloc, false_moves, b, *on_false)?;
                    dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[*on_false as usize]);
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
        )?;

        // OSR-entry trampolines, one per eligible loop header. Each sets up the
        // frame, reloads every live interpreter register from the frame window
        // `[x19, r*8]` into the home the header expects, then branches to the
        // header block. Only headers whose live values all live in tagged homes
        // (loop-carried phis and tagged invariants) are emitted; a header with a
        // typed-home invariant is skipped (the function still runs via its normal
        // entry and the interpreter).
        let mut osr_offsets: rustc_hash::FxHashMap<u32, usize> = rustc_hash::FxHashMap::default();
        for osr in osr_entries {
            if osr.registers.iter().any(|(_, v)| {
                alloc.location.contains_key(v) && graph.node(*v).kind.repr() != Repr::Tagged
            }) {
                continue;
            }
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
            let mut reloads: Vec<(u16, Location)> = Vec::new();
            for &(r, v) in &osr.registers {
                if !phis.contains(&v) && !live_in.contains(&v) {
                    continue;
                }
                if let Some(&home) = alloc.location.get(&v) {
                    reloads.push((r, home));
                }
            }
            let mut homes: Vec<Location> = reloads.iter().map(|&(_, h)| h).collect();
            homes.sort_unstable_by_key(|h| match h {
                Location::Reg(i) => (0u8, *i),
                Location::Spill(i) => (1u8, *i),
            });
            if homes.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }
            let off = ops.offset();
            emit_prologue(&mut ops, spill_bytes);
            for (r, home) in reloads {
                let src_off = u32::from(r) * 8;
                dynasm!(&mut ops ; .arch aarch64 ; ldr X(BOX_SCRATCH), [x19, src_off]);
                store_loc(&mut ops, home, BOX_SCRATCH);
            }
            dynasm!(&mut ops ; .arch aarch64 ; b =>block_labels[osr.block as usize]);
            osr_offsets.insert(osr.byte_pc, off.0);
        }

        let buf = ops
            .finalize()
            .map_err(|_| Unsupported::Unlowered("assembler finalize failed"))?;
        Ok(OptimizedCode {
            code: CompiledCode::new(buf, entry),
            osr_offsets,
        })
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
        deopt_labels: &mut FxHashMap<NodeId, DynamicLabel>,
        nid: NodeId,
        box_scratch: u32,
    ) -> Result<(), Unsupported> {
        let node = graph.node(nid);
        let dst = alloc.location.get(&nid).copied();
        match &node.kind {
            // Entry per-register defs and phis carry no body code.
            NodeKind::Param(_) | NodeKind::Phi(_) => Ok(()),
            NodeKind::ConstUndefined => {
                if let Some(loc) = dst {
                    emit_load_u64(ops, box_scratch, TAG_SPECIAL << 48);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::ConstBool(b) => {
                if let Some(loc) = dst {
                    let special = if *b { SPECIAL_TRUE } else { SPECIAL_FALSE };
                    let bits = (TAG_SPECIAL << 48) | u64::from(special);
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
                // Guard the operand (a boxed Tagged value) is int32; its low 32
                // bits are the unboxed int. A non-int32 input deopts.
                let oloc = require_loc(alloc, *operand)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                load_loc(ops, box_scratch, oloc);
                // Guard top16(value) == TAG_INT32 (0x7FF9) using only the move
                // scratch (x16): two immediate subtracts form 0x7FF9 = 0x7000 +
                // 0xFF9 (each ≤ 0xFFF), avoiding a third reserved register so the
                // value stays in box_scratch and the allocatable file (x9..x15) is
                // never clobbered.
                debug_assert_eq!(TAG_INT32, 0x7FF9);
                dynasm!(ops
                    ; .arch aarch64
                    ; lsr x16, X(box_scratch), #48
                    ; sub x16, x16, #0xFF9
                    ; subs x16, x16, #7, lsl #12
                    ; b.ne =>exit
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
            | NodeKind::Int32Shr(a, b) => {
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
                    _ => unreachable!(),
                }
                if let Some(loc) = dst {
                    dynasm!(ops ; .arch aarch64 ; mov W(box_scratch), w16);
                    store_loc(ops, loc, box_scratch);
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
                // Guard the operand (a boxed Tagged value) is a number, unboxing
                // it to an f64 in the FP home. An int32-tagged operand is widened
                // (scvtf); a real double is its bits verbatim; a non-number
                // (special / pointer tag 0x7FFA..=0x7FFF) deopts.
                let oloc = require_loc(alloc, *operand)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                load_loc(ops, box_scratch, oloc);
                debug_assert_eq!(TAG_INT32, 0x7FF9);
                let int32_path = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                // x16 = top16(value) - TAG_INT32 (= 0x7000 + 0xFF9). Z set ⇒ the
                // value is int32-tagged. Then `(tag - 0x7FF9) ∈ [1, 6]` ⇒ a
                // special / pointer tag (0x7FFA..=0x7FFF) ⇒ non-number ⇒ deopt;
                // every other prefix (the whole double range, including the
                // canonical NaN at 0x7FF8 and the negative half ≥ 0x8000) passes.
                dynasm!(ops
                    ; .arch aarch64
                    ; lsr x16, X(box_scratch), #48
                    ; sub x16, x16, #0xFF9
                    ; subs x16, x16, #7, lsl #12
                    ; b.eq =>int32_path
                    ; cmp x16, #6
                    ; b.ls =>exit
                    // Double: the operand bits are the f64 verbatim.
                    ; fmov D(FP_LOAD_SCRATCH), X(box_scratch)
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
            NodeKind::CheckShape(obj, shape_offset) => {
                // Guard the receiver is an ordinary object of the baked shape:
                // pointer tag, GC type tag, then receiver-shape == baked shape. A
                // miss deopts. The guarded (tagged) receiver is the result.
                let oloc = require_loc(alloc, *obj)?;
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                load_loc(ops, box_scratch, oloc);
                debug_assert_eq!(TAG_PTR_OBJECT, 0x7FFC);
                dynasm!(ops
                    ; .arch aarch64
                    ; lsr x16, X(box_scratch), #48
                    ; sub x16, x16, #0xFFC
                    ; subs x16, x16, #7, lsl #12
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
                dynasm!(ops ; .arch aarch64 ; cmp W(box_scratch), W(MOVE_SCRATCH) ; b.ne =>exit);
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
                    let values_ptr_byte = view.object_values_ptr_byte;
                    let slot_byte = *value_byte;
                    load_loc(ops, box_scratch, oloc);
                    dynasm!(ops ; .arch aarch64 ; mov W(MOVE_SCRATCH), W(box_scratch));
                    emit_load_u64(ops, box_scratch, view.cage_base as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x16, x16, X(box_scratch)
                        ; ldr X(box_scratch), [x16, values_ptr_byte]
                        ; ldr X(box_scratch), [X(box_scratch), slot_byte]
                    );
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::StoreSlot(obj, value_byte, value) => {
                // Write a primitive (int32 / f64) into the shape-guarded
                // receiver's value slab. No write barrier: a primitive `Value` is
                // never a `Gc` pointer (the builder admits only Int32 / Float64).
                if *value_byte > 32760 {
                    return Err(Unsupported::Unlowered(
                        "property slot offset out of str range",
                    ));
                }
                debug_assert_eq!(box_scratch, BOX_SCRATCH);
                let oloc = require_loc(alloc, *obj)?;
                let vloc = require_loc(alloc, *value)?;
                let vrepr = graph.node(*value).kind.repr();
                let values_ptr_byte = view.object_values_ptr_byte;
                let slot_byte = *value_byte;
                // Slab base pointer → x16.
                load_loc(ops, box_scratch, oloc);
                dynasm!(ops ; .arch aarch64 ; mov W(MOVE_SCRATCH), W(box_scratch));
                emit_load_u64(ops, box_scratch, view.cage_base as u64);
                dynasm!(ops
                    ; .arch aarch64
                    ; add x16, x16, X(box_scratch)
                    ; ldr x16, [x16, values_ptr_byte]
                );
                // Boxed value → x17 (box_scratch). Float boxing uses the FP load
                // scratch and never touches x16; int32 boxing inserts the tag with
                // `movk` (the producer zeroed bits 63:32), needing no scratch.
                match vrepr {
                    Repr::Float64 => {
                        box_into_gp(ops, box_scratch, Repr::Float64, vloc, MOVE_SCRATCH)
                    }
                    Repr::Int32 => {
                        load_loc(ops, box_scratch, vloc);
                        dynasm!(ops ; .arch aarch64 ; movk x17, TAG_INT32 as u32, lsl #48);
                    }
                    _ => return Err(Unsupported::Unlowered("store-slot value not int32/f64")),
                }
                dynasm!(ops ; .arch aarch64 ; str x17, [x16, slot_byte]);
                Ok(())
            }
            NodeKind::LoadThis => {
                // `this` bits from JitCtx; a TDZ hole (derived-ctor this before
                // super) deopts to the interpreter.
                let exit = deopt_exit_label(ops, frames, deopt_labels, nid)?;
                dynasm!(ops ; .arch aarch64 ; ldr X(box_scratch), [x20, THIS_VALUE_OFFSET]);
                emit_load_u64(ops, MOVE_SCRATCH, (TAG_SPECIAL << 48) | SPECIAL_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp X(box_scratch), X(MOVE_SCRATCH) ; b.eq =>exit);
                if let Some(loc) = dst {
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
            NodeKind::LoadHole => {
                if let Some(loc) = dst {
                    emit_load_u64(ops, box_scratch, (TAG_SPECIAL << 48) | SPECIAL_HOLE);
                    store_loc(ops, loc, box_scratch);
                }
                Ok(())
            }
        }
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
    fn emit_deopt_exits(
        ops: &mut Assembler,
        graph: &Graph,
        alloc: &Allocation,
        frames: &FxHashMap<NodeId, DeoptPoint>,
        deopt_labels: &FxHashMap<NodeId, DynamicLabel>,
        spill_bytes: u32,
        box_scratch: u32,
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
            // Restore each live register: load the SSA value from its home, box
            // it to a tagged Value per its repr, store to the frame slot.
            for &(regn, value) in &point.registers {
                let loc = require_loc(alloc, value)?;
                let repr = graph.node(value).repr;
                // Box with the move scratch (x16) as the tag temp so it never
                // aliases the value being boxed in box_scratch (x17) or any
                // allocatable register (x9..x15). A Float64 value is fmov-ed
                // verbatim from its FP home (box_into_gp).
                box_into_gp(ops, box_scratch, repr, loc, MOVE_SCRATCH);
                let off = u32::from(regn) * 8;
                dynasm!(ops ; .arch aarch64 ; str X(box_scratch), [x19, off]);
            }
            // Stamp the resume byte-PC into ctx.bail_pc (32-bit).
            emit_load_u64(ops, box_scratch, u64::from(point.byte_pc));
            dynasm!(ops ; .arch aarch64 ; str W(box_scratch), [x20, BAIL_PC_OFFSET]);
            dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_BAILED as u32);
            emit_epilogue(ops, spill_bytes);
        }
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

    const TAG_INT32: u64 = 0x7FF9;

    fn r(n: u16) -> Operand {
        Operand::Register(n)
    }
    fn imm(n: i32) -> Operand {
        Operand::Imm32(n)
    }
    fn boxi(v: i32) -> u64 {
        (TAG_INT32 << 48) | (v as u32 as u64)
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
            jit_proto_byte: 12,
            closure_fid_byte: 8,
            instructions,
            inline_callees: Default::default(),
            inline_methods: Default::default(),
        }
    }

    /// Compile `v`, run it with frame slot `r` preloaded from `params[r]`, and
    /// return `(status, boxed value)`.
    fn run(v: &JitFunctionView, params: &[u64]) -> (u64, u64) {
        let g = build_graph(v).expect("builds");
        let bcl = deopt::bytecode_liveness(v);
        let frames = deopt::capture_frame_states(&g, &bcl);
        let deopt_uses = deopt::deopt_value_uses(&frames);
        let live = liveness::analyze(&g, &deopt_uses);
        let alloc = regalloc::allocate(&g, &live, super::GP_REGS, super::FP_REGS, &deopt_uses);
        let osr = deopt::capture_osr_entries(&g, &bcl);
        let code = super::emit(v, &g, &live, &alloc, &frames, &osr).expect("emits");

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
        let double_bits = 3.5_f64.to_bits(); // a real double, not NaN-boxed int32
        let (status, _value) = run(&v, &[double_bits]);
        assert_eq!(status, 1, "non-int32 param deopts to the interpreter");
    }

    const SPECIAL_UNDEFINED: u64 = 0x7FFA << 48;

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
        assert_eq!(f64::from_bits(value), 2.5);
    }

    #[test]
    fn float_divide_double_param_verbatim() {
        // a = 7.0 (real double, bits verbatim): CheckNumber takes the double
        // path, 7.0/2 = 3.5.
        let (status, value) = run(&divide_by_two(), &[7.0_f64.to_bits()]);
        assert_eq!(status, 0);
        assert_eq!(f64::from_bits(value), 3.5);
    }

    #[test]
    fn float_divide_non_number_bails() {
        // a = undefined: CheckNumber sees a non-number tag and deopts.
        let (status, _value) = run(&divide_by_two(), &[SPECIAL_UNDEFINED]);
        assert_eq!(status, 1, "non-number operand deopts to the interpreter");
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
        assert_eq!(f64::from_bits(value), 6.0, "4 * 1.5");
        let (s2, v2) = run(&v, &[boxi(100)]);
        assert_eq!(s2, 0);
        assert_eq!(f64::from_bits(v2), 150.0, "100 * 1.5");
    }
}

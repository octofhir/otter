//! Arm64 emission for the reducible numeric and reentrant-transition optimizing subset.
//!
//! # Contents
//! - Backend eligibility validation over a pre-verified optimizing unit.
//! - Reducible loop checks, numeric comparisons, and per-edge phi copies.
//! - Loop-header OSR trampolines materializing allocated state from the
//!   interpreter register window.
//! - Bounded batched back-edge polling with loop-header bail writeback.
//! - Precise live-tagged GC safepoints around element, property, global,
//!   comparison, and method-call transitions.
//! - Direct live-value reads from baked global lexical cells and guarded
//!   global-object property records.
//! - Baked stable-entry plain and method calls with stack-owned rooted callee
//!   frames.
//! - Guarded plain- and method-callee splicing with multi-frame exact-PC
//!   deoptimization and synthetic `this` binding.
//! - Loop-invariant method-identity caching and receiver-property guard fusion.
//! - Loop-versioned own-data Number property loads, activated only after one
//!   complete all-hit iteration and invalidated by every semantic miss.
//! - Unboxed numeric residency through source-lowered coercion scaffolding.
//! - Tagged-number guards, mixed-representation arithmetic, spills, boxing,
//!   and bail exits backed by exact deopt frame states.
//!
//! # Invariants
//! - `x20` retains the sole `JitCtx` argument and `x19` retains the canonical
//!   `NativeFrame.register_base`.
//!   GPR linear-scan registers `0..8` map to `x21..x28`, disjoint from both
//!   fixed ABI registers; FP registers `0..8` map to the AAPCS64 callee-saved
//!   `d8..d15`. `x8..x15` and `d16..d17` are caller-saved scratch registers.
//!   GPR spill slots precede FP spill slots in one aligned stack frame.
//! - Every tagged numeric input, including an element-load result, is checked
//!   with the VM's frozen number-tag mask before entering an unboxed operation.
//!   `ToPrimitive` / `ToNumeric` accept only those checked number encodings;
//!   other values bail before any user-observable coercion can run.
//! - Property loop caches contain only tagged Number bits, never GC pointers.
//!   Accessor/proxy/non-number/miss paths cannot activate a version, and every
//!   non-backedge loop entry starts with an empty cache.
//! - `Add`/`Sub` use the arm64 signed-overflow flag and `Mul` compares its
//!   signed 64-bit product with the sign-extended low word. Overflow bails at
//!   the arithmetic instruction's exact logical PC; it never silently wraps.
//!   Int32 `Neg` also bails on zero and overflow so `-0` and `-INT_MIN` retain
//!   exact ECMAScript number semantics; Float64 `Neg` is a native `fneg`.
//! - Every CFG edge targets a block label in reverse postorder. Sequentialized
//!   phi moves execute only on their owning edge before its final jump.
//!   Structurally dead compiler-scratch phis are initialized at block entry
//!   instead of receiving cross-representation edge copies.
//! - Float64 arithmetic never bails for overflow or division by zero. NaNs
//!   remain unordered in comparisons and are canonicalized whenever boxed.
//! - Every backwards bytecode edge targets a dominating loop header. Its phi
//!   moves execute before a native countdown; every sixteenth edge checks the
//!   interrupt cell and debits the shared fuel by sixteen. A slow poll therefore
//!   reconstructs the loop-header frame, and interrupt latency remains bounded.
//! - Every OSR trampoline loads exactly the live loop-header frame-state
//!   values, unboxes them into their allocated locations, and only then
//!   branches to the header body. A representation mismatch bails with the
//!   untouched interpreter window.
//! - Conditional inputs are tagged values. Proven booleans branch by exact
//!   comparison with the VM's `true` immediate; all other values run the full
//!   inline `ToBoolean` reduction before selecting an edge. `LogicalNot` uses
//!   the same reduction and materializes the inverted canonical boolean.
//! - Every reentrant transition boxes its operands plus tagged SSA values live
//!   across the call into their canonical native-frame slots. Its precise frame
//!   bitmap names every tagged input and live-across value; moving-GC reloads
//!   restore live values and load results while numeric machine locations
//!   remain untouched.
//! - A baked global lexical address names a permanent old-space cell. Generated
//!   code loads the cell's current value and uses the canonical transition for
//!   TDZ holes.
//! - A baked global-object load proves the realm epoch, dictionary shape, and
//!   property slot before reading its live value; structural drift uses the
//!   canonical transition.
//! - A non-spliced call enters only its VM-baked native generation. Method
//!   edges additionally prove receiver/prototype/slot identity in generated
//!   code. A method guard-chain miss completes through the canonical
//!   `GetMethod + Call` transition; plain-call misses and native-entry lease
//!   failures take the caller's exact deopt exit.
//! - A spliced method guard may be cached only when the receiver is defined
//!   outside one natural loop and every loop operation is non-mutating and
//!   non-reentrant. Entry and OSR initialize the cache independently; the
//!   first iteration proves identity and later iterations reuse only the
//!   guarded receiver body.
//!   Poll slow paths still bail so the interpreter owns interrupt/budget handling.
//!
//! # See also
//! - [`super`] — public optimizing code object.
//! - [`crate::entry`] — shared reentrant entry ABI and activation publication.
//! - [`crate::ir`] — source analyses and allocation contracts.

// dynasm's dynamic-register forms inject an internal conversion that Clippy
// sees as redundant when our register selector is already a `u8`.
#![allow(clippy::useless_conversion)]

use std::collections::{BTreeMap, BTreeSet};

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::{Op, Operand};
use otter_vm::deopt::{DeoptExitId, DeoptFrame, DeoptLocation, DeoptRepr, DeoptTable};
use otter_vm::native_abi::{
    FrameMap, NO_FRAME_STATE, RuntimeStubDescriptor, STUB_JIT_BACKEDGE_POLL, STUB_JIT_CONSTRUCT,
    STUB_JIT_DEOPT_REIFY_FRAME, STUB_JIT_DEOPT_STACK_CALL, STUB_JIT_LOAD_ELEMENT,
    STUB_JIT_LOAD_GLOBAL, STUB_JIT_LOAD_PROPERTY, STUB_JIT_LOAD_UPVALUE, STUB_JIT_LOOSE_EQ,
    STUB_JIT_SPREAD_CALL_OP, STUB_JIT_STORE_ELEMENT, STUB_JIT_STORE_PROPERTY,
    STUB_JIT_STORE_UPVALUE, STUB_JIT_STORE_UPVALUE_CHECKED, STUB_JIT_WRITE_BARRIER, SafepointId,
    SafepointRecord,
};
use otter_vm::{JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG};

use crate::template::arm64::ic_probe;

use super::{
    OptimizedCode, OptimizedMetadata,
    artifact::render_optimized_unit,
    loop_versioning::{
        PropertyLoopCache, PropertyLoopCachePlan, analyze_property_loop_caches,
        natural_loop_blocks, transparent_origin,
    },
    pipeline::{
        OptimizationError, OptimizationPipeline, total_spill_slots as analyzed_spill_slot_count,
    },
};
use crate::{
    CompiledCode,
    arm64::{
        DirectCallForm, DirectCallSite, MethodGuardSite, direct_call_artifact,
        direct_call_target_is_supported, emit_direct_call, emit_method_guard,
        emit_method_guard_from_tagged_register,
    },
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::{PropertyIcAccess, RelocationCapture, RelocationTarget},
    },
    entry::{
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, GLOBAL_THIS_OFFSET_PTR_OFFSET, MAX_METHOD_ARGS,
        NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET, NUMBER_TAG_HI16,
        OBJECT_BODY_TYPE_TAG, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, THREAD_OFFSET,
        TransitionTable, Unsupported, VALUE_FALSE, VALUE_FALSE_LOW, VALUE_HOLE, VALUE_NULL,
        VALUE_TRUE, VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
        VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET, WhiskerIcCell,
        pack_method_arg_regs,
    },
    ir::{
        cfg::{BlockId, ControlFlowGraph, Terminator},
        deopt_lower::{DeoptLowering, rematerialized_deopt_slot},
        dom::DominatorTree,
        frame_state::{AbstractFrameState, FrameStateTable},
        inline::{InlineCallKind, InlineFrame, InlineId, InlineTree},
        liveness::Liveness,
        regalloc::{
            Allocation, EdgeMoves, Location, Move, RegClass, RegisterBudget, has_non_dead_use,
        },
        repr::{ConversionKind, ReprMap, Representation},
        ssa::{SsaFunction, SsaInstr, SsaOp, ValueDef, ValueId},
    },
    template::arm64::ic_probe::{
        DenseIndexForm, element_access_for, emit_element_address, emit_element_read,
        emit_element_write, emit_guarded_method_call, emit_native_leaf_call,
        guarded_method_call_is_supported, native_leaf_call_is_supported, native_leaf_call_name,
    },
    template::arm64::values::{CellTest, emit_cell_test},
};

mod eligibility;
use eligibility::*;

mod emit_support;
use emit_support::*;

#[cfg(test)]
mod tests;

const ALLOCATABLE_REGISTER_COUNT: u8 = 8;
const REGISTER_BUDGET: RegisterBudget = RegisterBudget {
    gpr: ALLOCATABLE_REGISTER_COUNT,
    fp: 8,
};
const VALUE_REGISTERS: [u8; ALLOCATABLE_REGISTER_COUNT as usize] = [21, 22, 23, 24, 25, 26, 27, 28];
const FP_REGISTERS: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];
const FP_SCRATCH: u8 = 16;
const FP_SCRATCH_2: u8 = 17;
const STACK_SLOT_BYTES: u32 = 8;
const OPTIMIZED_POLL_BATCH: u32 = 16;
const MAX_SPILL_FRAME_BYTES: u32 = 1 << 20;
const MAX_PARAMETER_OFFSET: u32 = 32_760;

#[derive(Debug, Clone, Copy)]
struct GuardedUse {
    use_pc: u32,
}

#[derive(Debug)]
struct Eligibility {
    guarded_uses: Vec<GuardedUse>,
    /// `(deopt-table byte PC, native-frame logical resume PC)` per back-edge.
    /// Back edge -> the exit its poll deoptimizes through, and the header's
    /// logical PC. A poll's deopt state is the target header's entry state.
    back_edges: BTreeMap<(BlockId, BlockId), (DeoptExitId, u32)>,
    /// Verified loop-header entry state keyed by target block.
    osr_entries: BTreeMap<BlockId, OsrEntrySite>,
    /// Precise transition protocol per element load/store logical PC.
    element_transitions: ElementTransitionSafepoints,
    /// Sites whose feedback cell has never recorded an execution. Emission
    /// replaces each with an unconditional deopt: if the cold path is ever
    /// reached, the interpreter runs it, records feedback, bumps the epoch,
    /// and the next compile sees real types.
    insufficient_feedback: BTreeSet<(InlineId, u32)>,
    /// Sole monomorphic method guard proven loop-invariant for this unit. Its
    /// receiver body is cached after the first exact identity check in each
    /// native entry/OSR activation.
    cached_method_guard: Option<(InlineId, u32)>,
    /// Loop-versioned own-data Number loads. A loop becomes cache-active only
    /// after every listed site completed one fast IC hit in the same iteration.
    property_loop_cache: PropertyLoopCachePlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OsrLiveValue {
    value: ValueId,
    register: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OsrEntrySite {
    logical_pc: u32,
    live_values: Box<[OsrLiveValue]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaggedLiveAcross {
    value: ValueId,
    register: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ElementTransitionSite {
    safepoint_id: SafepointId,
    frame_map: FrameMap,
    tagged_live_across: Box<[TaggedLiveAcross]>,
}

#[derive(Debug)]
struct ElementTransitionSafepoints {
    sites: BTreeMap<u32, ElementTransitionSite>,
    /// Concatenated immutable frame-map bitmap words owned by the code object.
    bitmap_words: Box<[u64]>,
}

struct EligibilityAnalyses<'a> {
    liveness: &'a Liveness,
    reprs: &'a ReprMap,
    allocation: &'a Allocation,
    frame_states: &'a FrameStateTable,
}

#[derive(Clone, Copy)]
struct ResolvedRuntimeEntry {
    descriptor: RuntimeStubDescriptor,
    address: u64,
}

impl ResolvedRuntimeEntry {
    const fn new(descriptor: RuntimeStubDescriptor, address: u64) -> Self {
        Self {
            descriptor,
            address,
        }
    }
}

struct EmissionPlan<'a> {
    reprs: &'a ReprMap,
    allocation: &'a Allocation,
    eligibility: &'a Eligibility,
    deopt_table: &'a DeoptTable,
    /// Abstract states, so an emitted exit can be named by its dense id rather
    /// than by a PC, which a body may guard more than once.
    frame_states: &'a FrameStateTable,
    /// This unit's frames; a spliced call guards its callee against the body
    /// the tree chose, and chain exits resolve per-frame logical PCs here.
    tree: &'a InlineTree,
    load_element_entry: ResolvedRuntimeEntry,
    store_element_entry: ResolvedRuntimeEntry,
    load_property_entry: ResolvedRuntimeEntry,
    store_property_entry: ResolvedRuntimeEntry,
    load_global_entry: ResolvedRuntimeEntry,
    loose_eq_entry: ResolvedRuntimeEntry,
    construct_entry: ResolvedRuntimeEntry,
    /// Completes a method-call guard miss through canonical `GetMethod + Call`.
    method_call_entry: ResolvedRuntimeEntry,
    /// Rebuilds a spliced callee's interpreter frame at a deopt exit.
    reify_frame_entry: ResolvedRuntimeEntry,
    /// Refills the back-edge budget and reports raised interrupts.
    poll_entry: ResolvedRuntimeEntry,
    /// Resumes an already-entered generated stack callee after native bailout.
    deopt_stack_call_entry: ResolvedRuntimeEntry,
    /// Repairs an empty stable function-entry cell from installed generations.
    resolve_direct_entry: ResolvedRuntimeEntry,
    /// Reads one captured binding into a window slot; TDZ reads throw.
    load_upvalue_entry: ResolvedRuntimeEntry,
    /// Writes one captured binding with the generational barrier.
    store_upvalue_entry: ResolvedRuntimeEntry,
    /// TDZ-checked captured-binding write.
    store_upvalue_checked_entry: ResolvedRuntimeEntry,
    /// Generational barrier for the inline property-store hit path.
    write_barrier_entry: ResolvedRuntimeEntry,
    /// Total leaf `ToBoolean` probe used by tagged truthiness reduction.
    to_boolean_entry: ResolvedRuntimeEntry,
    /// Exact non-allocating IEEE-754 remainder probe.
    number_rem_entry: ResolvedRuntimeEntry,
    /// Owning function id, baked into property/global transitions so the stub
    /// resolves the name constant against this function's constant pool.
    function_id: u64,
}

struct OptimizedEmission {
    code: CompiledCode,
    osr_entries: BTreeMap<u32, usize>,
    direct_call_events: Option<BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    code_map: Option<CodeMapCapture>,
    relocations: RelocationCapture,
}

#[cfg(test)]
fn compile(view: &JitCompileSnapshot, code_object_id: u64) -> Result<OptimizedCode, Unsupported> {
    let transitions = TransitionTable::resolve();
    compile_with_transitions(view, code_object_id, &transitions)
}

#[cfg(test)]
pub(super) fn compile_with_transitions(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
) -> Result<OptimizedCode, Unsupported> {
    compile_with_artifacts(view, code_object_id, transitions, None, false).map(|output| output.code)
}

pub(super) fn compile_with_artifacts(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
    artifact_request: Option<ArtifactRequest>,
    capture_events: bool,
) -> Result<NativeCompileOutput<OptimizedCode>, Unsupported> {
    // The unit is the root function plus every callee body the inline tree
    // splices into it, from the VM-baked monomorphic candidates. Only bodies
    // this backend lowers entirely into machine registers are spliced: a
    // reentrant transition inside a callee would need an interpreter window the
    // spliced frame does not have, and one unsuitable callee would otherwise
    // cost the whole unit its compilation.
    let unit = OptimizationPipeline::new(REGISTER_BUDGET)
        .analyze(view, splice_lowerable, splice_lowerable_method)
        .map_err(OptimizationError::into_unsupported)?;

    let eligibility = check_eligibility(
        view,
        &unit.tree,
        &unit.cfg,
        &unit.dom,
        &unit.ssa,
        EligibilityAnalyses {
            liveness: &unit.liveness,
            reprs: &unit.reprs,
            allocation: &unit.allocation,
            frame_states: &unit.frame_states,
        },
    )?;
    let load_property_sites = unit
        .dom
        .reverse_postorder()
        .iter()
        .flat_map(|block| unit.ssa.blocks[block.0 as usize].instrs.iter())
        .filter(|instruction| {
            instruction.op == SsaOp::Bytecode(Op::LoadProperty)
                && inline_method_property(&unit.tree, instruction).is_none()
        })
        .count();
    let mut load_ic_cells =
        vec![crate::entry::WhiskerIcCell::default(); load_property_sites].into_boxed_slice();
    let store_property_sites = unit
        .dom
        .reverse_postorder()
        .iter()
        .flat_map(|block| unit.ssa.blocks[block.0 as usize].instrs.iter())
        .filter(|instruction| instruction.op == SsaOp::Bytecode(Op::StoreProperty))
        .count();
    let mut store_ic_cells =
        vec![crate::entry::WhiskerIcCell::default(); store_property_sites].into_boxed_slice();
    let mut emission = emit(
        view,
        &unit.cfg,
        unit.dom.reverse_postorder(),
        &unit.ssa,
        &mut load_ic_cells,
        &mut store_ic_cells,
        EmissionPlan {
            reprs: &unit.reprs,
            allocation: &unit.allocation,
            eligibility: &eligibility,
            deopt_table: unit.deopt.table(),
            frame_states: &unit.frame_states,
            tree: &unit.tree,
            load_element_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_LOAD_ELEMENT,
                transitions.variadic_entry(STUB_JIT_LOAD_ELEMENT),
            ),
            store_element_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_STORE_ELEMENT,
                transitions.variadic_entry(STUB_JIT_STORE_ELEMENT),
            ),
            load_property_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_LOAD_PROPERTY,
                transitions.variadic_entry(STUB_JIT_LOAD_PROPERTY),
            ),
            store_property_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_STORE_PROPERTY,
                transitions.variadic_entry(STUB_JIT_STORE_PROPERTY),
            ),
            load_global_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_LOAD_GLOBAL,
                transitions.variadic_entry(STUB_JIT_LOAD_GLOBAL),
            ),
            loose_eq_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_LOOSE_EQ,
                transitions.variadic_entry(STUB_JIT_LOOSE_EQ),
            ),
            construct_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_CONSTRUCT,
                transitions.variadic_entry(STUB_JIT_CONSTRUCT),
            ),
            method_call_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_SPREAD_CALL_OP,
                transitions.variadic_entry(STUB_JIT_SPREAD_CALL_OP),
            ),
            reify_frame_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_DEOPT_REIFY_FRAME,
                transitions.variadic_entry(STUB_JIT_DEOPT_REIFY_FRAME),
            ),
            poll_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_BACKEDGE_POLL,
                transitions.entry(STUB_JIT_BACKEDGE_POLL),
            ),
            deopt_stack_call_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_DEOPT_STACK_CALL,
                transitions.entry(STUB_JIT_DEOPT_STACK_CALL),
            ),
            resolve_direct_entry: ResolvedRuntimeEntry::new(
                otter_vm::native_abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
                transitions.entry(otter_vm::native_abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            ),
            load_upvalue_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_LOAD_UPVALUE,
                transitions.variadic_entry(STUB_JIT_LOAD_UPVALUE),
            ),
            store_upvalue_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_STORE_UPVALUE,
                transitions.variadic_entry(STUB_JIT_STORE_UPVALUE),
            ),
            store_upvalue_checked_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_STORE_UPVALUE_CHECKED,
                transitions.variadic_entry(STUB_JIT_STORE_UPVALUE_CHECKED),
            ),
            write_barrier_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_WRITE_BARRIER,
                transitions.entry(STUB_JIT_WRITE_BARRIER),
            ),
            to_boolean_entry: ResolvedRuntimeEntry::new(
                otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.descriptor,
                otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
            ),
            number_rem_entry: ResolvedRuntimeEntry::new(
                otter_vm::runtime_stubs::NUMBER_REM_LEAF.descriptor,
                otter_vm::runtime_stubs::NUMBER_REM_LEAF.entry_addr() as u64,
            ),
            function_id: u64::from(view.code_block.id),
        },
        artifact_request.is_some(),
        capture_events,
    )?;
    let frame_maps = eligibility
        .element_transitions
        .sites
        .values()
        .map(|site| site.frame_map)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let safepoint_records = frame_maps
        .iter()
        .copied()
        .map(|frame_map| {
            SafepointRecord::from_frame_map(
                frame_map,
                NO_FRAME_STATE,
                &eligibility.element_transitions.bitmap_words,
            )
            .ok_or(Unsupported::OperandShape(
                "optimizing precise frame-map expansion",
            ))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_boxed_slice();
    let tier_input = artifact_request
        .as_ref()
        .map(|_| render_optimized_unit(&unit));
    let artifact = artifact_request.map(|request| {
        build_bundle(
            request,
            view,
            code_object_id,
            &emission.code,
            otter_vm::JitArtifactFileName::OptimizedIr,
            tier_input.expect("requested artifact has tier input"),
            emission
                .code_map
                .take()
                .expect("requested artifact has code map"),
            std::mem::take(&mut emission.relocations),
            Some(unit.deopt.table()),
            &safepoint_records,
        )
    });
    let code = OptimizedCode::new(
        emission.code,
        None,
        unit.deopt.table().clone(),
        safepoint_records,
        frame_maps,
        eligibility.element_transitions.bitmap_words,
        emission.osr_entries,
        Box::new([]),
        load_ic_cells,
        store_ic_cells,
        OptimizedMetadata {
            code_object_id,
            function_id: view.code_block.id,
            param_count: view.code_block.param_count,
            register_count: view.code_block.register_count,
            machine_register_count: unit
                .allocation
                .register_budget
                .gpr
                .checked_add(unit.allocation.register_budget.fp)
                .ok_or(Unsupported::OperandShape(
                    "optimizing machine register count overflow",
                ))?,
            linear_scan_spill_slot_count: unit.linear_scan_spill_slot_count,
            spill_slot_count: unit.spill_slot_count,
        },
    );
    Ok(NativeCompileOutput {
        code,
        artifact,
        diagnostics: emission
            .direct_call_events
            .map(|events| events.into_values().collect::<Vec<_>>().into_boxed_slice())
            .unwrap_or_default(),
    })
}

/// `true` when every instruction of `callee` lowers into machine registers.
///
/// This is the backend's own splice test, mirrored ahead of tree construction:
/// arithmetic, compares, branches, moves, constants, and returns qualify;
/// anything that calls, allocates, or reaches the heap through a reentrant
/// window transition does not.
fn emit(
    view: &JitCompileSnapshot,
    cfg: &ControlFlowGraph,
    rpo: &[BlockId],
    ssa: &SsaFunction,
    load_ic_cells: &mut [WhiskerIcCell],
    store_ic_cells: &mut [WhiskerIcCell],
    plan: EmissionPlan<'_>,
    capture_artifacts: bool,
    capture_events: bool,
) -> Result<OptimizedEmission, Unsupported> {
    let EmissionPlan {
        reprs,
        allocation,
        eligibility,
        deopt_table,
        frame_states,
        tree,
        load_element_entry,
        store_element_entry,
        load_property_entry,
        store_property_entry,
        load_global_entry,
        loose_eq_entry,
        construct_entry,
        method_call_entry,
        reify_frame_entry,
        poll_entry,
        deopt_stack_call_entry,
        resolve_direct_entry,
        load_upvalue_entry,
        store_upvalue_entry,
        store_upvalue_checked_entry,
        write_barrier_entry,
        to_boolean_entry,
        number_rem_entry,
        function_id,
    } = plan;
    let allocated_spill_bytes = aligned_spill_bytes(total_spill_slots(allocation)?)?;
    let fused_method_receiver_slot = ssa
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .any(|instruction| fused_inline_method_property(tree, ssa, instruction).is_some())
        .then_some(allocated_spill_bytes);
    // Keep the ordinary allocation spill namespace unchanged. Focused
    // codegen state occupies aligned pairs after it and is never part of the
    // deopt spill namespace.
    let after_fused_slots = if fused_method_receiver_slot.is_some() {
        allocated_spill_bytes
            .checked_add(16)
            .ok_or(Unsupported::OperandShape(
                "optimizing fused method spill frame overflow",
            ))?
    } else {
        allocated_spill_bytes
    };
    let property_cache_base = after_fused_slots;
    let property_cache_bytes = eligibility
        .property_loop_cache
        .slot_count
        .checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape(
            "optimizing property loop cache frame overflow",
        ))?;
    let spill_frame_bytes = property_cache_base
        .checked_add(property_cache_bytes)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape(
            "optimizing property loop cache frame overflow",
        ))?;
    if spill_frame_bytes > MAX_SPILL_FRAME_BYTES {
        return Err(Unsupported::OperandShape(
            "optimizing property loop cache frame exceeds arm64 immediates",
        ));
    }
    let mut code_map = capture_artifacts.then(CodeMapCapture::default);
    let mut relocations = RelocationCapture::new(capture_artifacts);
    let mut direct_call_events = capture_events.then(|| {
        let mut events = view
            .direct_callees
            .iter()
            .filter_map(|(&byte_pc, target)| {
                let instruction = view
                    .instructions
                    .iter()
                    .find(|instruction| instruction.byte_pc == byte_pc)?;
                let instruction_pc = instruction.instruction_pc(&view.code_block);
                let outcome = otter_vm::JitDirectCallLoweringOutcome::Rejected {
                    reason: otter_vm::JitDirectCallLoweringRejectionReason::Eliminated,
                };
                Some((
                    (byte_pc, 0),
                    optimizing_direct_call_event(
                        otter_vm::JitDirectCallKind::Plain,
                        instruction_pc,
                        byte_pc,
                        target,
                        0,
                        1,
                        outcome,
                    ),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        for (&byte_pc, methods) in &view.direct_methods {
            let Some(instruction) = view
                .instructions
                .iter()
                .find(|instruction| instruction.byte_pc == byte_pc)
            else {
                continue;
            };
            let instruction_pc = instruction.instruction_pc(&view.code_block);
            for method in methods {
                events.insert(
                    (byte_pc, method.target_index),
                    optimizing_direct_call_event(
                        otter_vm::JitDirectCallKind::Method,
                        instruction_pc,
                        byte_pc,
                        &method.callee,
                        method.target_index,
                        method.target_count,
                        otter_vm::JitDirectCallLoweringOutcome::Rejected {
                            reason: otter_vm::JitDirectCallLoweringRejectionReason::Eliminated,
                        },
                    ),
                );
            }
        }
        for (&byte_pc, target) in &view.static_native_calls {
            let Some(instruction) = view
                .instructions
                .iter()
                .find(|instruction| instruction.byte_pc == byte_pc)
            else {
                continue;
            };
            let instruction_pc = instruction.instruction_pc(&view.code_block);
            events.insert(
                (byte_pc, 0),
                otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                    instruction_pc,
                    byte_pc,
                    target: native_leaf_call_name(target.leaf_stub_id),
                    outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Rejected {
                        reason: otter_vm::JitStaticNativeCallLoweringRejectionReason::Eliminated,
                    },
                },
            );
        }
        events
    });
    let mut next_load_ic = 0usize;
    let mut next_store_ic = 0usize;
    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut boxed_slot_slow_paths = Vec::new();
    let mut deopt_exits = Vec::<(DynamicLabel, DeoptExitId, u32)>::new();
    let threw = ops.new_dynamic_label();
    let block_labels: Vec<_> = (0..cfg.blocks.len())
        .map(|_| ops.new_dynamic_label())
        .collect();
    let entry = ops.offset();
    emit_prologue(&mut ops, spill_frame_bytes);
    if eligibility.cached_method_guard.is_some() {
        dynasm!(ops ; .arch aarch64 ; mov x9, xzr);
        let receiver_slot = fused_method_receiver_slot.expect("cached guard reserves a slot");
        emit_sp_str_x(&mut ops, 9, receiver_slot);
    }
    emit_reset_all_property_loop_caches(
        &mut ops,
        property_cache_base,
        &eligibility.property_loop_cache,
    )?;
    if !eligibility.back_edges.is_empty() {
        dynasm!(ops ; .arch aarch64 ; movz w29, OPTIMIZED_POLL_BATCH);
    }

    dynasm!(ops
        ; .arch aarch64
        ; mov x20, x0
        ; ldr x9, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x19, [x9, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
    // Entry seeds initialize at their own defining block, not at the unit
    // entry: a spliced frame's seeds come alive only when control reaches that
    // frame, and their registers may legitimately be reused from values that
    // are still live at the unit entry.
    for value in &ssa.values {
        if value.def_block != cfg.entry {
            continue;
        }
        match value.def {
            ValueDef::Param { index, .. } => {
                emit_load_parameter(&mut ops, index, 9);
                emit_store_tagged_location(&mut ops, allocation.location(value.id), 9)?;
            }
            ValueDef::Uninitialized { .. } if has_non_dead_use(ssa, value.id) => {
                emit_load_u32(&mut ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                emit_store_tagged_location(&mut ops, allocation.location(value.id), 9)?;
            }
            ValueDef::Uninitialized { .. }
            | ValueDef::ExceptionInput { .. }
            | ValueDef::InlineUndefinedReturn { .. }
            | ValueDef::InlineResult { .. }
            | ValueDef::Phi { .. }
            | ValueDef::Op { .. } => {}
        }
    }
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "entryPrelude",
            entry.0,
            ops.offset().0,
        ));
    }

    let mut operation_index = 0u32;
    for block_id in rpo.iter().copied() {
        let block = &cfg.blocks[block_id.0 as usize];
        let block_prelude_start = ops.offset().0;
        let label = block_labels[block_id.0 as usize];
        dynasm!(ops ; .arch aarch64 ; =>label);
        if block_id != cfg.entry {
            for &head in &ssa.blocks[block_id.0 as usize].phis {
                if matches!(
                    ssa.values[head.0 as usize].def,
                    ValueDef::Uninitialized { .. } | ValueDef::InlineUndefinedReturn { .. }
                ) && has_non_dead_use(ssa, head)
                {
                    emit_load_u32(&mut ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                    emit_store_tagged_location(&mut ops, allocation.location(head), 9)?;
                }
            }
        }
        if let Some(code_map) = code_map.as_mut() {
            code_map.record(CodeRegion::block(
                "blockPrelude",
                block_prelude_start,
                ops.offset().0,
                block_id.0,
            ));
        }
        let block_instructions = &ssa.blocks[block_id.0 as usize].instrs;
        // A lowered site's loop-cache fast path is opened by its shape check
        // and closed by the field read that follows it.
        let mut property_cache_done: Option<DynamicLabel> = None;
        for (instruction_index, instruction) in block_instructions.iter().enumerate() {
            let instruction_start = ops.offset().0;
            if eligibility
                .insufficient_feedback
                .contains(&(instruction.inline, instruction.pc))
            {
                // Never-executed site: deopt to the interpreter, which runs it,
                // records feedback, and triggers a recompile via the epoch.
                let deopt = ops.new_dynamic_label();
                deopt_exits.push((
                    deopt,
                    deopt_exit_at(frame_states, instruction)?,
                    instruction.pc,
                ));
                dynasm!(ops ; .arch aarch64 ; b =>deopt);
                if let Some(code_map) = code_map.as_mut() {
                    let frame = &tree.frames[instruction.inline.0 as usize];
                    code_map.record(CodeRegion::instruction(
                        instruction_start,
                        ops.offset().0,
                        Some(block_id.0),
                        Some(instruction.inline.0),
                        frame.function_id,
                        instruction.pc,
                        frame.instructions[instruction.pc as usize].byte_pc(),
                        Some(operation_index),
                        format!("{:?}", instruction.op),
                    ));
                }
                operation_index = operation_index.saturating_add(1);
                continue;
            }
            let guard_deopt = match eligibility
                .guarded_uses
                .iter()
                .find(|guarded| guarded.use_pc == instruction.pc)
            {
                Some(_) => {
                    let label = ops.new_dynamic_label();
                    deopt_exits.push((
                        label,
                        deopt_exit_at(frame_states, instruction)?,
                        instruction.pc,
                    ));
                    Some(label)
                }
                None => None,
            };
            match instruction.op {
                // The receiver's hidden class is a compile-time constant at a
                // settled site, so the guard is one compare against an
                // immediate. It resolves the holder the field read consumes.
                SsaOp::CheckShape { shape } => {
                    // A loop whose body cannot mutate the heap reads this site
                    // once and replays the value; the guard the cache skips is
                    // this one, so the fast path branches over the whole pair.
                    if let Some(loop_cache_site) = eligibility
                        .property_loop_cache
                        .sites
                        .get(&(instruction.inline, instruction.pc))
                    {
                        let loop_cache =
                            &eligibility.property_loop_cache.loops[&loop_cache_site.header];
                        let field = block_instructions.get(instruction_index + 1).ok_or(
                            Unsupported::OperandShape("optimizing shape check has no read"),
                        )?;
                        let field_location =
                            allocation.location(field.result.ok_or(Unsupported::OperandShape(
                                "optimizing field-read result",
                            ))?);
                        emit_sp_ldr_x(
                            &mut ops,
                            9,
                            property_cache_offset(property_cache_base, loop_cache.ready_slot)?,
                        );
                        let not_ready = ops.new_dynamic_label();
                        dynasm!(ops ; .arch aarch64 ; cbz x9, =>not_ready);
                        emit_sp_ldr_x(
                            &mut ops,
                            9,
                            property_cache_offset(property_cache_base, loop_cache_site.value_slot)?,
                        );
                        emit_store_tagged_location(&mut ops, field_location, 9)?;
                        let done = ops.new_dynamic_label();
                        dynasm!(ops ; .arch aarch64 ; b =>done ; =>not_ready);
                        property_cache_done = Some(done);
                    }
                    let deopt = ops.new_dynamic_label();
                    deopt_exits.push((
                        deopt,
                        deopt_exit_at(frame_states, instruction)?,
                        instruction.pc,
                    ));
                    ic_probe::emit_check_shape(
                        &mut ops,
                        &mut relocations,
                        view,
                        |ops, register| {
                            emit_load_tagged_location(
                                ops,
                                allocation.location(instruction.inputs[0]),
                                register,
                            )
                        },
                        shape,
                        deopt,
                    )?;
                }
                // Reads the holder the preceding check resolved. Nothing
                // between them can allocate or move it, and a slot the
                // compressed encoding cannot hold takes the shared cold path.
                SsaOp::LoadField { byte } => {
                    let result_location = allocation.location(
                        instruction
                            .result
                            .expect("eligibility checked field-load result"),
                    );
                    let deopt = ops.new_dynamic_label();
                    deopt_exits.push((
                        deopt,
                        deopt_exit_at(frame_states, instruction)?,
                        instruction.pc,
                    ));
                    ic_probe::emit_load_field(
                        &mut ops,
                        &mut relocations,
                        view,
                        byte,
                        &mut boxed_slot_slow_paths,
                        deopt,
                    );
                    if let Some(loop_cache_site) = eligibility
                        .property_loop_cache
                        .sites
                        .get(&(instruction.inline, instruction.pc))
                    {
                        // Only a number is replayable from a raw slot: any
                        // other value leaves the slot unfilled, and the loop
                        // never arms.
                        let not_number = ops.new_dynamic_label();
                        let cache_number = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; movz x15, NUMBER_TAG_HI16, lsl #48
                            ; and x14, x9, x15
                            ; cmp x14, x15
                            ; b.eq =>cache_number
                            ; tst x9, x15
                            ; b.eq =>not_number
                            ; =>cache_number
                        );
                        emit_sp_str_x(
                            &mut ops,
                            9,
                            property_cache_offset(property_cache_base, loop_cache_site.value_slot)?,
                        );
                        dynasm!(ops ; .arch aarch64 ; =>not_number);
                    }
                    emit_store_tagged_location(&mut ops, result_location, 9)?;
                    if let Some(done) = property_cache_done.take() {
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                }
                SsaOp::Bytecode(op) => match op {
                    Op::LoadInt32 => {
                        let value = load_int32(view, instruction.pc)?;
                        emit_load_i32(&mut ops, 9, value);
                        emit_store_location(
                            &mut ops,
                            allocation
                                .location(instruction.result.expect("eligibility checked result")),
                            9,
                        )?;
                    }
                    Op::LoadNumber => {
                        let result = instruction.result.expect("eligibility checked result");
                        let value = load_number(view, instruction.pc)?;
                        match reprs.representation(result) {
                            Representation::Int32 => {
                                emit_load_i32(&mut ops, 9, value as i32);
                                emit_store_location(&mut ops, allocation.location(result), 9)?;
                            }
                            Representation::Float64 => {
                                emit_load_u64(&mut ops, 9, value.to_bits());
                                dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), x9);
                                emit_store_fp_location(
                                    &mut ops,
                                    allocation,
                                    allocation.location(result),
                                    FP_SCRATCH,
                                )?;
                            }
                            Representation::Tagged => {
                                return Err(Unsupported::OperandShape(
                                    "optimizing LoadNumber tagged representation",
                                ));
                            }
                        }
                    }
                    Op::LoadTrue | Op::LoadFalse => {
                        let value = if op == Op::LoadTrue {
                            VALUE_TRUE
                        } else {
                            VALUE_FALSE
                        };
                        emit_load_u32(&mut ops, 9, value as u32);
                        emit_store_tagged_location(
                            &mut ops,
                            allocation
                                .location(instruction.result.expect("eligibility checked result")),
                            9,
                        )?;
                    }
                    Op::LoadUndefined => {
                        emit_load_u32(&mut ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                        emit_store_tagged_location(
                            &mut ops,
                            allocation.location(
                                instruction
                                    .result
                                    .expect("eligibility checked undefined result"),
                            ),
                            9,
                        )?;
                    }
                    Op::LoadThis => {
                        if instruction.inline == InlineId::ROOT {
                            // Root `this` is canonical tagged state in NativeFrame.
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                                ; ldr x9, [x10, NATIVE_FRAME_THIS_OFFSET]
                            );
                        } else {
                            emit_load_tagged_location(
                                &mut ops,
                                allocation.location(instruction.inputs[0]),
                                9,
                            )?;
                        }
                        emit_store_tagged_location(
                            &mut ops,
                            allocation.location(
                                instruction
                                    .result
                                    .expect("eligibility checked LoadThis result"),
                            ),
                            9,
                        )?;
                    }
                    Op::LoadNull => {
                        emit_load_u32(&mut ops, 9, otter_vm::Value::null().to_bits() as u32);
                        emit_store_tagged_location(
                            &mut ops,
                            allocation.location(
                                instruction.result.expect("eligibility checked null result"),
                            ),
                            9,
                        )?;
                    }
                    Op::LoadLocal | Op::StoreLocal => {
                        emit_move(
                            &mut ops,
                            allocation,
                            Move {
                                src: allocation.location(instruction.inputs[0]),
                                dst: allocation.location(
                                    instruction.result.expect("eligibility checked local move"),
                                ),
                                conversion: None,
                            },
                        )?;
                    }
                    Op::ToPrimitive | Op::ToNumeric => {
                        emit_tagged_numeric_coercion(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            guard_deopt,
                        )?;
                    }
                    Op::LoadElement => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked element-load destination");
                        let receiver = instruction.input_registers[0];
                        let index = instruction.input_registers[1];
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing element load missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        // Dense fast path: guard an ordinary in-bounds dense read
                        // and load the element directly — no window materialize, no
                        // stub, no reload; nothing here can allocate or move. A
                        // hole is an absent property (the prototype chain answers),
                        // so it takes the generic path like every other miss.
                        let miss = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();
                        let frame = &tree.frames[instruction.inline.0 as usize];
                        let byte_pc = frame
                            .instructions
                            .get(instruction.pc as usize)
                            .map(|metadata| metadata.byte_pc)
                            .ok_or(Unsupported::OperandShape("optimizing element byte PC"))?;
                        let load_access = (instruction.inline == InlineId::ROOT)
                            .then(|| element_access_for(view, byte_pc))
                            .flatten()
                            .copied();
                        if let Some(access) = load_access.as_ref() {
                            emit_element_address(
                                &mut ops,
                                &mut relocations,
                                view,
                                access,
                                |ops, register| {
                                    emit_load_tagged_location(
                                        ops,
                                        allocation.location(instruction.inputs[0]),
                                        register,
                                    )
                                },
                                |ops, register| {
                                    emit_load_dense_index(
                                        ops,
                                        reprs,
                                        allocation,
                                        instruction.inputs[1],
                                        register,
                                    )
                                },
                                dense_index_form(reprs, instruction.inputs[1])?,
                                miss,
                            )?;
                            emit_element_read(&mut ops, access.element, miss);
                            emit_store_tagged_location(
                                &mut ops,
                                allocation.location(
                                    instruction
                                        .result
                                        .expect("eligibility checked element-load result"),
                                ),
                                9,
                            )?;
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                        }
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, dst as u32
                            ; movz x2, receiver as u32
                            ; movz x3, index as u32
                        );
                        emit_runtime_entry(&mut ops, &mut relocations, 16, load_element_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(
                            &mut ops,
                            allocation,
                            site,
                            Some((
                                dst,
                                allocation.location(
                                    instruction
                                        .result
                                        .expect("eligibility checked element-load result"),
                                ),
                            )),
                        )?;
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                    Op::StoreElement => {
                        let receiver = instruction.input_registers[0];
                        let index = instruction.input_registers[1];
                        let value = instruction.input_registers[2];
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing element store missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        // Dense fast path for a numeric value: an in-bounds store
                        // over an existing non-hole element replaces a boxed number
                        // with a boxed number, so no write barrier can be owed (a
                        // number is never a heap cell) and nothing can allocate. A
                        // hole is an absent property — a prototype setter may
                        // observe the store — and a tagged value may be a cell that
                        // needs the generational barrier, so both take the stub.
                        let value_repr = reprs.representation(instruction.inputs[2]);
                        let frame = &tree.frames[instruction.inline.0 as usize];
                        let byte_pc = frame
                            .instructions
                            .get(instruction.pc as usize)
                            .map(|metadata| metadata.byte_pc)
                            .ok_or(Unsupported::OperandShape("optimizing element byte PC"))?;
                        let store_access = (instruction.inline == InlineId::ROOT)
                            .then(|| element_access_for(view, byte_pc))
                            .flatten()
                            .filter(|_| {
                                matches!(
                                    value_repr,
                                    Representation::Int32 | Representation::Float64
                                )
                            })
                            .copied();
                        let miss = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();
                        if let Some(access) = store_access.as_ref() {
                            emit_element_address(
                                &mut ops,
                                &mut relocations,
                                view,
                                access,
                                |ops, register| {
                                    emit_load_tagged_location(
                                        ops,
                                        allocation.location(instruction.inputs[0]),
                                        register,
                                    )
                                },
                                |ops, register| {
                                    emit_load_dense_index(
                                        ops,
                                        reprs,
                                        allocation,
                                        instruction.inputs[1],
                                        register,
                                    )
                                },
                                dense_index_form(reprs, instruction.inputs[1])?,
                                miss,
                            )?;
                            emit_element_read(&mut ops, access.element, miss);
                            match value_repr {
                                Representation::Int32 => {
                                    emit_load_location(
                                        &mut ops,
                                        allocation.location(instruction.inputs[2]),
                                        9,
                                    )?;
                                    emit_box_int32(&mut ops, 9, 11);
                                }
                                Representation::Float64 => {
                                    emit_load_fp_location(
                                        &mut ops,
                                        allocation,
                                        allocation.location(instruction.inputs[2]),
                                        FP_SCRATCH,
                                    )?;
                                    emit_box_double(&mut ops, FP_SCRATCH, 9);
                                }
                                Representation::Tagged => {
                                    return Err(Unsupported::OperandShape(
                                        "optimizing element-store tagged fast value",
                                    ));
                                }
                            }
                            emit_element_write(&mut ops, access.element, miss);
                            dynasm!(ops ; .arch aarch64 ; b =>done ; =>miss);
                        }
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, receiver as u32
                            ; movz x2, index as u32
                            ; movz x3, value as u32
                        );
                        emit_runtime_entry(&mut ops, &mut relocations, 16, store_element_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(&mut ops, allocation, site, None)?;
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                    Op::LoadProperty if inline_method_property(tree, instruction).is_some() => {
                        let (_inline_frame, value_byte, expected_shape) =
                            inline_method_property(tree, instruction).expect("guarded above");
                        let fused = fused_inline_method_property(tree, ssa, instruction).is_some();
                        let result_location = allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked inlined property result"),
                        );
                        let deopt = ops.new_dynamic_label();
                        deopt_exits.push((
                            deopt,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                        if fused {
                            emit_sp_ldr_x(
                                &mut ops,
                                13,
                                fused_method_receiver_slot.expect("fused site reserves a slot"),
                            );
                        } else {
                            emit_load_tagged_location(
                                &mut ops,
                                allocation.location(instruction.inputs[0]),
                                9,
                            )?;
                            dynasm!(ops
                                ; .arch aarch64
                                ; movz x11, NUMBER_TAG_HI16, lsl #48
                                ; orr x11, x11, #0x2
                                ; tst x9, x11
                                ; b.ne =>deopt
                                ; mov w12, w9
                            );
                            emit_load_symbolic_u64(
                                &mut ops,
                                &mut relocations,
                                13,
                                view.cage_base as u64,
                                RelocationTarget::GcCageBase,
                            );
                            dynasm!(ops
                                ; .arch aarch64
                                ; add x13, x13, x12
                                ; ldrb w14, [x13]
                                ; cmp w14, OBJECT_BODY_TYPE_TAG
                                ; b.ne =>deopt
                                ; ldr w14, [x13, view.object_shape_byte]
                            );
                            emit_load_u32(&mut ops, 15, expected_shape);
                            dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.ne =>deopt);
                        }
                        crate::template::arm64::values::emit_slab_base(&mut ops, view, 13, 14);
                        emit_load_u32(&mut ops, 15, value_byte);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x13, =>deopt
                            ; ldr w9, [x13, x15]
                        );
                        crate::template::arm64::values::emit_decompress_slot(
                            &mut ops,
                            &mut relocations,
                            view.cage_base as u64,
                            deopt,
                        );
                        emit_store_tagged_location(&mut ops, result_location, 9)?;
                    }
                    Op::LoadProperty => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked property-load destination");
                        let object = instruction.input_registers[0];
                        let result_location = allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked property-load result"),
                        );
                        let name = view.instructions[instruction.pc as usize]
                            .const_index(view.code_block.as_ref(), 2)
                            .ok_or(Unsupported::OperandShape("property-load name constant"))?;
                        let ic_site = view.instructions[instruction.pc as usize]
                            .property_ic_site(view.code_block.as_ref())
                            .unwrap_or(usize::MAX) as u64;
                        let cell_ordinal = u32::try_from(next_load_ic).map_err(|_| {
                            Unsupported::OperandShape("optimizing property IC ordinal")
                        })?;
                        let cell = load_ic_cells
                            .get_mut(next_load_ic)
                            .ok_or(Unsupported::OperandShape("optimizing property IC cell"))?;
                        let cell_addr = std::ptr::from_mut::<WhiskerIcCell>(cell) as usize;
                        next_load_ic += 1;
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing property load missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        let miss = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();
                        let loop_cache_site = eligibility
                            .property_loop_cache
                            .sites
                            .get(&(instruction.inline, instruction.pc));
                        if let Some(loop_cache_site) = loop_cache_site {
                            let loop_cache =
                                &eligibility.property_loop_cache.loops[&loop_cache_site.header];
                            emit_sp_ldr_x(
                                &mut ops,
                                9,
                                property_cache_offset(property_cache_base, loop_cache.ready_slot)?,
                            );
                            let not_ready = ops.new_dynamic_label();
                            dynasm!(ops ; .arch aarch64 ; cbz x9, =>not_ready);
                            emit_sp_ldr_x(
                                &mut ops,
                                9,
                                property_cache_offset(
                                    property_cache_base,
                                    loop_cache_site.value_slot,
                                )?,
                            );
                            emit_store_tagged_location(&mut ops, result_location, 9)?;
                            dynasm!(ops ; .arch aarch64 ; b =>done ; =>not_ready);
                        }

                        // Inline own-data probe through the self-patching cell:
                        // guard cell tag, body tag, and shape, then read the value
                        // slab slot straight into the destination. The sequence
                        // neither allocates nor calls, so it needs no safepoint, no
                        // frame materialize, and no reload — the receiver pointer is
                        // re-derived from its rooted location every access.
                        let property_byte_pc = tree.frames[instruction.inline.0 as usize]
                            .instructions
                            .get(instruction.pc as usize)
                            .map(|metadata| metadata.byte_pc)
                            .ok_or(Unsupported::OperandShape("optimizing property byte PC"))?;
                        if view.cage_base != 0 {
                            // `.length` on a dense array or a primitive string is
                            // not an own data slot, so no cache program describes
                            // it and the probe below cannot serve it.
                            if view.instructions[instruction.pc as usize].load_array_length {
                                let have_length = ops.new_dynamic_label();
                                let not_length = ops.new_dynamic_label();
                                emit_load_tagged_location(
                                    &mut ops,
                                    allocation.location(instruction.inputs[0]),
                                    9,
                                )?;
                                ic_probe::emit_exotic_length_fast(
                                    &mut ops,
                                    &mut relocations,
                                    view,
                                    have_length,
                                    not_length,
                                );
                                dynasm!(ops ; .arch aarch64 ; =>have_length);
                                emit_store_tagged_location(&mut ops, result_location, 9)?;
                                dynasm!(ops ; .arch aarch64 ; b =>done ; =>not_length);
                            }
                            ic_probe::emit_property_ic_load(
                                &mut ops,
                                &mut relocations,
                                view,
                                (instruction.inline == InlineId::ROOT)
                                    .then(|| view.property_loads.get(&property_byte_pc))
                                    .flatten()
                                    .map(Vec::as_slice),
                                |ops, register| {
                                    emit_load_tagged_location(
                                        ops,
                                        allocation.location(instruction.inputs[0]),
                                        register,
                                    )
                                },
                                cell_addr,
                                cell_ordinal,
                                &mut boxed_slot_slow_paths,
                                miss,
                            )?;
                            if let Some(loop_cache_site) = loop_cache_site {
                                let not_number = ops.new_dynamic_label();
                                let cache_number = ops.new_dynamic_label();
                                dynasm!(ops
                                    ; .arch aarch64
                                    ; movz x15, NUMBER_TAG_HI16, lsl #48
                                    ; and x14, x9, x15
                                    ; cmp x14, x15
                                    ; b.eq =>cache_number
                                    ; tst x9, x15
                                    ; b.eq =>not_number
                                    ; =>cache_number
                                );
                                emit_sp_str_x(
                                    &mut ops,
                                    9,
                                    property_cache_offset(
                                        property_cache_base,
                                        loop_cache_site.value_slot,
                                    )?,
                                );
                                dynasm!(ops ; .arch aarch64 ; =>not_number);
                            }
                            emit_store_tagged_location(&mut ops, result_location, 9)?;
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                        }

                        // Miss: the window transition resolves full `[[Get]]`
                        // semantics and self-patches this site's cell.
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                        if let Some(loop_cache_site) = loop_cache_site {
                            emit_reset_property_loop_cache(
                                &mut ops,
                                property_cache_base,
                                &eligibility.property_loop_cache.loops[&loop_cache_site.header],
                            )?;
                        }
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, dst as u32
                            ; movz x2, object as u32
                        );
                        emit_load_u64(&mut ops, 3, u64::from(name));
                        emit_load_u64(&mut ops, 4, ic_site);
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            5,
                            cell_addr as u64,
                            RelocationTarget::PropertyIcCell {
                                access: PropertyIcAccess::Load,
                                ordinal: cell_ordinal,
                            },
                        );
                        emit_load_u64(&mut ops, 6, function_id);
                        emit_runtime_entry(&mut ops, &mut relocations, 16, load_property_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(
                            &mut ops,
                            allocation,
                            site,
                            Some((dst, result_location)),
                        )?;
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                    Op::StoreProperty => {
                        let object = instruction.input_registers[0];
                        let value = instruction.input_registers[1];
                        let name = view.instructions[instruction.pc as usize]
                            .const_index(view.code_block.as_ref(), 1)
                            .ok_or(Unsupported::OperandShape("property-store name constant"))?;
                        let ic_site = view.instructions[instruction.pc as usize]
                            .property_ic_site(view.code_block.as_ref())
                            .unwrap_or(usize::MAX) as u64;
                        let cell_ordinal = u32::try_from(next_store_ic).map_err(|_| {
                            Unsupported::OperandShape("optimizing store IC ordinal")
                        })?;
                        let cell = store_ic_cells
                            .get_mut(next_store_ic)
                            .ok_or(Unsupported::OperandShape("optimizing store IC cell"))?;
                        let cell_addr = std::ptr::from_mut::<WhiskerIcCell>(cell) as usize;
                        next_store_ic += 1;
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing property store missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        let miss = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();

                        // Inline existing-own-data store through the self-patching
                        // cell: guard cell tag, body tag, and shape, walk the ways,
                        // then write the slab slot. A primitive compresses inline;
                        // a heap cell stores its low word and runs the generational
                        // write barrier through the window (receiver and value are
                        // staged into their slots first). Wide primitives and every
                        // failed guard take the window transition.
                        let store_byte_pc = tree.frames[instruction.inline.0 as usize]
                            .instructions
                            .get(instruction.pc as usize)
                            .map(|metadata| metadata.byte_pc)
                            .ok_or(Unsupported::OperandShape("optimizing property byte PC"))?;
                        if view.cage_base != 0 {
                            ic_probe::emit_property_ic_store_guard(
                                &mut ops,
                                &mut relocations,
                                view,
                                (instruction.inline == InlineId::ROOT)
                                    .then(|| view.property_stores.get(&store_byte_pc))
                                    .flatten()
                                    .map(Vec::as_slice),
                                |ops, register| {
                                    emit_load_tagged_location(
                                        ops,
                                        allocation.location(instruction.inputs[0]),
                                        register,
                                    )
                                },
                                cell_addr,
                                cell_ordinal,
                                miss,
                            )?;
                            // Boxed value bits into x9, whatever its representation.
                            match reprs.representation(instruction.inputs[1]) {
                                Representation::Tagged => {
                                    emit_load_tagged_location(
                                        &mut ops,
                                        allocation.location(instruction.inputs[1]),
                                        9,
                                    )?;
                                }
                                Representation::Int32 => {
                                    emit_load_location(
                                        &mut ops,
                                        allocation.location(instruction.inputs[1]),
                                        9,
                                    )?;
                                    emit_box_int32(&mut ops, 9, 11);
                                }
                                Representation::Float64 => {
                                    emit_load_fp_location(
                                        &mut ops,
                                        allocation,
                                        allocation.location(instruction.inputs[1]),
                                        FP_SCRATCH,
                                    )?;
                                    emit_box_double(&mut ops, FP_SCRATCH, 9);
                                }
                            }
                            let store_prim = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; movz x11, NUMBER_TAG_HI16, lsl #48
                                ; orr x11, x11, #0x2
                                ; tst x9, x11
                                ; b.ne =>store_prim        // primitive: no barrier
                                ; str w9, [x13, x17]
                            );
                            // Cell store: stage receiver and value into their window
                            // slots and run the barrier through the window stub.
                            emit_materialize_frame_value(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction.inputs[0],
                                object,
                            )?;
                            emit_materialize_frame_value(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction.inputs[1],
                                value,
                            )?;
                            dynasm!(ops
                                ; .arch aarch64
                                ; mov x0, x20
                                ; movz x1, object as u32
                                ; movz x2, value as u32
                            );
                            emit_runtime_entry(&mut ops, &mut relocations, 16, write_barrier_entry);
                            dynasm!(ops
                                ; .arch aarch64
                                ; blr x16
                                ; cbnz x0, =>threw
                                ; b =>done
                                ; =>store_prim
                            );
                            crate::template::arm64::values::emit_compress_slot_or_bail(
                                &mut ops, miss,
                            );
                            dynasm!(ops ; .arch aarch64 ; str w10, [x13, x17] ; b =>done);
                        }

                        // Miss: the window transition resolves the store and
                        // self-patches this site's cell.
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, object as u32
                        );
                        emit_load_u64(&mut ops, 2, u64::from(name));
                        dynasm!(ops ; .arch aarch64 ; movz x3, value as u32);
                        emit_load_u64(&mut ops, 4, ic_site);
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            5,
                            cell_addr as u64,
                            RelocationTarget::PropertyIcCell {
                                access: PropertyIcAccess::Store,
                                ordinal: cell_ordinal,
                            },
                        );
                        emit_load_u64(&mut ops, 6, function_id);
                        emit_runtime_entry(&mut ops, &mut relocations, 16, store_property_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(&mut ops, allocation, site, None)?;
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                    Op::LoadUpvalue => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked upvalue-load destination");
                        let result_location = allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked upvalue-load result"),
                        );
                        let index = view.instructions[instruction.pc as usize]
                            .imm32(view.code_block.as_ref(), 1)
                            .ok_or(Unsupported::OperandShape("upvalue-load index"))?;
                        let miss = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();
                        // Inline captured-binding read: spine of 4-byte compressed
                        // cell handles, value at a fixed cell offset. Only a TDZ
                        // hole misses into the stub, which raises the right error.
                        if view.cage_base != 0 && index >= 0 {
                            let spine_offset = (index as u32) * 4;
                            dynasm!(ops
                                        ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; ldr x9, [x10, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
                                        ; cbz x9, =>miss
                                        ; ldr w9, [x9, spine_offset]
                                    );
                            emit_load_symbolic_u64(
                                &mut ops,
                                &mut relocations,
                                13,
                                view.cage_base as u64,
                                RelocationTarget::GcCageBase,
                            );
                            dynasm!(ops
                                ; .arch aarch64
                                ; add x13, x13, x9
                                ; ldr x9, [x13, view.upvalue_value_byte]
                            );
                            emit_load_u64(&mut ops, 11, VALUE_HOLE);
                            dynasm!(ops
                                ; .arch aarch64
                                ; cmp x9, x11
                                ; b.eq =>miss
                            );
                            emit_store_tagged_location(&mut ops, result_location, 9)?;
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                        }
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, dst as u32
                        );
                        emit_load_u64(&mut ops, 2, u64::from(index as u32));
                        emit_runtime_entry(&mut ops, &mut relocations, 16, load_upvalue_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_load_frame_register(&mut ops, u32::from(dst), 9)?;
                        emit_store_tagged_location(&mut ops, result_location, 9)?;
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                    Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                        let src = instruction.input_registers[0];
                        let index = view.instructions[instruction.pc as usize]
                            .imm32(view.code_block.as_ref(), 1)
                            .ok_or(Unsupported::OperandShape("upvalue-store index"))?;
                        emit_materialize_frame_value(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction.inputs[0],
                            src,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, src as u32
                        );
                        emit_load_u64(&mut ops, 2, u64::from(index as u32));
                        emit_runtime_entry(
                            &mut ops,
                            &mut relocations,
                            16,
                            if op == Op::StoreUpvalueChecked {
                                store_upvalue_checked_entry
                            } else {
                                store_upvalue_entry
                            },
                        );
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                    }
                    Op::LoadGlobalOrThrow => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked global-load destination");
                        let metadata = &view.instructions[instruction.pc as usize];
                        let name = metadata
                            .const_index(view.code_block.as_ref(), 1)
                            .ok_or(Unsupported::OperandShape("global-load name constant"))?;
                        let result_location = allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked global-load result"),
                        );
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing global load missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        let miss = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();
                        if let Some(target) = view.global_lexical_loads.get(&metadata.byte_pc)
                            && let Some(cell_addr) =
                                view.cage_base.checked_add(target.cell_offset as usize)
                        {
                            emit_load_symbolic_u64(
                                &mut ops,
                                &mut relocations,
                                13,
                                cell_addr as u64,
                                RelocationTarget::GlobalLexicalCell {
                                    byte_pc: metadata.byte_pc,
                                },
                            );
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr x9, [x13, view.upvalue_value_byte]
                            );
                            emit_load_u64(&mut ops, 11, VALUE_HOLE);
                            dynasm!(ops
                                ; .arch aarch64
                                ; cmp x9, x11
                                ; b.eq =>miss
                            );
                            emit_store_tagged_location(&mut ops, result_location, 9)?;
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                        } else if let Some(target) = view.global_object_loads.get(&metadata.byte_pc)
                        {
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr x14, [x20, THREAD_OFFSET]
                                ; ldr x14, [x14, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET]
                                ; cbz x14, =>miss
                                ; ldr x15, [x14]
                            );
                            emit_load_u64(&mut ops, 11, target.global_lexical_epoch);
                            dynasm!(ops
                                ; .arch aarch64
                                ; cmp x15, x11
                                ; b.ne =>miss
                                ; ldr x14, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET]
                                ; ldr w12, [x14]
                            );
                            emit_load_symbolic_u64(
                                &mut ops,
                                &mut relocations,
                                14,
                                view.cage_base as u64,
                                RelocationTarget::GcCageBase,
                            );
                            dynasm!(ops
                                ; .arch aarch64
                                ; add x13, x14, x12
                                ; ldr w14, [x13, view.object_shape_byte]
                            );
                            if target.dictionary {
                                dynasm!(ops
                                    ; .arch aarch64
                                    ; cbnz w14, =>miss
                                    ; ldr x14, [x13, view.object_dictionary_shape_id_byte]
                                );
                                emit_load_u64(&mut ops, 11, target.shape);
                                dynasm!(ops
                                    ; .arch aarch64
                                    ; cmp x14, x11
                                    ; b.ne =>miss
                                );
                            } else {
                                emit_load_u64(&mut ops, 11, target.shape);
                                dynasm!(ops
                                    ; .arch aarch64
                                    ; cmp w14, w11
                                    ; b.ne =>miss
                                );
                            }
                            crate::template::arm64::values::emit_slab_base(&mut ops, view, 13, 14);
                            dynasm!(ops
                                ; .arch aarch64
                                ; cbz x13, =>miss
                                ; ldr w9, [x13, target.value_byte]
                            );
                            crate::template::arm64::values::emit_decompress_slot(
                                &mut ops,
                                &mut relocations,
                                view.cage_base as u64,
                                miss,
                            );
                            emit_store_tagged_location(&mut ops, result_location, 9)?;
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                        }
                        dynasm!(ops ; .arch aarch64 ; =>miss);
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, dst as u32
                        );
                        emit_load_u64(&mut ops, 2, u64::from(name));
                        emit_load_u64(&mut ops, 3, function_id);
                        emit_runtime_entry(&mut ops, &mut relocations, 16, load_global_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(
                            &mut ops,
                            allocation,
                            site,
                            Some((dst, result_location)),
                        )?;
                        dynasm!(ops ; .arch aarch64 ; =>done);
                    }
                    Op::LooseEqual | Op::LooseNotEqual => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked loose-eq destination");
                        let lhs = instruction.input_registers[0];
                        let rhs = instruction.input_registers[1];
                        let negate = u64::from(op == Op::LooseNotEqual);
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing loose-eq missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, dst as u32
                            ; movz x2, lhs as u32
                            ; movz x3, rhs as u32
                        );
                        emit_load_u64(&mut ops, 4, negate);
                        emit_runtime_entry(&mut ops, &mut relocations, 16, loose_eq_entry);
                        let succeeded = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(
                            &mut ops,
                            allocation,
                            site,
                            Some((
                                dst,
                                allocation.location(
                                    instruction
                                        .result
                                        .expect("eligibility checked loose-eq result"),
                                ),
                            )),
                        )?;
                    }
                    Op::CallMethodValue if is_spliced_call(cfg, block_id, instruction) => {
                        let Terminator::InlineCall { callee_entry, .. } =
                            cfg.blocks[block_id.0 as usize].terminator
                        else {
                            return Err(Unsupported::OperandShape(
                                "optimizing spliced-method terminator",
                            ));
                        };
                        let callee =
                            &tree.frames[cfg.blocks[callee_entry.0 as usize].inline.0 as usize];
                        let call_site = callee.call_site.as_ref().ok_or(
                            Unsupported::OperandShape("optimizing spliced-method call site"),
                        )?;
                        let InlineCallKind::Method { guard, .. } = &call_site.kind else {
                            return Err(Unsupported::OperandShape(
                                "optimizing spliced-method call kind",
                            ));
                        };
                        let deopt = ops.new_dynamic_label();
                        deopt_exits.push((
                            deopt,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                        let fused_receiver = fused_method_property_for_frame(
                            tree,
                            ssa,
                            cfg.blocks[callee_entry.0 as usize].inline,
                        );
                        let cached = eligibility.cached_method_guard
                            == Some((instruction.inline, instruction.pc));
                        let already_guarded = cached.then(|| ops.new_dynamic_label());
                        if let Some(already_guarded) = already_guarded {
                            emit_sp_ldr_x(
                                &mut ops,
                                17,
                                fused_method_receiver_slot.expect("cached guard reserves a slot"),
                            );
                            dynasm!(ops ; .arch aarch64 ; cbnz x17, =>already_guarded);
                        }
                        emit_load_tagged_location(
                            &mut ops,
                            allocation.location(instruction.inputs[0]),
                            9,
                        )?;
                        emit_method_guard_from_tagged_register(
                            &mut ops,
                            &mut relocations,
                            view,
                            guard,
                            9,
                            if fused_receiver { 16 } else { 17 },
                            fused_receiver.then_some(17),
                            deopt,
                        )?;
                        if fused_receiver {
                            emit_sp_str_x(
                                &mut ops,
                                17,
                                fused_method_receiver_slot.expect("fused site reserves a slot"),
                            );
                        }
                        if let Some(already_guarded) = already_guarded {
                            dynasm!(ops ; .arch aarch64 ; =>already_guarded);
                        }

                        if instruction.inline == InlineId::ROOT {
                            let frame = &tree.frames[instruction.inline.0 as usize];
                            let byte_pc = frame.instructions[instruction.pc as usize].byte_pc;
                            if let (Some(events), Some(target)) = (
                                direct_call_events.as_mut(),
                                view.direct_methods
                                    .get(&byte_pc)
                                    .and_then(|targets| targets.first()),
                            ) {
                                events.insert(
                                    (byte_pc, target.target_index),
                                    optimizing_direct_call_event(
                                        otter_vm::JitDirectCallKind::Method,
                                        instruction.pc,
                                        byte_pc,
                                        &target.callee,
                                        target.target_index,
                                        target.target_count,
                                        otter_vm::JitDirectCallLoweringOutcome::Inlined,
                                    ),
                                );
                            }
                        }
                    }
                    Op::CallMethodValue => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked method-call destination");
                        let receiver = instruction.input_registers[0];
                        let arg_regs = &instruction.input_registers[1..];
                        let name = view.instructions[instruction.pc as usize]
                            .const_index(view.code_block.as_ref(), 2)
                            .ok_or(Unsupported::OperandShape("optimizing method call name"))?;
                        let frame = &tree.frames[instruction.inline.0 as usize];
                        let byte_pc = frame
                            .instructions
                            .get(instruction.pc as usize)
                            .map(|metadata| metadata.byte_pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing direct method byte PC",
                            ))?;
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing method call missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                        );

                        let succeeded = ops.new_dynamic_label();
                        let bail = ops.new_dynamic_label();
                        let native_leaf = (instruction.inline == InlineId::ROOT)
                            .then(|| view.guarded_method_calls.get(&byte_pc))
                            .flatten()
                            .filter(|call| arg_regs.len() == usize::from(call.argument_count))
                            .filter(|call| guarded_method_call_is_supported(view, call));
                        if let Some(call) = native_leaf {
                            let leaf_miss = ops.new_dynamic_label();
                            emit_guarded_method_call(
                                &mut ops,
                                &mut relocations,
                                view,
                                call,
                                receiver,
                                byte_pc,
                                |ops, index, register| {
                                    let source = arg_regs.get(usize::from(index)).copied().ok_or(
                                        Unsupported::OperandShape("guarded method argument"),
                                    )?;
                                    crate::template::arm64::values::emit_load_reg(
                                        ops, register, source,
                                    )
                                },
                                leaf_miss,
                            )?;
                            emit_store_frame_register(&mut ops, u32::from(dst), 0)?;
                            dynasm!(ops
                                ; .arch aarch64
                                ; b =>succeeded
                                ; =>leaf_miss
                                ; b =>bail
                            );
                        }
                        let planned_methods = native_leaf
                            .is_none()
                            .then(|| {
                                (instruction.inline == InlineId::ROOT)
                                    .then(|| view.direct_methods.get(&byte_pc))
                                    .flatten()
                            })
                            .flatten();
                        for method in planned_methods.into_iter().flatten() {
                            if !direct_call_target_is_supported(&method.callee) {
                                if let Some(events) = direct_call_events.as_mut() {
                                    events.insert(
                                        (byte_pc, method.target_index),
                                        optimizing_direct_call_event(
                                            otter_vm::JitDirectCallKind::Method,
                                            instruction.pc,
                                            byte_pc,
                                            &method.callee,
                                            method.target_index,
                                            method.target_count,
                                            otter_vm::JitDirectCallLoweringOutcome::Rejected {
                                                reason: otter_vm::JitDirectCallLoweringRejectionReason::LayoutUnsupported,
                                            },
                                        ),
                                    );
                                }
                                continue;
                            }
                            let next_target = ops.new_dynamic_label();
                            let direct_site = DirectCallSite {
                                target: &method.callee,
                                caller_function_id: frame.function_id,
                                logical_pc: instruction.pc,
                                byte_pc,
                                dst,
                                form: DirectCallForm::Method {
                                    callable: 17,
                                    receiver,
                                },
                                arguments: arg_regs,
                            };
                            let direct_call = direct_call_artifact(view, direct_site)?;
                            let guard_start = ops.offset().0;
                            emit_method_guard(
                                &mut ops,
                                &mut relocations,
                                view,
                                MethodGuardSite {
                                    guard: &method.guard,
                                    receiver,
                                },
                                17,
                                None,
                                next_target,
                            )?;
                            if let Some(code_map) = code_map.as_mut() {
                                code_map.record(CodeRegion::method_call_structural(
                                    "directMethodGuard",
                                    guard_start,
                                    ops.offset().0,
                                    direct_site.caller_function_id,
                                    direct_site.logical_pc,
                                    direct_site.byte_pc,
                                    direct_call,
                                    receiver,
                                    &method.guard,
                                ));
                            }
                            emit_direct_call(
                                &mut ops,
                                &mut relocations,
                                view,
                                direct_site,
                                deopt_stack_call_entry.address,
                                resolve_direct_entry.address,
                                code_map.as_mut(),
                                bail,
                                threw,
                                succeeded,
                            )?;
                            if let Some(events) = direct_call_events.as_mut() {
                                events.insert(
                                    (byte_pc, method.target_index),
                                    optimizing_direct_call_event(
                                        otter_vm::JitDirectCallKind::Method,
                                        instruction.pc,
                                        byte_pc,
                                        &method.callee,
                                        method.target_index,
                                        method.target_count,
                                        otter_vm::JitDirectCallLoweringOutcome::Generated {
                                            code_object_id: method.callee.plan.code_object_id,
                                            target_tier: optimizing_direct_call_target_tier(
                                                &method.callee,
                                            ),
                                            this_mode:
                                                otter_vm::JitDirectCallThisMode::MethodReceiver,
                                        },
                                    ),
                                );
                            }
                            dynasm!(ops ; .arch aarch64 ; =>next_target);
                        }
                        let packed_meta = u64::from(dst)
                            | (u64::from(receiver) << 16)
                            | ((arg_regs.len() as u64) << 32);
                        let packed_args = pack_method_arg_regs(arg_regs);
                        dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                        emit_load_u64(&mut ops, 1, u64::from(Op::CallMethodValue as u8));
                        emit_load_u64(&mut ops, 2, packed_meta);
                        emit_load_u64(&mut ops, 3, packed_args);
                        emit_load_u64(&mut ops, 4, u64::from(name));
                        emit_runtime_entry(&mut ops, &mut relocations, 16, method_call_entry);
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbz x0, =>succeeded
                            ; cmp x0, STATUS_BAILED as u32
                            ; b.eq =>bail
                            ; cmp x0, STATUS_THREW as u32
                            ; b.eq =>threw
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(
                            &mut ops,
                            allocation,
                            site,
                            Some((
                                dst,
                                allocation.location(
                                    instruction
                                        .result
                                        .expect("eligibility checked method-call result"),
                                ),
                            )),
                        )?;
                        deopt_exits.push((
                            bail,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    Op::New => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked construct destination");
                        let callee = instruction.input_registers[0];
                        let arg_regs = &instruction.input_registers[1..];
                        let argc = arg_regs.len() as u32;
                        let packed = pack_method_arg_regs(arg_regs);
                        let site = eligibility
                            .element_transitions
                            .sites
                            .get(&instruction.pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing construct missing site",
                            ))?;
                        debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                        emit_materialize_element_transition(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            site,
                        )?;
                        emit_load_u32(&mut ops, 9, instruction.pc);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            ; mov x0, x20
                            ; movz x1, dst as u32
                            ; movz x2, callee as u32
                            ; movz x3, argc
                        );
                        emit_load_u64(&mut ops, 4, packed);
                        emit_runtime_entry(&mut ops, &mut relocations, 16, construct_entry);
                        let succeeded = ops.new_dynamic_label();
                        let bail = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cmp x0, #1
                            ; b.eq =>threw
                            ; cmp x0, #2
                            ; b.eq =>bail
                            ; cbz x0, =>succeeded
                            ; b =>threw
                            ; =>succeeded
                        );
                        emit_reload_element_transition(
                            &mut ops,
                            allocation,
                            site,
                            Some((
                                dst,
                                allocation.location(
                                    instruction
                                        .result
                                        .expect("eligibility checked construct result"),
                                ),
                            )),
                        )?;
                        // A non-constructor report (`2`) has no committed effects;
                        // deopt re-runs the opcode to create the canonical TypeError.
                        deopt_exits.push((
                            bail,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    Op::LogicalNot => {
                        let result = instruction
                            .result
                            .expect("eligibility checked logical-not result");
                        emit_load_boxed_value(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction.inputs[0],
                            9,
                        )?;
                        let bail = ops.new_dynamic_label();
                        emit_truthiness_reduce(&mut ops, &mut relocations, to_boolean_entry, bail);
                        emit_load_u32(&mut ops, 10, VALUE_TRUE as u32);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp w9, w10
                            ; cset w11, ne
                            ; movz w12, VALUE_FALSE_LOW
                            ; add w11, w11, w12
                        );
                        emit_store_tagged_location(&mut ops, allocation.location(result), 11)?;
                        deopt_exits.push((
                            bail,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    Op::Neg => {
                        let result = instruction
                            .result
                            .expect("eligibility checked negate result");
                        match reprs.representation(result) {
                            Representation::Int32 => {
                                emit_load_int_operand(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction,
                                    0,
                                    9,
                                    guard_deopt,
                                )?;
                                let deopt = ops.new_dynamic_label();
                                dynasm!(ops
                                    ; .arch aarch64
                                    ; cbz w9, =>deopt
                                    ; negs w11, w9
                                    ; b.vs =>deopt
                                );
                                emit_store_location(&mut ops, allocation.location(result), 11)?;
                                deopt_exits.push((
                                    deopt,
                                    deopt_exit_at(frame_states, instruction)?,
                                    instruction.pc,
                                ));
                            }
                            Representation::Float64 => {
                                emit_load_float_operand(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction,
                                    0,
                                    FP_SCRATCH,
                                    guard_deopt,
                                )?;
                                dynasm!(ops ; .arch aarch64 ; fneg D(FP_SCRATCH), D(FP_SCRATCH));
                                emit_store_fp_location(
                                    &mut ops,
                                    allocation,
                                    allocation.location(result),
                                    FP_SCRATCH,
                                )?;
                            }
                            Representation::Tagged => {
                                return Err(Unsupported::OperandShape(
                                    "optimizing negate tagged representation",
                                ));
                            }
                        }
                    }
                    Op::Increment => {
                        let result = instruction
                            .result
                            .expect("eligibility checked increment result");
                        let delta = view.instructions[instruction.pc as usize]
                            .imm32(view.code_block.as_ref(), 2)
                            .ok_or(Unsupported::OperandShape("increment delta operand"))?;
                        emit_load_int_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            0,
                            9,
                            guard_deopt,
                        )?;
                        emit_load_u32(&mut ops, 10, delta as u32);
                        let deopt = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; adds w11, w9, w10
                            ; b.vs =>deopt
                        );
                        emit_store_location(&mut ops, allocation.location(result), 11)?;
                        deopt_exits.push((
                            deopt,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    // Immediate-right int32 add/subtract — the register-plus-constant
                    // form of `Op::Add` / `Op::Sub` with an overflow deopt, mirroring
                    // `Op::Increment`.
                    Op::AddImm | Op::SubImm => {
                        let result = instruction
                            .result
                            .expect("eligibility checked immediate result");
                        let imm = view.instructions[instruction.pc as usize]
                            .imm32(view.code_block.as_ref(), 2)
                            .ok_or(Unsupported::OperandShape("immediate operand"))?;
                        emit_load_int_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            0,
                            9,
                            guard_deopt,
                        )?;
                        emit_load_u32(&mut ops, 10, imm as u32);
                        let deopt = ops.new_dynamic_label();
                        if op == Op::AddImm {
                            dynasm!(ops ; .arch aarch64 ; adds w11, w9, w10 ; b.vs =>deopt);
                        } else {
                            dynasm!(ops ; .arch aarch64 ; subs w11, w9, w10 ; b.vs =>deopt);
                        }
                        emit_store_location(&mut ops, allocation.location(result), 11)?;
                        deopt_exits.push((
                            deopt,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    // Immediate-right int32 bitwise AND. No overflow, so no deopt.
                    Op::BitwiseAndImm => {
                        let result = instruction
                            .result
                            .expect("eligibility checked immediate result");
                        let imm = view.instructions[instruction.pc as usize]
                            .imm32(view.code_block.as_ref(), 2)
                            .ok_or(Unsupported::OperandShape("immediate operand"))?;
                        emit_load_int_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            0,
                            9,
                            guard_deopt,
                        )?;
                        emit_load_u32(&mut ops, 10, imm as u32);
                        dynasm!(ops ; .arch aarch64 ; and w11, w9, w10);
                        emit_store_location(&mut ops, allocation.location(result), 11)?;
                    }
                    // Immediate-right int32 comparison, boxed to a boolean (never
                    // fused with a following branch — `fused_numeric_compare_at`
                    // matches only the register forms).
                    Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
                        let result = instruction
                            .result
                            .expect("eligibility checked immediate result");
                        let imm = view.instructions[instruction.pc as usize]
                            .imm32(view.code_block.as_ref(), 2)
                            .ok_or(Unsupported::OperandShape("immediate operand"))?;
                        emit_load_int_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            0,
                            9,
                            guard_deopt,
                        )?;
                        emit_load_u32(&mut ops, 10, imm as u32);
                        let register_op = match op {
                            Op::LessThanImm => Op::LessThan,
                            Op::EqualImm => Op::Equal,
                            _ => Op::NotEqual,
                        };
                        emit_int_comparison(&mut ops, register_op);
                        emit_store_tagged_location(&mut ops, allocation.location(result), 11)?;
                    }
                    Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
                        let result = instruction.result.expect("eligibility checked result");
                        match reprs.representation(result) {
                            Representation::Int32 => {
                                emit_load_int_operand(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction,
                                    0,
                                    9,
                                    guard_deopt,
                                )?;
                                emit_load_int_operand(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction,
                                    1,
                                    10,
                                    guard_deopt,
                                )?;
                                let deopt = ops.new_dynamic_label();
                                match op {
                                    Op::Add => dynasm!(ops
                                        ; .arch aarch64
                                        ; adds w11, w9, w10
                                        ; b.vs =>deopt
                                    ),
                                    Op::Sub => dynasm!(ops
                                        ; .arch aarch64
                                        ; subs w11, w9, w10
                                        ; b.vs =>deopt
                                    ),
                                    Op::Mul => dynasm!(ops
                                        ; .arch aarch64
                                        ; smull x11, w9, w10
                                        ; sxtw x12, w11
                                        ; cmp x11, x12
                                        ; b.ne =>deopt
                                    ),
                                    _ => return Err(Unsupported::Opcode(op)),
                                }
                                emit_store_location(&mut ops, allocation.location(result), 11)?;
                                deopt_exits.push((
                                    deopt,
                                    deopt_exit_at(frame_states, instruction)?,
                                    instruction.pc,
                                ));
                            }
                            Representation::Float64 => {
                                emit_load_float_operand(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction,
                                    0,
                                    FP_SCRATCH,
                                    guard_deopt,
                                )?;
                                emit_load_float_operand(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction,
                                    1,
                                    FP_SCRATCH_2,
                                    guard_deopt,
                                )?;
                                match op {
                                    Op::Add => dynasm!(ops
                                        ; .arch aarch64
                                        ; fadd D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                    ),
                                    Op::Sub => dynasm!(ops
                                        ; .arch aarch64
                                        ; fsub D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                    ),
                                    Op::Mul => dynasm!(ops
                                        ; .arch aarch64
                                        ; fmul D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                    ),
                                    Op::Div => dynasm!(ops
                                        ; .arch aarch64
                                        ; fdiv D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                    ),
                                    Op::Rem => {
                                        // AArch64 has no IEEE-754 fmod instruction.
                                        // Box the proven numeric operands and call the
                                        // frozen non-allocating exact remainder leaf,
                                        // then recover the unboxed Float64 result.
                                        emit_box_double(&mut ops, FP_SCRATCH, 1);
                                        emit_box_double(&mut ops, FP_SCRATCH_2, 2);
                                        dynasm!(ops
                                            ; .arch aarch64
                                            ; ldr x0, [x20, THREAD_OFFSET]
                                            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                                        );
                                        emit_runtime_entry(
                                            &mut ops,
                                            &mut relocations,
                                            16,
                                            number_rem_entry,
                                        );
                                        let deopt = ops.new_dynamic_label();
                                        dynasm!(ops
                                            ; .arch aarch64
                                            ; blr x16
                                            ; cbnz x1, =>deopt
                                        );
                                        emit_num_to_double(&mut ops, 0, FP_SCRATCH, deopt);
                                        deopt_exits.push((
                                            deopt,
                                            deopt_exit_at(frame_states, instruction)?,
                                            instruction.pc,
                                        ));
                                    }
                                    _ => return Err(Unsupported::Opcode(op)),
                                }
                                emit_store_fp_location(
                                    &mut ops,
                                    allocation,
                                    allocation.location(result),
                                    FP_SCRATCH,
                                )?;
                            }
                            Representation::Tagged => {
                                return Err(Unsupported::OperandShape(
                                    "optimizing arithmetic tagged representation",
                                ));
                            }
                        }
                    }
                    Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr => {
                        let result = instruction.result.expect("eligibility checked result");
                        let float_form = reprs.representation(result) == Representation::Float64;
                        if float_form {
                            // Mixed numeric operands: exact JS ToInt32 per operand
                            // (fjcvtzs truncates and wraps modulo 2^32), integer
                            // op, and the int32 result back as an exact double.
                            emit_load_float_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                0,
                                FP_SCRATCH,
                                guard_deopt,
                            )?;
                            emit_load_float_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                1,
                                FP_SCRATCH_2,
                                guard_deopt,
                            )?;
                            dynasm!(ops
                                ; .arch aarch64
                                ; fjcvtzs w9, D(FP_SCRATCH)
                                ; fjcvtzs w10, D(FP_SCRATCH_2)
                            );
                        } else {
                            emit_load_int_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                0,
                                9,
                                guard_deopt,
                            )?;
                            emit_load_int_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                1,
                                10,
                                guard_deopt,
                            )?;
                        }
                        match op {
                            Op::BitwiseAnd => dynasm!(ops ; .arch aarch64 ; and w11, w9, w10),
                            Op::BitwiseOr => dynasm!(ops ; .arch aarch64 ; orr w11, w9, w10),
                            Op::BitwiseXor => dynasm!(ops ; .arch aarch64 ; eor w11, w9, w10),
                            // JS masks the shift count to the low 5 bits.
                            Op::Shl => dynasm!(ops
                                ; .arch aarch64
                                ; and w10, w10, #31
                                ; lsl w11, w9, w10
                            ),
                            Op::Shr => dynasm!(ops
                                ; .arch aarch64
                                ; and w10, w10, #31
                                ; asr w11, w9, w10
                            ),
                            _ => return Err(Unsupported::Opcode(op)),
                        }
                        if float_form {
                            dynasm!(ops ; .arch aarch64 ; scvtf D(FP_SCRATCH), w11);
                            emit_store_fp_location(
                                &mut ops,
                                allocation,
                                allocation.location(result),
                                FP_SCRATCH,
                            )?;
                        } else {
                            emit_store_location(&mut ops, allocation.location(result), 11)?;
                        }
                    }
                    Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual => {
                        let feedback = frame_feedback(tree, instruction);
                        let fused_branch = fused_numeric_compare_at(
                            tree,
                            block_instructions,
                            instruction_index,
                            &eligibility.insufficient_feedback,
                        );
                        if feedback.is_int32_only() {
                            emit_load_int_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                0,
                                9,
                                guard_deopt,
                            )?;
                            emit_load_int_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                1,
                                10,
                                guard_deopt,
                            )?;
                            if fused_branch {
                                emit_int_compare_flags(&mut ops);
                            } else {
                                emit_int_comparison(&mut ops, op);
                            }
                        } else if feedback.is_numeric_only() {
                            emit_load_float_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                0,
                                FP_SCRATCH,
                                guard_deopt,
                            )?;
                            emit_load_float_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                1,
                                FP_SCRATCH_2,
                                guard_deopt,
                            )?;
                            if fused_branch {
                                emit_float_compare_flags(&mut ops);
                            } else {
                                emit_float_comparison(&mut ops, op);
                            }
                        } else {
                            // Mixed operands: total strict (in)equality over the
                            // tagged values, shared with the template tier. The
                            // probe's only miss is a null heap.
                            let deopt = ops.new_dynamic_label();
                            deopt_exits.push((
                                deopt,
                                deopt_exit_at(frame_states, instruction)?,
                                instruction.pc,
                            ));
                            emit_load_tagged_location(
                                &mut ops,
                                allocation.location(instruction.inputs[0]),
                                9,
                            )?;
                            emit_load_tagged_location(
                                &mut ops,
                                allocation.location(instruction.inputs[1]),
                                10,
                            )?;
                            crate::template::arm64::arith::emit_strict_eq_tagged(
                                &mut ops,
                                &mut relocations,
                                op == Op::NotEqual,
                                deopt,
                            );
                            crate::template::arm64::values::emit_box_bool(&mut ops, 13, 12);
                            dynasm!(ops ; .arch aarch64 ; mov x11, x13);
                        }
                        if !fused_branch {
                            emit_store_tagged_location(
                                &mut ops,
                                allocation.location(
                                    instruction.result.expect("eligibility checked result"),
                                ),
                                11,
                            )?;
                        }
                    }
                    Op::Jump => {
                        let target = block.normal_succs[0];
                        emit_cfg_edge(
                            &mut ops,
                            &mut relocations,
                            allocation,
                            eligibility,
                            property_cache_base,
                            poll_entry,
                            threw,
                            &block_labels,
                            block_id,
                            target,
                        )?;
                    }
                    Op::JumpIfTrue | Op::JumpIfFalse => {
                        let Terminator::Branch { taken, fallthrough } = block.terminator else {
                            return Err(Unsupported::OperandShape("optimizing branch terminator"));
                        };
                        if let Some(compare_index) =
                            instruction_index.checked_sub(1).filter(|&index| {
                                fused_numeric_compare_at(
                                    tree,
                                    block_instructions,
                                    index,
                                    &eligibility.insufficient_feedback,
                                )
                            })
                        {
                            let comparison = &block_instructions[compare_index];
                            let comparison_op = comparison
                                .op
                                .bytecode()
                                .expect("a fused comparison is a bytecode node");
                            let result = comparison
                                .result
                                .expect("fused comparison owns its branch condition");
                            let branch_on_true = op == Op::JumpIfTrue;
                            let taken_edge = ops.new_dynamic_label();
                            let feedback = frame_feedback(tree, comparison);
                            if feedback.is_int32_only() {
                                emit_int_comparison_branch(
                                    &mut ops,
                                    comparison_op,
                                    branch_on_true,
                                    taken_edge,
                                );
                            } else {
                                debug_assert!(feedback.is_numeric_only());
                                emit_float_comparison_branch(
                                    &mut ops,
                                    comparison_op,
                                    branch_on_true,
                                    taken_edge,
                                );
                            }

                            // The comparison's bytecode destination remains part of
                            // later exact-PC frame states. Each outgoing edge proves
                            // its boolean value, so materialize that constant after
                            // the flags branch instead of boxing and spilling before
                            // immediately loading it back for `JumpIf*`.
                            emit_store_boolean_constant(
                                &mut ops,
                                allocation.location(result),
                                !branch_on_true,
                            )?;
                            emit_cfg_edge(
                                &mut ops,
                                &mut relocations,
                                allocation,
                                eligibility,
                                property_cache_base,
                                poll_entry,
                                threw,
                                &block_labels,
                                block_id,
                                fallthrough,
                            )?;
                            dynasm!(ops ; .arch aarch64 ; =>taken_edge);
                            emit_store_boolean_constant(
                                &mut ops,
                                allocation.location(result),
                                branch_on_true,
                            )?;
                            emit_cfg_edge(
                                &mut ops,
                                &mut relocations,
                                allocation,
                                eligibility,
                                property_cache_base,
                                poll_entry,
                                threw,
                                &block_labels,
                                block_id,
                                taken,
                            )?;
                        } else {
                            emit_load_tagged_location(
                                &mut ops,
                                allocation.location(instruction.inputs[0]),
                                9,
                            )?;
                            // A provably-boolean condition compares directly; any other
                            // tagged value is reduced to `VALUE_TRUE`/`VALUE_FALSE` first.
                            if !is_boolean_value(ssa, instruction.inputs[0]) {
                                let bail = ops.new_dynamic_label();
                                emit_truthiness_reduce(
                                    &mut ops,
                                    &mut relocations,
                                    to_boolean_entry,
                                    bail,
                                );
                                deopt_exits.push((
                                    bail,
                                    deopt_exit_at(frame_states, instruction)?,
                                    instruction.pc,
                                ));
                            }
                            emit_load_u32(&mut ops, 10, VALUE_TRUE as u32);
                            let taken_edge = ops.new_dynamic_label();
                            if op == Op::JumpIfTrue {
                                dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.eq =>taken_edge);
                            } else {
                                dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>taken_edge);
                            }

                            emit_cfg_edge(
                                &mut ops,
                                &mut relocations,
                                allocation,
                                eligibility,
                                property_cache_base,
                                poll_entry,
                                threw,
                                &block_labels,
                                block_id,
                                fallthrough,
                            )?;
                            dynasm!(ops ; .arch aarch64 ; =>taken_edge);

                            emit_cfg_edge(
                                &mut ops,
                                &mut relocations,
                                allocation,
                                eligibility,
                                property_cache_base,
                                poll_entry,
                                threw,
                                &block_labels,
                                block_id,
                                taken,
                            )?;
                        }
                    }
                    // A plain call: generated code guards one VM-baked target,
                    // enters its stable code generation with a stack-owned rooted
                    // frame, and returns directly. Every pre-entry miss deopts at
                    // this exact Call; an entered callee bailout resumes through
                    // the cold stack-call deoptimizer and is never replayed.
                    Op::Call if !is_spliced_call(cfg, block_id, instruction) => {
                        let dst = instruction
                            .result_register
                            .expect("eligibility checked call destination");
                        let callee = instruction.input_registers[0];
                        let arg_regs = &instruction.input_registers[1..];
                        let bail = ops.new_dynamic_label();
                        let frame = &tree.frames[instruction.inline.0 as usize];
                        let byte_pc = frame
                            .instructions
                            .get(instruction.pc as usize)
                            .map(|metadata| metadata.byte_pc)
                            .ok_or(Unsupported::OperandShape("optimizing direct call byte PC"))?;
                        let static_target = (instruction.inline == InlineId::ROOT)
                            .then(|| view.static_native_calls.get(&byte_pc))
                            .flatten();
                        if let Some(target) = static_target {
                            let stub_id = target.leaf_stub_id;
                            let name = native_leaf_call_name(stub_id);
                            let start = ops.offset().0;
                            if native_leaf_call_is_supported(view, stub_id, arg_regs.len()) {
                                emit_load_boxed_value(
                                    &mut ops,
                                    reprs,
                                    allocation,
                                    instruction.inputs[0],
                                    9,
                                )?;
                                emit_native_leaf_call(
                                    &mut ops,
                                    &mut relocations,
                                    view,
                                    stub_id,
                                    target.builtin_fn_addr,
                                    9,
                                    |ops, index, register| {
                                        let value = instruction
                                            .inputs
                                            .get(usize::from(index) + 1)
                                            .copied()
                                            .ok_or(Unsupported::OperandShape(
                                                "native leaf call argument",
                                            ))?;
                                        emit_load_boxed_value(
                                            ops, reprs, allocation, value, register,
                                        )
                                    },
                                    bail,
                                )?;
                                emit_store_tagged_location(
                                    &mut ops,
                                    allocation.location(
                                        instruction
                                            .result
                                            .expect("eligibility checked call result"),
                                    ),
                                    0,
                                )?;
                                if let Some(code_map) = code_map.as_mut() {
                                    code_map.record(CodeRegion::static_native_structural(
                                        "nativeLeafCall",
                                        start,
                                        ops.offset().0,
                                        frame.function_id,
                                        instruction.pc,
                                        byte_pc,
                                        name,
                                    ));
                                }
                                if let Some(events) = direct_call_events.as_mut() {
                                    events.insert(
                                        (byte_pc, 0),
                                        otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                                            instruction_pc: instruction.pc,
                                            byte_pc,
                                            target: name,
                                            outcome:
                                                otter_vm::JitStaticNativeCallLoweringOutcome::Generated,
                                        },
                                    );
                                }
                            } else {
                                if let Some(events) = direct_call_events.as_mut() {
                                    events.insert(
                                        (byte_pc, 0),
                                        otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                                            instruction_pc: instruction.pc,
                                            byte_pc,
                                            target: name,
                                            outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Rejected {
                                                reason:
                                                    otter_vm::JitStaticNativeCallLoweringRejectionReason::ArityUnsupported,
                                            },
                                        },
                                    );
                                }
                                dynasm!(ops ; .arch aarch64 ; b =>bail);
                            }
                        } else {
                            let site = eligibility
                                .element_transitions
                                .sites
                                .get(&instruction.pc)
                                .ok_or(Unsupported::OperandShape(
                                "optimizing call missing site",
                            ))?;
                            debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                            emit_materialize_element_transition(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                site,
                            )?;
                            emit_load_u32(&mut ops, 9, instruction.pc);
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                                ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                            );
                            let succeeded = ops.new_dynamic_label();
                            let direct_target = (instruction.inline == InlineId::ROOT)
                                .then(|| view.direct_callees.get(&byte_pc))
                                .flatten();
                            if let Some(target) = direct_target
                                .filter(|target| direct_call_target_is_supported(target))
                            {
                                emit_direct_call(
                                    &mut ops,
                                    &mut relocations,
                                    view,
                                    DirectCallSite {
                                        target,
                                        caller_function_id: frame.function_id,
                                        logical_pc: instruction.pc,
                                        byte_pc,
                                        dst,
                                        form: DirectCallForm::Plain { callable: callee },
                                        arguments: arg_regs,
                                    },
                                    deopt_stack_call_entry.address,
                                    resolve_direct_entry.address,
                                    code_map.as_mut(),
                                    bail,
                                    threw,
                                    succeeded,
                                )?;
                                if let Some(events) = direct_call_events.as_mut() {
                                    events.insert(
                                        (byte_pc, 0),
                                        optimizing_direct_call_event(
                                            otter_vm::JitDirectCallKind::Plain,
                                            instruction.pc,
                                            byte_pc,
                                            target,
                                            0,
                                            1,
                                            otter_vm::JitDirectCallLoweringOutcome::Generated {
                                                code_object_id: target.plan.code_object_id,
                                                target_tier: optimizing_direct_call_target_tier(
                                                    target,
                                                ),
                                                this_mode: target.plan.this_mode,
                                            },
                                        ),
                                    );
                                }
                            } else {
                                if let (Some(events), Some(target)) =
                                    (direct_call_events.as_mut(), direct_target)
                                {
                                    events.insert(
                                        (byte_pc, 0),
                                        optimizing_direct_call_event(
                                            otter_vm::JitDirectCallKind::Plain,
                                            instruction.pc,
                                            byte_pc,
                                            target,
                                            0,
                                            1,
                                            otter_vm::JitDirectCallLoweringOutcome::Rejected {
                                                reason: otter_vm::JitDirectCallLoweringRejectionReason::LayoutUnsupported,
                                            },
                                        ),
                                    );
                                }
                                dynasm!(ops ; .arch aarch64 ; b =>bail);
                            }
                            dynasm!(ops ; .arch aarch64 ; =>succeeded);
                            emit_reload_element_transition(
                                &mut ops,
                                allocation,
                                site,
                                Some((
                                    dst,
                                    allocation.location(
                                        instruction
                                            .result
                                            .expect("eligibility checked call result"),
                                    ),
                                )),
                            )?;
                        }
                        deopt_exits.push((
                            bail,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    // A spliced call: guard that the callee is still the body that
                    // was spliced, then fall into it. A different callee deopts and
                    // the interpreter re-runs the call generically.
                    Op::Call if is_spliced_call(cfg, block_id, instruction) => {
                        let Terminator::InlineCall { callee_entry, .. } =
                            cfg.blocks[block_id.0 as usize].terminator
                        else {
                            return Err(Unsupported::OperandShape(
                                "optimizing spliced-call terminator",
                            ));
                        };
                        let callee =
                            &tree.frames[cfg.blocks[callee_entry.0 as usize].inline.0 as usize];
                        let deopt = ops.new_dynamic_label();
                        deopt_exits.push((
                            deopt,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                        emit_load_tagged_location(
                            &mut ops,
                            allocation.location(instruction.inputs[0]),
                            9,
                        )?;
                        emit_cell_test(&mut ops, 9, 11, CellTest::IsNotCell, deopt);
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov w12, w9              // low-32 Gc offset
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            13,
                            view.cage_base as u64,
                            RelocationTarget::GcCageBase,
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]
                            ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG as u32
                            ; b.ne =>deopt
                            ; ldr w14, [x13, view.closure_call_layout.function_id_byte]
                        );
                        emit_load_u32(&mut ops, 15, callee.function_id);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp w14, w15
                            ; b.ne =>deopt
                        );
                        if instruction.inline == InlineId::ROOT {
                            let caller = &tree.frames[instruction.inline.0 as usize];
                            let byte_pc = caller
                                .instructions
                                .get(instruction.pc as usize)
                                .map(|metadata| metadata.byte_pc)
                                .ok_or(Unsupported::OperandShape(
                                    "optimizing inlined call byte PC",
                                ))?;
                            if let (Some(events), Some(target)) = (
                                direct_call_events.as_mut(),
                                view.direct_callees.get(&byte_pc),
                            ) {
                                events.insert(
                                    (byte_pc, 0),
                                    optimizing_direct_call_event(
                                        otter_vm::JitDirectCallKind::Plain,
                                        instruction.pc,
                                        byte_pc,
                                        target,
                                        0,
                                        1,
                                        otter_vm::JitDirectCallLoweringOutcome::Inlined,
                                    ),
                                );
                            }
                        }
                    }
                    // A spliced return hands its value to the continuation's merge
                    // through the edge; the block's terminator emits that edge.
                    Op::Return | Op::ReturnValue | Op::ReturnUndefined
                        if matches!(
                            cfg.blocks[block_id.0 as usize].terminator,
                            Terminator::InlineReturn { .. }
                        ) => {}
                    Op::Return | Op::ReturnValue => {
                        let returned = instruction.inputs[0];
                        match reprs.representation(returned) {
                            Representation::Int32 => {
                                emit_load_location(&mut ops, allocation.location(returned), 9)?;
                                emit_box_int32(&mut ops, 9, 10);
                            }
                            Representation::Float64 => {
                                emit_load_fp_location(
                                    &mut ops,
                                    allocation,
                                    allocation.location(returned),
                                    FP_SCRATCH,
                                )?;
                                emit_box_double(&mut ops, FP_SCRATCH, 9);
                            }
                            Representation::Tagged => {
                                emit_load_tagged_location(
                                    &mut ops,
                                    allocation.location(returned),
                                    9,
                                )?;
                            }
                        }
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov x0, x9
                            ; movz x1, STATUS_RETURNED as u32
                        );
                        emit_epilogue(&mut ops, spill_frame_bytes);
                    }
                    Op::ReturnUndefined => {
                        emit_load_u32(&mut ops, 0, otter_vm::Value::undefined().to_bits() as u32);
                        dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_RETURNED as u32);
                        emit_epilogue(&mut ops, spill_frame_bytes);
                    }
                    _ => return Err(Unsupported::Opcode(op)),
                },
            }
            if let Some(code_map) = code_map.as_mut() {
                let frame = &tree.frames[instruction.inline.0 as usize];
                let byte_pc = frame.instructions[instruction.pc as usize].byte_pc();
                code_map.record(CodeRegion::instruction(
                    instruction_start,
                    ops.offset().0,
                    Some(block_id.0),
                    Some(instruction.inline.0),
                    frame.function_id,
                    instruction.pc,
                    byte_pc,
                    Some(operation_index),
                    format!("{:?}", instruction.op),
                ));
            }
            operation_index = operation_index.saturating_add(1);
        }

        if matches!(
            block.terminator,
            Terminator::FallThrough
                | Terminator::InlineCall { .. }
                | Terminator::InlineReturn { .. }
        ) {
            let edge_start = ops.offset().0;
            let target = block.normal_succs[0];
            emit_cfg_edge(
                &mut ops,
                &mut relocations,
                allocation,
                eligibility,
                property_cache_base,
                poll_entry,
                threw,
                &block_labels,
                block_id,
                target,
            )?;
            if let Some(code_map) = code_map.as_mut() {
                code_map.record(CodeRegion::edge(
                    "fallthroughEdge",
                    edge_start,
                    ops.offset().0,
                    block_id.0,
                    target.0,
                ));
            }
        }
    }

    let threw_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>threw
        ; mov x0, xzr
        ; movz x1, STATUS_THREW as u32
    );
    emit_epilogue(&mut ops, spill_frame_bytes);
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "throwEpilogue",
            threw_start,
            ops.offset().0,
        ));
    }

    let boxed_slow_start = ops.offset().0;
    crate::template::arm64::values::emit_boxed_slot_slow_paths(
        &mut ops,
        &mut relocations,
        view,
        boxed_slot_slow_paths,
    );
    if let Some(code_map) = code_map.as_mut()
        && ops.offset().0 != boxed_slow_start
    {
        code_map.record(CodeRegion::structural(
            "boxedSlotSlowPaths",
            boxed_slow_start,
            ops.offset().0,
        ));
    }

    for (label, exit, resume_pc) in deopt_exits {
        let deopt_start = ops.offset().0;
        dynasm!(ops ; .arch aarch64 ; =>label);
        let frame_state = deopt_table.lookup(exit).ok_or(Unsupported::OperandShape(
            "optimizing deopt exit missing frame state",
        ))?;
        // The compiled function's own frame is always rebuilt first, in the
        // window it already runs on.
        emit_deopt_writeback(&mut ops, allocation, frame_state.outermost(), 19)?;
        if frame_state.is_single_frame() {
            emit_load_u32(&mut ops, 9, resume_pc);
            dynasm!(ops
                ; .arch aarch64
                ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                ; mov x0, xzr
                ; movz x1, STATUS_BAILED as u32
            );
            emit_epilogue(&mut ops, spill_frame_bytes);
            if let Some(code_map) = code_map.as_mut() {
                code_map.record(CodeRegion::deopt(
                    deopt_start,
                    ops.offset().0,
                    exit.0,
                    resume_pc,
                ));
            }
            continue;
        }

        // The exit was inside a spliced callee, so the interpreter is owed that
        // callee's frame too. Reify rewinds the just-restored caller to its call
        // and lets the interpreter's own call path build the frame — which also
        // leaves the caller advanced past the call, so no PC is stamped here.
        for (depth, frame) in frame_state.frames.iter().enumerate().skip(1) {
            // The reify stub speaks logical PCs — a frame's `pc` is a canonical
            // instruction index — while the chain records byte PCs. The caller
            // resumes one past its call, so the call itself is `resume - 1`.
            let caller = &frame_state.frames[depth - 1];
            let call_pc = logical_pc(tree, caller.function_id, caller.byte_pc)?
                .checked_sub(1)
                .ok_or(Unsupported::OperandShape(
                    "optimizing chain caller resumes at its entry",
                ))?;
            let callee_pc = logical_pc(tree, frame.function_id, frame.byte_pc)?;
            dynasm!(ops ; .arch aarch64 ; mov x0, x20);
            emit_load_u64(&mut ops, 1, u64::from(call_pc));
            emit_load_u64(&mut ops, 2, u64::from(callee_pc));
            emit_runtime_entry(&mut ops, &mut relocations, 16, reify_frame_entry);
            dynasm!(ops
                ; .arch aarch64
                ; blr x16
                ; cbz x0, =>threw
                ; mov x13, x0
            );
            emit_deopt_writeback(&mut ops, allocation, frame, 13)?;
        }
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, xzr
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(&mut ops, spill_frame_bytes);
        if let Some(code_map) = code_map.as_mut() {
            code_map.record(CodeRegion::deopt(
                deopt_start,
                ops.offset().0,
                exit.0,
                resume_pc,
            ));
        }
    }

    let mut osr_entries = BTreeMap::new();
    for (&block, site) in &eligibility.osr_entries {
        let target = block_labels[block.0 as usize];
        let offset = ops.offset().0;
        let representation_bail = ops.new_dynamic_label();
        emit_prologue(&mut ops, spill_frame_bytes);
        if eligibility.cached_method_guard.is_some() {
            dynasm!(ops ; .arch aarch64 ; mov x9, xzr);
            let receiver_slot = fused_method_receiver_slot.expect("cached guard reserves a slot");
            emit_sp_str_x(&mut ops, 9, receiver_slot);
        }
        emit_reset_all_property_loop_caches(
            &mut ops,
            property_cache_base,
            &eligibility.property_loop_cache,
        )?;
        if !eligibility.back_edges.is_empty() {
            dynasm!(ops ; .arch aarch64 ; movz w29, OPTIMIZED_POLL_BATCH);
        }
        dynasm!(ops
            ; .arch aarch64
            ; mov x20, x0
            ; ldr x9, [x20, NATIVE_FRAME_OFFSET]
            ; ldr x19, [x9, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        );
        emit_osr_materialization(&mut ops, reprs, allocation, site, representation_bail)?;
        dynasm!(ops ; .arch aarch64 ; b =>target ; =>representation_bail);
        emit_load_u32(&mut ops, 9, site.logical_pc);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
            ; mov x0, xzr
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(&mut ops, spill_frame_bytes);
        if let Some(code_map) = code_map.as_mut() {
            code_map.record_osr(site.logical_pc, offset, ops.offset().0);
        }
        osr_entries.insert(site.logical_pc, offset);
    }

    let buffer = ops
        .finalize()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::Finalization))?;
    Ok(OptimizedEmission {
        code: CompiledCode::new(buffer, entry),
        osr_entries,
        direct_call_events,
        code_map,
        relocations,
    })
}

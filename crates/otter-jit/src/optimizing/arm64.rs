//! Arm64 emission for the reducible numeric and reentrant-transition optimizing subset.
//!
//! # Contents
//! - Backend eligibility validation over a pre-verified optimizing unit.
//! - Reducible loop checks, numeric comparisons, and per-edge phi copies.
//! - Loop-header OSR trampolines materializing allocated state from the
//!   interpreter register window.
//! - Cooperative back-edge polling with loop-header bail writeback.
//! - Precise live-tagged GC safepoints around element, property, global,
//!   comparison, and method-call transitions.
//! - Direct live-value reads from baked global lexical cells and guarded
//!   global-object property records.
//! - Baked stable-entry plain and method calls with stack-owned rooted callee
//!   frames.
//! - Guarded numeric fast paths for source-lowered coercion scaffolding.
//! - Tagged-number guards, mixed-representation arithmetic, spills, boxing,
//!   and bail exits backed by exact deopt frame states.
//!
//! # Invariants
//! - `x20` retains the sole `JitCtx` argument and `x19` retains the canonical
//!   `NativeFrame.register_base`.
//!   GPR linear-scan registers `0..4` map to `x21..x24`, disjoint from both
//!   fixed ABI registers; FP registers `0..8` map to the AAPCS64 callee-saved
//!   `d8..d15`. `x8..x15` and `d16..d17` are caller-saved scratch registers.
//!   GPR spill slots precede FP spill slots in one aligned stack frame.
//! - Every tagged numeric input, including an element-load result, is checked
//!   with the VM's frozen number-tag mask before entering an unboxed operation.
//!   `ToPrimitive` / `ToNumeric` accept only those checked number encodings;
//!   other values bail before any user-observable coercion can run.
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
//! - Every backwards bytecode edge targets a dominating loop header. Its phi moves
//!   execute before the interrupt/fuel poll so a poll exit reconstructs the
//!   loop-header frame, and no optimized loop can bypass cooperative polling.
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
    STUB_JIT_MATH_CALL, STUB_JIT_SPREAD_CALL_OP, STUB_JIT_STORE_ELEMENT, STUB_JIT_STORE_PROPERTY,
    STUB_JIT_STORE_UPVALUE, STUB_JIT_STORE_UPVALUE_CHECKED, STUB_JIT_WRITE_BARRIER, SafepointId,
    SafepointRecord,
};
use otter_vm::{JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG};

use super::{
    OptimizedCode, OptimizedMetadata,
    artifact::render_optimized_unit,
    pipeline::{
        OptimizationError, OptimizationPipeline, total_spill_slots as analyzed_spill_slot_count,
    },
};
use crate::{
    CompiledCode,
    arm64::{
        DirectCallForm, DirectCallSite, MethodGuardSite, StaticNativeCallSite,
        direct_call_artifact, direct_call_target_is_supported, emit_direct_call, emit_method_guard,
        emit_static_native_call, static_native_target_is_supported,
    },
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::{PropertyIcAccess, RelocationCapture, RelocationTarget},
    },
    entry::{
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, GLOBAL_THIS_OFFSET_PTR_OFFSET, IC_WAYS,
        MAX_METHOD_ARGS, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_THIS_OFFSET,
        NATIVE_FRAME_UPVALUE_BASE_OFFSET, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, STATUS_BAILED,
        STATUS_RETURNED, STATUS_THREW, THREAD_OFFSET, TransitionTable, Unsupported, VALUE_FALSE,
        VALUE_FALSE_LOW, VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED,
        VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
        VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET, WhiskerIcCell,
        pack_method_arg_regs,
    },
    ir::{
        cfg::{BlockId, ControlFlowGraph, Terminator},
        deopt_lower::DeoptLowering,
        dom::DominatorTree,
        frame_state::{AbstractFrameState, FrameStateTable},
        inline::{InlineId, InlineTree},
        liveness::Liveness,
        regalloc::{
            Allocation, EdgeMoves, Location, Move, RegClass, RegisterBudget, has_non_dead_use,
            is_dead_phi,
        },
        repr::{ConversionKind, ReprMap, Representation},
        ssa::{SsaFunction, SsaInstr, ValueDef, ValueId},
    },
};

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
    /// Per-`MathCall`-site argument window registers, keyed by logical PC. The
    /// emitted call passes a pointer into the boxed slice, so the arena must
    /// live exactly as long as the code; `OptimizedCode` takes ownership.
    math_call_arguments: BTreeMap<u32, Box<[u16]>>,
    /// Sites whose feedback cell has never recorded an execution. Emission
    /// replaces each with an unconditional deopt: if the cold path is ever
    /// reached, the interpreter runs it, records feedback, bumps the epoch,
    /// and the next compile sees real types.
    insufficient_feedback: BTreeSet<(InlineId, u32)>,
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
    /// Dispatches a `Math.<method>` intrinsic through the window transition.
    math_call_entry: ResolvedRuntimeEntry,
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
        .analyze(view, splice_lowerable)
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
        .filter(|instruction| instruction.op == Op::LoadProperty)
        .count();
    let mut load_ic_cells =
        vec![crate::entry::WhiskerIcCell::default(); load_property_sites].into_boxed_slice();
    let store_property_sites = unit
        .dom
        .reverse_postorder()
        .iter()
        .flat_map(|block| unit.ssa.blocks[block.0 as usize].instrs.iter())
        .filter(|instruction| instruction.op == Op::StoreProperty)
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
            math_call_entry: ResolvedRuntimeEntry::new(
                STUB_JIT_MATH_CALL,
                transitions.variadic_entry(STUB_JIT_MATH_CALL),
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
        eligibility.math_call_arguments,
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
fn splice_lowerable(callee: &otter_vm::JitInlineCallee) -> bool {
    callee.instructions.iter().all(|instruction| {
        matches!(
            instruction.op(callee.code_block.as_ref()),
            Op::LoadInt32
                | Op::LoadNumber
                | Op::LoadUndefined
                | Op::LoadNull
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::LoadLocal
                | Op::StoreLocal
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::Neg
                | Op::Increment
                | Op::LogicalNot
                | Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Equal
                | Op::NotEqual
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    })
}

/// Canonical instruction index of `byte_pc` within `function_id`'s body.
fn logical_pc(tree: &InlineTree, function_id: u32, byte_pc: u32) -> Result<u32, Unsupported> {
    let frame = tree
        .frames
        .iter()
        .find(|frame| frame.function_id == function_id)
        .ok_or(Unsupported::OperandShape("optimizing chain frame body"))?;
    frame
        .instructions
        .iter()
        .position(|instruction| instruction.byte_pc == byte_pc)
        .map(|position| position as u32)
        .ok_or(Unsupported::OperandShape("optimizing chain frame byte PC"))
}

/// `true` when this instruction is the call a spliced frame replaces.
fn is_spliced_call(cfg: &ControlFlowGraph, block: BlockId, instruction: &SsaInstr) -> bool {
    let block = &cfg.blocks[block.0 as usize];
    matches!(block.terminator, Terminator::InlineCall { .. })
        && block.instr_pcs.last() == Some(&instruction.pc)
}

/// Arithmetic feedback for one instruction of its own frame — a spliced
/// callee's instruction must never read the root body's cell.
fn frame_feedback(
    tree: &InlineTree,
    instruction: &SsaInstr,
) -> otter_vm::jit_feedback::ArithFeedback {
    tree.frames[instruction.inline.0 as usize]
        .instructions
        .get(instruction.pc as usize)
        .map_or_else(
            otter_vm::jit_feedback::ArithFeedback::default,
            otter_vm::JitInstructionMetadata::arith_feedback,
        )
}

fn check_eligibility(
    view: &JitCompileSnapshot,
    tree: &InlineTree,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    ssa: &SsaFunction,
    analyses: EligibilityAnalyses<'_>,
) -> Result<Eligibility, Unsupported> {
    let EligibilityAnalyses {
        liveness,
        reprs,
        allocation,
        frame_states,
    } = analyses;
    if cfg.entry.0 != 0 || dom.reverse_postorder().len() != cfg.blocks.len() {
        return Err(Unsupported::OperandShape(
            "optimizing subset requires one reachable entry graph",
        ));
    }

    let mut back_edges = BTreeMap::new();
    for block in &cfg.blocks {
        if !block.exception_succs.is_empty() {
            return Err(Unsupported::OperandShape(
                "optimizing subset rejects exception edges",
            ));
        }
        for successor in block.normal_succs.iter().copied() {
            // A back edge is a same-frame edge to a leader at or before this
            // block's. A splice edge leaves the frame — a callee entering at PC
            // 0 from a call block at PC 0 is not a loop — and PCs of different
            // frames are not comparable at all.
            let target = &cfg.blocks[successor.0 as usize];
            if target.inline == block.inline && target.start_pc <= block.start_pc {
                if !dom.dominates(successor, block.id) {
                    return Err(Unsupported::OperandShape(
                        "optimizing subset rejects irreducible back-edges",
                    ));
                }
                let header = &cfg.blocks[successor.0 as usize];
                back_edges.insert(
                    (block.id, successor),
                    (
                        DeoptLowering::exit_at(frame_states, header.inline, header.start_pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing back edge header has no frame state",
                            ))?,
                        header.start_pc,
                    ),
                );
            }
        }
        match block.terminator {
            Terminator::FallThrough | Terminator::Jump if block.normal_succs.len() == 1 => {}
            Terminator::Branch { .. } if !block.normal_succs.is_empty() => {}
            Terminator::Return if block.normal_succs.is_empty() => {}
            // A spliced call reaches its callee's entry; the callee's returns
            // reach the call's continuation. The graph already verified both as
            // single-successor frame-crossing edges.
            Terminator::InlineCall { .. } | Terminator::InlineReturn { .. }
                if block.normal_succs.len() == 1 => {}
            _ => {
                return Err(Unsupported::OperandShape(
                    "optimizing subset has an unsupported terminator",
                ));
            }
        }
    }
    let spill_bytes = total_spill_slots(allocation)?
        .checked_mul(STACK_SLOT_BYTES)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape("optimizing spill frame overflow"))?;
    if spill_bytes > MAX_SPILL_FRAME_BYTES {
        return Err(Unsupported::OperandShape(
            "optimizing spill frame exceeds arm64 immediates",
        ));
    }

    let mut guarded_uses = BTreeMap::<(u32, ValueId), Option<u32>>::new();
    let mut allowed_conversions = BTreeSet::new();
    let mut element_transition_instructions = Vec::new();
    let mut math_call_arguments = BTreeMap::new();
    let mut insufficient_feedback = BTreeSet::new();
    for block in dom.reverse_postorder().iter().copied() {
        for (instruction_index, instruction) in
            ssa.blocks[block.0 as usize].instrs.iter().enumerate()
        {
            // A feedback-driven op whose cell never recorded an execution is
            // unreachable-by-feedback: lower it as an unconditional deopt
            // instead of refusing the whole function for a cold path.
            if matches!(
                instruction.op,
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::Rem
                    | Op::Neg
                    | Op::Increment
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual
                    | Op::BitwiseAnd
                    | Op::BitwiseOr
                    | Op::BitwiseXor
                    | Op::Shl
                    | Op::Shr
            ) && frame_feedback(tree, instruction).is_unseen()
            {
                insufficient_feedback.insert((instruction.inline, instruction.pc));
                // Whatever conversions representation selection planned at
                // this site are moot — the op is never emitted — but they must
                // not fail the unit-wide conversion sweep.
                for conversion in reprs.conversions() {
                    if conversion.inline == instruction.inline && conversion.at_pc == instruction.pc
                    {
                        allowed_conversions.insert((conversion.at_pc, conversion.operand_index));
                    }
                }
                continue;
            }
            match instruction.op {
                Op::LoadInt32 => check_constant_result(instruction, reprs)?,
                Op::LoadNumber => check_number_constant_result(view, instruction, reprs)?,
                Op::LoadUndefined => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadNull => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadThis => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadTrue | Op::LoadFalse => check_boolean_result(instruction, reprs)?,
                Op::LoadLocal | Op::StoreLocal => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("local move result"))?;
                    if instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(result)
                            != reprs.representation(instruction.inputs[0])
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                Op::ToPrimitive | Op::ToNumeric => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("numeric coercion result"))?;
                    if instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(result) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    if reprs.representation(instruction.inputs[0]) == Representation::Tagged {
                        guarded_uses.insert((instruction.pc, instruction.inputs[0]), None);
                    }
                }
                Op::LoadElement => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("element-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 2
                        || instruction.input_registers.len() != 2
                        || instruction.result_register.is_none()
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::StoreElement => {
                    if instruction.result.is_some()
                        || instruction.result_register.is_some()
                        || instruction.inputs.len() != 3
                        || instruction.input_registers.len() != 3
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || reprs.representation(instruction.inputs[1]) == Representation::Float64
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LoadProperty => {
                    // `WRITE_READ_CONST`: the object is the sole register input,
                    // the name is a constant operand (an immediate, not a window
                    // slot), and the loaded value is tagged.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("property-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::StoreProperty => {
                    // `READ_CONST_READ_WRITE`: object + value are register inputs,
                    // the name is a constant immediate, and the scratch WRITE slot
                    // is a spare interpreter-window register the stub may clobber.
                    let scratch = instruction
                        .result_register
                        .ok_or(Unsupported::OperandShape("property-store scratch"))?;
                    if instruction.result.is_none()
                        || instruction.inputs.len() != 2
                        || instruction.input_registers.len() != 2
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || instruction.input_registers.contains(&scratch)
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LoadGlobalOrThrow => {
                    // `WRITE_CONST`: the name is a constant immediate and there are
                    // no register inputs; the global value is tagged. The reentrant
                    // stub can allocate/throw, so it is a precise-rooted transition.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("global-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || !instruction.inputs.is_empty()
                        || !instruction.input_registers.is_empty()
                        || instruction.result_register.is_none()
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LooseEqual | Op::LooseNotEqual => {
                    // `WRITE_READ_READ`: two tagged register operands compared
                    // under §7.2.14 abstract equality, which may run `ToPrimitive`
                    // (user `valueOf`/`toString`), allocate, and throw — a precise
                    // reentrant transition. The boolean result is tagged.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("loose-eq result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 2
                        || instruction.input_registers.len() != 2
                        || instruction.result_register.is_none()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || reprs.representation(instruction.inputs[1]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::MathCall => {
                    // `dst, method-const, argc-const, arg-regs...`. Dispatches a
                    // `Math.<method>` intrinsic through the same reentrant window
                    // transition as a method call: a shadowed `Math` binding or
                    // an exotic argument coercion may run arbitrary JS.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("math-call result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.inputs.len() != instruction.input_registers.len()
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    math_call_arguments.insert(
                        instruction.pc,
                        instruction.input_registers.iter().copied().collect(),
                    );
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::CallMethodValue => {
                    // `dst, receiver, name-const, argc-const, arg-regs...`. The
                    // receiver plus up to `MAX_METHOD_ARGS` tagged arguments are
                    // register inputs; resolution runs the full method walk and
                    // may reenter arbitrary (possibly compiled) callee code, so it
                    // is a precise reentrant transition. An exotic-receiver report
                    // side-exits with a full deopt to this pc.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("method-call result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.input_registers.is_empty()
                        || instruction.input_registers.len() > 1 + MAX_METHOD_ARGS
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::New => {
                    // `dst, callee, argc-const, arg-regs...`. Construction may
                    // execute arbitrary JS, allocate, or throw, so every tagged
                    // operand is materialized in the precise frame window.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("construct result"))?;
                    let argc = view.instructions[instruction.pc as usize]
                        .const_index(view.code_block.as_ref(), 2)
                        .ok_or(Unsupported::OperandShape("construct argument count"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.input_registers.is_empty()
                        || instruction.input_registers.len() > 1 + MAX_METHOD_ARGS
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || argc as usize != instruction.input_registers.len() - 1
                        || instruction
                            .inputs
                            .iter()
                            .any(|&input| reprs.representation(input) != Representation::Tagged)
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::Add | Op::Sub | Op::Mul => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("arithmetic result"))?;
                    let result_repr = reprs.representation(result);
                    if !matches!(result_repr, Representation::Int32 | Representation::Float64)
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        result_repr,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::Increment => {
                    // `dst = src + delta` over int32 (the common loop-counter form);
                    // float increments stay ineligible and complete elsewhere.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("increment result"))?;
                    if reprs.representation(result) != Representation::Int32
                        || instruction.inputs.len() != 1
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        Representation::Int32,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::Neg => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("negate result"))?;
                    let result_repr = reprs.representation(result);
                    if !matches!(result_repr, Representation::Int32 | Representation::Float64)
                        || instruction.inputs.len() != 1
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        result_repr,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::LogicalNot => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("logical-not result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                }
                Op::Div | Op::Rem => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("float arithmetic result"))?;
                    if reprs.representation(result) != Representation::Float64
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        Representation::Float64,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr => {
                    // Bitwise and shift results are int32 in JS. Int32-only
                    // feedback computes directly; mixed numeric feedback takes
                    // float64 operands through the exact JS ToInt32 conversion
                    // (fjcvtzs) and returns the int32 result as an exact double.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("bitwise result"))?;
                    if instruction.inputs.len() != 2 {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    let required = match reprs.representation(result) {
                        Representation::Int32 => Representation::Int32,
                        Representation::Float64 => Representation::Float64,
                        Representation::Tagged => {
                            return Err(Unsupported::Opcode(instruction.op));
                        }
                    };
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        required,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("comparison result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    let feedback = frame_feedback(tree, instruction);
                    if feedback.is_int32_only() {
                        check_numeric_inputs(
                            instruction,
                            ssa,
                            reprs,
                            Representation::Int32,
                            &mut guarded_uses,
                            &mut allowed_conversions,
                        )?;
                    } else if feedback.is_numeric_only() {
                        check_numeric_inputs(
                            instruction,
                            ssa,
                            reprs,
                            Representation::Float64,
                            &mut guarded_uses,
                            &mut allowed_conversions,
                        )?;
                    } else if matches!(instruction.op, Op::Equal | Op::NotEqual) {
                        // Mixed operands: strict equality is total over tagged
                        // values, so it lowers inline whatever the feedback.
                        check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    } else {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                // A plain call uses only a VM-baked monomorphic native target.
                // Every tagged operand is materialized in the precise frame
                // window first; absence or invalidation of that target takes
                // the exact pre-effect deopt exit.
                Op::Call if !is_spliced_call(cfg, block, instruction) => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("call result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.input_registers.is_empty()
                        || instruction.inputs.len() != instruction.input_registers.len()
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    let frame = &tree.frames[instruction.inline.0 as usize];
                    let byte_pc = frame
                        .instructions
                        .get(instruction.pc as usize)
                        .map(|metadata| metadata.byte_pc)
                        .ok_or(Unsupported::OperandShape("optimizing call byte PC"))?;
                    if instruction.inline != InlineId::ROOT
                        || !view.static_native_calls.contains_key(&byte_pc)
                    {
                        element_transition_instructions.push((
                            instruction.pc,
                            block,
                            instruction_index,
                        ));
                    }
                }
                // A spliced call is not lowered as a call: control enters the
                // callee's body. Its operands stay inputs so the emitter can
                // guard the callee's identity, and the continuation's merge —
                // not the call — defines its result.
                Op::Call if is_spliced_call(cfg, block, instruction) => {
                    if instruction.result.is_some()
                        || instruction.result_register.is_some()
                        || instruction.inputs.is_empty()
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                // A spliced return hands its value to the continuation's merge
                // through the edge at the merge's representation, so it needs
                // no boxing of its own and never leaves the unit.
                Op::Return | Op::ReturnValue
                    if matches!(
                        cfg.blocks[block.0 as usize].terminator,
                        Terminator::InlineReturn { .. }
                    ) =>
                {
                    if instruction.result.is_some() || instruction.inputs.len() != 1 {
                        return Err(Unsupported::OperandShape("optimizing return shape"));
                    }
                }
                Op::ReturnUndefined
                    if matches!(
                        cfg.blocks[block.0 as usize].terminator,
                        Terminator::InlineReturn { .. }
                    ) =>
                {
                    if instruction.result.is_some() || !instruction.inputs.is_empty() {
                        return Err(Unsupported::OperandShape(
                            "optimizing return-undefined shape",
                        ));
                    }
                }
                // A captured-binding write is a leaf with a write barrier; the
                // checked form throws on a TDZ write. The value materializes
                // into its window slot for the stub.
                Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                    if instruction.result.is_some()
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                }
                // A captured-binding read is a leaf: it reaches the upvalue
                // cell through pointers, never allocates or runs JS, and only
                // a TDZ read throws. No safepoint or materialization is owed;
                // the destination reloads from the window after the call.
                Op::LoadUpvalue => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("upvalue-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || !instruction.inputs.is_empty()
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                Op::Jump => {
                    if instruction.result.is_some() || !instruction.inputs.is_empty() {
                        return Err(Unsupported::OperandShape("optimizing jump shape"));
                    }
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    if instruction.result.is_some() || instruction.inputs.len() != 1 {
                        return Err(Unsupported::OperandShape("optimizing branch shape"));
                    }
                    // A provably-boolean condition compares directly; any other
                    // tagged value reduces through the total `ToBoolean` sequence
                    // (numbers/bool/null/undefined inline, heap cells via the leaf
                    // probe) at emit time.
                    if reprs.representation(instruction.inputs[0]) != Representation::Tagged {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                Op::Return | Op::ReturnValue => {
                    if instruction.result.is_some() || instruction.inputs.len() != 1 {
                        return Err(Unsupported::OperandShape("optimizing return shape"));
                    }
                    let returned = instruction.inputs[0];
                    let returned_repr = reprs.representation(returned);
                    let expected_conversion = match returned_repr {
                        Representation::Int32 => ConversionKind::BoxInt32,
                        Representation::Float64 => ConversionKind::BoxFloat64,
                        Representation::Tagged => continue,
                    };
                    let conversion = reprs.conversions().iter().find(|conversion| {
                        conversion.at_pc == instruction.pc && conversion.operand_index == 0
                    });
                    if !matches!(
                        conversion,
                        Some(conversion)
                            if conversion.value == returned
                                && conversion.kind == expected_conversion
                                && !conversion.may_deopt
                    ) {
                        return Err(Unsupported::OperandShape(
                            "optimizing return requires numeric boxing",
                        ));
                    }
                    allowed_conversions.insert((instruction.pc, 0));
                }
                Op::ReturnUndefined => {
                    if instruction.result.is_some() || !instruction.inputs.is_empty() {
                        return Err(Unsupported::OperandShape(
                            "optimizing return-undefined shape",
                        ));
                    }
                }
                other => return Err(Unsupported::Opcode(other)),
            }
        }
    }

    if reprs.conversions().iter().any(|conversion| {
        !allowed_conversions.contains(&(conversion.at_pc, conversion.operand_index))
    }) {
        return Err(Unsupported::OperandShape(
            "optimizing subset has an unsupported conversion",
        ));
    }

    let mut guarded_numeric_uses = Vec::with_capacity(guarded_uses.len());
    for ((use_pc, _value), parameter_index) in guarded_uses {
        if let Some(index) = parameter_index {
            let offset = index
                .checked_mul(STACK_SLOT_BYTES)
                .ok_or(Unsupported::OperandShape("optimizing parameter offset"))?;
            if offset > MAX_PARAMETER_OFFSET {
                return Err(Unsupported::OperandShape(
                    "optimizing parameter exceeds arm64 load range",
                ));
            }
        }
        guarded_numeric_uses.push(GuardedUse { use_pc });
    }
    guarded_numeric_uses.sort_by_key(|guarded| guarded.use_pc);
    guarded_numeric_uses.dedup_by_key(|guarded| guarded.use_pc);
    // A reentrant stub addresses its operands as indices into the *caller's*
    // interpreter register window. A spliced callee has no window of its own
    // until its frame is reified at a deopt exit, so those indices would name
    // the caller's registers and corrupt them. Splicing is therefore confined
    // to bodies the tier lowers entirely into machine registers.
    for &(_, block, _) in &element_transition_instructions {
        if cfg.blocks[block.0 as usize].inline != InlineId::ROOT {
            return Err(Unsupported::OperandShape(
                "optimizing subset rejects a spliced frame that needs a register window",
            ));
        }
    }
    element_transition_instructions.sort_unstable_by_key(|&(pc, _, _)| pc);
    element_transition_instructions.dedup_by_key(|instruction| instruction.0);
    let element_transitions = build_element_transition_sites(
        ssa,
        liveness,
        reprs,
        frame_states,
        element_transition_instructions,
    )?;
    let osr_entries = build_osr_entry_sites(cfg, ssa, liveness, frame_states)?;
    Ok(Eligibility {
        guarded_uses: guarded_numeric_uses,
        back_edges,
        osr_entries,
        element_transitions,
        math_call_arguments,
        insufficient_feedback,
    })
}

fn build_osr_entry_sites(
    cfg: &ControlFlowGraph,
    ssa: &SsaFunction,
    liveness: &Liveness,
    frame_states: &FrameStateTable,
) -> Result<BTreeMap<BlockId, OsrEntrySite>, Unsupported> {
    let mut sites = BTreeMap::new();
    // Only the root frame's headers are OSR targets: the interpreter requests
    // OSR by the root function's PC, and a PC inside a spliced callee is not in
    // that namespace. A hot loop inside a spliced body runs compiled from the
    // unit's entry instead.
    for block in cfg
        .blocks
        .iter()
        .filter(|block| block.is_loop_header && block.inline == InlineId::ROOT)
    {
        let frame_state =
            frame_states
                .at(InlineId::ROOT, block.start_pc)
                .ok_or(Unsupported::OperandShape(
                    "optimizing OSR header frame state",
                ))?;
        let live_in = liveness.live_in(block.id);
        let mut register_by_value = BTreeMap::<ValueId, u16>::new();
        for (register, value) in frame_state.registers.iter().copied().enumerate() {
            let Some(value) = value.filter(|value| live_in.contains(value)) else {
                continue;
            };
            let register = u16::try_from(register)
                .map_err(|_| Unsupported::OperandShape("optimizing OSR register overflow"))?;
            register_by_value.entry(value).or_insert(register);
        }
        if live_in
            .iter()
            .any(|value| has_non_dead_use(ssa, *value) && !register_by_value.contains_key(value))
        {
            return Err(Unsupported::OperandShape(
                "optimizing OSR live value is absent from header frame state",
            ));
        }
        let live_values = register_by_value
            .into_iter()
            .map(|(value, register)| OsrLiveValue { value, register })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        sites.insert(
            block.id,
            OsrEntrySite {
                logical_pc: block.start_pc,
                live_values,
            },
        );
    }
    Ok(sites)
}

fn build_element_transition_sites(
    ssa: &SsaFunction,
    liveness: &Liveness,
    reprs: &ReprMap,
    frame_states: &FrameStateTable,
    instructions: Vec<(u32, BlockId, usize)>,
) -> Result<ElementTransitionSafepoints, Unsupported> {
    let bitmap_word_count = usize::from(ssa.register_count).div_ceil(u64::BITS as usize);
    let bitmap_word_count_u16 = u16::try_from(bitmap_word_count)
        .map_err(|_| Unsupported::OperandShape("optimizing frame-map word count overflow"))?;
    let mut bitmap_words = Vec::<u64>::new();
    let mut sites = BTreeMap::new();

    for (id, (pc, block, instruction_index)) in instructions.into_iter().enumerate() {
        let safepoint_id = u32::try_from(id)
            .map_err(|_| Unsupported::OperandShape("optimizing safepoint id overflow"))?;
        let instruction = ssa.blocks[block.0 as usize]
            .instrs
            .get(instruction_index)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition instruction boundary",
            ))?;
        // Eligibility confines reentrant transitions to the root frame — a
        // spliced callee has no interpreter window for the stub to address.
        let frame_state = frame_states
            .at(InlineId::ROOT, pc)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition abstract frame state",
            ))?;
        let live_after = liveness
            .live_after_instruction(ssa, block, instruction_index)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition live-out boundary",
            ))?;
        let result = instruction.result;
        let mut tagged_live_across = Vec::new();
        for value in live_after {
            if Some(value) == result || reprs.representation(value) != Representation::Tagged {
                continue;
            }
            let register =
                frame_register_for_value(frame_state, value).ok_or(Unsupported::OperandShape(
                    "optimizing tagged live-out value is absent from frame state",
                ))?;
            tagged_live_across.push(TaggedLiveAcross { value, register });
        }
        tagged_live_across.sort_unstable_by_key(|live| (live.register, live.value));
        tagged_live_across.dedup();

        let mut root_registers = tagged_live_across
            .iter()
            .map(|live| live.register)
            .collect::<BTreeSet<_>>();
        for (&value, &register) in instruction.inputs.iter().zip(&instruction.input_registers) {
            if reprs.representation(value) == Representation::Tagged {
                root_registers.insert(register);
            }
        }
        if instruction.op == Op::StoreProperty
            && root_registers.contains(
                &instruction
                    .result_register
                    .expect("eligibility checked property-store scratch"),
            )
        {
            return Err(Unsupported::OperandShape(
                "optimizing store-transition scratch aliases a tagged root",
            ));
        }

        let bitmap_offset = u32::try_from(bitmap_words.len())
            .map_err(|_| Unsupported::OperandShape("optimizing frame-map bitmap overflow"))?;
        let mut site_words = vec![0_u64; bitmap_word_count];
        for register in root_registers {
            let register = usize::from(register);
            site_words[register / u64::BITS as usize] |= 1_u64 << (register % u64::BITS as usize);
        }
        bitmap_words.extend(site_words);
        let frame_map = FrameMap {
            id: safepoint_id,
            bitmap_offset,
            bitmap_word_count: bitmap_word_count_u16,
            slot_count: ssa.register_count,
        };
        sites.insert(
            pc,
            ElementTransitionSite {
                safepoint_id,
                frame_map,
                tagged_live_across: tagged_live_across.into_boxed_slice(),
            },
        );
    }

    Ok(ElementTransitionSafepoints {
        sites,
        bitmap_words: bitmap_words.into_boxed_slice(),
    })
}

fn frame_register_for_value(frame_state: &AbstractFrameState, value: ValueId) -> Option<u16> {
    frame_state
        .registers
        .iter()
        .position(|candidate| *candidate == Some(value))
        .and_then(|register| u16::try_from(register).ok())
}

fn check_tagged_inputs(
    instruction: &SsaInstr,
    reprs: &ReprMap,
    allowed_conversions: &mut BTreeSet<(u32, usize)>,
) -> Result<(), Unsupported> {
    for (operand_index, &input) in instruction.inputs.iter().enumerate() {
        let expected_kind = match reprs.representation(input) {
            Representation::Tagged => continue,
            Representation::Int32 => ConversionKind::BoxInt32,
            Representation::Float64 => ConversionKind::BoxFloat64,
        };
        let conversion = reprs.conversions().iter().find(|conversion| {
            conversion.at_pc == instruction.pc && conversion.operand_index == operand_index
        });
        if !matches!(
            conversion,
            Some(conversion)
                if conversion.value == input
                    && conversion.kind == expected_kind
                    && !conversion.may_deopt
        ) {
            return Err(Unsupported::OperandShape(
                "optimizing element transition requires tagged operands",
            ));
        }
        allowed_conversions.insert((instruction.pc, operand_index));
    }
    Ok(())
}

fn check_numeric_inputs(
    instruction: &SsaInstr,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    required: Representation,
    guarded_uses: &mut BTreeMap<(u32, ValueId), Option<u32>>,
    allowed_conversions: &mut BTreeSet<(u32, usize)>,
) -> Result<(), Unsupported> {
    for (operand_index, &input) in instruction.inputs.iter().enumerate() {
        let actual = reprs.representation(input);
        if actual == required {
            continue;
        }
        let expected_kind = match (actual, required) {
            (Representation::Int32, Representation::Float64) => ConversionKind::Int32ToFloat64,
            (Representation::Tagged, Representation::Int32) => ConversionKind::CheckedTaggedToInt32,
            (Representation::Tagged, Representation::Float64) => {
                ConversionKind::CheckedTaggedToFloat64
            }
            _ => return Err(Unsupported::Opcode(instruction.op)),
        };
        let conversion = reprs.conversions().iter().find(|conversion| {
            conversion.at_pc == instruction.pc && conversion.operand_index == operand_index
        });
        let may_deopt = matches!(
            expected_kind,
            ConversionKind::CheckedTaggedToInt32 | ConversionKind::CheckedTaggedToFloat64
        );
        if !matches!(
            conversion,
            Some(conversion)
                if conversion.value == input
                    && conversion.kind == expected_kind
                    && conversion.may_deopt == may_deopt
        ) {
            return Err(Unsupported::Opcode(instruction.op));
        }
        if may_deopt {
            let parameter_index = match ssa.values[input.0 as usize].def {
                ValueDef::Param { index, .. } => Some(index),
                ValueDef::Op {
                    op: Op::LoadElement | Op::ToPrimitive | Op::ToNumeric,
                    ..
                } => None,
                _ => return Err(Unsupported::Opcode(instruction.op)),
            };
            guarded_uses.insert((instruction.pc, input), parameter_index);
        }
        allowed_conversions.insert((instruction.pc, operand_index));
    }
    Ok(())
}

fn is_boolean_value(ssa: &SsaFunction, value: ValueId) -> bool {
    matches!(
        ssa.values[value.0 as usize].def,
        ValueDef::Op {
            op: Op::LoadTrue
                | Op::LoadFalse
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual,
            ..
        }
    )
}

/// Reduce the tagged `Value` in `x9` to `VALUE_TRUE` / `VALUE_FALSE` in `x9`
/// per §7.1.2 `ToBoolean`. Int32 and boxed doubles (including `±0`/NaN),
/// booleans, `null`, and `undefined` decide inline; every heap cell resolves
/// through the total leaf `ToBoolean` probe, whose only miss is an isolate-less
/// null heap (never on a live VM) and side-exits at `bail`. Clobbers
/// `x14`/`x15` and the leaf-call argument registers.
fn emit_truthiness_reduce(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    to_boolean_entry: ResolvedRuntimeEntry,
    bail: DynamicLabel,
) {
    let int_case = ops.new_dynamic_label();
    let double_case = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.eq =>int_case                       // all tag bits → int32
        ; cbnz x14, =>double_case               // some tag bits → boxed double
        ; cmp x9, VALUE_TRUE as u32
        ; b.eq =>truthy
        ; cmp x9, VALUE_FALSE as u32
        ; b.eq =>falsy
        ; cmp x9, VALUE_NULL as u32
        ; b.eq =>falsy
        ; cmp x9, VALUE_UNDEFINED as u32
        ; b.eq =>falsy
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        ; mov x1, x9
        ; movz x2, #0
    );
    emit_runtime_entry(ops, relocations, 16, to_boolean_entry);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; cbnz x1, =>bail
        ; mov x9, x0                            // boolean Value from the probe
        ; b =>done
        ; =>int_case
        ; cbz w9, =>falsy
        ; b =>truthy
        ; =>double_case
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, x9, x14                      // raw f64 bit pattern
        ; cbz x14, =>falsy                      // +0.0
        ; movz x15, #0x8000, lsl #48
        ; cmp x14, x15
        ; b.eq =>falsy                          // -0.0
        ; movz x15, CANONICAL_NAN_HI16, lsl #48
        ; cmp x14, x15
        ; b.eq =>falsy                          // canonical NaN
        ; =>truthy
        ; movz x9, VALUE_TRUE as u32
        ; b =>done
        ; =>falsy
        ; movz x9, VALUE_FALSE as u32
        ; =>done
    );
}

fn check_constant_result(instruction: &SsaInstr, reprs: &ReprMap) -> Result<(), Unsupported> {
    let result = instruction
        .result
        .ok_or(Unsupported::OperandShape("optimizing constant result"))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Int32 {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn check_number_constant_result(
    view: &JitCompileSnapshot,
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction.result.ok_or(Unsupported::OperandShape(
        "optimizing number constant result",
    ))?;
    let number = load_number(view, instruction.pc)?;
    let expected = if is_exact_i32(number) {
        Representation::Int32
    } else {
        Representation::Float64
    };
    if !instruction.inputs.is_empty() || reprs.representation(result) != expected {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn check_boolean_result(instruction: &SsaInstr, reprs: &ReprMap) -> Result<(), Unsupported> {
    let result = instruction
        .result
        .ok_or(Unsupported::OperandShape("optimizing boolean result"))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Tagged {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn check_tagged_constant_result(
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction.result.ok_or(Unsupported::OperandShape(
        "optimizing tagged constant result",
    ))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Tagged {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn load_number(view: &JitCompileSnapshot, pc: u32) -> Result<f64, Unsupported> {
    view.instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.load_number)
        .ok_or(Unsupported::OperandShape("optimizing LoadNumber metadata"))
}

fn is_exact_i32(number: f64) -> bool {
    number.is_finite()
        && !(number == 0.0 && number.is_sign_negative())
        && number >= f64::from(i32::MIN)
        && number <= f64::from(i32::MAX)
        && number == f64::from(number as i32)
}

/// Name the exit an instruction deoptimizes through.
fn deopt_exit_at(
    frame_states: &FrameStateTable,
    instruction: &SsaInstr,
) -> Result<DeoptExitId, Unsupported> {
    DeoptLowering::exit_at(frame_states, instruction.inline, instruction.pc).ok_or(
        Unsupported::OperandShape("optimizing deopt exit has no frame state"),
    )
}

fn optimizing_direct_call_target_tier(
    target: &otter_vm::JitDirectCallee,
) -> otter_vm::JitDebugTier {
    match target.plan.tier {
        otter_vm::native_abi::NativeFrameKind::Baseline => otter_vm::JitDebugTier::Template,
        otter_vm::native_abi::NativeFrameKind::Optimizing => otter_vm::JitDebugTier::Optimizing,
        otter_vm::native_abi::NativeFrameKind::Interpreter => {
            unreachable!("interpreter has no entry-capable code generation")
        }
    }
}

fn optimizing_direct_call_event(
    call_kind: otter_vm::JitDirectCallKind,
    instruction_pc: u32,
    byte_pc: u32,
    target: &otter_vm::JitDirectCallee,
    target_index: u32,
    target_count: u32,
    outcome: otter_vm::JitDirectCallLoweringOutcome,
) -> otter_vm::JitCompilerDiagnostic {
    otter_vm::JitCompilerDiagnostic::DirectCallLowered {
        call_kind,
        instruction_pc,
        byte_pc,
        callee_function_id: target.plan.function_id,
        target_index,
        target_count,
        outcome,
    }
}

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
        math_call_entry,
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
    let spill_frame_bytes = aligned_spill_bytes(total_spill_slots(allocation)?)?;
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
                    target: target.kind,
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
            ValueDef::Uninitialized { .. } => {
                emit_load_u32(&mut ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                emit_store_tagged_location(&mut ops, allocation.location(value.id), 9)?;
            }
            ValueDef::ExceptionInput { .. }
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
        emit_initialize_dead_phis(
            &mut ops,
            ssa,
            reprs,
            allocation,
            &ssa.blocks[block_id.0 as usize].phis,
        )?;
        if block_id != cfg.entry {
            for &head in &ssa.blocks[block_id.0 as usize].phis {
                if matches!(
                    ssa.values[head.0 as usize].def,
                    ValueDef::Uninitialized { .. } | ValueDef::InlineUndefinedReturn { .. }
                ) {
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
        for instruction in &ssa.blocks[block_id.0 as usize].instrs {
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
                    let value = if instruction.op == Op::LoadTrue {
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
                    // `this` is canonical tagged state in the active NativeFrame.
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                        ; ldr x9, [x10, NATIVE_FRAME_THIS_OFFSET]
                    );
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
                        allocation
                            .location(instruction.result.expect("eligibility checked null result")),
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
                    emit_dense_element_guards(
                        &mut ops,
                        &mut relocations,
                        view,
                        reprs,
                        allocation,
                        instruction.inputs[0],
                        instruction.inputs[1],
                        miss,
                    )?;
                    emit_load_u64(&mut ops, 11, VALUE_HOLE);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x9, [x16]
                        ; cmp x9, x11
                        ; b.eq =>miss
                    );
                    emit_store_tagged_location(
                        &mut ops,
                        allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked element-load result"),
                        ),
                        9,
                    )?;
                    dynasm!(ops ; .arch aarch64 ; b =>done ; =>miss);
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
                    let store_fast =
                        matches!(value_repr, Representation::Int32 | Representation::Float64);
                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    if store_fast {
                        emit_dense_element_guards(
                            &mut ops,
                            &mut relocations,
                            view,
                            reprs,
                            allocation,
                            instruction.inputs[0],
                            instruction.inputs[1],
                            miss,
                        )?;
                        emit_load_u64(&mut ops, 11, VALUE_HOLE);
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x16]
                            ; cmp x9, x11
                            ; b.eq =>miss
                        );
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
                        dynasm!(ops ; .arch aarch64 ; str x9, [x16]);
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
                    let cell_ordinal = u32::try_from(next_load_ic)
                        .map_err(|_| Unsupported::OperandShape("optimizing property IC ordinal"))?;
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

                    // Inline own-data probe through the self-patching cell:
                    // guard cell tag, body tag, and shape, then read the value
                    // slab slot straight into the destination. The sequence
                    // neither allocates nor calls, so it needs no safepoint, no
                    // frame materialize, and no reload — the receiver pointer is
                    // re-derived from its rooted location every access.
                    if view.cage_base != 0 {
                        let shape_byte = view.object_shape_byte;
                        emit_load_tagged_location(
                            &mut ops,
                            allocation.location(instruction.inputs[0]),
                            9,
                        )?;
                        dynasm!(ops
                            ; .arch aarch64
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #0x2       // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>miss
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
                            ; cmp w14, OBJECT_BODY_TYPE_TAG
                            ; b.ne =>miss
                            ; ldr w14, [x13, shape_byte]
                            ; cbz w14, =>miss          // empty-cell sentinel
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            15,
                            cell_addr as u64,
                            RelocationTarget::PropertyIcCell {
                                access: PropertyIcAccess::Load,
                                ordinal: cell_ordinal,
                            },
                        );
                        let do_load = ops.new_dynamic_label();
                        for way in 0..IC_WAYS as u32 {
                            let shape_off = way * 8;
                            let value_byte_off = shape_off + 4;
                            let next = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr w16, [x15, shape_off]
                                ; cmp w14, w16
                                ; b.ne =>next
                                ; ldr w17, [x15, value_byte_off]
                                ; b =>do_load
                                ; =>next
                            );
                        }
                        dynasm!(ops ; .arch aarch64 ; b =>miss ; =>do_load);
                        crate::template::arm64::values::emit_slab_base(&mut ops, view, 13, 14);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x13, =>miss
                            ; ldr w9, [x13, x17]       // 4-byte compressed slot
                        );
                        let boxed_entry = ops.new_dynamic_label();
                        let continuation = ops.new_dynamic_label();
                        boxed_slot_slow_paths.push(
                            crate::template::arm64::values::BoxedSlotSlowPath {
                                entry: boxed_entry,
                                continuation,
                                miss,
                            },
                        );
                        crate::template::arm64::values::emit_decompress_slot(
                            &mut ops,
                            &mut relocations,
                            view.cage_base as u64,
                            boxed_entry,
                        );
                        dynasm!(ops ; .arch aarch64 ; =>continuation);
                        emit_store_tagged_location(&mut ops, result_location, 9)?;
                        dynasm!(ops ; .arch aarch64 ; b =>done);
                    }

                    // Miss: the window transition resolves full `[[Get]]`
                    // semantics and self-patches this site's cell.
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
                    let cell_ordinal = u32::try_from(next_store_ic)
                        .map_err(|_| Unsupported::OperandShape("optimizing store IC ordinal"))?;
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
                    if view.cage_base != 0 {
                        let shape_byte = view.object_shape_byte;
                        emit_load_tagged_location(
                            &mut ops,
                            allocation.location(instruction.inputs[0]),
                            9,
                        )?;
                        dynasm!(ops
                            ; .arch aarch64
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #0x2       // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>miss
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
                            ; add x13, x13, x12
                            ; ldrb w14, [x13]
                            ; cmp w14, OBJECT_BODY_TYPE_TAG
                            ; b.ne =>miss
                            ; ldr w14, [x13, shape_byte]
                            ; cbz w14, =>miss
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            15,
                            cell_addr as u64,
                            RelocationTarget::PropertyIcCell {
                                access: PropertyIcAccess::Store,
                                ordinal: cell_ordinal,
                            },
                        );
                        let do_store = ops.new_dynamic_label();
                        for way in 0..IC_WAYS as u32 {
                            let shape_off = way * 8;
                            let value_byte_off = shape_off + 4;
                            let next = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr w16, [x15, shape_off]
                                ; cmp w14, w16
                                ; b.ne =>next
                                ; ldr w17, [x15, value_byte_off]
                                ; b =>do_store
                                ; =>next
                            );
                        }
                        dynasm!(ops ; .arch aarch64 ; b =>miss ; =>do_store);
                        crate::template::arm64::values::emit_slab_base(&mut ops, view, 13, 14);
                        dynasm!(ops ; .arch aarch64 ; cbz x13, =>miss);
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
                        crate::template::arm64::values::emit_compress_slot_or_bail(&mut ops, miss);
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
                        if instruction.op == Op::StoreUpvalueChecked {
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
                    } else if let Some(target) = view.global_object_loads.get(&metadata.byte_pc) {
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
                    let negate = u64::from(instruction.op == Op::LooseNotEqual);
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
                Op::MathCall => {
                    let dst = instruction
                        .result_register
                        .expect("eligibility checked math-call destination");
                    let method = view.instructions[instruction.pc as usize]
                        .const_index(view.code_block.as_ref(), 1)
                        .ok_or(Unsupported::OperandShape("math-call method constant"))?;
                    let arguments = eligibility
                        .math_call_arguments
                        .get(&instruction.pc)
                        .ok_or(Unsupported::OperandShape("math-call argument arena"))?;
                    let arguments_len = u32::try_from(arguments.len()).map_err(|_| {
                        Unsupported::OperandShape("optimizing math-call argument length")
                    })?;
                    let site = eligibility
                        .element_transitions
                        .sites
                        .get(&instruction.pc)
                        .ok_or(Unsupported::OperandShape(
                            "optimizing math call missing site",
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
                    );
                    emit_load_u64(&mut ops, 2, u64::from(method));
                    // The boxed argument-register slice is owned by the produced
                    // code object, so its interior pointer is stable for the
                    // code's whole life.
                    emit_load_symbolic_u64(
                        &mut ops,
                        &mut relocations,
                        3,
                        arguments.as_ptr() as u64,
                        RelocationTarget::OptimizedMathArguments {
                            inline_frame: instruction.inline.0,
                            logical_pc: instruction.pc,
                            len: arguments_len,
                        },
                    );
                    emit_load_u64(&mut ops, 4, arguments.len() as u64);
                    emit_runtime_entry(&mut ops, &mut relocations, 16, math_call_entry);
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
                                    .expect("eligibility checked math-call result"),
                            ),
                        )),
                    )?;
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
                    let planned_methods = (instruction.inline == InlineId::ROOT)
                        .then(|| view.direct_methods.get(&byte_pc))
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
                                        this_mode: otter_vm::JitDirectCallThisMode::MethodReceiver,
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
                    emit_load_boxed_value(&mut ops, reprs, allocation, instruction.inputs[0], 9)?;
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
                            match instruction.op {
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
                                _ => return Err(Unsupported::Opcode(instruction.op)),
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
                            match instruction.op {
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
                                _ => return Err(Unsupported::Opcode(instruction.op)),
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
                    match instruction.op {
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
                        _ => return Err(Unsupported::Opcode(instruction.op)),
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
                        emit_int_comparison(&mut ops, instruction.op);
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
                        emit_float_comparison(&mut ops, instruction.op);
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
                            instruction.op == Op::NotEqual,
                            deopt,
                        );
                        crate::template::arm64::values::emit_box_bool(&mut ops, 13, 12);
                        dynasm!(ops ; .arch aarch64 ; mov x11, x13);
                    }
                    emit_store_tagged_location(
                        &mut ops,
                        allocation
                            .location(instruction.result.expect("eligibility checked result")),
                        11,
                    )?;
                }
                Op::Jump => {
                    let target = block.normal_succs[0];
                    emit_cfg_edge(
                        &mut ops,
                        &mut relocations,
                        allocation,
                        eligibility,
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
                    emit_load_tagged_location(
                        &mut ops,
                        allocation.location(instruction.inputs[0]),
                        9,
                    )?;
                    // A provably-boolean condition compares directly; any other
                    // tagged value is reduced to `VALUE_TRUE`/`VALUE_FALSE` first.
                    if !is_boolean_value(ssa, instruction.inputs[0]) {
                        let bail = ops.new_dynamic_label();
                        emit_truthiness_reduce(&mut ops, &mut relocations, to_boolean_entry, bail);
                        deopt_exits.push((
                            bail,
                            deopt_exit_at(frame_states, instruction)?,
                            instruction.pc,
                        ));
                    }
                    emit_load_u32(&mut ops, 10, VALUE_TRUE as u32);
                    let taken_edge = ops.new_dynamic_label();
                    if instruction.op == Op::JumpIfTrue {
                        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.eq =>taken_edge);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>taken_edge);
                    }

                    emit_cfg_edge(
                        &mut ops,
                        &mut relocations,
                        allocation,
                        eligibility,
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
                        poll_entry,
                        threw,
                        &block_labels,
                        block_id,
                        taken,
                    )?;
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
                        let static_site = StaticNativeCallSite {
                            target,
                            caller_function_id: frame.function_id,
                            logical_pc: instruction.pc,
                            byte_pc,
                            argc: arg_regs.len(),
                        };
                        if instruction.inputs.len() >= 2
                            && static_native_target_is_supported(view, static_site)
                        {
                            emit_load_boxed_value(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction.inputs[0],
                                9,
                            )?;
                            emit_load_boxed_value(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction.inputs[1],
                                10,
                            )?;
                            emit_static_native_call(
                                &mut ops,
                                &mut relocations,
                                view,
                                static_site,
                                9,
                                10,
                                code_map.as_mut(),
                                bail,
                            )?;
                            emit_store_tagged_location(
                                &mut ops,
                                allocation.location(
                                    instruction.result.expect("eligibility checked call result"),
                                ),
                                9,
                            )?;
                            if let Some(events) = direct_call_events.as_mut() {
                                events.insert(
                                    (byte_pc, 0),
                                    otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                                        instruction_pc: instruction.pc,
                                        byte_pc,
                                        target: target.kind,
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
                                        target: target.kind,
                                        outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Rejected {
                                            reason: if instruction.inputs.len() < 2 {
                                                otter_vm::JitStaticNativeCallLoweringRejectionReason::ArityUnsupported
                                            } else {
                                                otter_vm::JitStaticNativeCallLoweringRejectionReason::LayoutUnsupported
                                            },
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
                            .ok_or(Unsupported::OperandShape("optimizing call missing site"))?;
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
                        if let Some(target) =
                            direct_target.filter(|target| direct_call_target_is_supported(target))
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
                                            target_tier: optimizing_direct_call_target_tier(target),
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
                                    instruction.result.expect("eligibility checked call result"),
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
                    dynasm!(ops
                        ; .arch aarch64
                        ; movz x11, NUMBER_TAG_HI16, lsl #48
                        ; orr x11, x11, #0x2       // NOT_CELL_MASK
                        ; tst x9, x11
                        ; b.ne =>deopt
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
                            .ok_or(Unsupported::OperandShape("optimizing inlined call byte PC"))?;
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
                            emit_load_tagged_location(&mut ops, allocation.location(returned), 9)?;
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
                _ => return Err(Unsupported::Opcode(instruction.op)),
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

/// Emit one normal edge. Loop-carried phi destinations are populated before
/// the poll because the poll's deopt state is the target header's entry state.
#[allow(clippy::too_many_arguments)]
fn emit_cfg_edge(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    allocation: &Allocation,
    eligibility: &Eligibility,
    poll_entry: ResolvedRuntimeEntry,
    threw: DynamicLabel,
    block_labels: &[DynamicLabel],
    predecessor: BlockId,
    target: BlockId,
) -> Result<(), Unsupported> {
    emit_edge_moves(
        ops,
        allocation,
        edge_moves(allocation, predecessor, target)?,
    )?;
    if eligibility.back_edges.contains_key(&(predecessor, target)) {
        emit_backedge_poll(ops, relocations, poll_entry, threw);
    }
    let target_label = block_labels[target.0 as usize];
    dynasm!(ops ; .arch aarch64 ; b =>target_label);
    Ok(())
}

/// Poll every optimized back-edge without re-entering the VM.
///
/// The policy mirrors the baseline read/decrement pattern: the interrupt byte
/// is read first, then the shared fuel is decremented and stored (clamped at
/// zero so repeated entries cannot underflow it). An interrupt or non-positive
/// fuel value bails at the loop header. The interpreter then
/// resumes with the shared fuel still exhausted and performs its ordinary
/// interrupt/budget checkpoint on its next back-edge. Thus optimized code
/// bails at least as often as the interpreter/baseline poll cadence, never
/// less often, while keeping all refill and error policy inside the VM.
/// Cooperative back-edge poll, matching the template tier: the fast path
/// decrements the fuel cell inline; an exhausted budget or a raised interrupt
/// calls the leaf poll stub, which refills the budget and returns to the
/// compiled loop, or raises (interrupt, timeout) through the throw epilogue.
/// Deoptimizing here would abandon the compiled loop once per budget window.
fn emit_backedge_poll(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    poll_entry: ResolvedRuntimeEntry,
    threw: DynamicLabel,
) {
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x20, THREAD_OFFSET]
        ; ldr x9, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w9, [x9]
        ; cbnz w9, =>slow
        ; ldr x9, [x17, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x10, [x9]
        ; subs x10, x10, #1
        ; str x10, [x9]
        ; b.gt =>cont
        ; =>slow
        ; mov x0, x20
    );
    emit_runtime_entry(ops, relocations, 16, poll_entry);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; =>cont
    );
}

fn emit_int_comparison(ops: &mut Assembler, op: Op) {
    dynasm!(ops ; .arch aarch64 ; cmp w9, w10);
    match op {
        Op::LessThan => dynasm!(ops ; .arch aarch64 ; cset w11, lt),
        Op::LessEq => dynasm!(ops ; .arch aarch64 ; cset w11, le),
        Op::GreaterThan => dynasm!(ops ; .arch aarch64 ; cset w11, gt),
        Op::GreaterEq => dynasm!(ops ; .arch aarch64 ; cset w11, ge),
        Op::Equal => dynasm!(ops ; .arch aarch64 ; cset w11, eq),
        Op::NotEqual => dynasm!(ops ; .arch aarch64 ; cset w11, ne),
        _ => unreachable!("eligibility checked comparison"),
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w12, VALUE_FALSE_LOW
        ; add w11, w11, w12
    );
}

/// Materialize a tagged boolean for an IEEE-754 comparison. AArch64's
/// unordered `fcmp` flags make `mi`/`ls` the relational conditions that stay
/// false for NaN; `eq` is likewise false, while `ne` is true as JavaScript
/// requires for numeric inequality.
fn emit_float_comparison(ops: &mut Assembler, op: Op) {
    dynasm!(ops ; .arch aarch64 ; fcmp D(FP_SCRATCH), D(FP_SCRATCH_2));
    match op {
        Op::LessThan => dynasm!(ops ; .arch aarch64 ; cset w11, mi),
        Op::LessEq => dynasm!(ops ; .arch aarch64 ; cset w11, ls),
        Op::GreaterThan => dynasm!(ops ; .arch aarch64 ; cset w11, gt),
        Op::GreaterEq => dynasm!(ops ; .arch aarch64 ; cset w11, ge),
        Op::Equal => dynasm!(ops ; .arch aarch64 ; cset w11, eq),
        Op::NotEqual => dynasm!(ops ; .arch aarch64 ; cset w11, ne),
        _ => unreachable!("eligibility checked comparison"),
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w12, VALUE_FALSE_LOW
        ; add w11, w11, w12
    );
}

fn edge_moves(
    allocation: &Allocation,
    predecessor: BlockId,
    block: BlockId,
) -> Result<&EdgeMoves, Unsupported> {
    allocation
        .edge_moves
        .iter()
        .find(|edge| edge.predecessor == predecessor && edge.block == block)
        .ok_or(Unsupported::OperandShape(
            "optimizing edge is missing phi moves",
        ))
}

fn emit_edge_moves(
    ops: &mut Assembler,
    allocation: &Allocation,
    edge: &EdgeMoves,
) -> Result<(), Unsupported> {
    for &movement in &edge.moves {
        emit_move(ops, allocation, movement)?;
    }
    Ok(())
}

fn emit_move(
    ops: &mut Assembler,
    allocation: &Allocation,
    movement: Move,
) -> Result<(), Unsupported> {
    match movement.conversion {
        None => emit_raw_move(ops, allocation, movement.src, movement.dst),
        Some(ConversionKind::BoxInt32) => {
            emit_load_move_gpr(ops, movement.src, 9)?;
            emit_box_int32(ops, 9, 10);
            emit_store_move_gpr(ops, movement.dst, 9)
        }
        Some(ConversionKind::Int32ToFloat64) => {
            emit_load_move_gpr(ops, movement.src, 9)?;
            dynasm!(ops ; .arch aarch64 ; scvtf d17, w9);
            emit_store_move_fp(ops, allocation, movement.dst, FP_SCRATCH_2)
        }
        Some(ConversionKind::BoxFloat64) => {
            emit_load_move_fp(ops, allocation, movement.src, FP_SCRATCH_2)?;
            emit_box_double(ops, FP_SCRATCH_2, 9);
            emit_store_move_gpr(ops, movement.dst, 9)
        }
        Some(ConversionKind::CheckedTaggedToInt32 | ConversionKind::CheckedTaggedToFloat64) => Err(
            Unsupported::OperandShape("optimizing checked phi conversion"),
        ),
    }
}

fn emit_raw_move(
    ops: &mut Assembler,
    allocation: &Allocation,
    src: Location,
    dst: Location,
) -> Result<(), Unsupported> {
    match (src, dst) {
        (Location::Register(RegClass::Gpr, src), Location::Register(RegClass::Gpr, dst)) => {
            let src = gpr_move_register(src)?;
            let dst = gpr_move_register(dst)?;
            dynasm!(ops ; .arch aarch64 ; mov X(dst), X(src));
        }
        (Location::Register(RegClass::Gpr, src), Location::Spill(RegClass::Gpr, dst)) => {
            let src = gpr_move_register(src)?;
            let offset = spill_offset(dst)?;
            emit_sp_str_x(ops, src, offset);
        }
        (Location::Spill(RegClass::Gpr, src), Location::Register(RegClass::Gpr, dst)) => {
            let dst = gpr_move_register(dst)?;
            let offset = spill_offset(src)?;
            emit_sp_ldr_x(ops, dst, offset);
        }
        (Location::Spill(RegClass::Gpr, src), Location::Spill(RegClass::Gpr, dst)) => {
            let src_offset = spill_offset(src)?;
            let dst_offset = spill_offset(dst)?;
            emit_sp_ldr_x(ops, 9, src_offset);
            emit_sp_str_x(ops, 9, dst_offset);
        }
        (Location::Register(RegClass::Fp, src), Location::Register(RegClass::Fp, dst)) => {
            let src = fp_move_register(src)?;
            let dst = fp_move_register(dst)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(dst), D(src));
        }
        (Location::Register(RegClass::Fp, src), Location::Spill(RegClass::Fp, dst)) => {
            let src = fp_move_register(src)?;
            let offset = fp_spill_offset(allocation, dst)?;
            emit_sp_str_d(ops, src, offset);
        }
        (Location::Spill(RegClass::Fp, src), Location::Register(RegClass::Fp, dst)) => {
            let dst = fp_move_register(dst)?;
            let offset = fp_spill_offset(allocation, src)?;
            emit_sp_ldr_d(ops, dst, offset);
        }
        (Location::Spill(RegClass::Fp, src), Location::Spill(RegClass::Fp, dst)) => {
            let src_offset = fp_spill_offset(allocation, src)?;
            let dst_offset = fp_spill_offset(allocation, dst)?;
            emit_sp_ldr_d(ops, FP_SCRATCH_2, src_offset);
            emit_sp_str_d(ops, FP_SCRATCH_2, dst_offset);
        }
        _ => return Err(Unsupported::OperandShape("optimizing cross-class phi move")),
    }
    Ok(())
}

fn emit_load_move_gpr(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = gpr_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; mov X(scratch), X(physical));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_ldr_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing non-GPR phi source"));
        }
    }
    Ok(())
}

fn emit_store_move_gpr(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = gpr_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; mov X(physical), X(scratch));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_str_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape(
                "optimizing non-GPR phi destination",
            ));
        }
    }
    Ok(())
}

fn emit_load_move_fp(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = fp_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(scratch), D(physical));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            emit_sp_ldr_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP phi source"));
        }
    }
    Ok(())
}

fn emit_store_move_fp(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = fp_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(physical), D(scratch));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            emit_sp_str_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape(
                "optimizing non-FP phi destination",
            ));
        }
    }
    Ok(())
}

/// Give pruned, structurally dead phis representation-valid homes. They emit
/// no predecessor copies, but exact deopt writeback may still name their
/// compiler-scratch register slots before later bytecode overwrites them.
fn emit_initialize_dead_phis(
    ops: &mut Assembler,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    allocation: &Allocation,
    phis: &[ValueId],
) -> Result<(), Unsupported> {
    for &phi in phis {
        if !is_dead_phi(ssa, phi) {
            continue;
        }
        match reprs.representation(phi) {
            Representation::Tagged => {
                emit_load_u32(ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                emit_store_tagged_location(ops, allocation.location(phi), 9)?;
            }
            Representation::Int32 => {
                emit_load_u32(ops, 9, 0);
                emit_store_location(ops, allocation.location(phi), 9)?;
            }
            Representation::Float64 => {
                emit_load_u64(ops, 9, 0.0_f64.to_bits());
                dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), x9);
                emit_store_fp_location(ops, allocation, allocation.location(phi), FP_SCRATCH)?;
            }
        }
    }
    Ok(())
}

fn gpr_move_register(register: u8) -> Result<u8, Unsupported> {
    if register == ALLOCATABLE_REGISTER_COUNT {
        Ok(12)
    } else {
        VALUE_REGISTERS
            .get(register as usize)
            .copied()
            .ok_or(Unsupported::OperandShape(
                "optimizing phi move register mapping",
            ))
    }
}

fn fp_move_register(register: u8) -> Result<u8, Unsupported> {
    if register == REGISTER_BUDGET.fp {
        Ok(FP_SCRATCH)
    } else {
        FP_REGISTERS
            .get(register as usize)
            .copied()
            .ok_or(Unsupported::OperandShape(
                "optimizing FP phi move register mapping",
            ))
    }
}

fn emit_load_parameter(ops: &mut Assembler, index: u32, scratch: u8) {
    let offset = index * STACK_SLOT_BYTES;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, x12]);
    }
}

/// Inverse of [`emit_deopt_writeback`] for a loop-header live set.
///
/// The interpreter window is still the canonical rooted state on entry. Each
/// live frame-state value is loaded from its bytecode register, checked and
/// unboxed according to the representation analysis, then written to the same
/// allocated location that deopt later reads. Until every value has been
/// materialized, a representation failure returns through `bail` without
/// writing any machine location back over the window.
fn emit_osr_materialization(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    site: &OsrEntrySite,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    for live in &site.live_values {
        emit_load_frame_register(ops, u32::from(live.register), 9)?;
        match reprs.representation(live.value) {
            Representation::Tagged => {
                emit_store_tagged_location(ops, allocation.location(live.value), 9)?;
            }
            Representation::Int32 => {
                emit_guard_int32(ops, 9, bail);
                emit_store_location(ops, allocation.location(live.value), 9)?;
            }
            Representation::Float64 => {
                emit_num_to_double(ops, 9, FP_SCRATCH, bail);
                emit_store_fp_location(
                    ops,
                    allocation,
                    allocation.location(live.value),
                    FP_SCRATCH,
                )?;
            }
        }
    }
    Ok(())
}

/// Restore one interpreter frame's registers from an exit's slots.
///
/// `window` addresses the frame being rebuilt: `x19` for the compiled function's
/// own frame, or the window a reify handed back for an inlined callee's.
fn emit_deopt_writeback(
    ops: &mut Assembler,
    allocation: &Allocation,
    frame: &DeoptFrame,
    window: u8,
) -> Result<(), Unsupported> {
    for (register, slot) in frame.slots.iter().enumerate() {
        match slot.location {
            DeoptLocation::Register(machine_register) => match slot.repr {
                DeoptRepr::Int32 | DeoptRepr::Tagged => {
                    let physical = VALUE_REGISTERS
                        .get(machine_register as usize)
                        .copied()
                        .ok_or(Unsupported::OperandShape("optimizing deopt GPR mapping"))?;
                    if slot.repr == DeoptRepr::Int32 {
                        dynasm!(ops ; .arch aarch64 ; mov w9, W(physical));
                    } else {
                        dynasm!(ops ; .arch aarch64 ; mov x9, X(physical));
                    }
                }
                DeoptRepr::Float64 => {
                    let fp_index = machine_register
                        .checked_sub(u16::from(allocation.register_budget.gpr))
                        .ok_or(Unsupported::OperandShape(
                            "optimizing deopt FP register class",
                        ))?;
                    let physical = FP_REGISTERS
                        .get(fp_index as usize)
                        .copied()
                        .ok_or(Unsupported::OperandShape("optimizing deopt FP mapping"))?;
                    dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), D(physical));
                }
            },
            DeoptLocation::StackSlot(offset) => {
                let offset = u32::try_from(offset).map_err(|_| {
                    Unsupported::OperandShape("optimizing negative deopt spill offset")
                })?;
                match slot.repr {
                    DeoptRepr::Int32 => emit_sp_ldr_w(ops, 9, offset),
                    DeoptRepr::Tagged => emit_sp_ldr_x(ops, 9, offset),
                    DeoptRepr::Float64 => {
                        emit_sp_ldr_d(ops, FP_SCRATCH, offset);
                    }
                }
            }
            DeoptLocation::Constant(_) if slot.repr == DeoptRepr::Float64 => {
                return Err(Unsupported::OperandShape(
                    "optimizing float64 deopt constant",
                ));
            }
            DeoptLocation::Constant(_) => {
                emit_load_u32(ops, 9, otter_vm::Value::undefined().to_bits() as u32);
            }
        }
        match slot.repr {
            DeoptRepr::Tagged => {}
            DeoptRepr::Int32 => dynasm!(ops
                ; .arch aarch64
                ; movz x10, NUMBER_TAG_HI16, lsl #48
                ; orr x9, x10, x9
            ),
            DeoptRepr::Float64 => emit_box_double(ops, FP_SCRATCH, 9),
        }
        emit_store_frame_register_in(ops, window, register as u32, 9)?;
    }
    Ok(())
}

/// Materialize only the transition operands and tagged values live across the
/// call. Optimizing spills are private and unscanned, so live tagged spills
/// take the same interpreter-window rooting path as tagged machine registers.
fn emit_materialize_element_transition(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    site: &ElementTransitionSite,
) -> Result<(), Unsupported> {
    let mut materialized_registers = BTreeSet::new();
    for (&value, &register) in instruction.inputs.iter().zip(&instruction.input_registers) {
        emit_materialize_frame_value(ops, reprs, allocation, value, register)?;
        materialized_registers.insert(register);
    }
    for live in &site.tagged_live_across {
        if !materialized_registers.insert(live.register) {
            continue;
        }
        emit_load_tagged_location(ops, allocation.location(live.value), 9)?;
        emit_store_frame_register(ops, u32::from(live.register), 9)?;
    }
    Ok(())
}

fn emit_materialize_frame_value(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    value: ValueId,
    register: u16,
) -> Result<(), Unsupported> {
    match reprs.representation(value) {
        Representation::Tagged => {
            emit_load_tagged_location(ops, allocation.location(value), 9)?;
        }
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(value), 9)?;
            emit_box_int32(ops, 9, 10);
        }
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(value), FP_SCRATCH)?;
            emit_box_double(ops, FP_SCRATCH, 9);
        }
    }
    emit_store_frame_register(ops, u32::from(register), 9)
}

fn emit_load_boxed_value(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    value: ValueId,
    scratch: u8,
) -> Result<(), Unsupported> {
    match reprs.representation(value) {
        Representation::Tagged => {
            emit_load_tagged_location(ops, allocation.location(value), scratch)
        }
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(value), scratch)?;
            let tag_scratch = if scratch == 10 { 11 } else { 10 };
            emit_box_int32(ops, scratch, tag_scratch);
            Ok(())
        }
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(value), FP_SCRATCH)?;
            emit_box_double(ops, FP_SCRATCH, scratch);
            Ok(())
        }
    }
}

/// Reload every tagged value live across moving GC, then optionally load an
/// element-load result last. Numeric homes and indices stay untouched.
fn emit_reload_element_transition(
    ops: &mut Assembler,
    allocation: &Allocation,
    site: &ElementTransitionSite,
    load_result: Option<(u16, Location)>,
) -> Result<(), Unsupported> {
    let mut reloaded = BTreeSet::new();
    for live in &site.tagged_live_across {
        if !reloaded.insert(live.value) {
            continue;
        }
        emit_load_frame_register(ops, u32::from(live.register), 9)?;
        emit_store_tagged_location(ops, allocation.location(live.value), 9)?;
    }
    if let Some((result_register, dst_location)) = load_result {
        emit_load_frame_register(ops, u32::from(result_register), 9)?;
        emit_store_tagged_location(ops, dst_location, 9)?;
    }
    Ok(())
}

fn emit_load_frame_register(
    ops: &mut Assembler,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    let offset = register
        .checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape(
            "optimizing frame register offset",
        ))?;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, x12]);
    }
    Ok(())
}

fn emit_store_frame_register(
    ops: &mut Assembler,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    emit_store_frame_register_in(ops, 19, register, scratch)
}

/// Store into an interpreter register window addressed by `window`.
///
/// The compiled frame's own window is `x19`; a frame reified for an inlined
/// callee lives in the window its reify handed back, and the callee's registers
/// belong there and not in its caller's.
fn emit_store_frame_register_in(
    ops: &mut Assembler,
    window: u8,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    let offset = register
        .checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape(
            "optimizing frame register offset",
        ))?;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [X(window), offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [X(window), x12]);
    }
    Ok(())
}

fn load_int32(view: &JitCompileSnapshot, pc: u32) -> Result<i32, Unsupported> {
    match view
        .instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.operand(&view.code_block, 1))
    {
        Some(Operand::Imm32(value)) => Ok(value),
        _ => Err(Unsupported::OperandShape("optimizing LoadInt32 operands")),
    }
}

fn total_spill_slots(allocation: &Allocation) -> Result<u32, Unsupported> {
    analyzed_spill_slot_count(allocation).map_err(OptimizationError::into_unsupported)
}

fn aligned_spill_bytes(spill_slot_count: u32) -> Result<u32, Unsupported> {
    let bytes = spill_slot_count
        .checked_mul(STACK_SLOT_BYTES)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape("optimizing spill frame overflow"))?;
    if bytes > MAX_SPILL_FRAME_BYTES {
        return Err(Unsupported::OperandShape(
            "optimizing spill frame exceeds arm64 immediates",
        ));
    }
    Ok(bytes)
}

/// Largest unsigned scaled immediate offset for a 64-bit `ldr`/`str`.
const MAX_SP_IMM_OFFSET: u32 = 4095 * 8;

/// `ldr X(reg), [sp, offset]` for any spill offset; big frames go through the
/// `x12` scratch the same way window addressing does.
fn emit_sp_ldr_x(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(reg), [sp, x12]);
    }
}

/// `str X(reg), [sp, offset]` for any spill offset.
fn emit_sp_str_x(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str X(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str X(reg), [sp, x12]);
    }
}

/// `ldr W(reg), [sp, offset]` for any spill offset.
fn emit_sp_ldr_w(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr W(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr W(reg), [sp, x12]);
    }
}

/// `ldr D(reg), [sp, offset]` for any spill offset.
fn emit_sp_ldr_d(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr D(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr D(reg), [sp, x12]);
    }
}

/// `str D(reg), [sp, offset]` for any spill offset.
fn emit_sp_str_d(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str D(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str D(reg), [sp, x12]);
    }
}

fn spill_offset(slot: u32) -> Result<u32, Unsupported> {
    slot.checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape("optimizing spill offset"))
}

fn fp_spill_offset(allocation: &Allocation, slot: u32) -> Result<u32, Unsupported> {
    let unified = allocation
        .spill_slot_counts
        .gpr
        .checked_add(slot)
        .ok_or(Unsupported::OperandShape("optimizing FP spill offset"))?;
    spill_offset(unified)
}

fn conversion_kind_at(
    reprs: &ReprMap,
    instruction: &SsaInstr,
    operand_index: usize,
) -> Option<ConversionKind> {
    reprs
        .conversions()
        .iter()
        .find(|conversion| {
            conversion.at_pc == instruction.pc && conversion.operand_index == operand_index
        })
        .map(|conversion| conversion.kind)
}

/// Fast-path source-lowered numeric coercions without invoking user code.
/// Tagged non-numbers bail at the coercion's exact bytecode PC so the
/// interpreter performs the observable `ToPrimitive` / `ToNumeric` semantics.
fn emit_tagged_numeric_coercion(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    deopt: Option<DynamicLabel>,
) -> Result<(), Unsupported> {
    let input = instruction.inputs[0];
    match reprs.representation(input) {
        Representation::Tagged => {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing numeric coercion missing deopt exit",
            ))?;
            emit_load_tagged_location(ops, allocation.location(input), 9)?;
            emit_guard_number(ops, 9, deopt);
        }
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(input), 9)?;
            emit_box_int32(ops, 9, 10);
        }
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(input), FP_SCRATCH)?;
            emit_box_double(ops, FP_SCRATCH, 9);
        }
    }
    emit_store_tagged_location(
        ops,
        allocation.location(
            instruction
                .result
                .expect("eligibility checked numeric coercion result"),
        ),
        9,
    )
}

fn emit_load_int_operand(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    operand_index: usize,
    scratch: u8,
    deopt: Option<DynamicLabel>,
) -> Result<(), Unsupported> {
    let input = instruction.inputs[operand_index];
    match reprs.representation(input) {
        Representation::Int32 => emit_load_location(ops, allocation.location(input), scratch),
        Representation::Tagged
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::CheckedTaggedToInt32) =>
        {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing int32 guard missing deopt exit",
            ))?;
            emit_load_tagged_location(ops, allocation.location(input), scratch)?;
            emit_guard_int32(ops, scratch, deopt);
            Ok(())
        }
        Representation::Float64 | Representation::Tagged => Err(Unsupported::OperandShape(
            "optimizing int32 operand conversion",
        )),
    }
}

fn emit_load_float_operand(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    operand_index: usize,
    scratch: u8,
    deopt: Option<DynamicLabel>,
) -> Result<(), Unsupported> {
    let input = instruction.inputs[operand_index];
    match reprs.representation(input) {
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(input), scratch)
        }
        Representation::Int32
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::Int32ToFloat64) =>
        {
            emit_load_location(ops, allocation.location(input), 9)?;
            dynasm!(ops ; .arch aarch64 ; scvtf D(scratch), w9);
            Ok(())
        }
        Representation::Tagged
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::CheckedTaggedToFloat64) =>
        {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing float64 guard missing deopt exit",
            ))?;
            emit_load_tagged_location(ops, allocation.location(input), 9)?;
            emit_num_to_double(ops, 9, scratch, deopt);
            Ok(())
        }
        Representation::Int32 | Representation::Tagged => Err(Unsupported::OperandShape(
            "optimizing float64 operand conversion",
        )),
    }
}

/// Exact frozen number-tag test used by the template tier.
fn emit_guard_int32(ops: &mut Assembler, register: u8, deopt: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(register), x15
        ; cmp x14, x15
        ; b.ne =>deopt
    );
}

/// Accept either frozen tagged-number encoding while rejecting all other
/// primitives and heap references before a source-level coercion can run.
fn emit_guard_number(ops: &mut Assembler, register: u8, deopt: DynamicLabel) {
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(register), x15
        ; cmp x14, x15
        ; b.eq =>done
        ; tst X(register), x15
        ; b.eq =>deopt
        ; =>done
    );
}

/// Decode an engine-tagged number exactly as the template tier does: all
/// number-tag bits select int32, some select a double, and none is non-number.
fn emit_num_to_double(ops: &mut Assembler, source: u8, destination: u8, deopt: DynamicLabel) {
    let non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(source), x15
        ; cmp x14, x15
        ; b.ne =>non_int
        ; scvtf D(destination), W(source)
        ; b =>done
        ; =>non_int
        ; tst X(source), x15
        ; b.eq =>deopt
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, X(source), x14
        ; fmov D(destination), x14
        ; =>done
    );
}

fn emit_load_fp_location(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = FP_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing FP register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; fmov D(scratch), D(physical));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            emit_sp_ldr_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP location"));
        }
    }
    Ok(())
}

fn emit_store_fp_location(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = FP_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing FP register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; fmov D(physical), D(scratch));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            emit_sp_str_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP location"));
        }
    }
    Ok(())
}

fn emit_load_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov W(scratch), W(physical));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_ldr_w(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

fn emit_store_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov W(physical), W(scratch));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            // An int32 spill slot is 8 bytes and other emitters read it with
            // 64-bit loads (raw moves, boxing edge moves), so the store must
            // cover the whole slot. A 32-bit store would leave the upper half
            // as stack garbage that a later 64-bit read folds into the boxed
            // value. The `mov` zero-extends in case the caller's register
            // carries tag bits above the payload (OSR reloads pass the boxed
            // form).
            dynasm!(ops ; .arch aarch64 ; mov W(scratch), W(scratch));
            emit_sp_str_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

fn emit_load_tagged_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov X(scratch), X(physical));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_ldr_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

fn emit_store_tagged_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov X(physical), X(scratch));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_str_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

/// Emit the dense-array fast-path guards for one element access.
///
/// On the hit path this leaves the element's address in `x16` and falls
/// through; any failed guard branches to `miss`, where the reentrant stub
/// completes the access generically. Guards, in order: the receiver is a heap
/// cell, its body is an ordinary `ArrayBody` with no exotic sidecar, and the
/// index is an int32 (untagged inline when its representation is `Tagged`)
/// inside the dense bounds. The dense base and length load from the
/// VM-maintained body cache, so `Vec` layout stays unobserved.
///
/// Clobbers `x9`, `x11`-`x16`.
#[allow(clippy::too_many_arguments)]
fn emit_dense_element_guards(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    reprs: &ReprMap,
    allocation: &Allocation,
    receiver: ValueId,
    index: ValueId,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let layout = view.array_layout;
    // Receiver: a heap cell whose body is an ordinary dense array.
    emit_load_tagged_location(ops, allocation.location(receiver), 9)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2       // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9              // low-32 Gc offset
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12        // x13 = GcHeader ptr
        ; ldrb w14, [x13]
        ; cmp w14, layout.type_tag as u32
        ; b.ne =>miss
        ; ldr x14, [x13, layout.exotic_byte]
        ; cbnz x14, =>miss         // exotic sidecar: stub owns the semantics
    );
    // Index: an int32, untagged inline when it reaches here tagged.
    match reprs.representation(index) {
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(index), 15)?;
        }
        Representation::Tagged => {
            emit_load_tagged_location(ops, allocation.location(index), 15)?;
            dynasm!(ops
                ; .arch aarch64
                ; lsr x11, x15, #48
                ; movz x12, NUMBER_TAG_HI16
                ; cmp x11, x12
                ; b.ne =>miss      // not an int32 payload
            );
        }
        Representation::Float64 => {
            return Err(Unsupported::OperandShape("dense element float64 index"));
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldr w16, [x13, layout.dense_len_byte]
        ; cmp w15, w16
        ; b.hs =>miss              // unsigned: negative indices miss too
        ; ldr x16, [x13, layout.elements_ptr_byte]
        ; add x16, x16, w15, uxtw #3
    );
    Ok(())
}

fn emit_box_int32(ops: &mut Assembler, value: u8, scratch: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz X(scratch), NUMBER_TAG_HI16, lsl #48
        ; orr X(value), X(value), X(scratch)
    );
}

/// Canonicalize NaN and add the VM's frozen JSC-style encode offset, matching
/// `template::arm64::values::emit_box_double` instruction for instruction.
fn emit_box_double(ops: &mut Assembler, source: u8, destination: u8) {
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fmov X(destination), D(source)
        ; fcmp D(source), D(source)
        ; b.vc =>ready
        ; movz X(destination), CANONICAL_NAN_HI16, lsl #48
        ; =>ready
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(destination), X(destination), x14
    );
}

fn emit_load_i32(ops: &mut Assembler, register: u8, value: i32) {
    emit_load_u32(ops, register, value as u32);
}

fn emit_load_u32(ops: &mut Assembler, register: u8, value: u32) {
    let low = value & 0xffff;
    let high = value >> 16;
    dynasm!(ops
        ; .arch aarch64
        ; movz W(register), low
        ; movk W(register), high, lsl #16
    );
}

fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(register), (value & 0xffff) as u32);
    if (value >> 16) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 16) & 0xffff) as u32, lsl #16
        );
    }
    if (value >> 32) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 32) & 0xffff) as u32, lsl #32
        );
    }
    if (value >> 48) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 48) & 0xffff) as u32, lsl #48
        );
    }
}

fn emit_load_symbolic_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}

fn emit_runtime_entry(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    entry: ResolvedRuntimeEntry,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        register,
        entry.address,
        RelocationTarget::runtime_stub(entry.descriptor),
    );
}

fn emit_prologue(ops: &mut Assembler, spill_frame_bytes: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
        ; stp x19, x20, [sp, #-80]!
        ; stp x21, x22, [sp, #16]
        ; stp x23, x24, [sp, #32]
        ; stp x25, x26, [sp, #48]
        ; stp x27, x28, [sp, #64]
        ; stp d8, d9, [sp, #-64]!
        ; stp d10, d11, [sp, #16]
        ; stp d12, d13, [sp, #32]
        ; stp d14, d15, [sp, #48]
    );
    if spill_frame_bytes != 0 {
        if spill_frame_bytes <= 4095 {
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, spill_frame_bytes);
        } else {
            emit_load_u32(ops, 12, spill_frame_bytes);
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, x12);
        }
    }
}

fn emit_epilogue(ops: &mut Assembler, spill_frame_bytes: u32) {
    if spill_frame_bytes != 0 {
        if spill_frame_bytes <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add sp, sp, spill_frame_bytes);
        } else {
            emit_load_u32(ops, 12, spill_frame_bytes);
            dynasm!(ops ; .arch aarch64 ; add sp, sp, x12);
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldp d14, d15, [sp, #48]
        ; ldp d12, d13, [sp, #32]
        ; ldp d10, d11, [sp, #16]
        ; ldp d8, d9, [sp], #64
        ; ldp x27, x28, [sp, #64]
        ; ldp x25, x26, [sp, #48]
        ; ldp x23, x24, [sp, #32]
        ; ldp x21, x22, [sp, #16]
        ; ldp x19, x20, [sp], #80
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otter_vm::{
        JitFunctionCode,
        jit::JitTestInstruction,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
    };

    use super::*;
    use crate::entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW};

    const STRIDE: u32 = 8;

    /// A spliced unit compiles, and its callee-identity guard deoptimizes when
    /// the runtime callee is not the body the tree spliced. Wrong-callee is the
    /// one splice path executable without a VM: the guard fails before any
    /// callee code runs, so the exit owes only the caller's own frame, and the
    /// interpreter re-runs the call generically from the call PC.
    #[test]
    fn a_spliced_unit_compiles_and_guards_the_callee_identity() {
        use crate::ir::inline::InlineTree;

        // Three params so the harness-supplied callee and argument values are
        // real inputs rather than compiler-seeded undefined.
        let mut view = JitCompileSnapshot::without_feedback(
            7,
            3,
            8,
            vec![
                JitTestInstruction::new(
                    Op::Call,
                    0,
                    0,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::ConstIndex(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(0)]),
            ],
        );
        let callee = JitCompileSnapshot::without_feedback(
            9,
            1,
            4,
            vec![JitTestInstruction::new(
                Op::ReturnValue,
                0,
                0,
                vec![Operand::Register(0)],
            )],
        );
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees.insert(
            call_byte_pc,
            otter_vm::JitInlineCallee {
                code_block: Arc::clone(&callee.code_block),
                function_id: 9,
                param_count: 1,
                register_count: callee.code_block.register_count,
                instructions: callee.instructions,
            },
        );

        let tree = InlineTree::build(&view);
        assert_eq!(tree.frames.len(), 2, "the fixture must splice");
        let code = compile(&view, 91).expect("a spliced unit compiles");

        // A non-cell callee value fails the guard's very first check.
        let (ret, frame, pc) = execute_with_frame(&code, &[box_i32(1), box_i32(5), box_i32(3)]);
        assert_eq!(ret.status, STATUS_BAILED);
        assert_eq!(pc, 0, "the interpreter re-runs the call itself");
        // The caller's registers were written back intact for that re-run.
        assert_eq!(frame[1], box_i32(5));
        assert_eq!(frame[2], box_i32(3));
    }

    fn box_i32(value: i32) -> u64 {
        (0xfffe_u64 << 48) | u64::from(value as u32)
    }

    fn unbox_i32(value: u64) -> i32 {
        value as u32 as i32
    }

    fn box_f64(value: f64) -> u64 {
        otter_vm::Value::number_f64(value).to_bits()
    }

    fn view(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            41,
            param_count,
            register_count,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * STRIDE + 3, operands)
                })
                .collect(),
        );
        for pc in 0..view.instructions.len() {
            if matches!(
                view.instructions[pc].op(&view.code_block),
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Neg
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual
            ) {
                view.seed_arith_feedback_for_test(pc as u32, ArithFeedback::from_bits(ARITH_INT32));
            }
        }
        view
    }

    fn float_view(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
        float_feedback_pcs: &[u32],
        numbers: &[(u32, f64)],
    ) -> JitCompileSnapshot {
        let mut view = view(param_count, register_count, instructions);
        for &pc in float_feedback_pcs {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_FLOAT64));
        }
        for &(pc, number) in numbers {
            view.instructions[pc as usize].load_number = Some(number);
        }
        view
    }

    fn execute(code: &OptimizedCode, args: &[u64]) -> JitRet {
        execute_with_frame(code, args).0
    }

    fn execute_with_frame(code: &OptimizedCode, args: &[u64]) -> (JitRet, Vec<u64>, u32) {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        execute_with_poll_cells(code, args, std::ptr::addr_of!(interrupt), &mut fuel)
    }

    fn execute_with_poll_cells(
        code: &OptimizedCode,
        args: &[u64],
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        // SAFETY: the compiler emitted `JitEntry`, `code` owns the mapping
        // through the call, and the context plus poll cells remain valid until
        // the entry returns.
        let entry = unsafe { code.compiled_code().entry_ptr() };
        let mut frame =
            vec![otter_vm::Value::undefined().to_bits(); code.metadata().register_count as usize];
        frame[..args.len()].copy_from_slice(args);
        execute_at(code, entry, frame, interrupt, fuel)
    }

    fn execute_osr_with_frame(
        code: &OptimizedCode,
        logical_pc: u32,
        frame: Vec<u64>,
    ) -> (JitRet, Vec<u64>, u32) {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        // SAFETY: the code object owns the recorded trampoline while this call
        // executes.
        let entry = unsafe {
            code.osr_entry_ptr_for_test(logical_pc)
                .expect("optimized OSR entry")
        };
        execute_at(code, entry, frame, std::ptr::addr_of!(interrupt), &mut fuel)
    }

    fn execute_at(
        code: &OptimizedCode,
        entry: *const u8,
        mut frame: Vec<u64>,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        assert_eq!(frame.len(), code.metadata().register_count as usize);
        // SAFETY: `entry` is a main entry or OSR trampoline in `code`, whose
        // executable mapping outlives this call.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let metadata = code.metadata();
        let mut native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id: metadata.function_id,
                code_block_id: metadata.function_id,
                pc: 0,
                register_count: metadata.register_count,
                kind: NativeFrameKind::Baseline,
                flags: NativeFrameFlags::empty(),
            },
            frame.as_mut_ptr() as u64,
            otter_vm::Value::undefined(),
            otter_vm::Value::undefined(),
        );
        native_frame.set_materialized_activation(0);
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.current_code_object_id = metadata.code_object_id;
        thread.interrupt_cell = interrupt as u64;
        thread.backedge_fuel_cell = std::ptr::from_mut(fuel) as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            error: &mut error,
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
            global_this_offset: std::ptr::null(),
            sync_reentry_depth: std::ptr::null_mut(),
            sync_reentry_limit: 0,
            native_stack_bytes: std::ptr::null_mut(),
            native_stack_bytes_limit: 0,
            generated_call_depth: std::ptr::null_mut(),
            generated_calls: 0,
            generated_call_deopts: 0,
        };
        let result = entry(&mut ctx);
        (result, frame, native_frame.header.pc)
    }

    fn summation_view() -> JitCompileSnapshot {
        view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    unsafe fn fixture_registers(ctx: *mut JitCtx) -> *mut u64 {
        // SAFETY: every execution fixture publishes a live canonical frame and
        // register window for the complete transition call.
        unsafe { (*(*ctx).native_frame).register_base as *mut u64 }
    }

    extern "C" fn relocating_element_load(
        ctx: *mut JitCtx,
        dst: u64,
        receiver: u64,
        _index: u64,
    ) -> u64 {
        // SAFETY: the execution fixture supplies a live three-or-more-slot
        // register window for the duration of this transition call.
        let regs = unsafe { fixture_registers(ctx) };
        unsafe {
            *regs.add(receiver as usize) = box_i32(5);
            *regs.add(dst as usize) = box_i32(37);
        }
        0
    }

    extern "C" fn relocating_precise_element_load(
        ctx: *mut JitCtx,
        dst: u64,
        receiver: u64,
        index: u64,
    ) -> u64 {
        // SAFETY: this fixture compiles a twelve-slot frame and keeps it live
        // for the transition. Tagged slots model moving-GC rewrites; numeric
        // slots are deliberately poisoned to prove they are never reloaded.
        let regs = unsafe { fixture_registers(ctx) };
        unsafe {
            assert_eq!(*regs.add(receiver as usize), box_i32(99));
            assert_eq!(*regs.add(index as usize), box_i32(0));
            assert_eq!(*regs.add(1), box_i32(20));
            assert_eq!(*regs.add(2), box_i32(30));
            assert_eq!(*regs.add(4), otter_vm::Value::undefined().to_bits());
            assert_eq!(*regs.add(5), otter_vm::Value::undefined().to_bits());
            *regs.add(0) = box_i32(5);
            *regs.add(1) = box_i32(7);
            *regs.add(2) = box_i32(11);
            *regs.add(3) = box_i32(1_000);
            *regs.add(4) = box_i32(1_000);
            *regs.add(5) = box_f64(1_000.0);
            *regs.add(dst as usize) = box_i32(37);
        }
        0
    }

    extern "C" fn throwing_element_load(
        ctx: *mut JitCtx,
        _dst: u64,
        _receiver: u64,
        _index: u64,
    ) -> u64 {
        // SAFETY: the execution fixture owns the live error slot for the
        // complete entry call, matching the production transition contract.
        unsafe {
            *(*ctx).error = Some(otter_vm::VmError::InvalidOperand);
        }
        1
    }

    extern "C" fn relocating_precise_element_store(
        ctx: *mut JitCtx,
        receiver: u64,
        index: u64,
        value: u64,
    ) -> u64 {
        // SAFETY: this fixture owns a seven-slot interpreter window for the
        // transition. Slots 0..=2 model moving-GC rewrites; the numeric index
        // is poisoned to prove optimized code ignores its window contents
        // after the call.
        let regs = unsafe { fixture_registers(ctx) };
        unsafe {
            assert_eq!(*regs.add(receiver as usize), box_i32(99));
            assert_eq!(*regs.add(index as usize), box_i32(0));
            assert_eq!(*regs.add(value as usize), box_i32(20));
            *regs.add(0) = box_i32(5);
            *regs.add(1) = box_i32(7);
            *regs.add(2) = box_i32(11);
            *regs.add(index as usize) = box_i32(1_000);
        }
        0
    }

    extern "C" fn throwing_element_store(
        ctx: *mut JitCtx,
        _receiver: u64,
        _index: u64,
        _value: u64,
    ) -> u64 {
        // SAFETY: the execution fixture owns the live error slot for the
        // complete entry call, matching the production transition contract.
        unsafe {
            *(*ctx).error = Some(otter_vm::VmError::InvalidOperand);
        }
        1
    }

    extern "C" fn successful_construct(
        ctx: *mut JitCtx,
        dst: u64,
        callee: u64,
        argc: u64,
        packed_args: u64,
    ) -> u64 {
        // SAFETY: the fixture supplies a three-slot frame window for the
        // duration of this transition and the emitted ABI passes slot ids.
        let regs = unsafe { fixture_registers(ctx) };
        unsafe {
            assert_eq!(*regs.add(callee as usize), box_i32(99));
            assert_eq!(argc, 1);
            assert_eq!(packed_args & 0xffff, 1);
            assert_eq!(*regs.add(1), box_i32(7));
            *regs.add(dst as usize) = box_i32(37);
        }
        0
    }

    fn element_load_transitions(entry: usize) -> TransitionTable {
        let mut transitions = TransitionTable::resolve();
        transitions.replace_variadic_entry_for_test(STUB_JIT_LOAD_ELEMENT, entry);
        transitions
    }

    fn element_store_transitions(entry: usize) -> TransitionTable {
        let mut transitions = TransitionTable::resolve();
        transitions.replace_variadic_entry_for_test(STUB_JIT_STORE_ELEMENT, entry);
        transitions
    }

    extern "C" fn successful_math_call(
        ctx: *mut JitCtx,
        dst: u64,
        method: u64,
        argument_regs: *const u16,
        argument_count: u64,
    ) -> u64 {
        // SAFETY: the fixture supplies the frame window for the duration of
        // this transition; the emitted ABI passes window slot ids and an
        // interior pointer into the code-owned argument arena.
        let regs = unsafe { fixture_registers(ctx) };
        unsafe {
            assert_eq!(method, 15, "Math.floor's method id");
            assert_eq!(argument_count, 1);
            let argument = *argument_regs;
            assert_eq!(*regs.add(argument as usize), box_i32(7));
            *regs.add(dst as usize) = box_i32(7);
        }
        0
    }

    fn math_call_transitions(entry: usize) -> TransitionTable {
        let mut transitions = TransitionTable::resolve();
        transitions.replace_variadic_entry_for_test(STUB_JIT_MATH_CALL, entry);
        transitions
    }

    #[test]
    fn executes_math_call_through_the_window_transition() {
        let view = view(
            1,
            3,
            vec![
                (
                    Op::MathCall,
                    vec![
                        Operand::Register(1),
                        Operand::ConstIndex(15),
                        Operand::ConstIndex(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let transitions = math_call_transitions(successful_math_call as *const () as usize);
        let code = compile_with_transitions(&view, 141, &transitions)
            .expect("math call is eligible through the window transition");

        let result = execute(&code, &[box_i32(7)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_i32(7));
    }

    #[test]
    fn method_call_accepts_boxed_numeric_arguments() {
        let view = view(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (
                    Op::CallMethodValue,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 142)
            .expect("numeric method arguments are boxed into the transition frame");

        assert_eq!(code.safepoint_count(), 1);
        let record = code.safepoint_record(0).expect("method-call safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![otter_vm::native_abi::TaggedLocation::frame_slot(0)]
        );
        let frame_map = code.frame_map(0).expect("method-call frame map");
        assert_eq!(frame_map.slot_count, 3);
        assert_eq!(frame_map.bitmap_word_count, 1);
        assert_eq!(
            code.frame_map_bitmap_words(),
            &[0b1],
            "the receiver is rooted while the boxed primitive argument is not traced"
        );
    }

    #[test]
    fn method_call_accepts_boxed_float_argument() {
        let view = float_view(
            1,
            3,
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(1), Operand::ConstIndex(0)],
                ),
                (
                    Op::CallMethodValue,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[],
            &[(0, 7.5)],
        );
        let code = compile(&view, 143)
            .expect("float method arguments are boxed into the transition frame");

        assert_eq!(code.safepoint_count(), 1);
        let record = code.safepoint_record(0).expect("method-call safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![otter_vm::native_abi::TaggedLocation::frame_slot(0)]
        );
        assert_eq!(
            code.frame_map_bitmap_words(),
            &[0b1],
            "the receiver is rooted while the boxed float argument is not traced"
        );
    }

    fn construct_transitions(entry: usize) -> TransitionTable {
        let mut transitions = TransitionTable::resolve();
        transitions.replace_variadic_entry_for_test(STUB_JIT_CONSTRUCT, entry);
        transitions
    }

    #[test]
    fn executes_element_load_and_reloads_relocated_tagged_values() {
        let view = view(
            2,
            4,
            vec![
                (
                    Op::LoadElement,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let transitions = element_load_transitions(relocating_element_load as *const () as usize);
        let code = compile_with_transitions(&view, 109, &transitions)
            .expect("element load is optimizing-eligible");
        let result = execute(&code, &[box_i32(99), box_i32(0)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 42);

        assert_eq!(code.safepoint_count(), 1);
        assert_eq!(JitFunctionCode::metadata(&code).safepoint_count, 1);
        let record = code.safepoint_record(0).expect("element-load safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
            ]
        );
        let frame_map = code.frame_map(0).expect("precise element-load frame map");
        assert_eq!(frame_map.slot_count, 4);
        assert_eq!(frame_map.bitmap_word_count, 1);
        assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
    }

    #[test]
    fn precise_element_load_reloads_tagged_spills_but_not_numeric_values() {
        let view = float_view(
            3,
            12,
            vec![
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(4)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(5), Operand::ConstIndex(0)],
                ),
                (
                    Op::LoadElement,
                    vec![
                        Operand::Register(6),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(7),
                        Operand::Register(6),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(8),
                        Operand::Register(7),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(9),
                        Operand::Register(8),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(10),
                        Operand::Register(9),
                        Operand::Register(4),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(11),
                        Operand::Register(10),
                        Operand::Register(5),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(11)]),
            ],
            &[8],
            &[(2, 2.5)],
        );
        let transitions =
            element_load_transitions(relocating_precise_element_load as *const () as usize);
        let code = compile_with_transitions(&view, 111, &transitions)
            .expect("precise element load is optimizing-eligible");
        assert!(
            code.metadata().linear_scan_spill_slot_count > 0,
            "tagged live-across fixture must exercise optimizing spills"
        );

        let result = execute(&code, &[box_i32(99), box_i32(20), box_i32(30)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(66.5));

        let record = code.safepoint_record(0).expect("precise safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
                otter_vm::native_abi::TaggedLocation::frame_slot(2),
            ]
        );
        assert_eq!(code.frame_map_bitmap_words(), &[0b111]);
    }

    #[test]
    fn element_load_nonzero_status_uses_shared_throw_exit() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::LoadElement,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let transitions = element_load_transitions(throwing_element_load as *const () as usize);
        let code = compile_with_transitions(&view, 110, &transitions)
            .expect("throwing element load is optimizing-eligible");
        let result = execute(&code, &[box_i32(99), box_i32(0)]);
        assert_eq!(result.status, STATUS_THREW);
        assert_eq!(result.value, 0);
    }

    #[test]
    fn element_store_reloads_tagged_roots() {
        let view = view(
            3,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::StoreElement,
                    vec![
                        Operand::Register(0),
                        Operand::Register(3),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        let transitions =
            element_store_transitions(relocating_precise_element_store as *const () as usize);
        let code = compile_with_transitions(&view, 112, &transitions)
            .expect("precise element store is optimizing-eligible");
        let result = execute(&code, &[box_i32(99), box_i32(20), box_i32(30)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 23);

        let record = code.safepoint_record(0).expect("element-store safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
                otter_vm::native_abi::TaggedLocation::frame_slot(2),
            ]
        );
        assert_eq!(code.frame_map_bitmap_words(), &[0b111]);
    }

    #[test]
    fn element_store_nonzero_status_uses_shared_throw_exit() {
        let view = view(
            1,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
                (
                    Op::StoreElement,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let transitions = element_store_transitions(throwing_element_store as *const () as usize);
        let code = compile_with_transitions(&view, 113, &transitions)
            .expect("throwing element store is optimizing-eligible");
        let result = execute(&code, &[box_i32(99)]);
        assert_eq!(result.status, STATUS_THREW);
        assert_eq!(result.value, 0);
    }

    #[test]
    fn property_store_roots_tagged_value_dead_after_transition() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::StoreProperty,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
        );
        let code = compile(&view, 114).expect("property store is optimizing-eligible");

        let record = code.safepoint_record(0).expect("property-store safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
            ]
        );
        assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
    }

    #[test]
    fn construct_transition_materializes_args_and_reloads_result() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::New,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let transitions = construct_transitions(successful_construct as *const () as usize);
        let code = compile_with_transitions(&view, 117, &transitions)
            .expect("construct is optimizing-eligible");

        let result = execute(&code, &[box_i32(99), box_i32(7)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_i32(37));

        let record = code.safepoint_record(0).expect("construct safepoint");
        assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
        assert_eq!(record.tagged_locations.len(), 2);
    }

    #[test]
    fn executes_add() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 1).expect("add is eligible");
        let result = execute(&code, &[box_i32(17), box_i32(-5)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 12);
    }

    #[test]
    fn executes_three_operation_expression() {
        let view = view(
            3,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(7)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(3),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        let code = compile(&view, 2).expect("three-op expression is eligible");
        let result = execute(&code, &[box_i32(6), box_i32(4), box_i32(3)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 23);
    }

    #[test]
    fn executes_float_add_mul_chain() {
        let view = float_view(
            3,
            5,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
            &[0, 1],
            &[],
        );
        let code = compile(&view, 101).expect("float add/mul chain is eligible");
        let result = execute(&code, &[box_f64(1.5), box_f64(2.0), box_f64(4.0)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(14.0));
    }

    #[test]
    fn executes_float_division_with_int32_widening() {
        let view = float_view(
            0,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(7)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::Div,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[2],
            &[],
        );
        let code = compile(&view, 102).expect("float division is eligible");
        let result = execute(&code, &[]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(3.5));
    }

    #[test]
    fn executes_float_remainder_with_exact_edge_semantics() {
        let view = float_view(
            2,
            3,
            vec![
                (
                    Op::Rem,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[0],
            &[],
        );
        let code = compile(&view, 116).expect("float remainder is eligible");

        let fractional = execute(&code, &[box_f64(7.5), box_f64(2.0)]);
        assert_eq!(fractional.status, STATUS_RETURNED);
        assert_eq!(fractional.value, box_f64(1.5));

        let negative_zero = execute(&code, &[box_f64(-6.0), box_f64(3.0)]);
        assert_eq!(negative_zero.status, STATUS_RETURNED);
        assert_eq!(negative_zero.value, box_f64(-0.0));

        let zero_divisor = execute(&code, &[box_f64(7.5), box_f64(0.0)]);
        assert_eq!(zero_divisor.status, STATUS_RETURNED);
        assert_eq!(zero_divisor.value, box_f64(f64::NAN));
    }

    #[test]
    fn executes_numeric_negate_and_deopts_int32_zero() {
        let int_view = view(
            1,
            2,
            vec![
                (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let int_code = compile(&int_view, 118).expect("int32 negate is eligible");
        assert_eq!(execute(&int_code, &[box_i32(7)]).value, box_i32(-7));
        let zero = execute(&int_code, &[box_i32(0)]);
        assert_eq!(zero.status, STATUS_BAILED);

        let float_view = float_view(
            1,
            2,
            vec![
                (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
            &[0],
            &[],
        );
        let float_code = compile(&float_view, 119).expect("float negate is eligible");
        assert_eq!(execute(&float_code, &[box_f64(-0.0)]).value, box_f64(0.0));
    }

    #[test]
    fn logical_not_inverts_full_inline_truthiness() {
        let view = view(
            1,
            2,
            vec![
                (
                    Op::LogicalNot,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let code = compile(&view, 120).expect("logical-not is eligible");

        assert_eq!(execute(&code, &[VALUE_TRUE]).value, VALUE_FALSE);
        assert_eq!(execute(&code, &[box_i32(0)]).value, VALUE_TRUE);
        assert_eq!(execute(&code, &[box_f64(f64::NAN)]).value, VALUE_TRUE);
        assert_eq!(execute(&code, &[box_f64(1.5)]).value, VALUE_FALSE);
    }

    #[test]
    fn executes_mixed_tagged_int_and_double_division() {
        let view = float_view(
            2,
            3,
            vec![
                (
                    Op::Div,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[0],
            &[],
        );
        let code = compile(&view, 103).expect("mixed division is eligible");
        let result = execute(&code, &[box_i32(7), box_f64(2.0)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(3.5));
        assert_eq!(
            execute(&code, &[box_f64(1.0), box_f64(0.0)]).value,
            box_f64(f64::INFINITY)
        );
        assert_eq!(
            execute(&code, &[box_f64(-1.0), box_f64(f64::INFINITY)]).value,
            box_f64(-0.0)
        );
        assert_eq!(
            execute(&code, &[box_f64(0.0), box_f64(0.0)]).value,
            box_f64(f64::NAN)
        );
    }

    #[test]
    fn executes_float_compare_branch() {
        let view = float_view(
            2,
            4,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(1)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            &[0],
            &[(2, 11.5), (4, 22.5)],
        );
        let code = compile(&view, 104).expect("float comparison branch is eligible");
        assert_eq!(
            execute(&code, &[box_f64(1.5), box_f64(2.5)]).value,
            box_f64(11.5)
        );
        assert_eq!(
            execute(&code, &[box_f64(3.5), box_f64(2.5)]).value,
            box_f64(22.5)
        );
    }

    #[test]
    fn executes_float_accumulation_loop_with_fp_phi_moves() {
        let view = float_view(
            1,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(4), Operand::ConstIndex(1)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(6), Operand::ConstIndex(2)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(5),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(4), Operand::Register(5)],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Register(4),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(6),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-6)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[7, 8],
            &[(1, -0.0), (3, 1.5), (4, -0.0)],
        );
        let code = compile(&view, 105).expect("float accumulation loop is eligible");
        let result = execute(&code, &[box_i32(5)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(15.0));
    }

    #[test]
    fn float_nan_relational_and_equality_compares_are_false() {
        let view = float_view(
            0,
            4,
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(0), Operand::ConstIndex(0)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(1), Operand::ConstIndex(1)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfTrue,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::Equal,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(2)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(3)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            &[2, 4],
            &[(0, f64::NAN), (1, 1.0), (6, 11.5), (8, 22.5)],
        );
        let code = compile(&view, 106).expect("NaN comparison is eligible");
        let result = execute(&code, &[]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(22.5));
    }

    #[test]
    fn non_number_float_guard_deopts_and_boxes_prior_fp_value() {
        let view = float_view(
            2,
            4,
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            &[1, 2],
            &[(0, 1.5)],
        );
        let code = compile(&view, 107).expect("float deopt fixture is eligible");
        let undefined = otter_vm::Value::undefined().to_bits();
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_f64(2.0), undefined]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 2);
        assert_eq!(frame[0], box_f64(2.0));
        assert_eq!(frame[1], undefined);
        assert_eq!(frame[2], box_f64(3.5));
    }

    #[test]
    fn executes_if_else_with_distinct_values() {
        let view = view(
            2,
            4,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(11)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(22)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let code = compile(&view, 7).expect("if/else is eligible");

        let taken = execute(&code, &[box_i32(3), box_i32(8)]);
        assert_eq!(taken.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(taken.value), 11);

        let fallthrough = execute(&code, &[box_i32(9), box_i32(4)]);
        assert_eq!(fallthrough.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(fallthrough.value), 22);
    }

    #[test]
    fn executes_tagged_phi_with_boxed_int32_edge() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::StoreLocal,
                    vec![Operand::Register(1), Operand::Imm32(2)],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 115).expect("tagged phi is eligible");

        let truthy = execute(&code, &[VALUE_TRUE, box_f64(1.5)]);
        assert_eq!(truthy.status, STATUS_RETURNED);
        assert_eq!(truthy.value, box_i32(0));

        let falsy = execute(&code, &[VALUE_FALSE, box_f64(1.5)]);
        assert_eq!(falsy.status, STATUS_RETURNED);
        assert_eq!(falsy.value, box_f64(1.5));
    }

    #[test]
    fn executes_max_diamond_phi_in_both_orders() {
        let view = view(
            2,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (
                    Op::GreaterThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(3)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        );
        let cfg = ControlFlowGraph::build(&view).expect("diamond CFG");
        let ssa = SsaFunction::build(&view, &cfg).expect("diamond SSA");
        let phi_block =
            ssa.blocks
                .iter()
                .find(|block| {
                    block.phis.iter().any(|value| {
                        matches!(ssa.values[value.0 as usize].def, ValueDef::Phi { .. })
                    })
                })
                .expect("diamond must contain a join phi")
                .id;
        let liveness = Liveness::compute(&ssa, &cfg);
        let tree = InlineTree::trivial(&view);
        let reprs = ReprMap::compute(&tree, &ssa);
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
            .expect("diamond allocation");
        let incoming: Vec<_> = allocation
            .edge_moves
            .iter()
            .filter(|edge| edge.block == phi_block)
            .collect();
        assert_eq!(incoming.len(), 2);
        assert!(
            incoming.iter().any(|edge| !edge.moves.is_empty()),
            "fixture must execute a concrete phi edge move"
        );
        let code = compile(&view, 8).expect("max diamond is eligible");

        let left = execute(&code, &[box_i32(19), box_i32(7)]);
        assert_eq!(left.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(left.value), 19);

        let right = execute(&code, &[box_i32(-4), box_i32(12)]);
        assert_eq!(right.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(right.value), 12);
    }

    #[test]
    fn executes_nested_if() {
        let view = view(
            3,
            6,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(6), Operand::Register(3)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(4)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(5), Operand::Imm32(11)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(5), Operand::Imm32(22)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(5), Operand::Imm32(33)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
            ],
        );
        let code = compile(&view, 9).expect("nested if is eligible");

        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(1), box_i32(2), box_i32(3)]).value),
            11
        );
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(1), box_i32(4), box_i32(3)]).value),
            22
        );
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(5), box_i32(2), box_i32(3)]).value),
            33
        );
    }

    #[test]
    fn non_entry_block_overflow_deopts() {
        let view = view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadTrue, vec![Operand::Register(2)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(2)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
                (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(7)]),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        );
        let code = compile(&view, 10).expect("branch-local overflow is eligible");
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(1)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 4);
        assert_eq!(frame[0], box_i32(1));
        assert_eq!(frame[1], box_i32(0));
        assert_eq!(frame[2], VALUE_TRUE);
        assert_eq!(frame[3], box_i32(i32::MAX));
    }

    #[test]
    fn executes_strict_int32_equality_branch() {
        let view = view(
            2,
            4,
            vec![
                (
                    Op::Equal,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let code = compile(&view, 11).expect("strict equality branch is eligible");
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(6), box_i32(6)]).value),
            1
        );
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(6), box_i32(7)]).value),
            0
        );
    }

    #[test]
    fn executes_forced_spills() {
        let mut instructions = vec![
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(2)]),
            (Op::LoadInt32, vec![Operand::Register(6), Operand::Imm32(3)]),
            (Op::LoadInt32, vec![Operand::Register(7), Operand::Imm32(4)]),
        ];
        for (dst, left, right) in [
            (8, 0, 1),
            (9, 2, 3),
            (10, 4, 5),
            (11, 6, 7),
            (12, 8, 9),
            (13, 10, 11),
            (14, 12, 13),
        ] {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(dst),
                    Operand::Register(left),
                    Operand::Register(right),
                ],
            ));
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(14)]));
        let view = view(4, 15, instructions);
        let code = compile(&view, 3).expect("spill expression is eligible");
        assert!(code.metadata().linear_scan_spill_slot_count > 0);
        assert!(code.metadata().spill_slot_count > 0);
        let result = execute(&code, &[box_i32(10), box_i32(20), box_i32(30), box_i32(40)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 110);
    }

    #[test]
    fn executes_forced_fp_spills() {
        let mut instructions = Vec::new();
        let mut numbers = Vec::new();
        for register in 0_u16..9 {
            instructions.push((
                Op::LoadNumber,
                vec![
                    Operand::Register(register),
                    Operand::ConstIndex(register as u32),
                ],
            ));
            numbers.push((u32::from(register), f64::from(register) + 0.5));
        }
        let mut float_pcs = Vec::new();
        let mut accumulator = 0_u16;
        for right in 1_u16..9 {
            let destination = 8 + right;
            let pc = instructions.len() as u32;
            float_pcs.push(pc);
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(destination),
                    Operand::Register(accumulator),
                    Operand::Register(right),
                ],
            ));
            accumulator = destination;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(accumulator)]));

        let view = float_view(0, 17, instructions, &float_pcs, &numbers);
        let code = compile(&view, 108).expect("FP spill expression is eligible");
        assert!(code.metadata().linear_scan_spill_slot_count > 0);
        assert!(code.metadata().spill_slot_count > 0);
        let result = execute(&code, &[]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(40.5));
    }

    #[test]
    fn parameter_guard_bails_at_first_use_logical_pc() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 4).expect("guarded add is eligible");
        let (result, frame, resume_pc) = execute_with_frame(&code, &[0, box_i32(9)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 0);
        assert_eq!(
            frame,
            vec![0, box_i32(9), otter_vm::Value::undefined().to_bits()]
        );
    }

    #[test]
    fn int32_overflow_bails_at_arithmetic_logical_pc() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 5).expect("overflow-checked add is eligible");
        let (result, frame, resume_pc) =
            execute_with_frame(&code, &[box_i32(i32::MAX), box_i32(1)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 0);
        assert_eq!(
            frame,
            vec![
                box_i32(i32::MAX),
                box_i32(1),
                otter_vm::Value::undefined().to_bits()
            ]
        );
    }

    #[test]
    fn later_parameter_guard_materializes_prior_intermediates() {
        let view = view(
            2,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        );
        let code = compile(&view, 6).expect("later guard leaf is eligible");
        let undefined = otter_vm::Value::undefined().to_bits();
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(5), undefined]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 2);
        assert_eq!(
            frame,
            vec![box_i32(5), undefined, box_i32(7), box_i32(12), undefined]
        );
    }

    #[test]
    fn refuses_unsupported_operation() {
        let view = view(
            1,
            2,
            vec![
                (Op::TypeOf, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        assert!(compile(&view, 6).is_err());
    }

    #[test]
    fn executes_summation_loop_with_header_phis() {
        let view = summation_view();
        let cfg = ControlFlowGraph::build(&view).expect("summation CFG");
        let header = cfg
            .blocks
            .iter()
            .find(|block| block.is_loop_header)
            .expect("summation loop header")
            .id;
        let ssa = SsaFunction::build(&view, &cfg).expect("summation SSA");
        assert!(ssa.blocks[header.0 as usize].phis.len() >= 2);
        let liveness = Liveness::compute(&ssa, &cfg);
        let tree = InlineTree::trivial(&view);
        let reprs = ReprMap::compute(&tree, &ssa);
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
            .expect("summation allocation");
        assert!(
            allocation
                .edge_moves
                .iter()
                .any(|edge| edge.block == header && !edge.moves.is_empty()),
            "fixture must require concrete loop-header phi moves"
        );

        let code = compile(&view, 12).expect("summation loop is eligible");
        for (n, expected) in [(0, 0), (1, 0), (5, 10), (10, 45), (100, 4_950)] {
            let result = execute(&code, &[box_i32(n)]);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(unbox_i32(result.value), expected, "n={n}");
        }
    }

    #[test]
    fn osr_materializes_live_header_phis_from_interpreter_window() {
        let code = compile(&summation_view(), 120).expect("summation loop is eligible");
        let frame = vec![box_i32(10), box_i32(4), box_i32(6), box_i32(1), VALUE_TRUE];
        let (result, _frame, _resume_pc) = execute_osr_with_frame(&code, 3, frame);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 45);
    }

    #[test]
    fn osr_representation_mismatch_bails_with_window_untouched() {
        let code = compile(&summation_view(), 121).expect("summation loop is eligible");
        let frame = vec![
            box_i32(10),
            box_f64(4.5),
            box_i32(6),
            box_i32(1),
            VALUE_TRUE,
        ];
        let (result, after, resume_pc) = execute_osr_with_frame(&code, 3, frame.clone());
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(resume_pc, 3);
        assert_eq!(after, frame);
    }

    #[test]
    fn loop_overflow_deopts_with_reconstructed_mid_loop_frame() {
        let view = view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(i32::MAX - 2)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 13).expect("overflowing loop is eligible");
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(5)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 5);
        assert_eq!(frame[0], box_i32(5));
        assert_eq!(frame[1], box_i32(2));
        assert_eq!(frame[2], box_i32(i32::MAX));
        assert_eq!(frame[3], box_i32(1));
        assert_eq!(frame[4], VALUE_TRUE);
    }

    #[test]
    fn overflow_after_osr_reconstructs_current_loop_frame() {
        let view = view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(i32::MAX - 2)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 122).expect("overflowing loop is eligible");
        let frame = vec![
            box_i32(5),
            box_i32(1),
            box_i32(i32::MAX - 1),
            box_i32(1),
            VALUE_TRUE,
        ];
        let (result, frame, resume_pc) = execute_osr_with_frame(&code, 3, frame);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(resume_pc, 5);
        assert_eq!(frame[0], box_i32(5));
        assert_eq!(frame[1], box_i32(2));
        assert_eq!(frame[2], box_i32(i32::MAX));
        assert_eq!(frame[3], box_i32(1));
        assert_eq!(frame[4], VALUE_TRUE);
    }

    #[test]
    fn executes_nested_loops() {
        let view = view(
            1,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(9), Operand::Register(3)],
                ),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(3)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-11)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let code = compile(&view, 14).expect("nested loops are eligible");
        for n in [0, 1, 5, 20] {
            let result = execute(&code, &[box_i32(n)]);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(unbox_i32(result.value), n, "n={n}");
        }
    }

    #[test]
    fn backedge_interrupt_raises_through_the_poll_stub() {
        // The poll matches the template tier: an interrupt reaches the leaf
        // poll stub, which raises; the loop never deoptimizes for it.
        extern "C" fn raising_poll(_ctx: *mut JitCtx) -> u64 {
            1
        }
        let view = view(
            0,
            2,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(0),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-2)]),
            ],
        );
        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL,
            raising_poll as *const () as usize,
        );
        let code = compile_with_transitions(&view, 15, &transitions)
            .expect("reducible infinite loop is eligible");
        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, _, _) =
            execute_with_poll_cells(&code, &[], std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_THREW);
    }

    #[test]
    fn exhausted_backedge_fuel_refills_through_the_poll_stub() {
        // An exhausted budget refills through the stub and the compiled loop
        // keeps running to its own return — no deopt, no interpreter resume.
        extern "C" fn refilling_poll(ctx: *mut JitCtx) -> u64 {
            // SAFETY: the fixture's thread cell outlives the call.
            unsafe {
                let thread = &*(*ctx).thread;
                let fuel = thread.backedge_fuel_cell as *mut u64;
                *fuel = 1_000_000;
            }
            0
        }
        let view = summation_view();
        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL,
            refilling_poll as *const () as usize,
        );
        let code =
            compile_with_transitions(&view, 16, &transitions).expect("summation loop is eligible");
        let interrupt = 0_u8;
        let mut fuel = 1_u64;
        let (result, _, _) = execute_with_poll_cells(
            &code,
            &[box_i32(5)],
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );

        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 10);
    }

    #[test]
    fn refuses_irreducible_loop() {
        let view = view(
            0,
            1,
            vec![
                (Op::LoadTrue, vec![Operand::Register(0)]),
                (
                    Op::JumpIfTrue,
                    vec![Operand::Imm32(1), Operand::Register(0)],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(-2)]),
            ],
        );
        assert!(compile(&view, 17).is_err());
    }
}

//! AArch64 dynasm backend for the template compiler.
//!
//! # Contents
//! - Code-buffer ownership, per-PC labels, and branch fixups.
//! - Prologue/epilogue establishing the shared compiled-entry ABI.
//! - One emit dispatch per [`TemplateOp`], including inline tagged truthiness.
//! - Schema-owned binding and declaration fast/cold control flow.
//! - Cooperative back-edge interrupt/fuel polling.
//! - [`values`] — tagged encode/decode primitives.
//! - [`arith`] — numeric, comparison, and bitwise emitters.
//! - [`binding`] — typed binding/declaration family emission.
//! - [`context`] — SELF context reads, unchecked context-slot access, and
//!   context allocation.
//!
//! # Invariants
//! - Generated JS entries retain the exact BLR return offset with the caller's
//!   source record. Collecting C helpers and abrupt call exits publish their
//!   canonical PC on their committed cold path before entering the runtime.
//! - Other side exits, runtime transitions and polls publish an active source;
//!   constants, moves, proven forward branches and returns need no publication.
//! - Tagged truthiness decides immediate primitives inline and delegates heap
//!   cells to the total leaf helper without allocating or re-entering JS.
//! - Boxed-double falsiness is decided by exact bit patterns (`+0.0`, `-0.0`,
//!   the canonical NaN); the VM's NaN-purification invariant makes this
//!   complete.
//! - Process-local addresses are materialized through typed relocation
//!   capture. Disabled capture allocates no record storage and emits the same
//!   variable-width instruction sequences.
//! - Global lexical addresses identify permanent cells only; their mutable
//!   value is loaded at execution time and TDZ holes take the exact VM path.
//! - Machine-visible offsets come only from the shared entry-ABI module and
//!   frozen value-tag contracts; no Rust container layout is probed.
//! - `x19` retains the tagged register-window base, `x20` retains `JitCtx`,
//!   and `x21` retains the current `Frame`. All three are callee-saved,
//!   so exact-PC publication and frame-field reads never reload the frame
//!   pointer from the context inside the instruction stream.
//! - A branch may skip general tagged truthiness only when its condition's
//!   nearest write in the same straight-line basic block is a canonical
//!   boolean producer. Any explicit control-flow target breaks that proof.
//! - `LoadThis` checks the TDZ hole only for a snapshot explicitly marked as a
//!   derived constructor; every other entry binding is initialized by frame
//!   setup.
//!
//! # See also
//! - [`super::plan`] — the validated operation stream consumed here.
//! - [`super::code`] — the owner of the finalized mapping.

// dynasm 5 normalizes dynamic AArch64 register operands through `Into<u8>`;
// when our register ids are already `u8`, that macro-generated conversion is
// intentionally redundant and outside the source-level emitter's control.
#![allow(clippy::useless_conversion)]

pub(crate) mod arith;
mod binding;
mod calls;
mod class_ops;
mod class_value;
mod construct;
mod context;
mod delete;
mod exceptions;
mod forward_call;
mod functions;
pub(crate) mod ic_probe;
mod iterators;
mod module_op;
mod primitive_strings;
mod private_access;
mod properties;
mod protocol;
mod scalar;
pub(crate) mod shared_property;
mod spread_call;
mod static_call;
mod structural;
mod super_access;
mod transitions;
mod value_load;
mod value_packet;
pub(crate) mod values;
mod variadic;

use std::collections::{BTreeMap, BTreeSet};

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::JitCompileSnapshot;

use self::arith::{
    emit_add_generic, emit_binary_arith, emit_bitwise_not, emit_coercion_slow_paths, emit_compare,
    emit_fused_numeric_chain, emit_increment, emit_int_bitwise, emit_loose_compare, emit_negate,
    emit_numeric_slow_paths, emit_to_numeric, emit_to_primitive, emit_unsigned_shift_right,
};
use self::values::{emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg};
use super::operation::OperationExits;
use super::{TemplateCode, TemplateOp, TemplatePlan};
use crate::CompiledCode;
use crate::artifact::{
    ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
    relocation::RelocationCapture,
};
use crate::entry::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_SELF_OFFSET,
    NATIVE_FRAME_THIS_OFFSET, NUMBER_TAG_HI16, THREAD_OFFSET, Unsupported, VALUE_FALSE, VALUE_HOLE,
    VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET,
    VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET, reg_offset,
};
use otter_vm::native_abi as abi;

/// Boolean/nullish immediates as 32-bit `dynasm` operands.
const VALUE_TRUE_IMM: u32 = VALUE_TRUE as u32;
const VALUE_FALSE_IMM: u32 = VALUE_FALSE as u32;
const VALUE_NULL_IMM: u32 = VALUE_NULL as u32;
const VALUE_UNDEFINED_IMM: u32 = VALUE_UNDEFINED as u32;

/// Bytes of code between two veneer islands. A conditional branch or
/// `cbz`/`cbnz` reaches ±1 MiB; every exit label a segment names is defined
/// in the island that closes it, so each such branch stays far inside range.
const VENEER_ISLAND_INTERVAL: usize = 256 * 1024;

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    artifact_request: Option<ArtifactRequest>,
    capture_events: bool,
) -> Result<NativeCompileOutput<TemplateCode>, Unsupported> {
    // Near branches are one instruction; only a body whose own control flow
    // spans more than a conditional branch reaches pays for the far form.
    match compile_with_reach(
        view,
        code_object_id,
        transitions,
        artifact_request.clone(),
        capture_events,
        false,
    ) {
        Err(Unsupported::Backend(crate::BackendFailure::Relocation)) => compile_with_reach(
            view,
            code_object_id,
            transitions,
            artifact_request,
            capture_events,
            true,
        ),
        result => result,
    }
}

/// Emit one Template body. `far_branches` lowers every conditional branch to
/// a bytecode label as an inverted local branch around an unconditional `b`
/// (±128 MiB), the veneer V8's arm64 assembler places for out-of-range
/// branches.
fn compile_with_reach(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    artifact_request: Option<ArtifactRequest>,
    capture_events: bool,
    far_branches: bool,
) -> Result<NativeCompileOutput<TemplateCode>, Unsupported> {
    let mut plan = TemplatePlan::build(view)?;
    let call_safepoints = crate::return_sites::template_source_safepoints(&mut plan)?;
    let mut return_sites = Vec::new();
    let mut call_source_exits = Vec::new();
    let mut code_map = artifact_request.as_ref().map(|_| CodeMapCapture::default());
    let mut relocations = RelocationCapture::new(artifact_request.is_some());
    let mut direct_call_events = capture_events.then(|| super::seed_direct_call_events(view));
    let poll_entry = transitions.entry(abi::STUB_JIT_BACKEDGE_POLL);
    let mut coercion_slow_paths = Vec::new();
    let mut numeric_slow_paths = Vec::new();
    let mut shared_property = shared_property::SharedPropertyProbes::default();
    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    // Shared exit labels. Each veneer island defines the labels its segment
    // named as `b` trampolines to the final epilogues and hands the next
    // segment fresh ones, so every branch to an exit stays near.
    let mut type_mismatch_exit = ops.new_dynamic_label();
    let mut identity_guard_exit = ops.new_dynamic_label();
    let mut allocation_miss_exit = ops.new_dynamic_label();
    let mut unsupported_exit = ops.new_dynamic_label();
    let mut runtime_transition_exit = ops.new_dynamic_label();
    let mut backedge_relink_exit = ops.new_dynamic_label();
    // Runtime-transition helpers share this local name; representation,
    // identity, allocation, and unsupported sites select dedicated labels.
    let mut bail = runtime_transition_exit;
    let mut returned = ops.new_dynamic_label();
    let mut committed_throw = ops.new_dynamic_label();
    let mut threw = ops.new_dynamic_label();
    let mut propagate_throw = ops.new_dynamic_label();
    let mut fatal = ops.new_dynamic_label();
    let final_exits = [
        type_mismatch_exit,
        identity_guard_exit,
        allocation_miss_exit,
        unsupported_exit,
        runtime_transition_exit,
        backedge_relink_exit,
        returned,
        committed_throw,
        threw,
        propagate_throw,
        fatal,
    ];
    // A far body names segment-local exits from its first operation, so its
    // first island can define them without touching the final epilogues.
    if far_branches {
        [
            type_mismatch_exit,
            identity_guard_exit,
            allocation_miss_exit,
            unsupported_exit,
            runtime_transition_exit,
            backedge_relink_exit,
            returned,
            committed_throw,
            threw,
            propagate_throw,
            fatal,
        ] = std::array::from_fn(|_| ops.new_dynamic_label());
        bail = runtime_transition_exit;
    }
    let mut island_base = 0usize;
    let labels: BTreeMap<u32, DynamicLabel> = plan
        .instructions
        .iter()
        .map(|instr| (instr.pc, ops.new_dynamic_label()))
        .collect();
    let explicit_targets = plan
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            TemplateOp::Jump { target, .. }
            | TemplateOp::Branch { target, .. }
            | TemplateOp::BranchNullish { target, .. } => Some(target),
            _ => None,
        })
        .collect::<BTreeSet<_>>();

    let mut spliced_functions = BTreeSet::new();

    let shape = crate::arm64::frame::EntryShape::of(
        view,
        code_object_id,
        abi::NativeFrameKind::Baseline,
        !plan.safepoint_records.is_empty(),
    )?;
    let activation_exits = crate::frame::ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    };
    // The tier entry continues a published interpreter frame; the call entry
    // builds this function's record and window and falls through.
    let entry = ops.offset();
    let body = ops.new_dynamic_label();
    crate::arm64::frame::emit_tier_prologue(&mut ops, crate::frame::SpillArea::NONE);
    dynasm!(ops ; .arch aarch64 ; b =>body);
    let call_entry = (!plan.osr_only).then(|| {
        let cold = crate::frame::CallEntryCold::new(&mut ops, shape);
        crate::arm64::frame::emit_call_entry_cold(
            &mut ops,
            &mut relocations,
            transitions,
            view,
            activation_exits,
            shape,
            0,
            cold,
        );
        crate::arm64::frame::emit_call_entry(
            &mut ops,
            &mut relocations,
            view,
            shape,
            crate::frame::SpillArea::NONE,
            cold,
        )
    });
    dynasm!(ops ; .arch aarch64 ; =>body);
    if let Some(code_map) = code_map.as_mut() {
        if let Some(offset) = call_entry {
            code_map.record_call_entry(offset.0);
        }
        code_map.record(CodeRegion::structural(
            "entryPrologue",
            entry.0,
            ops.offset().0,
        ));
    }
    // One logical PC can lower to several operations (an immediate-right
    // operator expands to a constant load plus the register operator). The
    // branch label belongs at the first operation for that PC; a later
    // operation sharing the PC must not redefine it.
    let mut labelled_pcs: BTreeSet<u32> = BTreeSet::new();
    for (operation_index, instr) in plan.instructions.iter().enumerate() {
        if far_branches && ops.offset().0 - island_base >= VENEER_ISLAND_INTERVAL {
            let island_start = ops.offset().0;
            let resume = ops.new_dynamic_label();
            dynasm!(ops ; .arch aarch64 ; b =>resume);
            emit_call_source_exits(&mut ops, &mut call_source_exits);
            emit_numeric_slow_paths(
                &mut ops,
                &mut relocations,
                transitions,
                std::mem::take(&mut numeric_slow_paths),
                threw,
                fatal,
            );
            emit_coercion_slow_paths(
                &mut ops,
                &mut relocations,
                transitions,
                std::mem::take(&mut coercion_slow_paths),
                threw,
                fatal,
            );
            let segment_exits = [
                type_mismatch_exit,
                identity_guard_exit,
                allocation_miss_exit,
                unsupported_exit,
                runtime_transition_exit,
                backedge_relink_exit,
                returned,
                committed_throw,
                threw,
                propagate_throw,
                fatal,
            ];
            for (segment, target) in segment_exits.into_iter().zip(final_exits) {
                dynasm!(ops ; .arch aarch64 ; =>segment ; b =>target);
            }
            [
                type_mismatch_exit,
                identity_guard_exit,
                allocation_miss_exit,
                unsupported_exit,
                runtime_transition_exit,
                backedge_relink_exit,
                returned,
                committed_throw,
                threw,
                propagate_throw,
                fatal,
            ] = std::array::from_fn(|_| ops.new_dynamic_label());
            bail = runtime_transition_exit;
            dynasm!(ops ; .arch aarch64 ; =>resume);
            island_base = ops.offset().0;
            if let Some(code_map) = code_map.as_mut() {
                code_map.record(CodeRegion::structural(
                    "veneerIsland",
                    island_start,
                    island_base,
                ));
            }
        }
        let instruction_start = ops.offset().0;
        if labelled_pcs.insert(instr.pc) {
            let label = labels[&instr.pc];
            dynasm!(ops ; .arch aarch64 ; =>label);
        }
        let canonical_boolean_branch = match instr.op {
            TemplateOp::Branch { condition, .. } => branch_condition_is_canonical_boolean(
                &plan,
                &explicit_targets,
                operation_index,
                condition,
            ),
            _ => false,
        };
        // Pure operations cannot leave native code. Every operation that can
        // exit or cross a runtime boundary publishes its exact logical PC
        // before observable work.
        if operation_requires_pc_stamp(instr.op, canonical_boolean_branch) {
            emit_stamp_pc(&mut ops, instr.pc);
        }
        let exits = OperationExits {
            type_mismatch_exit,
            identity_guard_exit,
            allocation_miss_exit,
            unsupported_exit,
            runtime_transition_exit,
            backedge_relink_exit,
            bail,
            returned,
            committed_throw,
            threw,
            propagate_throw,
            fatal,
        };
        // A JS-call operation publishes no PC up front; its abrupt exits
        // publish the call source on a cold relay instead.
        let exits = if super::operation_is_js_call(instr.op) {
            call_source_exit_labels(
                &mut ops,
                instr.pc,
                call_safepoints[&instr.pc],
                exits,
                &mut call_source_exits,
            )
        } else {
            exits
        };
        emit_operation(
            OperationContext {
                ops: &mut ops,
                relocations: &mut relocations,
                return_sites: &mut return_sites,
                call_safepoint: call_safepoints.get(&instr.pc).copied().unwrap_or(0),
                transitions,
                view,
                plan: &plan,
                spliced_functions: &mut spliced_functions,
                labels: &labels,
                exits,
                poll_entry,
                far_branches,
                numeric_slow_paths: &mut numeric_slow_paths,
                coercion_slow_paths: &mut coercion_slow_paths,
                shared_property: &mut shared_property,
                direct_call_events: &mut direct_call_events,
                code_map: &mut code_map,
                saved_pairs: 0,
            },
            instr,
            canonical_boolean_branch,
        )?;
        if let Some(code_map) = code_map.as_mut() {
            code_map.record(CodeRegion::instruction(
                instruction_start,
                ops.offset().0,
                None,
                None,
                view.code_block.id,
                instr.pc,
                instr.byte_pc,
                Some(u32::try_from(operation_index).unwrap_or(u32::MAX)),
                format!("{:?}", instr.op),
            ));
        }
    }

    if !call_source_exits.is_empty() {
        dynasm!(ops ; .arch aarch64 ; b =>unsupported_exit);
        emit_call_source_exits(&mut ops, &mut call_source_exits);
    }

    // The last segment's exits are the final epilogues' own labels once the
    // island trampolines are in place: alias them here, next to the tail.
    let segment_exits = [
        type_mismatch_exit,
        identity_guard_exit,
        allocation_miss_exit,
        unsupported_exit,
        runtime_transition_exit,
        backedge_relink_exit,
        returned,
        committed_throw,
        threw,
        propagate_throw,
        fatal,
    ];
    if segment_exits != final_exits {
        let skip = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; b =>skip);
        for (segment, target) in segment_exits.into_iter().zip(final_exits) {
            dynasm!(ops ; .arch aarch64 ; =>segment ; b =>target);
        }
        dynasm!(ops ; .arch aarch64 ; =>skip);
    }
    [
        type_mismatch_exit,
        identity_guard_exit,
        allocation_miss_exit,
        unsupported_exit,
        runtime_transition_exit,
        backedge_relink_exit,
        returned,
        committed_throw,
        threw,
        propagate_throw,
        fatal,
    ] = final_exits;
    bail = runtime_transition_exit;

    // Preserve the old end-of-stream exact exit while keeping cold
    // continuations out of line. Each returns to the label immediately after
    // its source operation's inline fast path.
    if !coercion_slow_paths.is_empty() || !numeric_slow_paths.is_empty() {
        let slow_paths_start = ops.offset().0;
        dynasm!(ops ; .arch aarch64 ; b =>unsupported_exit);
        emit_numeric_slow_paths(
            &mut ops,
            &mut relocations,
            transitions,
            numeric_slow_paths,
            threw,
            fatal,
        );
        emit_coercion_slow_paths(
            &mut ops,
            &mut relocations,
            transitions,
            coercion_slow_paths,
            threw,
            fatal,
        );
        if let Some(code_map) = code_map.as_mut() {
            code_map.record(CodeRegion::structural(
                "outlinedSlowPaths",
                slow_paths_start,
                ops.offset().0,
            ));
        }
    }

    // Shared normal-return epilogue. `x0` already carries the boxed value.
    let returned_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>returned
        ; movz x1, abi::NativeResultStatus::Success as u32
    );
    crate::arm64::frame::emit_epilogue(&mut ops, activation_exits, crate::frame::SpillArea::NONE);
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "returnEpilogue",
            returned_start,
            ops.offset().0,
        ));
    }

    // Typed exact-side-exit epilogues. Each payload repeats the frame PC so the
    // VM can assert that machine result and published state are identical.
    let bail_start = ops.offset().0;
    emit_side_exit_epilogue(
        &mut ops,
        activation_exits.side_exit,
        type_mismatch_exit,
        abi::ExitReason::TypeMismatch,
        abi::ExitAction::Recompile,
    );
    emit_side_exit_epilogue(
        &mut ops,
        activation_exits.side_exit,
        identity_guard_exit,
        abi::ExitReason::IdentityGuard,
        abi::ExitAction::Recompile,
    );
    emit_side_exit_epilogue(
        &mut ops,
        activation_exits.side_exit,
        allocation_miss_exit,
        abi::ExitReason::AllocationMiss,
        abi::ExitAction::Resume,
    );
    emit_side_exit_epilogue(
        &mut ops,
        activation_exits.side_exit,
        unsupported_exit,
        abi::ExitReason::UnsupportedOperation,
        abi::ExitAction::Recompile,
    );
    emit_side_exit_epilogue(
        &mut ops,
        activation_exits.side_exit,
        runtime_transition_exit,
        abi::ExitReason::RuntimeTransition,
        abi::ExitAction::Resume,
    );
    emit_side_exit_epilogue(
        &mut ops,
        activation_exits.side_exit,
        backedge_relink_exit,
        abi::ExitReason::Interrupt,
        abi::ExitAction::Resume,
    );
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "bailEpilogue",
            bail_start,
            ops.offset().0,
        ));
    }
    // Pure exceptions carry their exact JavaScript value in x0 and no parked
    // side channel. A materialized Template handler may select its PC; an
    // escaping throw returns the same x0 unchanged.
    let committed_throw_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>committed_throw
        ; mov x1, x0
        ; mov x0, x20
    );
    emit_load_runtime_stub(
        &mut ops,
        &mut relocations,
        16,
        transitions.entry(abi::STUB_JIT_ROUTE_THROW),
        abi::STUB_JIT_ROUTE_THROW,
    );
    // A handler of this function landed the throw: its exception register
    // holds the value and the record names the handler's PC.
    let caught = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, abi::NativeResultStatus::SideExit as u32
        ; b.eq =>caught
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>propagate_throw
        ; b =>fatal
    );
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "committedThrowRouter",
            committed_throw_start,
            ops.offset().0,
        ));
    }
    // A status-word runtime operation parked an error in the context. Finish
    // it once at the compiled-frame boundary, producing the same canonical
    // Bail/Throw/Fatal result domain as every generated callee.
    let threw_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>threw
        ; mov x0, x20
    );
    emit_load_runtime_stub(
        &mut ops,
        &mut relocations,
        16,
        transitions.entry(abi::STUB_JIT_FINISH_ERROR),
        abi::STUB_JIT_FINISH_ERROR,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, abi::NativeResultStatus::SideExit as u32
        ; b.eq =>caught
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>propagate_throw
        ; b =>fatal
    );
    emit_handler_dispatch(&mut ops, view, &labels, caught, bail);
    dynasm!(ops
        ; .arch aarch64
        ; =>propagate_throw
        ; movz x1, abi::NativeResultStatus::Throw as u32
    );
    crate::arm64::frame::emit_epilogue(&mut ops, activation_exits, crate::frame::SpillArea::NONE);
    dynasm!(ops
        ; .arch aarch64
        ; =>fatal
    );
    emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; movz x1, abi::NativeResultStatus::Fatal as u32);
    crate::arm64::frame::emit_epilogue(&mut ops, activation_exits, crate::frame::SpillArea::NONE);
    crate::arm64::frame::emit_exits(
        &mut ops,
        &mut relocations,
        transitions,
        view,
        shape.derived,
        activation_exits,
        crate::frame::SpillArea::NONE,
    );
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "throwEpilogue",
            threw_start,
            ops.offset().0,
        ));
    }

    // OSR trampolines: one per verified loop header. Each runs the standard
    // prologue (establishing the shared entry ABI from the ctx argument) and
    // branches to the header's body label, so the VM can enter mid-loop with
    // the live frame registers.
    let mut osr_entries: BTreeMap<u32, usize> = BTreeMap::new();
    for &header_pc in view.code_block.loop_headers() {
        let Some(&target) = labels.get(&header_pc) else {
            continue;
        };
        let offset = ops.offset().0;
        crate::arm64::frame::emit_tier_prologue(&mut ops, crate::frame::SpillArea::NONE);
        dynasm!(ops ; .arch aarch64 ; b =>target);
        if let Some(code_map) = code_map.as_mut() {
            code_map.record_osr(header_pc, offset, ops.offset().0);
        }
        osr_entries.insert(header_pc, offset);
    }

    let shared_start = ops.offset().0;
    shared_property.emit(&mut ops, &mut relocations, transitions, view);
    if let Some(code_map) = code_map.as_mut()
        && ops.offset().0 != shared_start
    {
        code_map.record(CodeRegion::structural(
            "sharedPropertyProbes",
            shared_start,
            ops.offset().0,
        ));
    }

    let buf = crate::entry::finalize_assembler(ops)?;
    let tier_input = artifact_request.as_ref().map(|_| plan.render_artifact());
    let TemplatePlan {
        register_operands,
        mut safepoint_records,
        osr_only,
        ..
    } = plan;
    safepoint_records.sort_by_key(|record| record.id);
    return_sites.sort_by_key(|site| site.native_return_offset);
    let compiled_code = CompiledCode::new(buf, entry);
    let artifact = artifact_request.map(|request| {
        build_bundle(
            request,
            view,
            code_object_id,
            &compiled_code,
            otter_vm::JitArtifactFileName::TemplatePlan,
            tier_input.expect("requested artifact has tier input"),
            code_map.expect("requested artifact has code map"),
            relocations,
            None,
            &safepoint_records,
            &return_sites,
        )
    });
    let source_work = super::code::retained_source_work(view, &spliced_functions);
    let code = TemplateCode::from_emission(
        compiled_code,
        code_object_id,
        view.code_block.id,
        Box::new([]),
        spliced_functions
            .into_iter()
            .filter(|&fid| fid != view.code_block.id)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        source_work,
        register_operands,
        safepoint_records.into_boxed_slice(),
        return_sites.into_boxed_slice(),
        osr_entries,
        call_entry.map(|offset| offset.0),
        osr_only,
    );
    Ok(NativeCompileOutput {
        code,
        artifact,
        ir_node_count: u64::try_from(plan.instructions.len()).unwrap_or(u64::MAX),
        diagnostics: direct_call_events
            .map(|events| events.into_values().collect::<Vec<_>>().into_boxed_slice())
            .unwrap_or_default(),
    })
}

/// The cold exit relay of one JS-call operation. Each private label names
/// its shared exit target; all of them join one publication of the call
/// source.
struct CallSourceExits {
    labels: [(DynamicLabel, DynamicLabel); 12],
    pc: u32,
    safepoint_id: abi::SafepointId,
}

/// Define the pending relays: each label loads its shared target into `x17`,
/// the join publishes the call's PC and safepoint, then branches to `x17`.
fn emit_call_source_exits(ops: &mut Assembler, records: &mut Vec<CallSourceExits>) {
    for CallSourceExits {
        labels,
        pc,
        safepoint_id,
    } in std::mem::take(records)
    {
        let publish = ops.new_dynamic_label();
        for (label, target) in labels {
            dynasm!(ops ; .arch aarch64 ; =>label ; adr x17, =>target ; b =>publish);
        }
        dynasm!(ops ; .arch aarch64 ; =>publish);
        emit_cold_call_source(ops, pc, safepoint_id);
        dynasm!(ops ; .arch aarch64 ; br x17);
    }
}

/// Route every exit of one JS-call operation through a fresh relay that
/// publishes `(pc, safepoint_id)` before reaching the shared exit.
fn call_source_exit_labels(
    ops: &mut Assembler,
    pc: u32,
    safepoint_id: abi::SafepointId,
    exits: OperationExits,
    records: &mut Vec<CallSourceExits>,
) -> OperationExits {
    let targets = [
        exits.type_mismatch_exit,
        exits.identity_guard_exit,
        exits.allocation_miss_exit,
        exits.unsupported_exit,
        exits.runtime_transition_exit,
        exits.backedge_relink_exit,
        exits.bail,
        exits.returned,
        exits.committed_throw,
        exits.threw,
        exits.propagate_throw,
        exits.fatal,
    ];
    let labels: [DynamicLabel; 12] = std::array::from_fn(|_| ops.new_dynamic_label());
    records.push(CallSourceExits {
        labels: std::array::from_fn(|index| (labels[index], targets[index])),
        pc,
        safepoint_id,
    });
    OperationExits {
        type_mismatch_exit: labels[0],
        identity_guard_exit: labels[1],
        allocation_miss_exit: labels[2],
        unsupported_exit: labels[3],
        runtime_transition_exit: labels[4],
        backedge_relink_exit: labels[5],
        bail: labels[6],
        returned: labels[7],
        committed_throw: labels[8],
        threw: labels[9],
        propagate_throw: labels[10],
        fatal: labels[11],
    }
}

/// Everything one template operation's emission reads or appends to.
pub(crate) struct OperationContext<'c, 'a> {
    pub(crate) ops: &'c mut Assembler,
    pub(crate) relocations: &'c mut RelocationCapture,
    pub(crate) return_sites: &'c mut Vec<abi::SafepointEntry>,
    pub(crate) call_safepoint: abi::SafepointId,
    pub(crate) transitions: &'a crate::entry::TransitionTable,
    pub(crate) view: &'a JitCompileSnapshot,
    pub(crate) plan: &'c TemplatePlan,
    pub(crate) spliced_functions: &'c mut BTreeSet<u32>,
    /// Branch targets by logical PC; control operations only.
    pub(crate) labels: &'c BTreeMap<u32, DynamicLabel>,
    pub(crate) exits: OperationExits,
    pub(crate) poll_entry: u64,
    pub(crate) far_branches: bool,
    pub(crate) numeric_slow_paths: &'c mut Vec<arith::NumericSlowPath>,
    pub(crate) coercion_slow_paths: &'c mut Vec<arith::CoercionSlowPath>,
    pub(crate) shared_property: &'c mut shared_property::SharedPropertyProbes,
    pub(crate) direct_call_events: &'c mut Option<super::DirectCallEvents>,
    pub(crate) code_map: &'c mut Option<CodeMapCapture>,
    /// Callee-saved pairs the enclosing body saved; a tail transfer that
    /// releases the frame restores them.
    pub(crate) saved_pairs: u8,
}

/// Emit one planned template operation over the register window.
///
/// The caller has bound the operation's label and stamped its PC when the
/// operation requires one.
pub(crate) fn emit_operation<'a>(
    context: OperationContext<'_, 'a>,
    instr: &super::plan::TemplateInstr,
    canonical_boolean_branch: bool,
) -> Result<(), Unsupported> {
    let OperationContext {
        ops,
        relocations,
        return_sites,
        call_safepoint,
        transitions,
        view,
        plan,
        spliced_functions,
        labels,
        exits,
        poll_entry,
        far_branches,
        numeric_slow_paths,
        coercion_slow_paths,
        shared_property,
        direct_call_events,
        code_map,
        saved_pairs,
    } = context;
    let mut call_source = crate::return_sites::ReturnSiteRecorder {
        entries: return_sites,
        safepoint_id: call_safepoint,
        logical_pc: instr.pc,
    };
    let return_sites = &mut call_source;
    super::mark_direct_call_site_reached(direct_call_events.as_mut(), instr.byte_pc);
    let OperationExits {
        type_mismatch_exit,
        identity_guard_exit,
        allocation_miss_exit,
        unsupported_exit,
        runtime_transition_exit,
        backedge_relink_exit,
        bail,
        returned,
        committed_throw,
        threw,
        propagate_throw,
        fatal,
    } = exits;
    let _ = (propagate_throw, far_branches);

    match instr.op {
        TemplateOp::LoadImmediate { dst, bits } => {
            emit_load_u64(ops, 9, bits);
            emit_store_reg(ops, 9, dst)?;
        }
        TemplateOp::Move { dst, src } => {
            emit_load_reg(ops, 9, src)?;
            emit_store_reg(ops, 9, dst)?;
        }
        TemplateOp::Jump { target, back_edge } => {
            let tgt = labels[&target];
            if back_edge {
                emit_backedge_poll(
                    ops,
                    relocations,
                    poll_entry,
                    target,
                    backedge_relink_exit,
                    threw,
                    fatal,
                );
            }
            dynasm!(ops ; .arch aarch64 ; b =>tgt);
        }
        TemplateOp::Branch {
            condition,
            target,
            when_truthy,
            back_edge,
        } => {
            let tgt = labels[&target];
            emit_load_reg(ops, 9, condition)?;
            if !canonical_boolean_branch {
                emit_truthiness_bool(ops, relocations, type_mismatch_exit);
            }
            dynasm!(ops ; .arch aarch64 ; cmp x9, VALUE_TRUE_IMM);
            if back_edge {
                let taken = ops.new_dynamic_label();
                let fallthrough = ops.new_dynamic_label();
                if when_truthy {
                    dynasm!(ops ; .arch aarch64 ; b.eq =>taken);
                } else {
                    dynasm!(ops ; .arch aarch64 ; b.ne =>taken);
                }
                dynasm!(ops ; .arch aarch64 ; b =>fallthrough ; =>taken);
                emit_backedge_poll(
                    ops,
                    relocations,
                    poll_entry,
                    target,
                    backedge_relink_exit,
                    threw,
                    fatal,
                );
                dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
            } else if far_branches {
                let fallthrough = ops.new_dynamic_label();
                if when_truthy {
                    dynasm!(ops ; .arch aarch64 ; b.ne =>fallthrough);
                } else {
                    dynasm!(ops ; .arch aarch64 ; b.eq =>fallthrough);
                }
                dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
            } else if when_truthy {
                dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
            } else {
                dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
            }
        }
        TemplateOp::BranchNullish {
            condition,
            target,
            back_edge,
        } => {
            let tgt = labels[&target];
            emit_load_reg(ops, 9, condition)?;
            let taken = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; cmp x9, VALUE_NULL_IMM
                ; b.eq =>taken
                ; cmp x9, VALUE_UNDEFINED_IMM
                ; b.eq =>taken
            );
            let fallthrough = ops.new_dynamic_label();
            dynasm!(ops ; .arch aarch64 ; b =>fallthrough ; =>taken);
            if back_edge {
                emit_backedge_poll(
                    ops,
                    relocations,
                    poll_entry,
                    target,
                    backedge_relink_exit,
                    threw,
                    fatal,
                );
            }
            dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
        }
        TemplateOp::Truthiness { dst, src, negate } => {
            emit_load_reg(ops, 9, src)?;
            emit_truthiness_bool(ops, relocations, type_mismatch_exit);
            if negate {
                // VALUE_TRUE and VALUE_FALSE differ exactly in bit 0.
                dynasm!(ops ; .arch aarch64 ; eor x9, x9, #1);
            }
            emit_store_reg(ops, 9, dst)?;
        }
        TemplateOp::FusedNumericChain {
            steps,
            leaves,
            jump_target,
        } => {
            emit_fused_numeric_chain(
                ops,
                plan.chain_step_tail(steps),
                plan.chain_leaf_tail(leaves),
                labels[&jump_target],
            )?;
        }
        TemplateOp::BinaryArith {
            dst,
            lhs,
            rhs,
            kind,
        } => {
            emit_binary_arith(
                ops,
                relocations,
                dst,
                lhs,
                rhs,
                kind,
                arith::ArithSite::of(view, instr.pc),
                numeric_slow_paths,
            )?;
        }
        TemplateOp::Compare {
            dst,
            lhs,
            rhs,
            kind,
        } => {
            emit_compare(
                ops,
                relocations,
                dst,
                lhs,
                rhs,
                kind,
                arith::ArithSite::of(view, instr.pc),
                type_mismatch_exit,
                fatal,
                numeric_slow_paths,
            )?;
        }
        TemplateOp::TestTypeOf { dst, src, test } => {
            arith::emit_test_typeof(ops, relocations, view, dst, src, test, type_mismatch_exit)?;
        }
        TemplateOp::LooseCompare {
            dst,
            lhs,
            rhs,
            negate,
        } => {
            emit_loose_compare(
                ops,
                relocations,
                transitions,
                view,
                dst,
                lhs,
                rhs,
                negate,
                type_mismatch_exit,
                threw,
                fatal,
            )?;
        }
        TemplateOp::IntBitwise {
            dst,
            lhs,
            rhs,
            kind,
        } => {
            emit_int_bitwise(ops, dst, lhs, rhs, kind, numeric_slow_paths)?;
        }
        TemplateOp::UnsignedShiftRight { dst, lhs, rhs } => {
            emit_unsigned_shift_right(
                ops,
                relocations,
                dst,
                lhs,
                rhs,
                arith::ArithSite::of(view, instr.pc),
                numeric_slow_paths,
            )?;
        }
        TemplateOp::Increment { dst, src, delta } => {
            emit_increment(
                ops,
                relocations,
                dst,
                src,
                delta,
                arith::ArithSite::of(view, instr.pc),
                numeric_slow_paths,
            )?;
        }
        TemplateOp::Negate { dst, src } => {
            emit_negate(
                ops,
                relocations,
                dst,
                src,
                arith::ArithSite::of(view, instr.pc),
                numeric_slow_paths,
            )?;
        }
        TemplateOp::BitwiseNot { dst, src } => {
            emit_bitwise_not(ops, dst, src, numeric_slow_paths)?;
        }
        TemplateOp::ToNumeric { dst, src } => {
            emit_to_numeric(ops, dst, src, coercion_slow_paths)?;
        }
        TemplateOp::ToPrimitive { dst, src, hint } => {
            emit_to_primitive(ops, dst, src, hint, coercion_slow_paths)?;
        }
        TemplateOp::AddGeneric {
            dst,
            lhs,
            rhs,
            concat_safepoint,
        } => {
            emit_add_generic(
                ops,
                relocations,
                shared_property,
                transitions,
                dst,
                lhs,
                rhs,
                concat_safepoint,
                arith::ArithSite::of(view, instr.pc),
                threw,
                fatal,
            )?;
        }
        TemplateOp::LoadThis { dst } => {
            dynasm!(ops ; .arch aarch64 ; ldr x9, [x21, NATIVE_FRAME_THIS_OFFSET]);
            if view.derived_constructor {
                emit_load_u64(ops, 12, VALUE_HOLE);
                // A derived-ctor `this`-before-`super` hole resolves in the
                // interpreter.
                dynasm!(ops ; .arch aarch64 ; cmp x9, x12 ; b.eq =>runtime_transition_exit);
            }
            emit_store_reg(ops, 9, dst)?;
        }
        TemplateOp::LoadSelfClosure { dst } => {
            dynasm!(ops ; .arch aarch64 ; ldr x9, [x21, NATIVE_FRAME_SELF_OFFSET]);
            emit_store_reg(ops, 9, dst)?;
        }
        TemplateOp::LoadClosureContext { dst } => {
            context::emit_load_closure_context(ops, view, dst)?;
        }
        TemplateOp::LoadContextSlot {
            dst,
            context,
            depth,
            slot,
        } => {
            context::emit_load_context_slot(ops, view, dst, context, depth, slot)?;
        }
        TemplateOp::StoreContextSlot {
            src,
            context,
            depth,
            slot,
        } => {
            context::emit_store_context_slot(ops, relocations, view, src, context, depth, slot)?;
        }
        TemplateOp::CreateContext {
            dst,
            parent,
            scope,
            safepoint,
        } => {
            context::emit_context_allocation(
                ops,
                relocations,
                view,
                instr.pc,
                instr.byte_pc,
                dst,
                context::ContextAllocation::Create { parent, scope },
                safepoint,
                allocation_miss_exit,
                fatal,
            )?;
        }
        TemplateOp::CopyContext {
            dst,
            src,
            safepoint,
        } => {
            context::emit_context_allocation(
                ops,
                relocations,
                view,
                instr.pc,
                instr.byte_pc,
                dst,
                context::ContextAllocation::Copy { source: src },
                safepoint,
                allocation_miss_exit,
                fatal,
            )?;
        }
        TemplateOp::ClassSuperConstructor { dst, class } => {
            emit_load_reg(ops, 1, class)?;
            dynasm!(ops ; .arch aarch64 ; mov x0, x20);
            emit_load_runtime_stub(
                ops,
                relocations,
                16,
                transitions.variadic_entry(abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR),
                abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR,
            );
            dynasm!(ops ; .arch aarch64 ; blr x16);
            emit_load_u64(ops, 16, VALUE_HOLE);
            dynasm!(ops ; .arch aarch64 ; cmp x0, x16 ; b.eq =>runtime_transition_exit);
            emit_store_reg(ops, 0, dst)?;
        }
        TemplateOp::MakeFunction { dst, safepoint } => {
            context::emit_context_allocation(
                ops,
                relocations,
                view,
                instr.pc,
                instr.byte_pc,
                dst,
                context::ContextAllocation::Function,
                safepoint,
                allocation_miss_exit,
                fatal,
            )?;
        }
        TemplateOp::MakeClosure {
            dst,
            context: closure_context,
            safepoint,
        } => {
            context::emit_context_allocation(
                ops,
                relocations,
                view,
                instr.pc,
                instr.byte_pc,
                dst,
                context::ContextAllocation::Closure {
                    context: closure_context,
                },
                safepoint,
                allocation_miss_exit,
                fatal,
            )?;
        }
        TemplateOp::LoadRegExp { dst, constant } => {
            transitions::emit_load_regexp(
                ops,
                relocations,
                transitions,
                dst,
                constant,
                threw,
                fatal,
            );
        }
        TemplateOp::BindingValue {
            semantics,
            result,
            value0,
            value1,
            context_coord,
        } => {
            binding::emit_binding_value(
                ops,
                relocations,
                transitions,
                view,
                semantics,
                result,
                value0,
                value1,
                context_coord,
                instr.byte_pc,
                code_map.as_mut(),
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::GlobalDeclarationValue {
            semantics,
            value0,
            value1,
        } => {
            binding::emit_global_declaration_value(
                ops,
                relocations,
                transitions,
                semantics,
                value0,
                value1,
                instr.byte_pc,
                code_map.as_mut(),
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::LoadBuiltinError { dst, constant } => {
            transitions::emit_load_builtin_error(
                ops,
                relocations,
                transitions,
                dst,
                constant,
                threw,
                fatal,
            );
        }
        TemplateOp::NewObject { dst } => {
            transitions::emit_new_object(
                ops,
                relocations,
                transitions,
                dst,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::CollectArguments { dst } => {
            transitions::emit_collect_arguments(ops, relocations, transitions, dst, threw, fatal);
        }
        TemplateOp::CallForwardArguments {
            dst,
            method,
            receiver,
            this_value,
        } => {
            forward_call::emit_forward_call(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                code_map.as_mut(),
                instr.pc,
                [dst, method, receiver, this_value],
                threw,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::NewArray { dst, elements } => {
            transitions::emit_new_array(
                ops,
                relocations,
                transitions,
                dst,
                plan.register_tail(elements),
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::NewObjectLiteral { dst, elements } => {
            transitions::emit_new_object_literal(
                ops,
                relocations,
                transitions,
                dst,
                plan.register_tail(elements),
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::DefineDataProperty { object, key, value } => {
            transitions::emit_define_data_property(
                ops,
                relocations,
                transitions,
                object,
                key,
                value,
                threw,
                fatal,
            );
        }
        TemplateOp::DefineOwnProperty {
            target,
            key,
            descriptor,
        } => {
            transitions::emit_define_own_property(
                ops,
                relocations,
                transitions,
                target,
                key,
                descriptor,
                threw,
                fatal,
            );
        }
        TemplateOp::LoadElement {
            dst,
            receiver,
            index,
        } => {
            transitions::emit_load_element(
                ops,
                relocations,
                transitions,
                view,
                dst,
                receiver,
                index,
                instr.byte_pc,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::StoreElement {
            receiver,
            index,
            value,
        } => {
            transitions::emit_store_element(
                ops,
                relocations,
                transitions,
                view,
                receiver,
                index,
                value,
                instr.byte_pc,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::LoadProperty {
            dst,
            object,
            array_length,
            ..
        } => {
            properties::emit_load_property(
                ops,
                relocations,
                shared_property,
                view,
                dst,
                object,
                instr.byte_pc,
                array_length,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::StoreProperty { object, value, .. } => {
            properties::emit_store_property(
                ops,
                relocations,
                shared_property,
                view,
                object,
                value,
                instr.byte_pc,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::Call {
            dst,
            callee,
            argc,
            packed_args,
            byte_pc,
        } => {
            let argument_registers = plan.call_argument_registers(argc, packed_args);
            calls::emit_call(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                spliced_functions,
                direct_call_events.as_mut(),
                code_map.as_mut(),
                dst,
                callee,
                argc,
                &argument_registers,
                instr.pc,
                byte_pc,
                identity_guard_exit,
                threw,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::TailCall {
            dst,
            callee,
            argc,
            packed_args,
            byte_pc,
        } => {
            let argument_registers = plan.call_argument_registers(argc, packed_args);
            calls::emit_tail_call(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                spliced_functions,
                direct_call_events.as_mut(),
                code_map.as_mut(),
                dst,
                callee,
                argc,
                &argument_registers,
                instr.pc,
                byte_pc,
                bail,
                identity_guard_exit,
                threw,
                committed_throw,
                fatal,
                saved_pairs,
            )?;
        }
        TemplateOp::CallWithThis {
            dst,
            callee,
            this_value,
            argc,
            packed_args,
            byte_pc,
        } => {
            let argument_registers = plan.call_argument_registers(argc, packed_args);
            calls::emit_call_with_receiver(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                spliced_functions,
                direct_call_events.as_mut(),
                code_map.as_mut(),
                dst,
                callee,
                Some(this_value),
                argc,
                &argument_registers,
                instr.pc,
                byte_pc,
                identity_guard_exit,
                threw,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::Construct {
            dst,
            callee,
            argc,
            packed_args,
            super_construct,
            byte_pc,
        } => {
            let argument_registers = plan.call_argument_registers(argc, packed_args);
            calls::emit_construct(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                direct_call_events.as_mut(),
                code_map.as_mut(),
                dst,
                callee,
                &argument_registers,
                super_construct,
                instr.pc,
                byte_pc,
                threw,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::MethodCall {
            dst,
            receiver,
            arguments,
            byte_pc,
            arg0,
            arg1,
        } => {
            let argument_registers = plan.register_tail(arguments);
            calls::emit_method_call(
                ops,
                relocations,
                transitions,
                return_sites,
                shared_property,
                view,
                spliced_functions,
                direct_call_events.as_mut(),
                code_map.as_mut(),
                dst,
                receiver,
                argument_registers,
                instr.pc,
                byte_pc,
                arg0,
                arg1,
                identity_guard_exit,
                threw,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::Throw { src } => {
            scalar::emit_scalar_value(
                ops,
                relocations,
                transitions,
                view,
                otter_vm::native_abi::ScalarValueOp::PrepareThrow,
                src,
                Some(src),
                None,
                committed_throw,
                fatal,
            )?;
            values::emit_load_reg(ops, 0, src)?;
            dynasm!(ops ; .arch aarch64 ; b =>committed_throw);
        }
        TemplateOp::TdzError { local_index } => {
            exceptions::emit_exception_op(
                ops,
                relocations,
                transitions,
                Op::TdzError as u8,
                u64::from(local_index),
                bail,
                propagate_throw,
                fatal,
            );
        }
        TemplateOp::IteratorNext {
            value_dst,
            done_dst,
            iterator,
        } => {
            iterators::emit_iterator_next(
                ops,
                relocations,
                transitions,
                view,
                instr.byte_pc,
                value_dst,
                done_dst,
                iterator,
                bail,
                threw,
                fatal,
            )?;
        }
        TemplateOp::IteratorClose { iterator } => {
            iterators::emit_iterator_close(
                ops,
                relocations,
                transitions,
                view,
                Op::IteratorClose,
                iterator,
                bail,
                threw,
                fatal,
            )?;
        }
        TemplateOp::IteratorCloseThrow { iterator } => {
            iterators::emit_iterator_close(
                ops,
                relocations,
                transitions,
                view,
                Op::IteratorCloseThrow,
                iterator,
                bail,
                threw,
                fatal,
            )?;
        }
        TemplateOp::BindFunction {
            dst,
            callee,
            bound_this,
            argc,
            packed_args,
        } => {
            let packed_meta = u64::from(dst)
                | (u64::from(callee) << 16)
                | (u64::from(bound_this) << 32)
                | (u64::from(argc) << 48);
            functions::emit_bind_function(
                ops,
                relocations,
                transitions,
                packed_meta,
                packed_args,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ObjectProtocolValue {
            operation,
            result,
            value0,
            value1,
        } => {
            protocol::emit_object_protocol_value(
                ops,
                relocations,
                transitions,
                operation,
                result,
                value0,
                value1,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::DeleteOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            delete::emit_delete_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ScalarValue {
            operation,
            result,
            value0,
            value1,
        } => {
            scalar::emit_scalar_value(
                ops,
                relocations,
                transitions,
                view,
                operation,
                result,
                value0,
                value1,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::LoadLiteral { dst } => {
            scalar::emit_literal(ops, relocations, view, instr.byte_pc, dst)?;
        }
        TemplateOp::SuperOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            super_access::emit_super_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::PrivateOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            private_access::emit_private_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ValueLoadOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            value_load::emit_value_load_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ConstructOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            construct::emit_construct_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::StructuralOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            structural::emit_structural_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ClassOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            class_ops::emit_class_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ArrayConstruct {
            dst,
            length,
            safepoint,
        } => {
            transitions::emit_array_construct_alloc_call(
                ops,
                relocations,
                dst,
                length,
                safepoint,
                allocation_miss_exit,
            )?;
        }
        TemplateOp::VariadicOp {
            opcode,
            prefix,
            argc,
            packed_args,
        } => {
            variadic::emit_variadic_op(
                ops,
                relocations,
                transitions,
                opcode,
                prefix,
                argc,
                packed_args,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::StaticCallOp {
            opcode,
            packed_head,
            method,
            packed_args,
        } => {
            static_call::emit_static_call_op(
                ops,
                relocations,
                transitions,
                opcode,
                packed_head,
                method,
                packed_args,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::SpreadCallOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            spread_call::emit_spread_call_op(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                code_map.as_mut(),
                opcode,
                arg0,
                arg1,
                arg2,
                instr.pc,
                instr.byte_pc,
                threw,
                committed_throw,
                fatal,
            )?;
        }
        TemplateOp::ClassValueOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            class_value::emit_class_value_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::ModuleOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => {
            module_op::emit_module_op(
                ops,
                relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::NoOp => {}
        TemplateOp::GetIterator { dst, src } => {
            iterators::emit_get_iterator(
                ops,
                relocations,
                transitions,
                view,
                dst,
                src,
                bail,
                threw,
                fatal,
            )?;
        }
        TemplateOp::SpreadAppend { array, iterable } => {
            iterators::emit_iterator_op(
                ops,
                relocations,
                transitions,
                Op::SpreadAppend as u8,
                u64::from(array),
                u64::from(iterable),
                0,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::GetAsyncIterator { dst, src } => {
            iterators::emit_iterator_op(
                ops,
                relocations,
                transitions,
                Op::GetAsyncIterator as u8,
                u64::from(dst),
                u64::from(src),
                0,
                bail,
                threw,
                fatal,
            );
        }
        TemplateOp::Return { src } => {
            let off = reg_offset(src)?;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x0, [x19, off]
                ; b =>returned
            );
        }
        TemplateOp::ReturnUndefined => {
            emit_load_u64(ops, 0, VALUE_UNDEFINED);
            dynasm!(ops ; .arch aarch64 ; b =>returned);
        }
        TemplateOp::ReturnDerived {
            value,
            context,
            depth,
            slot,
        } => {
            // Only `undefined` over a bound receiver completes here; the
            // receiver is the construct result. Every other shape re-runs
            // `ReturnDerived` in the interpreter before any effect. Template
            // never compiles a return that crosses a `finally`, so reading
            // the `DerivedThis` slot now equals reading it at frame pop.
            emit_load_reg(ops, 9, value)?;
            emit_load_u64(ops, 10, VALUE_UNDEFINED);
            dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>runtime_transition_exit);
            context::emit_read_context_slot(ops, view, 0, context, depth, slot)?;
            emit_load_u64(ops, 10, VALUE_HOLE);
            dynasm!(ops
                ; .arch aarch64
                ; cmp x0, x10
                ; b.eq =>runtime_transition_exit
                ; b =>returned
            );
        }
        TemplateOp::UnsupportedBail => {
            dynasm!(ops ; .arch aarch64 ; b =>unsupported_exit);
        }
    }
    Ok(())
}

/// Continue at the handler a routed throw landed in. The router wrote the
/// exception into the handler's register and the handler's PC into the
/// published record; this body has a label at every handler target, so the
/// throw never leaves compiled code. A PC without a label leaves through
/// `resume`, which continues the interpreter there.
fn emit_handler_dispatch(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    labels: &BTreeMap<u32, DynamicLabel>,
    caught: DynamicLabel,
    resume: DynamicLabel,
) {
    let targets: BTreeSet<u32> = view
        .code_block
        .control_flow()
        .handlers()
        .iter()
        .map(|handler| handler.target)
        .collect();
    dynasm!(ops ; .arch aarch64 ; =>caught ; ldr w9, [x21, NATIVE_FRAME_PC_OFFSET]);
    for target in targets {
        let Some(&label) = labels.get(&target) else {
            continue;
        };
        let next = ops.new_dynamic_label();
        if target <= 4095 {
            dynasm!(ops ; .arch aarch64 ; cmp w9, target);
        } else {
            emit_load_u64(ops, 10, u64::from(target));
            dynasm!(ops ; .arch aarch64 ; cmp w9, w10);
        }
        // Handlers sit anywhere in the body: `b` reaches every label.
        dynasm!(ops ; .arch aarch64 ; b.ne =>next ; b =>label ; =>next);
    }
    dynasm!(ops ; .arch aarch64 ; b =>resume);
}

/// Whether an operation can leave native code and therefore needs an exact
/// resume PC published before it starts.
fn operation_requires_pc_stamp(op: TemplateOp, canonical_boolean_branch: bool) -> bool {
    if super::operation_is_js_call(op) {
        return false;
    }
    match op {
        TemplateOp::LoadImmediate { .. }
        | TemplateOp::Move { .. }
        | TemplateOp::LoadSelfClosure { .. }
        // Unchecked context accesses have no exit; the store's write-barrier
        // slow path is a frame-free leaf.
        | TemplateOp::LoadClosureContext { .. }
        | TemplateOp::LoadContextSlot { .. }
        | TemplateOp::StoreContextSlot { .. }
        // The fused chain performs no observable effect on its fast path — only
        // register-window stores and a branch — and its per-operation fallback
        // stamps each PC itself; the success target stamps its own on arrival.
        | TemplateOp::FusedNumericChain { .. }
        | TemplateOp::Return { .. }
        | TemplateOp::ReturnUndefined
        | TemplateOp::Jump {
            back_edge: false, ..
        }
        | TemplateOp::BranchNullish {
            back_edge: false, ..
        } => false,
        TemplateOp::Branch {
            back_edge: false, ..
        } if canonical_boolean_branch => false,
        _ => true,
    }
}

/// Whether `condition` is still the canonical boolean produced in this basic
/// block.
///
/// The narrow backward walk deliberately crosses only moves to other
/// registers. This captures compare-result bookkeeping without becoming a
/// second data-flow framework, while an explicit branch target prevents a
/// fallthrough-only proof from leaking across CFG joins or OSR headers.
fn branch_condition_is_canonical_boolean(
    plan: &TemplatePlan,
    explicit_targets: &BTreeSet<u32>,
    operation_index: usize,
    condition: u16,
) -> bool {
    if plan.instructions.iter().any(|instruction| {
        matches!(
            instruction.op,
            TemplateOp::Throw { .. } | TemplateOp::TdzError { .. }
        )
    }) {
        return false;
    }
    let mut index = operation_index;
    while index != 0 {
        if explicit_targets.contains(&plan.instructions[index].pc) {
            return false;
        }
        index -= 1;
        match plan.instructions[index].op {
            TemplateOp::Compare { dst, .. }
            | TemplateOp::LooseCompare { dst, .. }
            | TemplateOp::TestTypeOf { dst, .. }
            | TemplateOp::Truthiness { dst, .. }
                if dst == condition =>
            {
                return true;
            }
            TemplateOp::Move { dst, .. } if dst != condition => {}
            _ => return false,
        }
    }
    false
}

fn emit_side_exit_epilogue(
    ops: &mut Assembler,
    side_exit: DynamicLabel,
    label: DynamicLabel,
    reason: abi::ExitReason,
    action: abi::ExitAction,
) {
    let kind = abi::SideExit::new(0, reason, action).to_bits();
    dynasm!(ops
        ; .arch aarch64
        ; =>label
        ; ldr w0, [x21, NATIVE_FRAME_PC_OFFSET]
    );
    emit_load_u64(ops, 16, kind);
    dynasm!(ops
        ; .arch aarch64
        ; orr x0, x0, x16
        ; b =>side_exit
    );
}

/// Publish only before a committed collecting helper or an abrupt call exit.
fn emit_cold_call_source(ops: &mut Assembler, pc: u32, safepoint_id: abi::SafepointId) {
    emit_load_u64(ops, 16, u64::from(pc));
    dynasm!(ops ; .arch aarch64 ; str w16, [x21, NATIVE_FRAME_PC_OFFSET]);
    emit_load_u64(ops, 16, u64::from(safepoint_id));
    dynasm!(ops ; .arch aarch64 ; str w16, [x21, abi::NATIVE_FRAME_CALL_SITE_OFFSET]);
}

/// Publish the canonical instruction-index PC into the active native frame.
fn emit_stamp_pc(ops: &mut Assembler, pc: u32) {
    emit_load_u64(ops, 9, u64::from(pc));
    dynasm!(ops ; .arch aarch64 ; str w9, [x21, NATIVE_FRAME_PC_OFFSET]);
}

/// Reduce the tagged `Value` in `x9` to `VALUE_TRUE` / `VALUE_FALSE` in `x9`
/// per `ToBoolean`. Numbers (int32 and boxed double), booleans, `null`, and
/// `undefined` decide inline; a heap cell or any other encoding (the hole,
/// function-id immediates) branches to `bail` for the exact side exit.
/// Clobbers `x14`/`x15`.
fn emit_truthiness_bool(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
        ; cmp x9, VALUE_TRUE_IMM
        ; b.eq =>truthy
        ; cmp x9, VALUE_FALSE_IMM
        ; b.eq =>falsy
        ; cmp x9, VALUE_NULL_IMM
        ; b.eq =>falsy
        ; cmp x9, VALUE_UNDEFINED_IMM
        ; b.eq =>falsy
    );
    // Heap cells and remaining immediates (function ids, holes) resolve
    // through the total leaf ToBoolean probe; its only miss is a null heap
    // (isolate-less probe harness), which side-exits.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        ; mov x1, x9
        ; movz x2, #0
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
        abi::STUB_TO_BOOLEAN_LEAF,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
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
        ; movz x9, VALUE_TRUE_IMM
        ; b =>done
        ; =>falsy
        ; movz x9, VALUE_FALSE_IMM
        ; =>done
    );
}

/// Inline cooperative poll at a back edge: read the interrupt byte and
/// decrement the fuel counter, re-entering the poll stub only when the
/// interrupt is set or the counter reaches zero. `poll_entry` is the
/// descriptor-resolved poll transition. A side-exit status resumes the
/// interpreter at the loop header `target` (the VM relinked this body); unknown
/// status words branch directly to the fatal epilogue.
#[allow(clippy::too_many_arguments)]
fn emit_backedge_poll(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    poll_entry: u64,
    target: u32,
    relink: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
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
    );
    // The poll attributes the batch to this loop header for OSR tier-up, and
    // a side exit resumes the interpreter there.
    emit_stamp_pc(ops, target);
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        poll_entry,
        abi::STUB_JIT_BACKEDGE_POLL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, abi::NativeResultStatus::Success as u32
        ; b.eq =>cont
        ; cmp x0, abi::NativeResultStatus::Yield as u32
        ; b.eq =>cont
        ; cmp x0, abi::NativeResultStatus::Throw as u32
        ; b.eq =>threw
        ; cmp x0, abi::NativeResultStatus::SideExit as u32
        ; b.ne =>fatal
        ; b =>relink
        ; =>cont
    );
}

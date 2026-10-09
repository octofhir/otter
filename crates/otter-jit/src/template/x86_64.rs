//! x86-64 template code emission with the platform C and private JS conventions.
//!
//! # Contents
//! - Whole-function prologue, epilogue, control flow, and OSR trampolines.
//! - Conservative cold-exit reachability, preserving all shared frame exits.
//! - [`operation`] emits one reusable baseline operation for either tier.
//! - Tagged Number arithmetic, comparison, conversion, and truthiness paths.
//! - Prepared string-cell loads and full `+` semantics through the allocating
//!   concat packet and coercive runtime delegate.
//! - Monomorphic plain and bounded polymorphic method generated calls through
//!   the shared entry-cell, frame, deopt, and feedback contracts.
//! - Generated base/super constructor linkage with fixed or spread arguments
//!   and receiver-allocation fast paths.
//! - SELF context reads, unchecked context-slot access, and context
//!   allocation ([`context`]); stable lexical cells and guarded global-object
//!   slots ([`binding`]) with one committed cold owner for binding misses.
//! - Iterator lifecycle, descriptor definitions, and class-value transitions
//!   through shared VM descriptors.
//! - Actual-argument collection and intrinsic-apply forwarding through the
//!   shared activation-window descriptors.
//! - Canonical indexed loads and stores through committed element descriptors.
//! - Named-property CacheIR hits for existing slots and allocation-free shape
//!   transitions, including receiver storage and write-barrier proofs, plus
//!   guarded lookup through pinned intrinsic prototypes. PIC misses share the
//!   VM property-action table with Graph before one committed runtime miss.
//! - Structured try/catch/finally operations through the VM-owned exception
//!   transition protocol.
//! - Cooperative interrupt/work-budget polling on every generated backedge.
//! - Delete, super, private, value-load, structural, module, variadic and
//!   static-call opcodes, `bind`, RegExp/built-in error literals and
//!   `Array()` allocation through their shared typed runtime boundaries.
//!
//! # Invariants
//! - Emission covers the whole [`TemplateOp`] vocabulary: a function the plan
//!   accepts is never declined for this target.
//! - The input is the same target-neutral [`super::TemplatePlan`] consumed by
//!   the AArch64 backend; operand decoding and branch validation are not
//!   repeated here.
//! - `r15` retains `JitCtx`, `r14` the active `Frame`, and `r13` its
//!   register window. Runtime calls use the platform C boundary; its result is
//!   normalized to `rax`/`rdx` for the private JavaScript convention.
//! - Allocating string calls publish the plan-owned safepoint before reentry;
//!   other coercive `+` cases complete through the shared runtime descriptor.
//! - Every generated callee entry uses the shared x86 tier-up mailbox protocol;
//!   nested callees cannot overwrite an outer pending promotion request.
//! - Every exact exit publishes the canonical instruction PC before returning.
//! - Forwarding probes reject sources requiring caller materialization before
//!   the committed value-span boundary can perform call effects.
//! - The stack is 16-byte aligned at every generated call boundary.
//! - Value immediates use zero-extending 32-bit moves when possible; symbolic
//!   addresses retain fixed-width 64-bit moves for exact relocation offsets.
//! - Compact RAX comparisons encode the 64-bit `83 /7 ib` form directly:
//!   dynasm selects an imm32 form even for an explicitly byte-sized immediate.
//!   Their checked positive imm8 compares the complete word, never its low byte.
//!
//! # See also
//! - [`super::arm64`] for the peer target emitter.
//! - [`crate::entry`] for the shared compiled-entry ABI.

#![allow(clippy::useless_conversion)]

use std::collections::{BTreeMap, BTreeSet};

#[path = "x86_64/binding.rs"]
mod binding;
#[path = "x86_64/calls.rs"]
mod calls;
#[path = "x86_64/cold_exits.rs"]
mod cold_exits;
#[path = "x86_64/context.rs"]
mod context;
#[path = "x86_64/exceptions.rs"]
mod exceptions;
#[path = "x86_64/forward_call.rs"]
mod forward_call;
#[path = "x86_64/intrinsic_prototype.rs"]
pub(crate) mod intrinsic_prototype;
#[path = "x86_64/native_leaf.rs"]
mod native_leaf;
#[path = "x86_64/operation.rs"]
pub(crate) mod operation;
mod primitive_strings;
pub(crate) mod shared_property;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_bytecode::scalar_semantics::{Int32ResultPolicy, NegativeZeroCondition};
use otter_vm::{JitCompileSnapshot, native_abi as abi, runtime_stubs::alloc_value_stub_by_id};

use super::{ArithKind, BitwiseKind, CompareKind, TemplateCode, TemplateOp, TemplatePlan};
use crate::{
    CompiledCode, Unsupported,
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::{RelocationCapture, RelocationTarget},
    },
    call_linkage::EntryShape,
    entry::{
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET,
        OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_FALSE, VALUE_HOLE, VALUE_NULL, VALUE_TRUE,
        VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
        VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
    frame::{ActivationExits, CallEntryCold, SpillArea},
    x86_64::{
        call_abi::{emit_runtime_call, emit_variadic_call},
        fields::emit_field_base,
        frame,
        values::{
            DOUBLE_OFFSET, NUMBER_TAG, emit_box_double, emit_box_int32, emit_load_runtime_stub,
            emit_load_symbol_u64, emit_load_u64,
        },
    },
};

const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;
const _: () = assert!(
    VALUE_TRUE <= i8::MAX as u64
        && VALUE_FALSE <= i8::MAX as u64
        && VALUE_NULL <= i8::MAX as u64
        && VALUE_UNDEFINED <= i8::MAX as u64
);

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    artifact_request: Option<ArtifactRequest>,
    capture_events: bool,
) -> Result<NativeCompileOutput<TemplateCode>, Unsupported> {
    let mut plan = TemplatePlan::build(view)?;
    let call_safepoints = crate::return_sites::template_source_safepoints(&mut plan)?;
    let mut return_sites = Vec::new();
    let mut call_source_exits = Vec::new();
    let mut required_exits = plan.instructions.iter().fold(0, |mask, instruction| {
        mask | cold_exits::required(instruction.op)
    });
    let tier_input = artifact_request.as_ref().map(|_| plan.render_artifact());

    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut relocations = RelocationCapture::new(artifact_request.is_some());
    let mut code_map = artifact_request.as_ref().map(|_| CodeMapCapture::default());
    let mut direct_call_events = capture_events.then(|| super::seed_direct_call_events(view));
    let mut shared_property = shared_property::SharedPropertyProbes::default();
    let type_mismatch = ops.new_dynamic_label();
    let unsupported = ops.new_dynamic_label();
    let runtime_transition = ops.new_dynamic_label();
    let allocation_miss = ops.new_dynamic_label();
    let backedge_relink = ops.new_dynamic_label();
    let returned = ops.new_dynamic_label();
    let pair_exit = ops.new_dynamic_label();
    let committed_throw = ops.new_dynamic_label();
    let threw = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    let labels: BTreeMap<u32, DynamicLabel> = plan
        .instructions
        .iter()
        .map(|instruction| (instruction.pc, ops.new_dynamic_label()))
        .collect();
    let shape = EntryShape::of(
        view,
        code_object_id,
        abi::NativeFrameKind::Baseline,
        !plan.safepoint_records.is_empty(),
    )?;
    let activation_exits = ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    };
    // The tier entry continues a published interpreter frame; the call entry
    // builds this function's record and window and falls through.
    let entry = ops.offset();
    let body = ops.new_dynamic_label();
    frame::emit_tier_prologue(&mut ops, shape.kind, SpillArea::NONE);
    dynasm!(ops ; .arch x64 ; jmp =>body);
    let call_entry = (!plan.osr_only).then(|| {
        let cold = CallEntryCold::new(&mut ops, shape);
        let start = frame::emit_call_entry(
            &mut ops,
            &mut relocations,
            view,
            shape,
            SpillArea::NONE,
            cold,
        );
        (start, cold)
    });
    dynasm!(ops ; .arch x64 ; =>body);
    if let Some(code_map) = code_map.as_mut()
        && let Some((offset, _)) = call_entry.as_ref()
    {
        code_map.record_call_entry(offset.0);
    }
    let mut labelled = BTreeSet::new();

    for (operation_index, instruction) in plan.instructions.iter().enumerate() {
        if labelled.insert(instruction.pc) {
            let label = labels[&instruction.pc];
            dynasm!(ops ; .arch x64 ; =>label);
        }
        let instruction_start = ops.offset().0;
        let targets = [
            type_mismatch,
            unsupported,
            runtime_transition,
            allocation_miss,
            backedge_relink,
            returned,
            committed_throw,
            threw,
            fatal,
        ];
        // A JS-call operation's abrupt exits publish its call source on a
        // cold relay before reaching the shared exit.
        let current = if !super::operation_is_js_call(instruction.op) {
            targets
        } else {
            let labels: [DynamicLabel; 9] = std::array::from_fn(|_| ops.new_dynamic_label());
            call_source_exits.push((
                std::array::from_fn::<_, 9, _>(|index| (labels[index], targets[index])),
                instruction.pc,
                call_safepoints[&instruction.pc],
            ));
            labels
        };
        let [
            type_mismatch,
            unsupported,
            runtime_transition,
            allocation_miss,
            backedge_relink,
            returned,
            committed_throw,
            threw,
            fatal,
        ] = current;
        if requires_pc_stamp(instruction.op) {
            emit_stamp_pc(&mut ops, instruction.pc);
        }
        operation::emit_operation(
            operation::OperationContext {
                ops: &mut ops,
                relocations: &mut relocations,
                return_sites: &mut return_sites,
                call_safepoint: call_safepoints.get(&instruction.pc).copied().unwrap_or(0),
                transitions,
                view,
                plan: &plan,
                labels: &labels,
                exits: crate::template::operation::OperationExits {
                    type_mismatch_exit: type_mismatch,
                    allocation_miss_exit: allocation_miss,
                    unsupported_exit: unsupported,
                    runtime_transition_exit: runtime_transition,
                    backedge_relink_exit: backedge_relink,
                    returned,
                    committed_throw,
                    threw,
                    fatal,
                },
                frame_kind: shape.kind,
                shared_property: &mut shared_property,
                direct_call_events: &mut direct_call_events,
                code_map: &mut code_map,
            },
            instruction,
        )?;
        if let Some(code_map) = code_map.as_mut() {
            code_map.record(CodeRegion::instruction(
                instruction_start,
                ops.offset().0,
                None,
                None,
                view.code_block.id,
                instruction.pc,
                instruction.byte_pc,
                Some(u32::try_from(operation_index).unwrap_or(u32::MAX)),
                format!("{:?}", instruction.op),
            ));
        }
    }

    if !call_source_exits.is_empty() {
        // Each relay names all nine shared destinations, so all must exist.
        required_exits = cold_exits::ALL;
        dynasm!(ops ; .arch x64 ; jmp =>unsupported);
        for (labels, pc, safepoint_id) in call_source_exits {
            let publish = ops.new_dynamic_label();
            for (label, target) in labels {
                dynasm!(ops ; .arch x64 ; =>label ; lea r11, [=>target] ; jmp =>publish);
            }
            dynasm!(ops ; .arch x64 ; =>publish);
            emit_cold_call_source(&mut ops, pc, safepoint_id);
            dynasm!(ops ; .arch x64 ; jmp r11);
        }
    }
    let caught = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>returned ; xor edx, edx ; jmp =>pair_exit);
    if required_exits & cold_exits::COMMITTED_THROW != 0 {
        dynasm!(ops ; .arch x64 ; =>committed_throw ; mov rsi, rax ; mov rdi, r15);
        emit_load_runtime_stub(
            &mut ops,
            &mut relocations,
            transitions.entry(abi::STUB_JIT_ROUTE_THROW),
            abi::STUB_JIT_ROUTE_THROW,
        );
        // A handler lands at the same source PC and initialized exception
        // register as before; only an unreachable entry is omitted.
        emit_runtime_call(&mut ops, abi::STUB_JIT_ROUTE_THROW);
        dynasm!(ops ; .arch x64
            ; cmp edx, abi::NativeResultStatus::SideExit as i32
            ; je =>caught
            ; cmp edx, abi::NativeResultStatus::Throw as i32
            ; je =>pair_exit
            ; cmp edx, abi::NativeResultStatus::Fatal as i32
            ; je =>pair_exit
            ; jmp =>fatal
        );
    }
    dynasm!(ops ; .arch x64 ; =>pair_exit);
    frame::emit_epilogue(&mut ops, activation_exits, shape.kind, SpillArea::NONE);
    for (mask, label, reason, action) in [
        (
            cold_exits::TYPE_MISMATCH,
            type_mismatch,
            abi::ExitReason::TypeMismatch,
            abi::ExitAction::Recompile,
        ),
        (
            cold_exits::ALLOCATION_MISS,
            allocation_miss,
            abi::ExitReason::AllocationMiss,
            abi::ExitAction::Resume,
        ),
        (
            cold_exits::UNSUPPORTED,
            unsupported,
            abi::ExitReason::UnsupportedOperation,
            abi::ExitAction::Recompile,
        ),
        (
            cold_exits::RUNTIME_TRANSITION,
            runtime_transition,
            abi::ExitReason::RuntimeTransition,
            abi::ExitAction::Resume,
        ),
        (
            cold_exits::BACKEDGE_RELINK,
            backedge_relink,
            abi::ExitReason::Interrupt,
            abi::ExitAction::Resume,
        ),
    ] {
        if required_exits & mask != 0 {
            emit_side_exit(&mut ops, activation_exits.side_exit, label, reason, action);
        }
    }
    if required_exits & cold_exits::THREW != 0 {
        dynasm!(ops ; .arch x64 ; =>threw ; mov rdi, r15);
        emit_load_runtime_stub(
            &mut ops,
            &mut relocations,
            transitions.entry(abi::STUB_JIT_FINISH_ERROR),
            abi::STUB_JIT_FINISH_ERROR,
        );
        emit_variadic_call(&mut ops, abi::STUB_JIT_FINISH_ERROR, 1);
        dynasm!(ops ; .arch x64
            ; cmp edx, abi::NativeResultStatus::SideExit as i32
            ; je =>caught
            ; cmp edx, abi::NativeResultStatus::Throw as i32
            ; je =>pair_exit
            ; cmp edx, abi::NativeResultStatus::Fatal as i32
            ; je =>pair_exit
            ; jmp =>fatal
        );
    }
    if required_exits & (cold_exits::THREW | cold_exits::COMMITTED_THROW) != 0 {
        emit_handler_dispatch(&mut ops, view, &labels, caught, activation_exits.side_exit);
    }
    if required_exits & (cold_exits::FATAL | cold_exits::THREW | cold_exits::COMMITTED_THROW) != 0 {
        dynasm!(ops ; .arch x64 ; =>fatal);
        emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
        dynasm!(ops ; .arch x64 ; mov edx, abi::NativeResultStatus::Fatal as i32);
        frame::emit_epilogue(&mut ops, activation_exits, shape.kind, SpillArea::NONE);
    }
    frame::emit_exits(
        &mut ops,
        &mut relocations,
        transitions,
        view,
        shape,
        activation_exits,
        SpillArea::NONE,
    );
    if let Some((_, cold)) = call_entry {
        frame::emit_call_entry_cold(
            &mut ops,
            &mut relocations,
            transitions,
            view,
            shape,
            activation_exits,
            cold,
        );
    }

    let mut osr_entries = BTreeMap::new();
    for &header_pc in view.code_block.loop_headers() {
        let Some(&target) = labels.get(&header_pc) else {
            continue;
        };
        let offset = ops.offset().0;
        frame::emit_tier_prologue(&mut ops, shape.kind, SpillArea::NONE);
        dynasm!(ops ; .arch x64 ; jmp =>target);
        if let Some(code_map) = code_map.as_mut() {
            code_map.record_osr(header_pc, offset, ops.offset().0);
        }
        osr_entries.insert(header_pc, offset);
    }
    shared_property.emit(&mut ops, &mut relocations, transitions, view);

    let buffer = crate::entry::finalize_assembler(ops)?;
    let TemplatePlan {
        register_operands,
        mut safepoint_records,
        osr_only,
        instructions,
        ..
    } = plan;
    safepoint_records.sort_by_key(|record| record.id);
    return_sites.sort_by_key(|site| site.native_return_offset);
    let compiled_code = CompiledCode::new(buffer, entry);
    if let Some(code_map) = code_map.as_mut() {
        code_map.record(CodeRegion::structural(
            "templateFunction",
            0,
            compiled_code.len(),
        ));
    }
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
    let code = TemplateCode::from_emission(
        compiled_code,
        code_object_id,
        view.code_block.id,
        Box::new([]),
        Box::new([]),
        super::code::retained_source_work(view, &BTreeSet::new()),
        register_operands,
        safepoint_records.into_boxed_slice(),
        return_sites.into_boxed_slice(),
        osr_entries,
        call_entry.map(|(offset, _)| offset.0),
        osr_only,
    );
    Ok(NativeCompileOutput {
        code,
        artifact,
        diagnostics: direct_call_events
            .map(|events| events.into_values().collect::<Vec<_>>().into_boxed_slice())
            .unwrap_or_default(),
        ir_node_count: instructions.len() as u64,
    })
}

fn requires_pc_stamp(op: TemplateOp) -> bool {
    if super::operation_is_js_call(op) {
        return false;
    }
    !matches!(
        op,
        TemplateOp::LoadImmediate { .. }
            | TemplateOp::Move { .. }
            | TemplateOp::LoadSelfClosure { .. }
            // Unchecked context accesses have no exit; the store's barrier
            // is a frame-free leaf.
            | TemplateOp::LoadClosureContext { .. }
            | TemplateOp::LoadContextSlot { .. }
            | TemplateOp::StoreContextSlot { .. }
            | TemplateOp::FusedNumericChain { .. }
            | TemplateOp::Return { .. }
            | TemplateOp::ReturnUndefined
            | TemplateOp::Jump {
                back_edge: false,
                ..
            }
            | TemplateOp::BranchNullish {
                back_edge: false,
                ..
            }
    )
}

/// Publish only before a committed collecting helper or an abrupt call exit.
fn emit_cold_call_source(ops: &mut Assembler, pc: u32, safepoint_id: abi::SafepointId) {
    dynasm!(ops ; .arch x64
        ; mov DWORD [r14 + NATIVE_FRAME_PC_OFFSET as i32], pc as i32
        ; mov DWORD [r14 + abi::NATIVE_FRAME_CALL_SITE_OFFSET as i32], safepoint_id as i32
    );
}

fn emit_stamp_pc(ops: &mut Assembler, pc: u32) {
    dynasm!(ops ; .arch x64 ; mov DWORD [r14 + NATIVE_FRAME_PC_OFFSET as i32], pc as i32);
}

/// Continue at the handler a routed throw landed in. The router wrote the
/// exception into the handler's register and the handler's PC into the
/// published record; this body has a label at every handler target, so the
/// throw never leaves compiled code. A PC without a label leaves through
/// `resume` with the router's exit in `rax`.
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
    dynasm!(ops
        ; .arch x64
        ; =>caught
        ; mov r11d, DWORD [r14 + NATIVE_FRAME_PC_OFFSET as i32]
    );
    for target in targets {
        if let Some(&label) = labels.get(&target) {
            dynasm!(ops ; .arch x64 ; cmp r11d, target as i32 ; je =>label);
        }
    }
    dynasm!(ops ; .arch x64 ; jmp =>resume);
}

fn emit_side_exit(
    ops: &mut Assembler,
    side_exit: DynamicLabel,
    label: DynamicLabel,
    reason: abi::ExitReason,
    action: abi::ExitAction,
) {
    let kind = abi::SideExit::new(0, reason, action).to_bits();
    dynasm!(ops ; .arch x64 ; =>label ; mov eax, DWORD [r14 + NATIVE_FRAME_PC_OFFSET as i32]);
    emit_load_u64(ops, 11, kind);
    dynasm!(ops
        ; .arch x64
        ; or rax, r11
        ; jmp =>side_exit
    );
}

/// Inline cooperative poll at a back edge. A side-exit status resumes the
/// interpreter at the loop header `target` after the VM relinked this body.
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
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov r10, [r11 + VM_THREAD_INTERRUPT_CELL_OFFSET as i32]
        ; cmp BYTE [r10], 0
        ; jne =>slow
        ; mov r10, [r11 + VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET as i32]
        ; sub QWORD [r10], 1
        ; jg =>done
        ; =>slow
    );
    // The poll attributes the batch to this loop header for OSR tier-up, and
    // a side exit resumes the interpreter there.
    emit_stamp_pc(ops, target);
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(ops, relocations, poll_entry, abi::STUB_JIT_BACKEDGE_POLL);
    emit_runtime_call(ops, abi::STUB_JIT_BACKEDGE_POLL);
    dynasm!(ops
        ; .arch x64
        ; cmp eax, abi::NativeResultStatus::Success as i32
        ; je =>done
        ; cmp eax, abi::NativeResultStatus::Yield as i32
        ; je =>done
        ; cmp eax, abi::NativeResultStatus::Throw as i32
        ; je =>threw
        ; cmp eax, abi::NativeResultStatus::SideExit as i32
        ; jne =>fatal
        ; jmp =>relink
        ; =>done
    );
}

fn emit_truthiness_bool(ops: &mut Assembler, bail: DynamicLabel) {
    let int_case = ops.new_dynamic_label();
    let double_case = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; mov r10, rax
        ; and r10, r11
        ; cmp r10, r11
        ; je =>int_case
        ; test r10, r10
        ; jne =>double_case
    );
    emit_cmp_rax_imm8(ops, VALUE_TRUE);
    dynasm!(ops ; .arch x64 ; je =>truthy);
    emit_cmp_rax_imm8(ops, VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; je =>falsy);
    emit_cmp_rax_imm8(ops, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; je =>falsy);
    emit_cmp_rax_imm8(ops, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; je =>falsy ; jmp =>bail ; =>int_case);
    dynasm!(ops ; .arch x64 ; test eax, eax ; jne =>truthy ; jmp =>falsy ; =>double_case);
    emit_load_u64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; mov r10, rax
        ; sub r10, r11
        ; movq xmm0, r10
        ; xorpd xmm1, xmm1
        ; ucomisd xmm0, xmm1
        ; jp =>falsy
        ; jne =>truthy
        ; jmp =>falsy
        ; =>truthy
    );
    emit_load_u64(ops, 0, VALUE_TRUE);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>falsy);
    emit_load_u64(ops, 0, VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; =>done);
}

fn emit_binary_arith(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: ArithKind,
    bail: DynamicLabel,
) {
    if kind == ArithKind::Rem {
        emit_remainder(ops, relocations, dst, lhs, rhs, bail);
        return;
    }
    if kind == ArithKind::Pow {
        dynasm!(ops ; .arch x64 ; jmp =>bail);
        return;
    }
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    if kind != ArithKind::Div {
        let float_path = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        emit_guard_int32_pair(ops, 0, 8, float_path);
        dynasm!(ops ; .arch x64 ; mov r10d, eax);
        match kind {
            ArithKind::Sub => dynasm!(ops ; .arch x64 ; sub r10d, r8d ; jo =>float_path),
            ArithKind::Mul => dynasm!(ops ; .arch x64 ; imul r10d, r8d ; jo =>float_path),
            _ => unreachable!(),
        }
        if matches!(
            kind.semantics().int32_result,
            Int32ResultPolicy::PromoteOverflowOrNegativeZero(
                NegativeZeroCondition::OppositeOperandSigns
            )
        ) {
            let publish = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch x64
                ; test r10d, r10d
                ; jne =>publish
                ; mov r11d, eax
                ; xor r11d, r8d
                ; js =>float_path
                ; =>publish
            );
        }
        emit_box_int32(ops, 10, 0, 11);
        emit_store_reg(ops, 0, dst);
        dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
        emit_load_reg(ops, 0, lhs);
        emit_load_reg(ops, 8, rhs);
        emit_number_to_double(ops, 0, 0, bail);
        emit_number_to_double(ops, 8, 1, bail);
        match kind {
            ArithKind::Sub => dynasm!(ops ; .arch x64 ; subsd xmm0, xmm1),
            ArithKind::Mul => dynasm!(ops ; .arch x64 ; mulsd xmm0, xmm1),
            _ => unreachable!(),
        }
        emit_box_double(ops, 0, 0, 11);
        emit_store_reg(ops, 0, dst);
        dynasm!(ops ; .arch x64 ; =>done);
        return;
    }
    emit_number_to_double(ops, 0, 0, bail);
    emit_number_to_double(ops, 8, 1, bail);
    dynasm!(ops ; .arch x64 ; divsd xmm0, xmm1);
    emit_box_double(ops, 0, 0, 11);
    emit_store_reg(ops, 0, dst);
}

#[allow(clippy::too_many_arguments)]
fn emit_add_generic(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    shared: &mut shared_property::SharedPropertyProbes,
    transitions: &crate::entry::TransitionTable,
    dst: u16,
    lhs: u16,
    rhs: u16,
    concat_safepoint: otter_vm::native_abi::SafepointId,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let float_path = ops.new_dynamic_label();
    let runtime_path = ops.new_dynamic_label();
    let delegate_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_guard_int32_pair(ops, 0, 8, float_path);
    dynasm!(ops ; .arch x64 ; mov r10d, eax ; add r10d, r8d ; jo =>float_path);
    emit_box_int32(ops, 10, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    emit_number_to_double(ops, 0, 0, runtime_path);
    emit_number_to_double(ops, 8, 1, runtime_path);
    dynasm!(ops ; .arch x64 ; addsd xmm0, xmm1);
    emit_box_double(ops, 0, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>runtime_path);

    let collecting = ops.new_dynamic_label();
    primitive_strings::emit_concat_fit(ops, shared, dst, lhs, rhs, collecting, done);
    dynasm!(ops ; .arch x64 ; =>collecting);

    if let Some(stub_addr) =
        alloc_value_stub_by_id(abi::STUB_STRING_CONCAT_ALLOC.id).and_then(|stub| stub.entry_addr())
    {
        dynasm!(ops
            ; .arch x64
            ; sub rsp, ALLOC_CTX_STACK_SIZE as i32
            ; mov r11, [r15 + THREAD_OFFSET as i32]
            ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r11
            ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], concat_safepoint as i32
            ; mov QWORD [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], 0
            ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], 0
            ; mov rdi, rsp
            ; mov esi, concat_safepoint as i32
        );
        emit_load_reg(ops, 2, lhs);
        emit_load_reg(ops, 1, rhs);
        emit_load_u64(ops, 8, VALUE_UNDEFINED);
        emit_load_runtime_stub(
            ops,
            relocations,
            stub_addr as u64,
            abi::STUB_STRING_CONCAT_ALLOC,
        );
        emit_runtime_call(ops, abi::STUB_STRING_CONCAT_ALLOC);
        dynasm!(ops
            ; .arch x64
            ; add rsp, ALLOC_CTX_STACK_SIZE as i32
            ; test rdx, rdx
            ; jne =>delegate_path
        );
        emit_store_reg(ops, 0, dst);
        dynasm!(ops ; .arch x64 ; jmp =>done);
    } else {
        dynasm!(ops ; .arch x64 ; jmp =>delegate_path);
    }

    dynasm!(ops
        ; .arch x64
        ; =>delegate_path
        ; mov rdi, r15
        ; mov esi, i32::from(dst)
        ; mov edx, i32::from(lhs)
        ; mov ecx, i32::from(rhs)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_ADD),
        abi::STUB_JIT_ADD,
    );
    emit_variadic_call(ops, abi::STUB_JIT_ADD, 4);
    emit_status_word_result(ops, threw, fatal);
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

fn emit_remainder(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    lhs: u16,
    rhs: u16,
    bail: DynamicLabel,
) {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_guard_int32_pair(ops, 0, 8, slow);
    // A zero divisor yields NaN, and `idiv` traps on `INT32_MIN / -1`; a
    // `-1` divisor's result is the dividend's signed zero anyway.
    dynasm!(ops
        ; .arch x64
        ; test r8d, r8d
        ; je =>slow
        ; cmp r8d, -1
        ; je =>slow
        ; mov r10d, eax
        ; cdq
        ; idiv r8d
        ; test edx, edx
        ; jne >publish
        ; test r10d, r10d
        ; js =>slow
        ; publish:
    );
    emit_box_int32(ops, 2, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
    emit_load_reg(ops, 6, lhs);
    emit_load_reg(ops, 2, rhs);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::NUMBER_REM_LEAF.entry_addr() as u64,
        abi::STUB_NUMBER_REM_LEAF,
    );
    emit_runtime_call(ops, abi::STUB_NUMBER_REM_LEAF);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; jne =>bail
    );
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; =>done);
}

fn emit_compare(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: CompareKind,
    bail: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let numbers = ops.new_dynamic_label();
    let false_case = ops.new_dynamic_label();
    let true_case = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    if matches!(kind, CompareKind::Eq | CompareKind::Ne) {
        let lhs_non_number = ops.new_dynamic_label();
        let strict_false = ops.new_dynamic_label();
        let cell_path = ops.new_dynamic_label();
        let distinct_cells = ops.new_dynamic_label();
        let leaf_call = ops.new_dynamic_label();
        emit_load_u64(ops, 11, NUMBER_TAG);
        dynasm!(ops
            ; .arch x64
            ; mov r10, rax
            ; and r10, r11
            ; test r10, r10
            ; jz =>lhs_non_number
            ; mov r10, r8
            ; and r10, r11
            ; test r10, r10
            ; jz =>strict_false
            ; jmp =>numbers
            ; =>lhs_non_number
            ; mov r10, r8
            ; and r10, r11
            ; test r10, r10
            ; jnz =>strict_false
        );
        emit_load_u64(ops, 11, NOT_CELL_MASK);
        dynasm!(ops
            ; .arch x64
            ; mov r10, rax
            ; and r10, r11
            ; test r10, r10
            ; jz =>cell_path
            ; mov r10, r8
            ; and r10, r11
            ; test r10, r10
            ; jz =>cell_path
            ; cmp rax, r8
        );
        if kind == CompareKind::Eq {
            dynasm!(ops ; .arch x64 ; je =>true_case ; jmp =>false_case);
        } else {
            dynasm!(ops ; .arch x64 ; jne =>true_case ; jmp =>false_case);
        }
        // A cell differs from an immediate, and two cells of different
        // kinds, or of a kind compared by identity, differ. Only two strings
        // or two BigInts ask the leaf probe. `r11` still holds NOT_CELL_MASK.
        dynasm!(ops
            ; .arch x64
            ; =>cell_path
            ; cmp rax, r8
            ; jne =>distinct_cells
        );
        if kind == CompareKind::Eq {
            dynasm!(ops ; .arch x64 ; jmp =>true_case);
        } else {
            dynasm!(ops ; .arch x64 ; jmp =>false_case);
        }
        dynasm!(ops
            ; .arch x64
            ; =>distinct_cells
            ; test rax, r11
            ; jnz =>strict_false
            ; test r8, r11
            ; jnz =>strict_false
            ; movzx r10d, BYTE [rax]
            ; movzx r11d, BYTE [r8]
            ; cmp r10d, r11d
            ; jne =>strict_false
            ; cmp r10d, i32::from(otter_vm::string::JS_STRING_BODY_TYPE_TAG)
            ; je =>leaf_call
            ; cmp r10d, i32::from(otter_vm::bigint::BIG_INT_BODY_TYPE_TAG)
            ; jne =>strict_false
            ; =>leaf_call
            ; mov rsi, rax
            ; mov rdx, r8
            ; mov rdi, [r15 + THREAD_OFFSET as i32]
            ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
        );
        emit_load_runtime_stub(
            ops,
            relocations,
            otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
            abi::STUB_STRICT_EQ_LEAF,
        );
        emit_runtime_call(ops, abi::STUB_STRICT_EQ_LEAF);
        dynasm!(ops
            ; .arch x64
                ; test rdx, rdx
            ; jne =>bail
        );
        emit_load_u64(ops, 11, VALUE_TRUE);
        dynasm!(ops ; .arch x64 ; cmp rax, r11);
        if kind == CompareKind::Eq {
            dynasm!(ops ; .arch x64 ; je =>true_case ; jmp =>false_case);
        } else {
            dynasm!(ops ; .arch x64 ; jne =>true_case ; jmp =>false_case);
        }
        dynasm!(ops ; .arch x64 ; =>strict_false);
        if kind == CompareKind::Eq {
            dynasm!(ops ; .arch x64 ; jmp =>false_case);
        } else {
            dynasm!(ops ; .arch x64 ; jmp =>true_case);
        }
    } else {
        emit_load_u64(ops, 11, NUMBER_TAG);
        dynasm!(ops
            ; .arch x64
            ; mov r10, rax
            ; and r10, r11
            ; test r10, r10
            ; jz =>primitive
            ; mov r10, r8
            ; and r10, r11
            ; test r10, r10
            ; jz =>primitive
            ; jmp =>numbers
        );
    }
    dynasm!(ops ; .arch x64 ; =>numbers);
    emit_number_to_double(ops, 0, 0, bail);
    emit_number_to_double(ops, 8, 1, bail);
    dynasm!(ops ; .arch x64 ; ucomisd xmm0, xmm1);
    match kind {
        CompareKind::Lt => dynasm!(ops ; .arch x64 ; jp =>false_case ; jb =>true_case),
        CompareKind::Le => dynasm!(ops ; .arch x64 ; jp =>false_case ; jbe =>true_case),
        CompareKind::Gt => dynasm!(ops ; .arch x64 ; ja =>true_case),
        CompareKind::Ge => dynasm!(ops ; .arch x64 ; jae =>true_case),
        CompareKind::Eq => dynasm!(ops ; .arch x64 ; jp =>false_case ; je =>true_case),
        CompareKind::Ne => dynasm!(ops ; .arch x64 ; jp =>true_case ; jne =>true_case),
    }
    dynasm!(ops ; .arch x64 ; jmp =>false_case);
    if !matches!(kind, CompareKind::Eq | CompareKind::Ne) {
        dynasm!(ops ; .arch x64 ; =>primitive);
        emit_load_reg(ops, 0, lhs);
        emit_load_reg(ops, 8, rhs);
        primitive_strings::emit_order(ops, relocations, kind, bail, fatal);
        emit_store_reg(ops, 0, dst);
        dynasm!(ops ; .arch x64 ; jmp =>done);
    }
    dynasm!(ops ; .arch x64 ; =>true_case);
    emit_load_u64(ops, 0, VALUE_TRUE);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>false_case);
    emit_load_u64(ops, 0, VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; =>done);
    emit_store_reg(ops, 0, dst);
}

/// `r<dst> = (typeof r<src> === kind)` through the leaf `(heap, value, test)`
/// probe; its only miss is a null heap, which exits to `bail`.
fn emit_test_typeof(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    src: u16,
    test: i32,
    bail: DynamicLabel,
) {
    emit_load_reg(ops, 6, src);
    emit_load_u64(ops, 2, u64::from(test as u32));
    dynasm!(ops
        ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::TYPEOF_TEST_LEAF.entry_addr() as u64,
        abi::STUB_TYPEOF_TEST_LEAF,
    );
    emit_runtime_call(ops, abi::STUB_TYPEOF_TEST_LEAF);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; jne =>bail
        ; mov [r13 + i32::from(dst) * 8], rax
    );
}

/// Emit abstract (in)equality: numbers and the null/undefined equivalence
/// class inline; every coercing pair, and a nullish operand against a native
/// function (the only `[[IsHTMLDDA]]` carrier), completes the whole opcode
/// through the reentrant loose-equality transition, which writes `dst`.
#[allow(clippy::too_many_arguments)]
fn emit_loose_compare(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    dst: u16,
    lhs: u16,
    rhs: u16,
    negate: bool,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    const NULLISH_BIT: i32 = 0x8;
    const _: () = assert!(VALUE_NULL | NULLISH_BIT as u64 == VALUE_UNDEFINED);
    const _: () = assert!(VALUE_UNDEFINED | NULLISH_BIT as u64 == VALUE_UNDEFINED);
    let native_tag = otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG as i8;
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let equal = ops.new_dynamic_label();
    let not_equal = ops.new_dynamic_label();
    let numeric = ops.new_dynamic_label();
    let lhs_nullish = ops.new_dynamic_label();
    let rhs_nullish = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Identical words are equal, except a number's: NaN is not NaN.
    dynasm!(ops ; .arch x64 ; cmp rax, r8 ; jne >distinct);
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops ; .arch x64
        ; test rax, r11 ; jnz =>numeric ; jmp =>equal
        ; distinct:
        ; mov r10, rax ; or r10, NULLISH_BIT ; cmp r10, VALUE_UNDEFINED as i32 ; je =>lhs_nullish
        ; mov r10, r8 ; or r10, NULLISH_BIT ; cmp r10, VALUE_UNDEFINED as i32 ; je =>rhs_nullish
        ; =>numeric
    );
    emit_number_to_double(ops, 0, 0, slow);
    emit_number_to_double(ops, 8, 1, slow);
    dynasm!(ops ; .arch x64
        ; ucomisd xmm0, xmm1 ; jp =>not_equal ; je =>equal ; jmp =>not_equal
        ; =>lhs_nullish
        ; mov r10, r8 ; or r10, NULLISH_BIT ; cmp r10, VALUE_UNDEFINED as i32 ; je =>equal
        ; mov r10, r8 ; jmp >nullish_against
        ; =>rhs_nullish
        ; mov r10, rax
        ; nullish_against:
    );
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test r10, r11 ; jnz =>not_equal
        ; test r10, r10 ; jz =>not_equal
        ; cmp BYTE [r10], native_tag ; je =>slow
        ; =>not_equal
    );
    emit_load_u64(ops, 0, if negate { VALUE_TRUE } else { VALUE_FALSE });
    dynasm!(ops ; .arch x64 ; jmp >store ; =>equal);
    emit_load_u64(ops, 0, if negate { VALUE_FALSE } else { VALUE_TRUE });
    dynasm!(ops ; .arch x64 ; store:);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64
        ; jmp =>done
        ; =>slow
        ; mov rdi, r15
        ; mov esi, i32::from(dst)
        ; mov edx, i32::from(lhs)
        ; mov ecx, i32::from(rhs)
        ; mov r8d, i32::from(negate)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_LOOSE_EQ),
        abi::STUB_JIT_LOOSE_EQ,
    );
    emit_variadic_call(ops, abi::STUB_JIT_LOOSE_EQ, 5);
    emit_side_exit_status_result(ops, bail, threw, fatal);
    dynasm!(ops ; .arch x64 ; =>done);
}

fn emit_bitwise(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: BitwiseKind,
    bail: DynamicLabel,
) {
    emit_load_reg(ops, 0, lhs);
    emit_to_int32(ops, 0, 12, bail);
    emit_load_reg(ops, 0, rhs);
    emit_to_int32(ops, 0, 1, bail);
    match kind {
        BitwiseKind::Or => dynasm!(ops ; .arch x64 ; or r12d, ecx),
        BitwiseKind::And => dynasm!(ops ; .arch x64 ; and r12d, ecx),
        BitwiseKind::Xor => dynasm!(ops ; .arch x64 ; xor r12d, ecx),
        BitwiseKind::Shl => dynasm!(ops ; .arch x64 ; shl r12d, cl),
        BitwiseKind::Shr => dynasm!(ops ; .arch x64 ; sar r12d, cl),
    }
    emit_box_int32(ops, 12, 0, 11);
    emit_store_reg(ops, 0, dst);
}

fn emit_unsigned_shift(ops: &mut Assembler, dst: u16, lhs: u16, rhs: u16, bail: DynamicLabel) {
    emit_load_reg(ops, 0, lhs);
    emit_to_int32(ops, 0, 12, bail);
    emit_load_reg(ops, 0, rhs);
    emit_to_int32(ops, 0, 1, bail);
    dynasm!(ops ; .arch x64 ; shr r12d, cl ; mov eax, r12d ; test eax, eax ; js >wide);
    emit_box_int32(ops, 0, 0, 11);
    dynasm!(ops ; .arch x64 ; jmp >done ; wide: ; cvtsi2sd xmm0, rax);
    emit_box_double(ops, 0, 0, 11);
    dynasm!(ops ; .arch x64 ; done:);
    emit_store_reg(ops, 0, dst);
}

fn emit_increment(ops: &mut Assembler, dst: u16, src: u16, delta: i32, bail: DynamicLabel) {
    emit_load_reg(ops, 0, src);
    let float_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_guard_int32(ops, 0, float_path);
    dynasm!(ops ; .arch x64 ; mov r10d, eax ; add r10d, delta ; jo =>float_path);
    emit_box_int32(ops, 10, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
    emit_load_reg(ops, 0, src);
    emit_number_to_double(ops, 0, 0, bail);
    emit_load_u64(ops, 11, (delta as f64).to_bits());
    dynasm!(ops ; .arch x64 ; movq xmm1, r11 ; addsd xmm0, xmm1);
    emit_box_double(ops, 0, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; =>done);
}

fn emit_negate(ops: &mut Assembler, dst: u16, src: u16, bail: DynamicLabel) {
    emit_load_reg(ops, 0, src);
    let float_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_guard_int32(ops, 0, float_path);
    dynasm!(ops
        ; .arch x64
        ; test eax, eax
        ; je =>float_path
        ; mov r10d, eax
        ; neg r10d
        ; jo =>float_path
    );
    emit_box_int32(ops, 10, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
    emit_load_reg(ops, 0, src);
    emit_number_to_double(ops, 0, 0, bail);
    emit_load_u64(ops, 11, 1_u64 << 63);
    dynasm!(ops ; .arch x64 ; movq xmm1, r11 ; xorpd xmm0, xmm1);
    emit_box_double(ops, 0, 0, 11);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; =>done);
}

fn emit_guard_int32_pair(ops: &mut Assembler, left: u8, right: u8, bail: DynamicLabel) {
    emit_guard_int32(ops, left, bail);
    emit_guard_int32(ops, right, bail);
}

fn emit_guard_int32(ops: &mut Assembler, register: u8, bail: DynamicLabel) {
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(register)
        ; and r10, r11
        ; cmp r10, r11
        ; jne =>bail
    );
}

fn emit_guard_number(ops: &mut Assembler, register: u8, bail: DynamicLabel) {
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(register)
        ; and r10, r11
        ; test r10, r10
        ; jz =>bail
    );
}

fn emit_number_to_double(ops: &mut Assembler, source: u8, destination: u8, bail: DynamicLabel) {
    let double_case = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(source)
        ; and r10, r11
        ; cmp r10, r11
        ; jne =>double_case
        ; cvtsi2sd Rx(destination), Rd(source)
        ; jmp =>done
        ; =>double_case
        ; test r10, r10
        ; jz =>bail
    );
    emit_load_u64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(source)
        ; sub r10, r11
        ; movq Rx(destination), r10
        ; =>done
    );
}

fn emit_to_int32(ops: &mut Assembler, source: u8, destination: u8, bail: DynamicLabel) {
    let double_case = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(source)
        ; and r10, r11
        ; cmp r10, r11
        ; jne =>double_case
        ; mov Rd(destination), Rd(source)
        ; jmp =>done
        ; =>double_case
        ; test r10, r10
        ; jz =>bail
    );
    emit_load_u64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(source)
        ; sub r10, r11
        ; movq xmm0, r10
        ; ucomisd xmm0, xmm0
        ; jp =>bail
    );
    emit_load_u64(ops, 11, 9_223_372_036_854_775_808.0_f64.to_bits());
    dynasm!(ops
        ; .arch x64
        ; movq xmm1, r11
        ; movq r10, xmm0
        ; shl r10, 1
        ; shr r10, 1
        ; movq xmm2, r10
        ; ucomisd xmm2, xmm1
        ; jae =>bail
        ; cvttsd2si Rq(destination), xmm0
        ; =>done
    );
}

fn emit_load_reg(ops: &mut Assembler, destination: u8, source: u16) {
    let offset = i32::from(source) * 8;
    dynasm!(ops ; .arch x64 ; mov Rq(destination), [r13 + offset]);
}

#[allow(clippy::too_many_arguments)]
fn emit_define_own_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    target: u16,
    key: u16,
    descriptor: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, i32::from(target)
        ; mov edx, i32::from(key)
        ; mov ecx, i32::from(descriptor)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_DEFINE_OWN_PROPERTY),
        abi::STUB_JIT_DEFINE_OWN_PROPERTY,
    );
    emit_variadic_call(ops, abi::STUB_JIT_DEFINE_OWN_PROPERTY, 4);
    emit_status_word_result(ops, threw, fatal);
}

/// Calls one typed opcode boundary with `(ctx, opcode, arg0, arg1, arg2)`.
/// The VM helper commits the whole opcode before returning success; the only
/// side exit is a missing published activation, raised before any effect.
#[allow(clippy::too_many_arguments)]
fn emit_opcode_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    stub: abi::RuntimeStubDescriptor,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, i32::from(opcode)
    );
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 1, arg1);
    emit_load_u64(ops, 8, arg2);
    emit_load_runtime_stub(ops, relocations, transitions.variadic_entry(stub), stub);
    emit_variadic_call(ops, stub, 5);
    emit_side_exit_status_result(ops, bail, threw, fatal);
}

/// Routes a status word that may also request an exact pre-effect side exit.
fn emit_side_exit_status_result(
    ops: &mut Assembler,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; test rax, rax
        ; je >completed
        ; cmp eax, abi::NativeResultStatus::SideExit as i32
        ; je =>bail
        ; cmp eax, abi::NativeResultStatus::Throw as i32
        ; je =>threw
        ; jmp =>fatal
        ; completed:
    );
}

/// Calls a `(ctx, dst, constant)` materialization boundary.
#[allow(clippy::too_many_arguments)]
fn emit_constant_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    stub: abi::RuntimeStubDescriptor,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, i32::from(dst)
        ; mov edx, constant as i32
    );
    emit_load_runtime_stub(ops, relocations, transitions.variadic_entry(stub), stub);
    emit_variadic_call(ops, stub, 3);
    emit_status_word_result(ops, threw, fatal);
}

/// Allocates `Array()` or `Array(Int32)` through the shared `AllocValue3`
/// boundary. The VM stub rejects a non-Int32 or negative length before any
/// allocation, so every non-success status exits at the original
/// `ArrayConstruct` and the interpreter owns wide lengths, `RangeError`, and
/// OOM reporting.
fn emit_array_construct_alloc_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    length: Option<u16>,
    safepoint: abi::SafepointId,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let descriptor = abi::STUB_ARRAY_CONSTRUCT_ALLOC;
    let stub_addr = alloc_value_stub_by_id(descriptor.id)
        .and_then(|stub| stub.entry_addr())
        .ok_or(Unsupported::OperandShape(
            "ArrayConstruct allocating stub entry",
        ))?;
    dynasm!(ops
        ; .arch x64
        ; sub rsp, ALLOC_CTX_STACK_SIZE as i32
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r11
        ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], safepoint as i32
        ; mov QWORD [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], 0
        ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], 0
        ; mov rdi, rsp
        ; mov esi, safepoint as i32
    );
    if let Some(length) = length {
        emit_load_reg(ops, 2, length);
    } else {
        emit_load_u64(ops, 2, otter_vm::Value::number_i32(0).to_bits());
    }
    emit_load_u64(ops, 1, VALUE_UNDEFINED);
    emit_load_u64(ops, 8, VALUE_UNDEFINED);
    emit_load_runtime_stub(ops, relocations, stub_addr as u64, descriptor);
    emit_runtime_call(ops, descriptor);
    dynasm!(ops
        ; .arch x64
        ; add rsp, ALLOC_CTX_STACK_SIZE as i32
        ; test rdx, rdx
        ; jne =>miss
    );
    emit_store_reg(ops, 0, dst);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_committed_value2(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    descriptor: abi::RuntimeStubDescriptor,
    result: Option<u16>,
    value0: Option<u16>,
    value1: Option<u16>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    match value0 {
        Some(register) => emit_load_reg(ops, 6, register),
        None => emit_load_u64(ops, 6, VALUE_UNDEFINED),
    }
    match value1 {
        Some(register) => emit_load_reg(ops, 2, register),
        None => emit_load_u64(ops, 2, VALUE_UNDEFINED),
    }
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(ops, relocations, transitions.entry(descriptor), descriptor);
    emit_runtime_call(ops, descriptor);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je >completed
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; completed:
    );
    if let Some(result) = result {
        emit_store_reg(ops, 0, result);
    }
}

/// `Rq(slot)` = the site's IC slot address.
fn emit_slot_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    slot: u8,
) -> Result<(), Unsupported> {
    let ic_slot = view
        .property_accesses
        .get(&byte_pc)
        .map(|access| access.ic_slot)
        .filter(|&slot| slot != 0 && view.cage_base != 0)
        .ok_or(Unsupported::OperandShape(
            "named property site without an IC slot",
        ))?;
    emit_load_symbol_u64(
        ops,
        relocations,
        slot,
        ic_slot,
        RelocationTarget::PropertyIcSlot {
            function_id: view.code_block.id,
            byte_pc,
        },
    );
    Ok(())
}

/// Inline own-field selection (JSC's baseline DataIC fast path): an ordinary
/// receiver whose shape equals the slot's own-field pair leaves the field's
/// bank base in `r8` and bank-relative index in `rax`; anything else jumps
/// to `miss`. Clobbers `r8`, `r10`, `r11`, `rax`.
fn emit_inline_own_field(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    slot: u8,
    miss: DynamicLabel,
) {
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let layout = view.field_layout;
    let ic = otter_vm::jit::PROPERTY_IC_LAYOUT;
    emit_load_u64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(receiver), r10 ; jnz =>miss
        ; test Rq(receiver), Rq(receiver) ; jz =>miss
        ; cmp BYTE [Rq(receiver)], OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss
        ; mov r8d, [Rq(receiver) + view.object_shape_byte as i32]
        ; cmp r8d, [Rq(slot) + ic.inline_shape_byte as i32] ; jne =>miss
        ; mov eax, [Rq(slot) + ic.inline_field_byte as i32]
        ; test eax, eax ; js =>inline
        ; mov r8d, [Rq(receiver) + layout.slab_handle_byte as i32]
        ; test r8d, r8d ; jz =>miss);
    emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(receiver) ; add r8, r11
        ; add r8, layout.slab_words_byte as i32 ; jmp =>ready
        ; =>inline
        ; and eax, 0x7fff_ffff
        ; lea r8, [Rq(receiver) + layout.inline_values_byte as i32]
        ; =>ready);
}

/// Route a shared routine's `NativeResultPair` status in `rdx`.
fn emit_pair_status(ops: &mut Assembler, throw_value: DynamicLabel, fatal: DynamicLabel) {
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je >completed
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; completed:
    );
}

/// Emit `obj.name = value`: inline own field, then the shared `StoreIC`.
#[allow(clippy::too_many_arguments)]
fn emit_store_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    shared_property: &mut shared_property::SharedPropertyProbes,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    object: u16,
    value: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    use shared_property::{STORE_RECEIVER, STORE_SLOT, STORE_VALUE};
    let shared = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_slot_address(ops, relocations, view, byte_pc, STORE_SLOT)?;
    emit_load_reg(ops, STORE_RECEIVER, object);
    emit_inline_own_field(ops, view, STORE_RECEIVER, STORE_SLOT, shared);
    emit_load_reg(ops, STORE_VALUE, value);
    dynasm!(ops ; .arch x64 ; mov [r8 + rax * 8], Rq(STORE_VALUE));
    emit_template_value_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>shared);
    let routine = shared_property.label(ops, true);
    emit_load_reg(ops, STORE_RECEIVER, object);
    emit_load_reg(ops, STORE_VALUE, value);
    dynasm!(ops ; .arch x64 ; call =>routine);
    emit_pair_status(ops, throw_value, fatal);
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

/// Static intrinsic-prototype reads: a receiver of an intrinsic body type
/// (primitive string, closure, collection) reading a data slot of its pinned
/// realm prototype. These programs describe realm intrinsics, not site
/// feedback; a miss falls through to the site's IC. A hit leaves the value in
/// `rax` and jumps to `done`.
fn emit_intrinsic_loads(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    object: u16,
    done: DynamicLabel,
) {
    use otter_vm::JitCacheIrOp as Op;
    let Some(programs) = view.property_programs.get(&byte_pc) else {
        return;
    };
    for program in programs {
        let Some((
            Op::LoadIntrinsicPrototype {
                object: 0,
                result: 1,
                target,
            },
            rest,
        )) = program.ops.split_first()
        else {
            continue;
        };
        let Some((&Op::LoadField { object: 1, field }, guards)) = rest.split_last() else {
            continue;
        };
        if !guards.iter().all(|op| {
            matches!(
                op,
                Op::GuardShape { object: 1, .. }
                    | Op::GuardDictionaryLayout { object: 1, .. }
                    | Op::GuardAtomSlot {
                        object: 1,
                        writable: false,
                        ..
                    }
                    | Op::GuardPrototypeValidity { .. }
            )
        }) {
            continue;
        }
        let next = ops.new_dynamic_label();
        emit_load_reg(ops, 0, object);
        intrinsic_prototype::emit(ops, relocations, view, *target, byte_pc, 0, next);
        dynasm!(ops ; .arch x64
            ; cmp BYTE [r8], OBJECT_BODY_TYPE_TAG as i8
            ; jne =>next);
        for op in guards {
            match *op {
                Op::GuardShape { shape, .. } => dynasm!(ops ; .arch x64
                    ; cmp DWORD [r8 + view.object_shape_byte as i32], shape as i32
                    ; jne =>next),
                Op::GuardDictionaryLayout { layout, .. } => {
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        11,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops ; .arch x64
                        ; mov r9d, [r8 + view.object_shape_byte as i32]
                        ; test BYTE [r11 + r9 + view.shape_state_byte as i32], otter_vm::object::ShapeState::DICTIONARY_MASK as i8
                        ; jz =>next
                        ; mov r9d, [r8 + view.object_exotic_handle_byte as i32]
                        ; test r9d, r9d
                        ; jz =>next
                        ; add r9, r11
                        ; mov r11d, layout as u32 as i32
                        ; cmp [r9 + view.exotic_dictionary_layout_byte as i32], r11d
                        ; jne =>next);
                }
                Op::GuardPrototypeValidity { validity } => {
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        9,
                        validity.address as u64,
                        RelocationTarget::PrototypeValidityCell {
                            identity: validity.identity,
                        },
                    );
                    dynasm!(ops ; .arch x64 ; cmp DWORD [r9], 0 ; je =>next);
                }
                _ => {}
            }
        }
        emit_field_base(ops, relocations, view, 8, 11, 9, field);
        dynasm!(ops ; .arch x64
            ; mov rax, [r11 + field.byte_offset() as i32]
            ; jmp =>done
            ; =>next);
    }
}

/// Emit `dst = obj.name`: inline own field, then the shared `LoadIC`.
#[allow(clippy::too_many_arguments)]
fn emit_load_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    shared_property: &mut shared_property::SharedPropertyProbes,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    dst: u16,
    object: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    use shared_property::{LOAD_RECEIVER, LOAD_SLOT};
    let shared = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_intrinsic_loads(ops, relocations, view, byte_pc, object, done);
    emit_slot_address(ops, relocations, view, byte_pc, LOAD_SLOT)?;
    emit_load_reg(ops, LOAD_RECEIVER, object);
    emit_inline_own_field(ops, view, LOAD_RECEIVER, LOAD_SLOT, shared);
    dynasm!(ops ; .arch x64
        ; mov rax, [r8 + rax * 8]
        ; jmp =>done
        ; =>shared);
    let routine = shared_property.label(ops, false);
    emit_load_reg(ops, LOAD_RECEIVER, object);
    dynasm!(ops ; .arch x64 ; call =>routine);
    emit_pair_status(ops, throw_value, fatal);
    dynasm!(ops ; .arch x64 ; =>done);
    emit_store_reg(ops, 0, dst);
    Ok(())
}

fn emit_template_value_barrier(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    parent: u8,
    value: u8,
) {
    let done = ops.new_dynamic_label();
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test Rq(value), r11
        ; jnz =>done
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
        ; mov rsi, Rq(parent)
    );
    if value != 2 {
        dynasm!(ops ; .arch x64 ; mov rdx, Rq(value));
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
        abi::STUB_WRITE_BARRIER,
    );
    emit_runtime_call(ops, abi::STUB_WRITE_BARRIER);
    dynasm!(ops ; .arch x64 ; =>done);
    let _ = view;
}

fn emit_status_word_result(ops: &mut Assembler, threw: DynamicLabel, fatal: DynamicLabel) {
    dynasm!(ops
        ; .arch x64
        ; test rax, rax
        ; je >completed
        ; cmp eax, abi::NativeResultStatus::Throw as i32
        ; je =>threw
        ; jmp =>fatal
        ; completed:
    );
}

fn emit_collect_arguments(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    dst: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, i32::from(dst)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_COLLECT_ARGUMENTS),
        abi::STUB_JIT_COLLECT_ARGUMENTS,
    );
    emit_variadic_call(ops, abi::STUB_JIT_COLLECT_ARGUMENTS, 2);
    emit_status_word_result(ops, threw, fatal);
}

#[derive(Debug, Clone, Copy)]
enum PacketWord {
    Register(u16),
}

fn emit_value_packet_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    descriptor: abi::RuntimeStubDescriptor,
    words: &[PacketWord],
    dst: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let packet_words = u32::try_from(words.len())
        .map_err(|_| Unsupported::OperandShape("x86-64 value-packet word count"))?;
    let packet_bytes = packet_words
        .checked_mul(8)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .filter(|bytes| *bytes <= 4_080)
        .ok_or(Unsupported::OperandShape("x86-64 value-packet frame"))?;
    if packet_bytes != 0 {
        dynasm!(ops ; .arch x64 ; sub rsp, packet_bytes as i32);
    }
    for (index, word) in words.iter().enumerate() {
        match *word {
            PacketWord::Register(register) => emit_load_reg(ops, 0, register),
        }
        let offset = i32::try_from(index * 8)
            .map_err(|_| Unsupported::OperandShape("x86-64 value-packet offset"))?;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset], rax);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov rsi, rsp
        ; mov edx, packet_words as i32
    );
    emit_load_runtime_stub(ops, relocations, transitions.entry(descriptor), descriptor);
    emit_runtime_call(ops, descriptor);
    if packet_bytes != 0 {
        dynasm!(ops ; .arch x64 ; add rsp, packet_bytes as i32);
    }
    let completed = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je =>completed
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; =>completed
    );
    emit_store_reg(ops, 0, dst);
    Ok(())
}

fn emit_load_element(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    shared_property: &mut shared_property::SharedPropertyProbes,
    dst: u16,
    receiver: u16,
    index: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    // A name key: the receiver class's table entry, as a named load's.
    use shared_property::{KEYED_LOAD_KEY, KEYED_LOAD_RECEIVER};
    let routine = shared_property.keyed_label(ops, false);
    emit_load_reg(ops, KEYED_LOAD_RECEIVER, receiver);
    emit_load_reg(ops, KEYED_LOAD_KEY, index);
    dynasm!(ops ; .arch x64 ; call =>routine ; test rdx, rdx ; jz >done);
    emit_load_reg(ops, 6, receiver);
    emit_load_reg(ops, 2, index);
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_LOAD_ELEMENT),
        abi::STUB_JIT_LOAD_ELEMENT,
    );
    emit_runtime_call(ops, abi::STUB_JIT_LOAD_ELEMENT);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je >done
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; done:
    );
    emit_store_reg(ops, 0, dst);
}

fn emit_store_element(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    shared_property: &mut shared_property::SharedPropertyProbes,
    receiver: u16,
    index: u16,
    value: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    // A name key: the receiver class's table entry, as a named store's.
    use shared_property::{KEYED_STORE_KEY, KEYED_STORE_RECEIVER, KEYED_STORE_VALUE};
    let routine = shared_property.keyed_label(ops, true);
    emit_load_reg(ops, KEYED_STORE_RECEIVER, receiver);
    emit_load_reg(ops, KEYED_STORE_VALUE, value);
    emit_load_reg(ops, KEYED_STORE_KEY, index);
    dynasm!(ops ; .arch x64 ; call =>routine ; test rdx, rdx ; jz >done);
    emit_load_reg(ops, 6, receiver);
    emit_load_reg(ops, 2, index);
    emit_load_reg(ops, 1, value);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_STORE_ELEMENT),
        abi::STUB_JIT_STORE_ELEMENT,
    );
    emit_runtime_call(ops, abi::STUB_JIT_STORE_ELEMENT);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je >done
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; done:
    );
}

fn emit_scalar_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    result: u16,
    value0: Option<u16>,
    value1: Option<u16>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    if let Some(value0) = value0 {
        emit_load_reg(ops, 6, value0);
    } else {
        emit_load_u64(ops, 6, VALUE_UNDEFINED);
    }
    if let Some(value1) = value1 {
        emit_load_reg(ops, 2, value1);
    } else {
        emit_load_u64(ops, 2, VALUE_UNDEFINED);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_SCALAR_VALUE),
        abi::STUB_JIT_SCALAR_VALUE,
    );
    emit_runtime_call(ops, abi::STUB_JIT_SCALAR_VALUE);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je >normal
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; normal:
    );
    emit_store_reg(ops, 0, result);
}

fn emit_store_reg(ops: &mut Assembler, source: u8, destination: u16) {
    let offset = i32::from(destination) * 8;
    dynasm!(ops ; .arch x64 ; mov [r13 + offset], Rq(source));
}

/// Compare the complete RAX word to a checked nonnegative sign-extended imm8.
/// dynasm's CMP matching widens BYTE to imm32; this fixed encoding uses
/// REX.W, opcode 83 and ModRM /7 with register-direct RAX instead.
fn emit_cmp_rax_imm8(ops: &mut Assembler, value: u64) {
    let immediate = i8::try_from(value).expect("compact RAX comparison requires 0..=127");
    ops.extend([0x48, 0x83, 0xf8, immediate as u8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_rax_comparison_checks_the_full_word_and_sets_exact_flags() {
        const FLAGS: u64 = 1 | (1 << 6) | (1 << 7) | (1 << 11); // CF, ZF, SF, OF.
        for immediate in [0, 1, VALUE_FALSE, VALUE_TRUE, VALUE_UNDEFINED, 127] {
            let mut ops = Assembler::new().unwrap();
            let entry = ops.offset();
            dynasm!(ops ; .arch x64 ; mov rax, rdi);
            let start = ops.offset().0;
            emit_cmp_rax_imm8(&mut ops, immediate);
            assert_eq!(ops.offset().0 - start, 4);
            dynasm!(ops ; .arch x64 ; pushfq ; pop rax ; ret);
            let code = CompiledCode::new(ops.finalize().unwrap(), entry);
            assert_eq!(
                &code.bytes()[start..start + 4],
                &[0x48, 0x83, 0xf8, immediate as u8]
            );
            // SAFETY: this System V entry accepts and compares one u64,
            // preserves the stack/callee-saved registers and returns flags.
            // Its executable mapping remains live through every invocation.
            let run: extern "sysv64" fn(u64) -> u64 =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            for value in [
                immediate.wrapping_sub(1),
                immediate,
                immediate + 1,
                (1 << 8) | immediate,
                (1 << 32) | immediate,
                (1 << 63) | immediate,
                u64::MAX,
            ] {
                let difference = value.wrapping_sub(immediate);
                let overflow = (value ^ immediate) & (value ^ difference) & (1 << 63) != 0;
                let expected = u64::from(value < immediate)
                    | (u64::from(difference == 0) << 6)
                    | ((difference >> 63) << 7)
                    | (u64::from(overflow) << 11);
                assert_eq!(
                    run(value) & FLAGS,
                    expected,
                    "{value:#x} versus {immediate}"
                );
            }
        }
    }

    #[test]
    fn value_immediates_zero_extend_u32_and_preserve_prepared_flags() {
        for register in [0, 1, 2, 6, 8, 9, 10, 11] {
            for value in [
                0,
                VALUE_TRUE,
                VALUE_UNDEFINED,
                i32::MAX as u64,
                i32::MAX as u64 + 1,
                u32::MAX as u64,
                u32::MAX as u64 + 1,
                NUMBER_TAG,
                u64::MAX,
            ] {
                let mut ops = Assembler::new().unwrap();
                let entry = ops.offset();
                let flags_register = if register == 11 { 10 } else { 11 };
                dynasm!(ops ; .arch x64
                    ; mov Rq(register), QWORD -1
                    ; mov Rd(flags_register), DWORD 41
                    ; cmp Rd(flags_register), BYTE 41
                );
                let start = ops.offset().0;
                emit_load_u64(&mut ops, register, value);
                let encoded = ops.offset().0 - start;
                let expected = if value <= u32::MAX as u64 {
                    5 + usize::from(register >= 8)
                } else {
                    10
                };
                assert_eq!(encoded, expected, "r{register} immediate {value:#x}");
                dynasm!(ops ; .arch x64
                    ; jne >failed
                    ; mov [rdi], Rq(register)
                    ; mov eax, 1
                    ; ret
                    ; failed:
                    ; xor eax, eax
                    ; ret
                );
                let code = CompiledCode::new(ops.finalize().unwrap(), entry);
                // SAFETY: the generated System V entry writes one owned u64,
                // uses only caller-saved registers, and returns before this
                // executable mapping is released.
                let run: extern "sysv64" fn(*mut u64) -> u64 =
                    unsafe { std::mem::transmute(code.entry_ptr()) };
                let mut output = !value;
                assert_eq!(run(&mut output), 1, "MOV must preserve prepared flags");
                assert_eq!(output, value, "r{register} immediate {value:#x}");
            }
        }
    }

    #[test]
    fn symbolic_low_addresses_keep_fixed_width_relocation_encoding() {
        for register in [0, 10] {
            let mut normalized = None;
            for value in [0, u32::MAX as u64, u32::MAX as u64 + 1, u64::MAX] {
                let mut ops = Assembler::new().unwrap();
                let entry = ops.offset();
                let mut relocations = RelocationCapture::new(true);
                let start = ops.offset().0;
                emit_load_symbol_u64(
                    &mut ops,
                    &mut relocations,
                    register,
                    value,
                    RelocationTarget::SourceWorkCell { function_id: 9 },
                );
                assert_eq!(ops.offset().0 - start, 10);
                dynasm!(ops ; .arch x64 ; ret);
                let code = CompiledCode::new(ops.finalize().unwrap(), entry);
                let rendered = relocations
                    .render(code.bytes())
                    .expect("every symbolic address must validate as an exact imm64 move");
                if let Some(expected) = &normalized {
                    assert_eq!(&rendered.normalized_code, expected);
                } else {
                    normalized = Some(rendered.normalized_code);
                }
            }
        }
    }
}

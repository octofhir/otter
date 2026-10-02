//! System V x86-64 template code emission.
//!
//! # Contents
//! - Whole-function prologue, epilogue, control flow, and OSR trampolines.
//! - Tagged Number arithmetic, comparison, conversion, and truthiness paths.
//! - Prepared string-cell loads and full `+` semantics through the allocating
//!   concat packet and coercive runtime delegate.
//! - Monomorphic plain and bounded polymorphic method generated calls through
//!   the shared entry-cell, frame, deopt, and feedback contracts.
//! - Generated base/super constructor linkage with fixed or spread arguments
//!   and receiver-allocation fast paths.
//! - SELF context reads, unchecked context-slot access, and context
//!   allocation ([`context`]); checked and lookup binding accesses through
//!   the committed binding boundary.
//! - Iterator lifecycle, descriptor definitions, and class-value transitions
//!   through shared VM descriptors.
//! - Actual-argument collection and intrinsic-apply forwarding through the
//!   shared activation-window descriptors.
//! - Canonical indexed loads and stores through committed element descriptors.
//! - Named-property CacheIR hits for existing slots and allocation-free shape
//!   transitions, including receiver storage and write-barrier proofs, plus
//!   guarded lookup through pinned intrinsic prototypes.
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
//!   register window. Runtime calls obey the System V integer ABI and return
//!   two-word native results in `rax`/`rdx`.
//! - Allocating string calls publish the plan-owned safepoint before reentry;
//!   other coercive `+` cases complete through the shared runtime descriptor.
//! - Every generated callee entry uses the shared x86 tier-up mailbox protocol;
//!   nested callees cannot overwrite an outer pending promotion request.
//! - Every exact exit publishes the canonical instruction PC before returning.
//! - Forwarding probes reject sources requiring caller materialization before
//!   the committed value-span boundary can perform call effects.
//! - The stack is 16-byte aligned at every generated call boundary.
//!
//! # See also
//! - [`super::arm64`] for the peer target emitter.
//! - [`crate::entry`] for the shared compiled-entry ABI.

#![allow(clippy::useless_conversion)]

use std::collections::{BTreeMap, BTreeSet};

#[path = "x86_64/activation.rs"]
mod activation;
#[path = "x86_64/calls.rs"]
mod calls;
#[path = "x86_64/context.rs"]
mod context;
#[path = "x86_64/exceptions.rs"]
mod exceptions;
#[path = "x86_64/intrinsic_prototype.rs"]
pub(crate) mod intrinsic_prototype;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_bytecode::opcode_schema::{BindingRead, BindingSemantics, BindingWrite};
use otter_bytecode::scalar_semantics::{Int32ResultPolicy, NegativeZeroCondition};
use otter_vm::{JitCompileSnapshot, native_abi as abi, runtime_stubs::alloc_value_stub_by_id};

use super::{ArithKind, BitwiseKind, CompareKind, TemplateCode, TemplateOp, TemplatePlan};
use crate::{
    CompiledCode, Unsupported,
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget},
    },
    entry::{
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_SELF_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, THREAD_OFFSET,
        VALUE_FALSE, VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED,
        VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
        VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
};

const NUMBER_TAG: u64 = (NUMBER_TAG_HI16 as u64) << 48;
const DOUBLE_OFFSET: u64 = (DOUBLE_OFFSET_HI16 as u64) << 48;
const CANONICAL_NAN: u64 = (CANONICAL_NAN_HI16 as u64) << 48;
const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    artifact_request: Option<ArtifactRequest>,
    capture_events: bool,
) -> Result<NativeCompileOutput<TemplateCode>, Unsupported> {
    let plan = TemplatePlan::build(view)?;
    let tier_input = artifact_request.as_ref().map(|_| plan.render_artifact());

    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut relocations = RelocationCapture::new(artifact_request.is_some());
    let mut code_map = artifact_request.as_ref().map(|_| CodeMapCapture::default());
    let mut direct_call_events = capture_events.then(|| super::seed_direct_call_events(view));
    let mut load_ic_cells =
        vec![crate::entry::PropertySourceCell::default(); plan.load_property_count]
            .into_boxed_slice();
    let mut store_ic_cells =
        vec![crate::entry::PropertySourceCell::default(); plan.store_property_count]
            .into_boxed_slice();
    let mut next_load_ic = 0usize;
    let mut next_store_ic = 0usize;
    let type_mismatch = ops.new_dynamic_label();
    let identity_guard = ops.new_dynamic_label();
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
    let shape = activation::EntryShape::of(
        view,
        code_object_id,
        abi::NativeFrameKind::Baseline,
        !plan.safepoint_records.is_empty(),
    )?;
    let activation_exits = activation::ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    };
    // The tier entry continues a published interpreter frame; the call entry
    // builds this function's record and window and falls through.
    let entry = ops.offset();
    let body = ops.new_dynamic_label();
    activation::emit_tier_prologue(&mut ops);
    dynasm!(ops ; .arch x64 ; jmp =>body);
    let call_entry = (!plan.osr_only).then(|| activation::emit_call_entry(&mut ops, view, shape));
    dynasm!(ops ; .arch x64 ; =>body);
    let mut labelled = BTreeSet::new();

    for (operation_index, instruction) in plan.instructions.iter().enumerate() {
        if labelled.insert(instruction.pc) {
            let label = labels[&instruction.pc];
            dynasm!(ops ; .arch x64 ; =>label);
        }
        let instruction_start = ops.offset().0;
        if requires_pc_stamp(instruction.op) {
            emit_stamp_pc(&mut ops, instruction.pc);
        }
        match instruction.op {
            TemplateOp::LoadImmediate { dst, bits } => {
                emit_load_u64(&mut ops, 0, bits);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::Move { dst, src } => {
                emit_load_reg(&mut ops, 0, src);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::Jump { target, back_edge } => {
                if back_edge {
                    emit_backedge_poll(
                        &mut ops,
                        &mut relocations,
                        transitions.entry(abi::STUB_JIT_BACKEDGE_POLL),
                        target,
                        backedge_relink,
                        threw,
                        fatal,
                    );
                }
                let target = labels[&target];
                dynasm!(ops ; .arch x64 ; jmp =>target);
            }
            TemplateOp::Branch {
                condition,
                target,
                when_truthy,
                back_edge,
            } => {
                emit_load_reg(&mut ops, 0, condition);
                emit_truthiness_bool(&mut ops, type_mismatch);
                let fallthrough = ops.new_dynamic_label();
                emit_load_u64(
                    &mut ops,
                    11,
                    if when_truthy { VALUE_FALSE } else { VALUE_TRUE },
                );
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>fallthrough);
                if back_edge {
                    emit_backedge_poll(
                        &mut ops,
                        &mut relocations,
                        transitions.entry(abi::STUB_JIT_BACKEDGE_POLL),
                        target,
                        backedge_relink,
                        threw,
                        fatal,
                    );
                }
                let target = labels[&target];
                dynasm!(ops ; .arch x64 ; jmp =>target ; =>fallthrough);
            }
            TemplateOp::BranchNullish {
                condition,
                target,
                back_edge,
            } => {
                emit_load_reg(&mut ops, 0, condition);
                let taken = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                emit_load_u64(&mut ops, 11, VALUE_NULL);
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>taken);
                emit_load_u64(&mut ops, 11, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; jne =>done ; =>taken);
                if back_edge {
                    emit_backedge_poll(
                        &mut ops,
                        &mut relocations,
                        transitions.entry(abi::STUB_JIT_BACKEDGE_POLL),
                        target,
                        backedge_relink,
                        threw,
                        fatal,
                    );
                }
                let target = labels[&target];
                dynasm!(ops ; .arch x64 ; jmp =>target ; =>done);
            }
            TemplateOp::Truthiness { dst, src, negate } => {
                emit_load_reg(&mut ops, 0, src);
                emit_truthiness_bool(&mut ops, type_mismatch);
                if negate {
                    emit_load_u64(&mut ops, 11, VALUE_TRUE ^ VALUE_FALSE);
                    dynasm!(ops ; .arch x64 ; xor rax, r11);
                }
                emit_store_reg(&mut ops, 0, dst);
            }
            // The plan retains the unfused per-operation stream immediately
            // after this hint. x86-64 deliberately executes that stream until
            // its own register-pressure measurements justify a fused form.
            TemplateOp::FusedNumericChain { .. } => {}
            TemplateOp::BinaryArith {
                dst,
                lhs,
                rhs,
                kind,
            } => emit_binary_arith(&mut ops, dst, lhs, rhs, kind, type_mismatch),
            TemplateOp::AddGeneric {
                dst,
                lhs,
                rhs,
                concat_safepoint,
            } => emit_add_generic(
                &mut ops,
                &mut relocations,
                transitions,
                dst,
                lhs,
                rhs,
                concat_safepoint,
                threw,
                fatal,
            )?,
            TemplateOp::Compare {
                dst,
                lhs,
                rhs,
                kind,
            } => emit_compare(
                &mut ops,
                &mut relocations,
                dst,
                lhs,
                rhs,
                kind,
                type_mismatch,
            ),
            TemplateOp::LooseCompare {
                dst,
                lhs,
                rhs,
                negate,
            } => emit_loose_compare(&mut ops, dst, lhs, rhs, negate, type_mismatch),
            TemplateOp::TestTypeOf { dst, src, test } => {
                emit_test_typeof(&mut ops, &mut relocations, dst, src, test, type_mismatch)
            }
            TemplateOp::IntBitwise {
                dst,
                lhs,
                rhs,
                kind,
            } => emit_bitwise(&mut ops, dst, lhs, rhs, kind, type_mismatch),
            TemplateOp::UnsignedShiftRight { dst, lhs, rhs } => {
                emit_unsigned_shift(&mut ops, dst, lhs, rhs, type_mismatch)
            }
            TemplateOp::Increment { dst, src, delta } => {
                emit_increment(&mut ops, dst, src, delta, type_mismatch)
            }
            TemplateOp::Negate { dst, src } => emit_negate(&mut ops, dst, src, type_mismatch),
            TemplateOp::BitwiseNot { dst, src } => {
                emit_load_reg(&mut ops, 0, src);
                emit_to_int32(&mut ops, 0, 10, type_mismatch);
                dynasm!(ops ; .arch x64 ; not r10d);
                emit_box_int32(&mut ops, 10, 0);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::ToNumeric { dst, src } => {
                emit_load_reg(&mut ops, 0, src);
                emit_guard_number(&mut ops, 0, type_mismatch);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::ToPrimitive { dst, src, .. } => {
                emit_load_reg(&mut ops, 0, src);
                emit_load_u64(&mut ops, 11, NOT_CELL_MASK);
                dynasm!(ops
                    ; .arch x64
                    ; mov r10, rax
                    ; and r10, r11
                    ; test r10, r10
                    ; jz =>type_mismatch
                );
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::LoadThis { dst } => {
                dynasm!(ops ; .arch x64 ; mov rax, [r14 + NATIVE_FRAME_THIS_OFFSET as i32]);
                emit_load_u64(&mut ops, 11, VALUE_HOLE);
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>type_mismatch);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::LoadSelfClosure { dst } => {
                dynasm!(ops ; .arch x64 ; mov rax, [r14 + NATIVE_FRAME_SELF_OFFSET as i32]);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::LoadClosureContext { dst } => {
                context::emit_load_closure_context(&mut ops, view, dst)?;
            }
            TemplateOp::LoadContextSlot {
                dst,
                context,
                depth,
                slot,
            } => context::emit_load_context_slot(&mut ops, view, dst, context, depth, slot)?,
            TemplateOp::StoreContextSlot {
                src,
                context,
                depth,
                slot,
            } => context::emit_store_context_slot(
                &mut ops,
                &mut relocations,
                view,
                src,
                context,
                depth,
                slot,
            )?,
            TemplateOp::CreateContext {
                dst,
                parent,
                scope,
                safepoint,
            } => context::emit_context_allocation(
                &mut ops,
                &mut relocations,
                view,
                dst,
                context::ContextAllocation::Create { parent, scope },
                safepoint,
                allocation_miss,
            )?,
            TemplateOp::CopyContext {
                dst,
                src,
                safepoint,
            } => context::emit_context_allocation(
                &mut ops,
                &mut relocations,
                view,
                dst,
                context::ContextAllocation::Copy { source: src },
                safepoint,
                allocation_miss,
            )?,
            TemplateOp::ClassSuperConstructor { dst, class } => {
                emit_load_reg(&mut ops, 6, class);
                dynasm!(ops ; .arch x64 ; mov rdi, r15);
                emit_load_runtime_stub(
                    &mut ops,
                    &mut relocations,
                    transitions.variadic_entry(abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR),
                    abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR,
                );
                dynasm!(ops ; .arch x64 ; call r11);
                emit_load_u64(&mut ops, 11, VALUE_HOLE);
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>runtime_transition);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::MakeFunction { dst, constant } => {
                let done = ops.new_dynamic_label();
                if let Some(&plan) = view.closure_allocations.get(&instruction.byte_pc) {
                    let slow = ops.new_dynamic_label();
                    emit_load_u64(&mut ops, 2, VALUE_UNDEFINED);
                    crate::x86_64::allocation::emit_closure(&mut ops, view, plan, 14, slow);
                    emit_store_reg(&mut ops, 0, dst);
                    dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
                }
                emit_make_function(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    dst,
                    constant,
                    threw,
                    fatal,
                );
                dynasm!(ops ; .arch x64 ; =>done);
            }
            TemplateOp::NewObject { dst } => emit_value_packet_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_NEW_OBJECT,
                &[],
                dst,
                committed_throw,
                fatal,
            )?,
            TemplateOp::CollectArguments { dst } => {
                emit_collect_arguments(&mut ops, &mut relocations, transitions, dst, threw, fatal)
            }
            TemplateOp::CallForwardArguments {
                dst,
                method,
                receiver,
                this_value,
            } => calls::emit_forward_call(
                &mut ops,
                &mut relocations,
                transitions,
                view,
                [dst, method, receiver, this_value],
                committed_throw,
                threw,
            )?,
            TemplateOp::NewArray { dst, elements } => {
                let words = plan
                    .register_tail(elements)
                    .iter()
                    .copied()
                    .map(PacketWord::Register)
                    .collect::<Vec<_>>();
                emit_value_packet_transition(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    abi::STUB_JIT_NEW_ARRAY,
                    &words,
                    dst,
                    committed_throw,
                    fatal,
                )?;
            }
            TemplateOp::NewObjectLiteral { dst, elements } => {
                let words = plan
                    .register_tail(elements)
                    .iter()
                    .copied()
                    .map(PacketWord::Register)
                    .collect::<Vec<_>>();
                emit_value_packet_transition(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    abi::STUB_JIT_NEW_OBJECT_LITERAL,
                    &words,
                    dst,
                    committed_throw,
                    fatal,
                )?;
            }
            TemplateOp::DefineDataProperty { object, key, value } => {
                dynasm!(ops
                    ; .arch x64
                    ; mov rdi, r15
                    ; mov esi, object as i32
                    ; mov edx, key as i32
                    ; mov ecx, value as i32
                );
                emit_load_runtime_stub(
                    &mut ops,
                    &mut relocations,
                    transitions.variadic_entry(abi::STUB_JIT_DEFINE_DATA_PROPERTY),
                    abi::STUB_JIT_DEFINE_DATA_PROPERTY,
                );
                dynasm!(ops ; .arch x64 ; call r11);
                emit_status_word_result(&mut ops, threw, fatal);
            }
            TemplateOp::DefineOwnProperty {
                target,
                key,
                descriptor,
            } => emit_define_own_property(
                &mut ops,
                &mut relocations,
                transitions,
                target,
                key,
                descriptor,
                threw,
                fatal,
            ),
            TemplateOp::ConstructOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_CONSTRUCT_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::ClassOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_CLASS_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::SpreadCallOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => calls::emit_spread_call_op(
                &mut ops,
                &mut relocations,
                transitions,
                opcode,
                arg0,
                arg1,
                arg2,
                committed_throw,
                threw,
            )?,
            TemplateOp::DeleteOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_DELETE_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::SuperOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_SUPER_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::PrivateOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_PRIVATE_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::ValueLoadOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_VALUE_LOAD_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::StructuralOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_STRUCTURAL_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::ModuleOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_MODULE_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::VariadicOp {
                opcode,
                prefix,
                argc,
                packed_args,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_VARIADIC_OP,
                opcode,
                u64::from(prefix),
                u64::from(argc),
                packed_args,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::StaticCallOp {
                opcode,
                packed_head,
                method,
                packed_args,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_STATIC_CALL_OP,
                opcode,
                packed_head,
                method,
                packed_args,
                runtime_transition,
                threw,
                fatal,
            ),
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
                dynasm!(ops ; .arch x64 ; mov rdi, r15);
                emit_load_u64(&mut ops, 6, packed_meta);
                emit_load_u64(&mut ops, 2, packed_args);
                emit_load_runtime_stub(
                    &mut ops,
                    &mut relocations,
                    transitions.variadic_entry(abi::STUB_JIT_BIND_FUNCTION),
                    abi::STUB_JIT_BIND_FUNCTION,
                );
                dynasm!(ops ; .arch x64 ; call r11);
                emit_side_exit_status_result(&mut ops, runtime_transition, threw, fatal);
            }
            TemplateOp::LoadRegExp { dst, constant } => emit_constant_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_LOAD_REGEXP,
                dst,
                constant,
                threw,
                fatal,
            ),
            TemplateOp::LoadBuiltinError { dst, constant } => emit_constant_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_LOAD_BUILTIN_ERROR,
                dst,
                constant,
                threw,
                fatal,
            ),
            TemplateOp::ArrayConstruct {
                dst,
                length,
                safepoint,
            } => emit_array_construct_alloc_call(
                &mut ops,
                &mut relocations,
                dst,
                length,
                safepoint,
                allocation_miss,
            )?,
            TemplateOp::ClassValueOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_CLASS_VALUE_OP,
                opcode,
                arg0,
                arg1,
                arg2,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::MakeClosure {
                dst,
                function,
                context,
            } => {
                let done = ops.new_dynamic_label();
                if let Some(&plan) = view.closure_allocations.get(&instruction.byte_pc) {
                    let slow = ops.new_dynamic_label();
                    emit_load_reg(&mut ops, 2, context);
                    crate::x86_64::allocation::emit_closure(&mut ops, view, plan, 14, slow);
                    emit_store_reg(&mut ops, 0, dst);
                    dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
                }
                emit_make_closure(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view.code_block.id,
                    dst,
                    function,
                    context,
                    threw,
                    fatal,
                );
                dynasm!(ops ; .arch x64 ; =>done);
            }
            TemplateOp::BindingValue {
                semantics,
                result,
                value0,
                value1,
                context_coord,
            } => {
                let checked = match (semantics, context_coord) {
                    (BindingSemantics::Read(BindingRead::ContextSlot { .. }), Some(coord)) => {
                        let dst = result.ok_or(Unsupported::OperandShape("context read result"))?;
                        Some((context::CheckedContextAccess::Load { dst }, value0, coord))
                    }
                    (BindingSemantics::Write(BindingWrite::ContextSlot { .. }), Some(coord)) => {
                        let src = value0.ok_or(Unsupported::OperandShape("context write value"))?;
                        Some((context::CheckedContextAccess::Store { src }, value1, coord))
                    }
                    _ => None,
                };
                let done = ops.new_dynamic_label();
                if let Some((access, register, coord)) = checked {
                    let miss = ops.new_dynamic_label();
                    context::emit_checked_context_slot(
                        &mut ops,
                        &mut relocations,
                        view,
                        access,
                        register.ok_or(Unsupported::OperandShape("checked context register"))?,
                        coord.depth,
                        coord.slot,
                        miss,
                    )?;
                    dynasm!(ops ; .arch x64 ; jmp =>done ; =>miss);
                }
                emit_committed_value2(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    abi::STUB_JIT_BINDING_VALUE,
                    result,
                    value0,
                    value1,
                    committed_throw,
                    fatal,
                );
                dynasm!(ops ; .arch x64 ; =>done);
            }
            TemplateOp::GlobalDeclarationValue { value0, value1, .. } => {
                emit_committed_value2(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    abi::STUB_JIT_GLOBAL_DECLARATION_VALUE,
                    None,
                    value0,
                    value1,
                    committed_throw,
                    fatal,
                );
            }
            TemplateOp::ObjectProtocolValue {
                operation: _,
                result,
                value0,
                value1,
            } => emit_committed_value2(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
                result,
                Some(value0),
                value1,
                committed_throw,
                fatal,
            ),
            TemplateOp::LoadStringConstant { dst } => {
                let target = view
                    .string_constant_cells
                    .get(&instruction.byte_pc)
                    .ok_or(Unsupported::OperandShape("prepared LoadString stable cell"))?;
                emit_load_symbol_u64(
                    &mut ops,
                    &mut relocations,
                    11,
                    target.cell_addr as u64,
                    RelocationTarget::StringConstantCell {
                        function_id: view.code_block.id,
                        byte_pc: instruction.byte_pc,
                    },
                );
                dynasm!(ops ; .arch x64 ; mov rax, [r11]);
                emit_store_reg(&mut ops, 0, dst);
            }
            TemplateOp::LoadProperty { dst, object, .. } => {
                let ordinal = u32::try_from(next_load_ic)
                    .map_err(|_| Unsupported::OperandShape("x86-64 property IC ordinal"))?;
                let cell = load_ic_cells
                    .get_mut(next_load_ic)
                    .ok_or(Unsupported::OperandShape("x86-64 property IC inventory"))?;
                next_load_ic += 1;
                cell.set_source(view.code_block.id, instruction.pc);
                emit_load_property(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    instruction.byte_pc,
                    dst,
                    object,
                    cell as *mut crate::entry::PropertySourceCell as u64,
                    ordinal,
                    view.property_programs
                        .get(&instruction.byte_pc)
                        .map(Vec::as_slice),
                    committed_throw,
                    fatal,
                );
            }
            TemplateOp::StoreProperty { object, value, .. } => {
                let ordinal = u32::try_from(next_store_ic)
                    .map_err(|_| Unsupported::OperandShape("x86-64 property IC ordinal"))?;
                let cell = store_ic_cells
                    .get_mut(next_store_ic)
                    .ok_or(Unsupported::OperandShape("x86-64 property IC inventory"))?;
                next_store_ic += 1;
                cell.set_source(view.code_block.id, instruction.pc);
                emit_store_property(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    object,
                    value,
                    cell as *mut crate::entry::PropertySourceCell as u64,
                    ordinal,
                    view.property_programs
                        .get(&instruction.byte_pc)
                        .map(Vec::as_slice),
                    committed_throw,
                    fatal,
                );
            }
            TemplateOp::LoadElement {
                dst,
                receiver,
                index,
            } => emit_load_element(
                &mut ops,
                &mut relocations,
                transitions,
                dst,
                receiver,
                index,
                committed_throw,
                fatal,
            ),
            TemplateOp::StoreElement {
                receiver,
                index,
                value,
            } => emit_store_element(
                &mut ops,
                &mut relocations,
                transitions,
                receiver,
                index,
                value,
                committed_throw,
                fatal,
            ),
            TemplateOp::Call {
                dst,
                callee,
                argc,
                packed_args,
                byte_pc,
            } => {
                let arguments = plan.call_argument_registers(argc, packed_args);
                if let Some(target) = view.static_native_calls.get(&byte_pc) {
                    let name = abi::runtime_stub_name(target.leaf_stub_id);
                    if crate::machine::native_leaf::supports_site(view, *target, arguments.len()) {
                        let start = ops.offset().0;
                        emit_load_reg(&mut ops, 10, callee);
                        for (index, &argument) in arguments.iter().enumerate() {
                            emit_load_reg(&mut ops, if index == 0 { 6 } else { 2 }, argument);
                        }
                        crate::machine::native_leaf::x86_64::emit_guard(
                            &mut ops,
                            view,
                            target.builtin_native_ref,
                            type_mismatch,
                        );
                        crate::machine::native_leaf::x86_64::emit_tagged_call(
                            &mut ops,
                            &mut relocations,
                            target.leaf_stub_id,
                            target.argument_count,
                            type_mismatch,
                        )?;
                        emit_store_reg(&mut ops, 0, dst);
                        if let Some(code_map) = code_map.as_mut() {
                            code_map.record(CodeRegion::static_native_structural(
                                "nativeLeafCall",
                                start,
                                ops.offset().0,
                                view.code_block.id,
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
                        continue;
                    }
                    if let Some(events) = direct_call_events.as_mut() {
                        events.insert(
                            (byte_pc, 0),
                            otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                                instruction_pc: instruction.pc,
                                byte_pc,
                                target: name,
                                outcome:
                                    otter_vm::JitStaticNativeCallLoweringOutcome::Rejected {
                                        reason: otter_vm::JitStaticNativeCallLoweringRejectionReason::ArityUnsupported,
                                    },
                            },
                        );
                    }
                }
                let known = view
                    .direct_callees
                    .get(&byte_pc)
                    .filter(|targets| targets.len() == 1)
                    .map(|targets| targets[0].plan);
                let start = ops.offset().0;
                calls::emit_call(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    Some(callee),
                    None,
                    calls::CallNewTarget::None,
                    &arguments,
                    known,
                    instruction.pc,
                    dst,
                    committed_throw,
                    threw,
                )?;
                if let Some(code_map) = code_map.as_mut() {
                    code_map.record(CodeRegion::call_structural(
                        "callTrampoline",
                        start,
                        ops.offset().0,
                        view.code_block.id,
                        instruction.pc,
                        byte_pc,
                        known.map(|plan| plan.function_id),
                    ));
                }
            }
            TemplateOp::CallWithThis {
                dst,
                callee,
                this_value,
                argc,
                packed_args,
                byte_pc: _,
            } => {
                let arguments = plan.call_argument_registers(argc, packed_args);
                calls::emit_call(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    Some(callee),
                    Some(this_value),
                    calls::CallNewTarget::None,
                    &arguments,
                    None,
                    instruction.pc,
                    dst,
                    committed_throw,
                    threw,
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
                let arguments = plan.call_argument_registers(argc, packed_args);
                let known = view
                    .direct_callees
                    .get(&byte_pc)
                    .filter(|targets| targets.len() == 1)
                    .map(|targets| targets[0].plan);
                let start = ops.offset().0;
                calls::emit_call(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    Some(callee),
                    None,
                    if super_construct {
                        calls::CallNewTarget::Super
                    } else {
                        calls::CallNewTarget::Callee
                    },
                    &arguments,
                    known,
                    instruction.pc,
                    dst,
                    committed_throw,
                    threw,
                )?;
                if let Some(code_map) = code_map.as_mut() {
                    code_map.record(CodeRegion::call_structural(
                        "callTrampoline",
                        start,
                        ops.offset().0,
                        view.code_block.id,
                        instruction.pc,
                        byte_pc,
                        known.map(|plan| plan.function_id),
                    ));
                }
            }
            TemplateOp::MethodCall {
                dst,
                receiver,
                arguments,
                ..
            } => {
                let argument_registers = plan.register_tail(arguments);
                calls::emit_method_call(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    receiver,
                    argument_registers,
                    dst,
                    committed_throw,
                    threw,
                )?;
            }
            TemplateOp::Throw { src } => {
                emit_scalar_value(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    src,
                    Some(src),
                    None,
                    committed_throw,
                    fatal,
                );
                emit_load_reg(&mut ops, 0, src);
                dynasm!(ops ; .arch x64 ; jmp =>committed_throw);
            }
            TemplateOp::ScalarValue {
                operation,
                result,
                value0,
                value1,
            } => {
                let slow = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                if matches!(
                    operation,
                    abi::ScalarValueOp::LoadArgumentsLength
                        | abi::ScalarValueOp::LoadArgumentsElement
                ) {
                    if let Some(src) = value0 {
                        emit_load_reg(&mut ops, 9, src);
                    }
                    dynasm!(ops ; .arch x64 ; mov r11, r14);
                    crate::x86_64::arguments::emit(&mut ops, value0.map(|_| 9), slow);
                    emit_store_reg(&mut ops, 8, result);
                    dynasm!(ops ; .arch x64 ; jmp =>done);
                }
                dynasm!(ops ; .arch x64 ; =>slow);
                emit_scalar_value(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    result,
                    value0,
                    value1,
                    committed_throw,
                    fatal,
                );
                dynasm!(ops ; .arch x64 ; =>done);
            }
            TemplateOp::TdzError { local_index } => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::TdzError as u8,
                u64::from(local_index),
                runtime_transition,
                committed_throw,
                fatal,
            ),
            TemplateOp::IteratorNext {
                value_dst,
                done_dst,
                iterator,
            } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_ITERATOR_OP,
                otter_bytecode::Op::IteratorNext as u8,
                u64::from(value_dst),
                u64::from(done_dst),
                u64::from(iterator),
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::IteratorClose { iterator } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_ITERATOR_OP,
                otter_bytecode::Op::IteratorClose as u8,
                u64::from(iterator),
                0,
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::IteratorCloseThrow { iterator } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_ITERATOR_OP,
                otter_bytecode::Op::IteratorCloseThrow as u8,
                u64::from(iterator),
                0,
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::NoOp => {}
            TemplateOp::GetIterator { dst, src } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_ITERATOR_OP,
                otter_bytecode::Op::GetIterator as u8,
                u64::from(dst),
                u64::from(src),
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::GetAsyncIterator { dst, src } => emit_opcode_transition(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_ITERATOR_OP,
                otter_bytecode::Op::GetAsyncIterator as u8,
                u64::from(dst),
                u64::from(src),
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::Return { src } => {
                emit_load_reg(&mut ops, 0, src);
                dynasm!(ops ; .arch x64 ; jmp =>returned);
            }
            TemplateOp::ReturnUndefined => {
                emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; jmp =>returned);
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
                emit_load_reg(&mut ops, 0, value);
                emit_load_u64(&mut ops, 11, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; jne =>runtime_transition);
                context::emit_read_context_slot_rax(&mut ops, view, context, depth, slot)?;
                emit_load_u64(&mut ops, 11, VALUE_HOLE);
                dynasm!(ops
                    ; .arch x64
                    ; cmp rax, r11
                    ; je =>runtime_transition
                    ; jmp =>returned
                );
            }
            TemplateOp::UnsupportedBail => dynasm!(ops ; .arch x64 ; jmp =>unsupported),
        }
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

    dynasm!(ops
        ; .arch x64
        ; =>returned
        ; xor edx, edx
        ; jmp =>pair_exit
        ; =>committed_throw
        ; mov rsi, rax
        ; mov rdi, r15
    );
    emit_load_runtime_stub(
        &mut ops,
        &mut relocations,
        transitions.entry(abi::STUB_JIT_ROUTE_THROW),
        abi::STUB_JIT_ROUTE_THROW,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>activation_exits.side_exit
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>pair_exit
        ; cmp edx, abi::NativeResultStatus::Fatal as i32
        ; je =>pair_exit
        ; jmp =>fatal
        ; =>pair_exit
    );
    activation::emit_epilogue(&mut ops, activation_exits);
    emit_side_exit(
        &mut ops,
        activation_exits.side_exit,
        type_mismatch,
        abi::ExitReason::TypeMismatch,
        abi::ExitAction::Recompile,
    );
    emit_side_exit(
        &mut ops,
        activation_exits.side_exit,
        identity_guard,
        abi::ExitReason::IdentityGuard,
        abi::ExitAction::Recompile,
    );
    emit_side_exit(
        &mut ops,
        activation_exits.side_exit,
        allocation_miss,
        abi::ExitReason::AllocationMiss,
        abi::ExitAction::Resume,
    );
    emit_side_exit(
        &mut ops,
        activation_exits.side_exit,
        unsupported,
        abi::ExitReason::UnsupportedOperation,
        abi::ExitAction::Recompile,
    );
    emit_side_exit(
        &mut ops,
        activation_exits.side_exit,
        runtime_transition,
        abi::ExitReason::RuntimeTransition,
        abi::ExitAction::Resume,
    );
    emit_side_exit(
        &mut ops,
        activation_exits.side_exit,
        backedge_relink,
        abi::ExitReason::Interrupt,
        abi::ExitAction::Resume,
    );
    dynasm!(ops
        ; .arch x64
        ; =>threw
        ; mov rdi, r15
    );
    emit_load_runtime_stub(
        &mut ops,
        &mut relocations,
        transitions.entry(abi::STUB_JIT_FINISH_ERROR),
        abi::STUB_JIT_FINISH_ERROR,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>activation_exits.side_exit
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>pair_exit
        ; cmp edx, abi::NativeResultStatus::Fatal as i32
        ; je =>pair_exit
        ; jmp =>fatal
        ; =>fatal
    );
    emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; mov edx, abi::NativeResultStatus::Fatal as i32);
    activation::emit_epilogue(&mut ops, activation_exits);
    activation::emit_exits(
        &mut ops,
        &mut relocations,
        transitions,
        view,
        shape.derived,
        activation_exits,
    );
    if let Some((_, cold)) = call_entry {
        activation::emit_call_entry_cold(
            &mut ops,
            &mut relocations,
            transitions,
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
        activation::emit_tier_prologue(&mut ops);
        dynasm!(ops ; .arch x64 ; jmp =>target);
        if let Some(code_map) = code_map.as_mut() {
            code_map.record_osr(header_pc, offset, ops.offset().0);
        }
        osr_entries.insert(header_pc, offset);
    }

    let buffer = crate::entry::finalize_assembler(ops)?;
    let TemplatePlan {
        register_operands,
        mut safepoint_records,
        osr_only,
        instructions,
        ..
    } = plan;
    safepoint_records.sort_by_key(|record| record.id);
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
        )
    });
    let code = TemplateCode::from_emission(
        compiled_code,
        code_object_id,
        view.code_block.id,
        Box::new([]),
        register_operands,
        load_ic_cells,
        store_ic_cells,
        safepoint_records.into_boxed_slice(),
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

fn emit_stamp_pc(ops: &mut Assembler, pc: u32) {
    dynasm!(ops ; .arch x64 ; mov DWORD [r14 + NATIVE_FRAME_PC_OFFSET as i32], pc as i32);
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
    dynasm!(ops
        ; .arch x64
        ; call r11
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
    emit_load_u64(ops, 11, VALUE_TRUE);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>truthy);
    emit_load_u64(ops, 11, VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>falsy);
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>falsy);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>falsy ; jmp =>bail ; =>int_case);
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
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: ArithKind,
    bail: DynamicLabel,
) {
    if kind == ArithKind::Rem {
        emit_remainder(ops, dst, lhs, rhs, bail);
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
        emit_box_int32(ops, 10, 0);
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
        emit_box_double(ops, 0, 0);
        emit_store_reg(ops, 0, dst);
        dynasm!(ops ; .arch x64 ; =>done);
        return;
    }
    emit_number_to_double(ops, 0, 0, bail);
    emit_number_to_double(ops, 8, 1, bail);
    dynasm!(ops ; .arch x64 ; divsd xmm0, xmm1);
    emit_box_double(ops, 0, 0);
    emit_store_reg(ops, 0, dst);
}

#[allow(clippy::too_many_arguments)]
fn emit_add_generic(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
    emit_box_int32(ops, 10, 0);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    emit_number_to_double(ops, 0, 0, runtime_path);
    emit_number_to_double(ops, 8, 1, runtime_path);
    dynasm!(ops ; .arch x64 ; addsd xmm0, xmm1);
    emit_box_double(ops, 0, 0);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>runtime_path);

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
        dynasm!(ops
            ; .arch x64
            ; call r11
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
    dynasm!(ops ; .arch x64 ; call r11);
    emit_status_word_result(ops, threw, fatal);
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

fn emit_remainder(ops: &mut Assembler, dst: u16, lhs: u16, rhs: u16, bail: DynamicLabel) {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_guard_int32_pair(ops, 0, 8, slow);
    dynasm!(ops
        ; .arch x64
        ; test r8d, r8d
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
    emit_box_int32(ops, 2, 0);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
    emit_load_reg(ops, 6, lhs);
    emit_load_reg(ops, 2, rhs);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
        ; mov r11, QWORD otter_vm::runtime_stubs::NUMBER_REM_LEAF.entry_addr() as i64
        ; call r11
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
) {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let numbers = ops.new_dynamic_label();
    let false_case = ops.new_dynamic_label();
    let true_case = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
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
        dynasm!(ops
            ; .arch x64
            ; call r11
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
            ; jz =>bail
            ; mov r10, r8
            ; and r10, r11
            ; test r10, r10
            ; jz =>bail
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
    dynasm!(ops ; .arch x64 ; jmp =>false_case ; =>true_case);
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
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; jne =>bail
        ; mov [r13 + i32::from(dst) * 8], rax
    );
}

fn emit_loose_compare(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    negate: bool,
    bail: DynamicLabel,
) {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let equal = ops.new_dynamic_label();
    let not_equal = ops.new_dynamic_label();
    let numeric = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; cmp rax, r8 ; je =>equal);
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je >lhs_nullish);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je >lhs_nullish ; jmp =>numeric ; lhs_nullish:);
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; cmp r8, r11 ; je =>equal);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; cmp r8, r11 ; je =>equal ; jmp =>not_equal ; =>numeric);
    emit_number_to_double(ops, 0, 0, bail);
    emit_number_to_double(ops, 8, 1, bail);
    dynasm!(ops ; .arch x64 ; ucomisd xmm0, xmm1 ; jp =>not_equal ; je =>equal ; =>not_equal);
    emit_load_u64(ops, 0, if negate { VALUE_TRUE } else { VALUE_FALSE });
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>equal);
    emit_load_u64(ops, 0, if negate { VALUE_FALSE } else { VALUE_TRUE });
    dynasm!(ops ; .arch x64 ; =>done);
    emit_store_reg(ops, 0, dst);
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
    emit_box_int32(ops, 12, 0);
    emit_store_reg(ops, 0, dst);
}

fn emit_unsigned_shift(ops: &mut Assembler, dst: u16, lhs: u16, rhs: u16, bail: DynamicLabel) {
    emit_load_reg(ops, 0, lhs);
    emit_to_int32(ops, 0, 12, bail);
    emit_load_reg(ops, 0, rhs);
    emit_to_int32(ops, 0, 1, bail);
    dynasm!(ops ; .arch x64 ; shr r12d, cl ; mov eax, r12d ; test eax, eax ; js >wide);
    emit_box_int32(ops, 0, 0);
    dynasm!(ops ; .arch x64 ; jmp >done ; wide: ; cvtsi2sd xmm0, rax);
    emit_box_double(ops, 0, 0);
    dynasm!(ops ; .arch x64 ; done:);
    emit_store_reg(ops, 0, dst);
}

fn emit_increment(ops: &mut Assembler, dst: u16, src: u16, delta: i32, bail: DynamicLabel) {
    emit_load_reg(ops, 0, src);
    let float_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_guard_int32(ops, 0, float_path);
    dynasm!(ops ; .arch x64 ; mov r10d, eax ; add r10d, delta ; jo =>float_path);
    emit_box_int32(ops, 10, 0);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
    emit_load_reg(ops, 0, src);
    emit_number_to_double(ops, 0, 0, bail);
    emit_load_u64(ops, 11, (delta as f64).to_bits());
    dynasm!(ops ; .arch x64 ; movq xmm1, r11 ; addsd xmm0, xmm1);
    emit_box_double(ops, 0, 0);
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
    emit_box_int32(ops, 10, 0);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>float_path);
    emit_load_reg(ops, 0, src);
    emit_number_to_double(ops, 0, 0, bail);
    emit_load_u64(ops, 11, 1_u64 << 63);
    dynasm!(ops ; .arch x64 ; movq xmm1, r11 ; xorpd xmm0, xmm1);
    emit_box_double(ops, 0, 0);
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

fn emit_box_int32(ops: &mut Assembler, source: u8, destination: u8) {
    dynasm!(ops ; .arch x64 ; mov Rd(destination), Rd(source));
    emit_load_u64(ops, 11, NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; or Rq(destination), r11);
}

fn emit_box_double(ops: &mut Assembler, source: u8, destination: u8) {
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; movq Rq(destination), Rx(source)
        ; ucomisd Rx(source), Rx(source)
        ; jnp =>ready
    );
    emit_load_u64(ops, destination, CANONICAL_NAN);
    dynasm!(ops ; .arch x64 ; =>ready);
    emit_load_u64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops ; .arch x64 ; add Rq(destination), r11);
}

fn emit_load_reg(ops: &mut Assembler, destination: u8, source: u16) {
    let offset = i32::from(source) * 8;
    dynasm!(ops ; .arch x64 ; mov Rq(destination), [r13 + offset]);
}

#[allow(clippy::too_many_arguments)]
fn emit_make_function(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_MAKE_FN),
        abi::STUB_JIT_MAKE_FN,
    );
    dynasm!(ops ; .arch x64 ; call r11);
    emit_status_word_result(ops, threw, fatal);
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
    dynasm!(ops ; .arch x64 ; call r11);
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
    dynasm!(ops ; .arch x64 ; call r11);
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
    dynasm!(ops ; .arch x64 ; call r11);
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
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; add rsp, ALLOC_CTX_STACK_SIZE as i32
        ; test rdx, rdx
        ; jne =>miss
    );
    emit_store_reg(ops, 0, dst);
    Ok(())
}

/// `MakeClosure dst, fn, ctx`: `esi` the compiling function id, `edx` the
/// destination, `ecx` the function constant, `r8d` the context register.
#[allow(clippy::too_many_arguments)]
fn emit_make_closure(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    code_block_id: u32,
    dst: u16,
    function: u32,
    context: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, code_block_id as i32
        ; mov edx, i32::from(dst)
        ; mov ecx, function as i32
        ; mov r8d, i32::from(context)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_MAKE_CLOSURE),
        abi::STUB_JIT_MAKE_CLOSURE,
    );
    dynasm!(ops ; .arch x64 ; call r11);
    emit_status_word_result(ops, threw, fatal);
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
    dynasm!(ops
        ; .arch x64
        ; call r11
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

#[allow(clippy::too_many_arguments)]
fn emit_store_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    object: u16,
    value: u16,
    cell_addr: u64,
    cell_ordinal: u32,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_existing_property_store(ops, relocations, view, object, value, programs, miss, done);
    dynasm!(ops ; .arch x64 ; =>miss);
    emit_load_reg(ops, 6, object);
    emit_load_reg(ops, 2, value);
    emit_load_symbol_u64(
        ops,
        relocations,
        1,
        cell_addr,
        RelocationTarget::PropertySourceCell {
            access: PropertySourceAccess::Store,
            ordinal: cell_ordinal,
        },
    );
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_STORE_PROPERTY),
        abi::STUB_JIT_STORE_PROPERTY,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; je >completed
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; completed:
        ; =>done
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_load_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    dst: u16,
    object: u16,
    cell_addr: u64,
    cell_ordinal: u32,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_existing_property_load(
        ops,
        relocations,
        view,
        byte_pc,
        object,
        programs,
        miss,
        done,
    );
    dynasm!(ops ; .arch x64 ; =>miss);
    emit_load_reg(ops, 6, object);
    emit_load_symbol_u64(
        ops,
        relocations,
        2,
        cell_addr,
        RelocationTarget::PropertySourceCell {
            access: PropertySourceAccess::Load,
            ordinal: cell_ordinal,
        },
    );
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_LOAD_PROPERTY),
        abi::STUB_JIT_LOAD_PROPERTY,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; je >completed
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; completed:
    );
    dynasm!(ops ; .arch x64 ; =>done);
    emit_store_reg(ops, 0, dst);
}

#[allow(clippy::too_many_arguments)]
fn emit_existing_property_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    object: u16,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    miss: DynamicLabel,
    done: DynamicLabel,
) {
    let Some(programs) = programs.filter(|programs| !programs.is_empty()) else {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    };
    emit_load_reg(ops, 0, object);
    let has_intrinsic = programs.iter().any(|program| {
        matches!(
            program.ops.first(),
            Some(otter_vm::JitCacheIrOp::LoadIntrinsicPrototype { .. })
        )
    });
    if !has_intrinsic {
        emit_template_object_header(ops, relocations, view, 0, 10, miss);
    }
    for program in programs {
        if !program
            .ops
            .iter()
            .any(|op| matches!(op, otter_vm::JitCacheIrOp::LoadField { .. }))
            || program.ops.iter().any(|op| {
                matches!(
                    op,
                    otter_vm::JitCacheIrOp::StoreField { .. }
                        | otter_vm::JitCacheIrOp::GuardExtensible { .. }
                        | otter_vm::JitCacheIrOp::PublishShape { .. }
                )
            })
        {
            continue;
        }
        let next = ops.new_dynamic_label();
        let intrinsic = matches!(
            program.ops.first(),
            Some(otter_vm::JitCacheIrOp::LoadIntrinsicPrototype { .. })
        );
        if has_intrinsic && !intrinsic {
            emit_template_object_header(ops, relocations, view, 0, 10, next);
        }
        let mut terminal = false;
        for op in &program.ops {
            match *op {
                otter_vm::JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target,
                } => {
                    intrinsic_prototype::emit(ops, relocations, view, target, byte_pc, 0, next);
                    dynasm!(ops
                        ; .arch x64
                        ; cmp BYTE [r8], OBJECT_BODY_TYPE_TAG as i8
                        ; jne =>next
                    );
                }
                otter_vm::JitCacheIrOp::LoadPrototypeHolder { root, result: 1 } => {
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        9,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    emit_load_u64(ops, 8, u64::from(root));
                    dynasm!(ops ; .arch x64 ; mov r8d, [r9 + r8 + view.shape_prototype_byte as i32] ; add r8, r9);
                }
                otter_vm::JitCacheIrOp::GuardPrototypeValidity { validity } => {
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
                otter_vm::JitCacheIrOp::GuardShape { object, shape } => {
                    let header = if object == 0 { 10 } else { 8 };
                    if intrinsic {
                        // Realm prototypes may have benign symbol sidecars.
                        // The live ordinary lookup flags, not sidecar absence,
                        // determine whether their shape slots remain valid.
                        emit_template_shape_state_guard(ops, view, header, next);
                        emit_template_flags_guard(
                            ops,
                            view,
                            header,
                            otter_vm::jit::JIT_OBJECT_FLAG_SLOT_ATTRS_OVERRIDDEN,
                            next,
                        );
                        emit_template_shape_identity_guard(ops, view, header, shape, next);
                    } else {
                        emit_template_shape_guard(ops, view, header, shape, next);
                    }
                }
                otter_vm::JitCacheIrOp::GuardDictionaryLayout { object: 1, layout } => {
                    // A dictionary holder keeps its slot-layout epoch in its
                    // sidecar.
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        11,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops
                        ; .arch x64
                        ; mov r9d, [r8 + view.object_shape_byte as i32]
                        ; test BYTE [r11 + r9 + view.shape_kind_byte as i32], otter_vm::jit::JIT_SHAPE_KIND_DICTIONARY as i8
                        ; jz =>next
                        ; mov r9d, [r8 + view.object_exotic_handle_byte as i32]
                        ; test r9d, r9d
                        ; jz =>next
                        ; add r9, r11
                        ; mov r11d, layout as u32 as i32
                        ; cmp [r9 + view.exotic_dictionary_layout_byte as i32], r11d
                        ; jne =>next
                    );
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot {
                    writable: false, ..
                } => {}
                otter_vm::JitCacheIrOp::LoadField { object, value_byte } => {
                    let header = if object == 0 { 10 } else { 8 };
                    emit_template_slab_base(ops, relocations, view, header, 11, 9);
                    dynasm!(ops
                        ; .arch x64
                        ; mov rax, [r11 + value_byte as i32]
                        ; jmp =>done
                    );
                    terminal = true;
                }
                _ => {
                    terminal = false;
                    break;
                }
            }
        }
        if terminal {
            dynasm!(ops ; .arch x64 ; =>next);
        }
    }
    dynasm!(ops ; .arch x64 ; jmp =>miss);
}

#[allow(clippy::too_many_arguments)]
fn emit_existing_property_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    object: u16,
    value: u16,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    miss: DynamicLabel,
    done: DynamicLabel,
) {
    let Some(programs) = programs.filter(|programs| !programs.is_empty()) else {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    };
    emit_load_reg(ops, 0, object);
    emit_template_object_header(ops, relocations, view, 0, 10, miss);
    emit_template_flags_guard(
        ops,
        view,
        10,
        otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE,
        miss,
    );
    for program in programs {
        if !program
            .ops
            .iter()
            .any(|op| matches!(op, otter_vm::JitCacheIrOp::StoreField { .. }))
        {
            continue;
        }
        let next = ops.new_dynamic_label();
        let mut terminal = false;
        let add_transition = program.ops.iter().any(|op| {
            matches!(
                op,
                otter_vm::JitCacheIrOp::GuardExtensible { .. }
                    | otter_vm::JitCacheIrOp::PublishShape { .. }
            )
        });
        for (index, op) in program.ops.iter().enumerate() {
            match *op {
                otter_vm::JitCacheIrOp::LoadPrototypeHolder { root, result: 1 } => {
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        9,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    emit_load_u64(ops, 8, u64::from(root));
                    dynasm!(ops ; .arch x64 ; mov r8d, [r9 + r8 + view.shape_prototype_byte as i32] ; add r8, r9);
                }
                otter_vm::JitCacheIrOp::GuardPrototypeValidity { validity } => {
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
                otter_vm::JitCacheIrOp::GuardShape { object, shape } => {
                    let header = if object == 0 { 10 } else { 8 };
                    if add_transition && object == 1 {
                        emit_template_shape_state_guard(ops, view, header, next);
                        emit_template_shape_identity_guard(ops, view, header, shape, next);
                    } else {
                        emit_template_shape_guard(ops, view, header, shape, next);
                    }
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot {
                    object,
                    writable: true,
                    ..
                } if object <= 1 => {}
                otter_vm::JitCacheIrOp::GuardPrototypeNull { object } => {
                    let header = if object == 0 { 10 } else { 8 };
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        9,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    emit_x64_load_prototype(ops, view, 11, header, 9);
                    dynasm!(ops ; .arch x64 ; test r11d, r11d ; jnz =>next);
                }
                otter_vm::JitCacheIrOp::GuardExtensible {
                    object: 0,
                    value_byte,
                } => {
                    let inline = ops.new_dynamic_label();
                    let storage_fits = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch x64
                        ; mov r11d, value_byte as i32
                        ; test r11d, 7
                        ; jnz =>next
                        ; mov r9d, [r10 + view.object_slab_handle_byte as i32]
                        ; test r9d, r9d
                        ; jz =>inline
                    );
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        8,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops
                        ; .arch x64
                        ; add r8, r9
                        ; mov r9d, [r8 + view.object_slab_capacity_byte as i32]
                        ; shr r11d, 3
                        ; cmp r11d, r9d
                        ; jae =>next
                        ; jmp =>storage_fits
                        ; =>inline
                        ; shr r11d, 3
                        ; movzx r9d, BYTE [r10 + view.object_inline_capacity_byte as i32]
                        ; cmp r11d, r9d
                        ; jae =>next
                        ; =>storage_fits
                        ; test BYTE [r10 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE as i8
                        ; jz =>next
                    );
                    // The program's receiver shape guard fixes the slot
                    // count at the appended index: the store is the exact
                    // append.
                }
                otter_vm::JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte,
                } => {
                    emit_template_slab_base(ops, relocations, view, 10, 11, 9);
                    emit_load_reg(ops, 2, value);
                    terminal = true;
                    if !matches!(
                        program.ops.get(index + 1),
                        Some(otter_vm::JitCacheIrOp::PublishShape { .. })
                    ) {
                        dynasm!(ops ; .arch x64 ; mov [r11 + value_byte as i32], rdx);
                        emit_template_value_barrier(ops, relocations, view, 10, 2);
                        dynasm!(ops ; .arch x64 ; jmp =>done);
                    }
                }
                otter_vm::JitCacheIrOp::PublishShape { object: 0, shape } if terminal => {
                    let Some(otter_vm::JitCacheIrOp::StoreField {
                        object: 0,
                        value_byte,
                    }) = index
                        .checked_sub(1)
                        .and_then(|index| program.ops.get(index))
                        .copied()
                    else {
                        terminal = false;
                        break;
                    };
                    dynasm!(ops
                        ; .arch x64
                        ; mov DWORD [r10 + view.object_shape_byte as i32], shape as i32
                        ; mov [r11 + value_byte as i32], rdx
                        ; sub rsp, 16
                        ; mov [rsp], r10
                        ; mov [rsp + 8], rdx
                    );
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        8,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops ; .arch x64 ; add r8, shape as i32);
                    emit_template_value_barrier(ops, relocations, view, 10, 8);
                    dynasm!(ops
                        ; .arch x64
                        ; mov r10, [rsp]
                        ; mov rdx, [rsp + 8]
                        ; add rsp, 16
                    );
                    emit_template_value_barrier(ops, relocations, view, 10, 2);
                    dynasm!(ops ; .arch x64 ; jmp =>done);
                }
                _ => {
                    terminal = false;
                    break;
                }
            }
        }
        if terminal {
            dynasm!(ops ; .arch x64 ; =>next);
        }
    }
    dynasm!(ops ; .arch x64 ; jmp =>miss);
}

fn emit_template_object_header(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    value: u8,
    header: u8,
    miss: DynamicLabel,
) {
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov Rq(header), Rq(value)
        ; test Rq(header), r11
        ; jnz =>miss
        ; mov Rd(header), Rd(header)
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        9,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add Rq(header), r9
        ; cmp BYTE [Rq(header)], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    emit_template_fast_state_guard(ops, view, header, miss);
}

fn emit_template_fast_state_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    miss: DynamicLabel,
) {
    emit_template_flags_guard(
        ops,
        view,
        header,
        otter_vm::jit::JIT_OBJECT_FLAG_DICTIONARY_COMPATIBLE
            | otter_vm::jit::JIT_OBJECT_FLAG_SLOT_ATTRS_OVERRIDDEN,
        miss,
    );
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [Rq(header) + view.object_exotic_handle_byte as i32], 0
        ; jne =>miss
    );
}

/// Branch to `miss` when any bit of `mask` is set in the object's flag byte.
fn emit_template_flags_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    mask: u8,
    miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; test BYTE [Rq(header) + view.object_flags_byte as i32], mask as i8
        ; jnz =>miss
    );
}

fn emit_template_shape_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    shape: u32,
    miss: DynamicLabel,
) {
    emit_template_fast_state_guard(ops, view, header, miss);
    emit_template_shape_identity_guard(ops, view, header, shape, miss);
}

fn emit_template_shape_state_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    miss: DynamicLabel,
) {
    emit_template_flags_guard(
        ops,
        view,
        header,
        otter_vm::jit::JIT_OBJECT_SHAPE_STATE_MASK,
        miss,
    );
}

fn emit_template_shape_identity_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    shape: u32,
    miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [Rq(header) + view.object_shape_byte as i32], shape as i32
        ; jne =>miss
    );
}

/// Load into `Rd(dst)` the compressed `[[Prototype]]` of the object whose
/// `GcHeader` pointer is in `Rq(object)`: the prototype word of its shape, an
/// ordinary object or null. `Rq(cage)` holds the cage base; `dst` may be
/// `object` but not `cage`.
pub(crate) fn emit_x64_load_prototype(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u8,
    object: u8,
    cage: u8,
) {
    debug_assert_ne!(dst, cage);
    dynasm!(ops
        ; .arch x64
        ; mov Rd(dst), [Rq(object) + view.object_shape_byte as i32]
        ; mov Rd(dst), [Rq(cage) + Rq(dst) + view.shape_prototype_byte as i32]
    );
}

/// Compute the slot base of the object whose `GcHeader` pointer is in
/// `header` into `destination` (`scratch` is clobbered). While the
/// out-of-line slab handle is null the slots are in-object at
/// `header + object_inline_values_byte`; a spilled object's slots are its
/// slab's words: cage base plus the compressed handle plus the slab's fixed
/// word offset.
pub(crate) fn emit_template_slab_base(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    header: u8,
    destination: u8,
    scratch: u8,
) {
    debug_assert_ne!(destination, scratch);
    debug_assert_ne!(header, scratch);
    let ready = ops.new_dynamic_label();
    let external = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov Rd(scratch), [Rq(header) + view.object_slab_handle_byte as i32]
        ; test Rd(scratch), Rd(scratch)
        ; jnz =>external
        ; lea Rq(destination), [Rq(header) + view.object_inline_values_byte as i32]
        ; jmp =>ready
        ; =>external
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        destination,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add Rq(destination), Rq(scratch)
        ; add Rq(destination), view.object_slab_words_byte as i32
        ; =>ready
    );
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
    dynasm!(ops ; .arch x64 ; call r11 ; =>done);
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
    dynasm!(ops ; .arch x64 ; call r11);
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
    dynasm!(ops ; .arch x64 ; call r11);
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
    dst: u16,
    receiver: u16,
    index: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_load_reg(ops, 6, receiver);
    emit_load_reg(ops, 2, index);
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_LOAD_ELEMENT),
        abi::STUB_JIT_LOAD_ELEMENT,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
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
    receiver: u16,
    index: u16,
    value: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
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
    dynasm!(ops
        ; .arch x64
        ; call r11
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
    dynasm!(ops
        ; .arch x64
        ; call r11
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

fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
}

fn emit_load_symbol_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_x86_imm64(start, ops.offset().0, register, target);
}

fn emit_load_runtime_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    address: u64,
    descriptor: abi::RuntimeStubDescriptor,
) {
    let start = ops.offset().0;
    dynasm!(ops ; .arch x64 ; mov r11, QWORD address as i64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(descriptor),
    );
}

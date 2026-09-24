//! System V x86-64 template code emission.
//!
//! # Contents
//! - Whole-function prologue, epilogue, control flow, and OSR trampolines.
//! - Tagged Number arithmetic, comparison, conversion, and truthiness paths.
//! - Prepared string-cell loads and full `+` semantics through the allocating
//!   concat packet and coercive runtime delegate.
//! - Monomorphic plain and bounded polymorphic method generated calls through
//!   the shared entry-cell, frame, deopt, and feedback contracts.
//! - Generated base/super constructor linkage with fixed or spread arguments,
//!   closure-capture refresh, and receiver-allocation fast paths.
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
//!
//! # Invariants
//! - The input is the same target-neutral [`super::TemplatePlan`] consumed by
//!   the AArch64 backend; operand decoding and branch validation are not
//!   repeated here.
//! - `r15` retains `JitCtx`, `r14` the active `NativeFrame`, and `r13` its
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

#[path = "x86_64/direct_call.rs"]
mod direct_call;
#[path = "x86_64/exceptions.rs"]
mod exceptions;
#[path = "x86_64/intrinsic_prototype.rs"]
pub(crate) mod intrinsic_prototype;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_bytecode::scalar_semantics::{Int32ResultPolicy, NegativeZeroCondition};
use otter_vm::{JitCompileSnapshot, native_abi as abi, runtime_stubs::alloc_value_stub_by_id};

use super::{ArithKind, BitwiseKind, CompareKind, TemplateCode, TemplateOp, TemplatePlan};
use crate::{
    CompiledCode, Unsupported,
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::{
            PropertySourceAccess, RelocationCapture, RelocationTarget, TemplateOperandArena,
            TemplateOperandRole,
        },
    },
    entry::{
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET,
        NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_FALSE, VALUE_HOLE, VALUE_NULL,
        VALUE_TRUE, VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
        VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
};

const NUMBER_TAG: u64 = (NUMBER_TAG_HI16 as u64) << 48;
const DOUBLE_OFFSET: u64 = (DOUBLE_OFFSET_HI16 as u64) << 48;
const CANONICAL_NAN: u64 = (CANONICAL_NAN_HI16 as u64) << 48;
const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;

/// Persistent machine-stack reservation held by [`emit_prologue`].
pub(super) const NATIVE_FRAME_BYTES: u32 = 40;

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    artifact_request: Option<ArtifactRequest>,
    capture_events: bool,
) -> Result<NativeCompileOutput<TemplateCode>, Unsupported> {
    let plan = TemplatePlan::build(view)?;
    let tier_input = artifact_request.as_ref().map(|_| plan.render_artifact());
    for instruction in &plan.instructions {
        if !supports(instruction.op) {
            return Err(Unsupported::Constraint {
                op: view
                    .instructions
                    .iter()
                    .find(|candidate| candidate.instruction_pc(&view.code_block) == instruction.pc)
                    .map_or(otter_bytecode::Op::Nop, |candidate| {
                        candidate.op(&view.code_block)
                    }),
                constraint: "opcode outside the x86-64 template emitter",
            });
        }
    }

    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut relocations = RelocationCapture::new(artifact_request.is_some());
    let mut code_map = artifact_request.as_ref().map(|_| CodeMapCapture::default());
    let mut diagnostics = capture_events.then(Vec::new);
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
    let entry = ops.offset();
    emit_prologue(&mut ops);
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
            TemplateOp::MakeFunction { dst, constant } => emit_make_function(
                &mut ops,
                &mut relocations,
                transitions,
                dst,
                constant,
                threw,
                fatal,
            ),
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
            } => emit_forward_call(
                &mut ops,
                &mut relocations,
                transitions,
                view,
                code_map.as_mut(),
                instruction.pc,
                dst,
                method,
                receiver,
                this_value,
                identity_guard,
                threw,
                committed_throw,
                fatal,
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
            TemplateOp::FreshUpvalue { index } => {
                emit_fresh_upvalue(&mut ops, &mut relocations, transitions, index, threw, fatal)
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
            } => emit_construct_op(
                &mut ops,
                &mut relocations,
                transitions,
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
            } => emit_class_op(
                &mut ops,
                &mut relocations,
                transitions,
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
            } => {
                let direct_done = ops.new_dynamic_label();
                let emitted_direct = if opcode == otter_bytecode::Op::CallSpread as u8 {
                    let lane = |index: usize| ((arg0 >> (index * 16)) & 0xffff) as u16;
                    direct_call::emit_spread_plain(
                        &mut ops,
                        &mut relocations,
                        transitions,
                        view,
                        lane(0),
                        lane(1),
                        lane(2),
                        lane(3),
                        instruction.pc,
                        instruction.byte_pc,
                        threw,
                        committed_throw,
                        fatal,
                        direct_done,
                    )?
                } else if opcode == otter_bytecode::Op::NewSpread as u8
                    || opcode == otter_bytecode::Op::SuperConstructSpread as u8
                {
                    direct_call::emit_spread_construct(
                        &mut ops,
                        &mut relocations,
                        transitions,
                        view,
                        code_map.as_mut(),
                        arg0 as u16,
                        arg1 as u16,
                        arg2 as u16,
                        instruction.pc,
                        instruction.byte_pc,
                        opcode == otter_bytecode::Op::SuperConstructSpread as u8,
                        committed_throw,
                        fatal,
                        direct_done,
                    )?
                } else {
                    false
                };
                emit_spread_call_op(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    runtime_transition,
                    threw,
                    fatal,
                );
                if emitted_direct {
                    dynasm!(ops ; .arch x64 ; =>direct_done);
                }
            }
            TemplateOp::ClassValueOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => emit_class_value_op(
                &mut ops,
                &mut relocations,
                transitions,
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
                parents,
            } => emit_make_closure(
                &mut ops,
                &mut relocations,
                transitions,
                view.code_block.id,
                dst,
                function,
                plan.index_tail(parents),
                parents,
                threw,
                fatal,
            )?,
            TemplateOp::BindingValue {
                result,
                value0,
                value1,
                ..
            } => emit_committed_value2(
                &mut ops,
                &mut relocations,
                transitions,
                abi::STUB_JIT_BINDING_VALUE,
                result,
                value0,
                value1,
                committed_throw,
                fatal,
            ),
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
                        if let Some(diagnostics) = diagnostics.as_mut() {
                            diagnostics.push(
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
                    if let Some(diagnostics) = diagnostics.as_mut() {
                        diagnostics.push(
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
                let direct_done = ops.new_dynamic_label();
                let emitted_direct = direct_call::emit_plain(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    dst,
                    callee,
                    &arguments,
                    instruction.pc,
                    byte_pc,
                    threw,
                    committed_throw,
                    fatal,
                    direct_done,
                )?;
                emit_generic_call_transition(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    dst,
                    callee,
                    None,
                    &arguments,
                    committed_throw,
                    fatal,
                )?;
                if emitted_direct {
                    dynasm!(ops ; .arch x64 ; =>direct_done);
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
                emit_generic_call_transition(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    dst,
                    callee,
                    Some(this_value),
                    &arguments,
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
                let arguments = plan.call_argument_registers(argc, packed_args);
                let direct_done = ops.new_dynamic_label();
                let emitted_direct = direct_call::emit_construct(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    code_map.as_mut(),
                    dst,
                    callee,
                    &arguments,
                    instruction.pc,
                    byte_pc,
                    super_construct,
                    committed_throw,
                    fatal,
                    direct_done,
                )?;
                emit_construct_transition(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    &plan,
                    dst,
                    callee,
                    argc,
                    packed_args,
                    super_construct,
                    runtime_transition,
                    threw,
                    fatal,
                )?;
                if emitted_direct {
                    dynasm!(ops ; .arch x64 ; =>direct_done);
                }
            }
            TemplateOp::MethodCall {
                dst,
                receiver,
                arguments,
                byte_pc,
                ..
            } => {
                let argument_registers = plan.register_tail(arguments);
                let direct_done = ops.new_dynamic_label();
                let emitted_direct = direct_call::emit_method(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    view,
                    dst,
                    receiver,
                    argument_registers,
                    instruction.pc,
                    byte_pc,
                    threw,
                    committed_throw,
                    fatal,
                    direct_done,
                )?;
                let mut words = Vec::with_capacity(arguments.len + 1);
                words.push(PacketWord::Register(receiver));
                words.extend(argument_registers.iter().copied().map(PacketWord::Register));
                emit_value_packet_transition(
                    &mut ops,
                    &mut relocations,
                    transitions,
                    abi::STUB_JIT_CALL_METHOD_VALUE,
                    &words,
                    dst,
                    committed_throw,
                    fatal,
                )?;
                if emitted_direct {
                    dynasm!(ops ; .arch x64 ; =>direct_done);
                }
            }
            TemplateOp::EnterTry {
                catch_pc,
                finally_pc,
                exception_register,
            } => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::EnterTry as u8,
                u64::from(catch_pc.unwrap_or(u32::MAX)),
                u64::from(finally_pc.unwrap_or(u32::MAX)),
                u64::from(exception_register),
                runtime_transition,
                returned,
                committed_throw,
                fatal,
            ),
            TemplateOp::LeaveTry => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::LeaveTry as u8,
                0,
                0,
                0,
                runtime_transition,
                returned,
                committed_throw,
                fatal,
            ),
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
                result,
                value0,
                value1,
                ..
            } => emit_scalar_value(
                &mut ops,
                &mut relocations,
                transitions,
                result,
                value0,
                value1,
                committed_throw,
                fatal,
            ),
            TemplateOp::TdzError { local_index } => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::TdzError as u8,
                u64::from(local_index),
                0,
                0,
                runtime_transition,
                returned,
                committed_throw,
                fatal,
            ),
            TemplateOp::EndFinally => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::EndFinally as u8,
                0,
                0,
                0,
                runtime_transition,
                returned,
                committed_throw,
                fatal,
            ),
            TemplateOp::PopParkedFinally { count } => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::PopParkedFinally as u8,
                u64::from(count),
                0,
                0,
                runtime_transition,
                returned,
                committed_throw,
                fatal,
            ),
            TemplateOp::IteratorNext {
                value_dst,
                done_dst,
                iterator,
            } => emit_iterator_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::IteratorNext as u8,
                u64::from(value_dst),
                u64::from(done_dst),
                u64::from(iterator),
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::IteratorClose { iterator } => emit_iterator_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::IteratorClose as u8,
                u64::from(iterator),
                0,
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::IteratorCloseStart { iterator } => emit_iterator_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::IteratorCloseStart as u8,
                u64::from(iterator),
                0,
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::IteratorCloseEnd { iterator } => emit_iterator_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::IteratorCloseEnd as u8,
                u64::from(iterator),
                0,
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::JumpViaFinally { target, floor } => exceptions::emit_exception_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::JumpViaFinally as u8,
                u64::from(target),
                u64::from(floor),
                0,
                runtime_transition,
                returned,
                committed_throw,
                fatal,
            ),
            TemplateOp::NoOp => {}
            TemplateOp::GetIterator { dst, src } => emit_iterator_op(
                &mut ops,
                &mut relocations,
                transitions,
                otter_bytecode::Op::GetIterator as u8,
                u64::from(dst),
                u64::from(src),
                0,
                runtime_transition,
                threw,
                fatal,
            ),
            TemplateOp::GetAsyncIterator { dst, src } => emit_iterator_op(
                &mut ops,
                &mut relocations,
                transitions,
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
            TemplateOp::UnsupportedBail => dynasm!(ops ; .arch x64 ; jmp =>unsupported),
            _ => unreachable!("support preflight and emission must stay exhaustive"),
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
        ; je =>pair_exit
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>pair_exit
        ; cmp edx, abi::NativeResultStatus::Fatal as i32
        ; je =>pair_exit
        ; jmp =>fatal
        ; =>pair_exit
    );
    emit_epilogue(&mut ops);
    emit_side_exit(
        &mut ops,
        type_mismatch,
        abi::ExitReason::TypeMismatch,
        abi::ExitAction::Recompile,
    );
    emit_side_exit(
        &mut ops,
        identity_guard,
        abi::ExitReason::IdentityGuard,
        abi::ExitAction::Recompile,
    );
    emit_side_exit(
        &mut ops,
        unsupported,
        abi::ExitReason::UnsupportedOperation,
        abi::ExitAction::Recompile,
    );
    emit_side_exit(
        &mut ops,
        runtime_transition,
        abi::ExitReason::RuntimeTransition,
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
        ; je =>pair_exit
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>pair_exit
        ; cmp edx, abi::NativeResultStatus::Fatal as i32
        ; je =>pair_exit
        ; jmp =>fatal
        ; =>fatal
    );
    emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; mov edx, abi::NativeResultStatus::Fatal as i32);
    emit_epilogue(&mut ops);

    let mut osr_entries = BTreeMap::new();
    for &header_pc in view.code_block.loop_headers() {
        let Some(&target) = labels.get(&header_pc) else {
            continue;
        };
        let offset = ops.offset().0;
        emit_prologue(&mut ops);
        dynasm!(ops ; .arch x64 ; jmp =>target);
        if let Some(code_map) = code_map.as_mut() {
            code_map.record_osr(header_pc, offset, ops.offset().0);
        }
        osr_entries.insert(header_pc, offset);
    }

    let buffer = ops
        .finalize()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::Finalization))?;
    let TemplatePlan {
        register_count,
        register_operands,
        index_operands,
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
        register_count,
        Box::new([]),
        register_operands,
        index_operands,
        load_ic_cells,
        store_ic_cells,
        safepoint_records.into_boxed_slice(),
        osr_entries,
        osr_only,
    );
    Ok(NativeCompileOutput {
        code,
        artifact,
        diagnostics: diagnostics.map(Vec::into_boxed_slice).unwrap_or_default(),
        ir_node_count: instructions.len() as u64,
    })
}

fn supports(op: TemplateOp) -> bool {
    matches!(
        op,
        TemplateOp::LoadImmediate { .. }
            | TemplateOp::Move { .. }
            | TemplateOp::Jump { .. }
            | TemplateOp::Branch { .. }
            | TemplateOp::BranchNullish { .. }
            | TemplateOp::Truthiness { .. }
            | TemplateOp::FusedNumericChain { .. }
            | TemplateOp::BinaryArith { .. }
            | TemplateOp::Compare { .. }
            | TemplateOp::LooseCompare { .. }
            | TemplateOp::IntBitwise { .. }
            | TemplateOp::UnsignedShiftRight { .. }
            | TemplateOp::Increment { .. }
            | TemplateOp::Negate { .. }
            | TemplateOp::BitwiseNot { .. }
            | TemplateOp::ToNumeric { .. }
            | TemplateOp::ToPrimitive { .. }
            | TemplateOp::AddGeneric { .. }
            | TemplateOp::LoadThis { .. }
            | TemplateOp::LoadSelfClosure { .. }
            | TemplateOp::ClassSuperConstructor { .. }
            | TemplateOp::MakeFunction { .. }
            | TemplateOp::NewObject { .. }
            | TemplateOp::CollectArguments { .. }
            | TemplateOp::CallForwardArguments { .. }
            | TemplateOp::NewArray { .. }
            | TemplateOp::FreshUpvalue { .. }
            | TemplateOp::DefineDataProperty { .. }
            | TemplateOp::DefineOwnProperty { .. }
            | TemplateOp::ConstructOp { .. }
            | TemplateOp::ClassOp { .. }
            | TemplateOp::SpreadCallOp { .. }
            | TemplateOp::ClassValueOp { .. }
            | TemplateOp::MakeClosure { .. }
            | TemplateOp::BindingValue { .. }
            | TemplateOp::GlobalDeclarationValue { .. }
            | TemplateOp::ObjectProtocolValue { .. }
            | TemplateOp::LoadStringConstant { .. }
            | TemplateOp::LoadProperty { .. }
            | TemplateOp::StoreProperty { .. }
            | TemplateOp::LoadElement { .. }
            | TemplateOp::StoreElement { .. }
            | TemplateOp::Call { .. }
            | TemplateOp::CallWithThis { .. }
            | TemplateOp::Construct { .. }
            | TemplateOp::MethodCall { .. }
            | TemplateOp::EnterTry { .. }
            | TemplateOp::LeaveTry
            | TemplateOp::Throw { .. }
            | TemplateOp::ScalarValue { .. }
            | TemplateOp::TdzError { .. }
            | TemplateOp::EndFinally
            | TemplateOp::PopParkedFinally { .. }
            | TemplateOp::JumpViaFinally { .. }
            | TemplateOp::IteratorNext { .. }
            | TemplateOp::IteratorClose { .. }
            | TemplateOp::IteratorCloseStart { .. }
            | TemplateOp::IteratorCloseEnd { .. }
            | TemplateOp::GetIterator { .. }
            | TemplateOp::GetAsyncIterator { .. }
            | TemplateOp::NoOp
            | TemplateOp::Return { .. }
            | TemplateOp::ReturnUndefined
            | TemplateOp::UnsupportedBail
    )
}

fn requires_pc_stamp(op: TemplateOp) -> bool {
    !matches!(
        op,
        TemplateOp::LoadImmediate { .. }
            | TemplateOp::Move { .. }
            | TemplateOp::LoadSelfClosure { .. }
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

fn emit_prologue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch x64
        ; push rbp
        ; mov rbp, rsp
        ; push r12
        ; push r13
        ; push r14
        ; push r15
        ; mov r15, rdi
        ; mov r14, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
    );
}

fn emit_epilogue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch x64
        ; pop r15
        ; pop r14
        ; pop r13
        ; pop r12
        ; pop rbp
        ; ret
    );
}

fn emit_stamp_pc(ops: &mut Assembler, pc: u32) {
    dynasm!(ops ; .arch x64 ; mov DWORD [r14 + NATIVE_FRAME_PC_OFFSET as i32], pc as i32);
}

fn emit_side_exit(
    ops: &mut Assembler,
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
        ; mov edx, abi::NativeResultStatus::SideExit as i32
    );
    emit_epilogue(ops);
}

fn emit_backedge_poll(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    poll_entry: u64,
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
        ; mov rdi, r15
    );
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
        ; jmp =>fatal
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
        dynasm!(ops
            ; .arch x64
            ; =>cell_path
            ; cmp rax, r8
            ; jne =>leaf_call
        );
        if kind == CompareKind::Eq {
            dynasm!(ops ; .arch x64 ; jmp =>true_case);
        } else {
            dynasm!(ops ; .arch x64 ; jmp =>false_case);
        }
        dynasm!(ops
            ; .arch x64
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

fn emit_fresh_upvalue(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    index: i32,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, index
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_FRESH_UPVALUE),
        abi::STUB_JIT_FRESH_UPVALUE,
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

#[allow(clippy::too_many_arguments)]
fn emit_construct_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_CONSTRUCT_OP),
        abi::STUB_JIT_CONSTRUCT_OP,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
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

#[allow(clippy::too_many_arguments)]
fn emit_class_value_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_CLASS_VALUE_OP),
        abi::STUB_JIT_CLASS_VALUE_OP,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
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

#[allow(clippy::too_many_arguments)]
fn emit_class_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_CLASS_OP),
        abi::STUB_JIT_CLASS_OP,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
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

#[allow(clippy::too_many_arguments)]
fn emit_spread_call_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_SPREAD_CALL_OP),
        abi::STUB_JIT_SPREAD_CALL_OP,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
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

#[allow(clippy::too_many_arguments)]
fn emit_iterator_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_ITERATOR_OP),
        abi::STUB_JIT_ITERATOR_OP,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
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

fn emit_make_closure(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    code_block_id: u32,
    dst: u16,
    function: u32,
    parents: &[u32],
    parents_tail: super::TemplateTail,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, code_block_id as i32
        ; mov edx, i32::from(dst)
        ; mov ecx, function as i32
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        8,
        parents.as_ptr() as u64,
        RelocationTarget::TemplateOperandSlice {
            arena: TemplateOperandArena::Indices,
            role: TemplateOperandRole::ClosureParents,
            start: u32::try_from(parents_tail.start)
                .map_err(|_| Unsupported::OperandShape("x86-64 closure parent start"))?,
            len: u32::try_from(parents_tail.len)
                .map_err(|_| Unsupported::OperandShape("x86-64 closure parent count"))?,
        },
    );
    emit_load_u64(ops, 9, parents.len() as u64);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_MAKE_CLOSURE),
        abi::STUB_JIT_MAKE_CLOSURE,
    );
    dynasm!(ops ; .arch x64 ; call r11);
    emit_status_word_result(ops, threw, fatal);
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
                otter_vm::JitCacheIrOp::GuardShape { object, shape } => {
                    let header = if object == 0 { 10 } else { 8 };
                    if intrinsic {
                        // Realm prototypes may have benign symbol sidecars.
                        // The live ordinary lookup flags, not sidecar absence,
                        // determine whether their shape slots remain valid.
                        emit_template_shape_state_guard(ops, view, header, next);
                        dynasm!(ops
                            ; .arch x64
                            ; cmp BYTE [Rq(header) + view.object_slot_attrs_overridden_byte as i32], 0
                            ; jne =>next
                        );
                        emit_template_shape_identity_guard(ops, view, header, shape, next);
                    } else {
                        emit_template_shape_guard(ops, view, header, shape, next);
                    }
                }
                otter_vm::JitCacheIrOp::GuardDictionaryLayout { object: 1, layout } => {
                    dynasm!(ops
                        ; .arch x64
                        ; cmp DWORD [r8 + view.object_shape_byte as i32], 0
                        ; jne =>next
                        ; mov r11, QWORD layout as i64
                        ; cmp [r8 + view.object_dictionary_shape_id_byte as i32], r11
                        ; jne =>next
                    );
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot {
                    writable: false, ..
                } => {}
                otter_vm::JitCacheIrOp::LoadPrototype {
                    object: 0,
                    result: 1,
                } => emit_template_prototype(ops, relocations, view, 10, false, next),
                otter_vm::JitCacheIrOp::LoadField { object, value_byte } => {
                    let header = if object == 0 { 10 } else { 8 };
                    emit_template_slab_base(ops, view, header, 11, next);
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
                otter_vm::JitCacheIrOp::LoadPrototype { object, result: 1 } if object <= 1 => {
                    let header = if object == 0 { 10 } else { 8 };
                    emit_template_prototype(ops, relocations, view, header, add_transition, next);
                }
                otter_vm::JitCacheIrOp::GuardPrototypeNull { object } => {
                    let header = if object == 0 { 10 } else { 8 };
                    dynasm!(ops
                        ; .arch x64
                        ; cmp DWORD [Rq(header) + view.jit_proto_byte as i32], 0
                        ; jne =>next
                    );
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
                        ; cmp r11d, view.object_inline_slot_cap as i32
                        ; jae =>next
                        ; =>storage_fits
                        ; cmp BYTE [r10 + view.object_extensible_byte as i32], 0
                        ; je =>next
                        ; movzx r9d, WORD [r10 + view.object_slab_len_byte as i32]
                        ; cmp r9d, r11d
                        ; jne =>next
                    );
                }
                otter_vm::JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte,
                } => {
                    emit_template_slab_base(ops, view, 10, 11, next);
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
                otter_vm::JitCacheIrOp::PublishShape {
                    object: 0,
                    shape,
                    new_len,
                    initialize_inline,
                } if terminal => {
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
                    if initialize_inline {
                        let initialized = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch x64
                            ; cmp DWORD [r10 + view.object_slab_handle_byte as i32], 0
                            ; jne =>initialized
                            ; lea r8, [r10 + view.object_inline_values_byte as i32]
                            ; mov [r10 + view.object_values_ptr_byte as i32], r8
                            ; =>initialized
                        );
                    }
                    dynasm!(ops
                        ; .arch x64
                        ; mov WORD [r10 + view.object_slab_len_byte as i32], new_len as i16
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
    dynasm!(ops
        ; .arch x64
        ; cmp BYTE [Rq(header) + view.object_shape_cache_mode_byte as i32], view.object_shape_cache_fast as i8
        ; jne =>miss
        ; cmp BYTE [Rq(header) + view.object_slot_attrs_overridden_byte as i32], 0
        ; jne =>miss
        ; cmp DWORD [Rq(header) + view.object_exotic_handle_byte as i32], 0
        ; jne =>miss
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
    dynasm!(ops
        ; .arch x64
        ; cmp BYTE [Rq(header) + view.object_shape_cache_mode_byte as i32], view.object_shape_cache_fast as i8
        ; jne =>miss
        ; cmp BYTE [Rq(header) + view.object_chain_link_opaque_byte as i32], 0
        ; jne =>miss
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

fn emit_template_prototype(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    header: u8,
    chain_link: bool,
    miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov r8d, [Rq(header) + view.jit_proto_byte as i32]
        ; test r8d, r8d
        ; jz =>miss
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
        ; add r8, r9
        ; cmp BYTE [r8], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    if chain_link {
        emit_template_shape_state_guard(ops, view, 8, miss);
    } else {
        emit_template_fast_state_guard(ops, view, 8, miss);
    }
}

fn emit_template_slab_base(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    destination: u8,
    miss: DynamicLabel,
) {
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [Rq(header) + view.object_slab_handle_byte as i32], 0
        ; jne >external
        ; lea Rq(destination), [Rq(header) + view.object_inline_values_byte as i32]
        ; jmp =>ready
        ; external:
        ; mov Rq(destination), [Rq(header) + view.object_values_ptr_byte as i32]
        ; test Rq(destination), Rq(destination)
        ; jz =>miss
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

#[allow(clippy::too_many_arguments)]
fn emit_forward_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    mut code_map: Option<&mut CodeMapCapture>,
    logical_pc: u32,
    dst: u16,
    method: u16,
    callee: u16,
    receiver: u16,
    identity_guard: DynamicLabel,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let byte_pc = view
        .instructions
        .get(logical_pc as usize)
        .ok_or(Unsupported::OperandShape("x86-64 forward instruction PC"))?
        .byte_pc;
    let canonical = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let native_start = ops.offset().0;
    crate::x86_64::emit_runtime_forward(
        ops,
        relocations,
        view,
        transitions,
        [dst, method, callee, receiver],
        logical_pc,
        byte_pc,
        code_map.as_deref_mut(),
        canonical,
        finish_error,
        throw_value,
        fatal,
        done,
        |ops, source, target, _| {
            emit_load_reg(ops, target, source);
            Ok(())
        },
        |ops, destination, source, _| {
            emit_store_reg(ops, source, destination);
            Ok(())
        },
        |ops| {
            dynasm!(ops
                ; .arch x64
                ; mov r14, [r15 + NATIVE_FRAME_OFFSET as i32]
                ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
            );
            Ok(())
        },
        |ops, register, _| {
            emit_load_reg(ops, 11, register);
            Ok(())
        },
    )?;
    if let Some(map) = code_map {
        let targets: Vec<_> = view
            .direct_callees
            .get(&byte_pc)
            .into_iter()
            .flatten()
            .collect();
        for (index, target) in targets.iter().enumerate() {
            if let Ok(artifact) =
                direct_call::forward_artifact(target, index as u32, targets.len() as u32)
            {
                map.record(CodeRegion::call_structural(
                    "runtimeForwardCallCandidate",
                    native_start,
                    ops.offset().0,
                    view.code_block.id,
                    logical_pc,
                    byte_pc,
                    artifact,
                ));
            }
        }
    }
    dynasm!(ops ; .arch x64 ; =>canonical);
    emit_load_reg(ops, 6, method);
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_FORWARD_SOURCE_READY),
        abi::STUB_JIT_FORWARD_SOURCE_READY,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rax, rax
        ; jz =>identity_guard
    );

    let mut words = vec![
        PacketWord::Register(method),
        PacketWord::Register(callee),
        PacketWord::Register(receiver),
    ];
    words.extend(
        view.code_block
            .forwarded_argument_bindings()
            .filter_map(|(_, storage)| match storage {
                otter_bytecode::ArgumentBindingStorage::Register { reg } => {
                    Some(PacketWord::Register(reg))
                }
                otter_bytecode::ArgumentBindingStorage::Upvalue { .. } => None,
            }),
    );
    if words.len() > 510 {
        dynasm!(ops ; .arch x64 ; jmp =>identity_guard);
        return Ok(());
    }
    emit_value_packet_transition(
        ops,
        relocations,
        transitions,
        abi::STUB_JIT_CALL_FORWARD_ARGUMENTS,
        &words,
        dst,
        throw_value,
        fatal,
    )?;
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum PacketWord {
    Register(u16),
    Undefined,
}

#[allow(clippy::too_many_arguments)]
fn emit_construct_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    plan: &super::TemplatePlan,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    super_construct: bool,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let (packed_args, packed_tail) = plan.resolve_packed_args(argc, packed_args);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov esi, i32::from(dst)
        ; mov edx, i32::from(callee)
    );
    emit_load_u64(ops, 1, u64::from(argc) | (u64::from(super_construct) << 63));
    if let Some(tail) = packed_tail {
        emit_load_symbol_u64(
            ops,
            relocations,
            8,
            packed_args,
            RelocationTarget::TemplateOperandSlice {
                arena: TemplateOperandArena::Registers,
                role: TemplateOperandRole::ConstructArguments,
                start: u32::try_from(tail.start)
                    .map_err(|_| Unsupported::OperandShape("x86-64 construct argument start"))?,
                len: u32::try_from(tail.len)
                    .map_err(|_| Unsupported::OperandShape("x86-64 construct argument count"))?,
            },
        );
    } else {
        emit_load_u64(ops, 8, packed_args);
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_CONSTRUCT),
        abi::STUB_JIT_CONSTRUCT,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rax, rax
        ; je >completed
        ; cmp eax, abi::NativeResultStatus::SideExit as i32
        ; je =>bail
        ; cmp eax, abi::NativeResultStatus::Throw as i32
        ; je =>threw
        ; jmp =>fatal
        ; completed:
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_generic_call_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    dst: u16,
    callee: u16,
    receiver: Option<u16>,
    arguments: &[u16],
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let mut words = Vec::with_capacity(arguments.len() + 2);
    words.push(PacketWord::Register(callee));
    words.push(receiver.map_or(PacketWord::Undefined, PacketWord::Register));
    words.extend(arguments.iter().copied().map(PacketWord::Register));
    emit_value_packet_transition(
        ops,
        relocations,
        transitions,
        abi::STUB_JIT_CALL_WITH_THIS_VALUE,
        &words,
        dst,
        throw_value,
        fatal,
    )
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
            PacketWord::Undefined => emit_load_u64(ops, 0, VALUE_UNDEFINED),
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

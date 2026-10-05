//! Reusable x86-64 baseline operations over a published register window.
//!
//! # Contents
//! - [`OperationContext`] supplies immutable lowering and caller-owned exits.
//! - [`emit_operation`] emits one planned operation for Template or Graph.
//!
//! # Invariants
//! - The caller binds labels and owns the instruction code-map region. This
//!   module records nested structural regions and actual generated return sites.
//! - Generated JS calls recover suspended sources from the caller's return table;
//!   collecting C helpers and abrupt exits publish the active source explicitly.
//! - Runtime C calls use the shared platform boundary. JavaScript calls retain
//!   the private argument-span convention and callee-owned activation.
//! - Tail teardown uses the caller's physical frame kind.
//!
//! # See also
//! - [`super`] for whole-function scaffolding and scalar emission helpers.
//! - [`crate::template::operation::OperationExits`] for the sole exit carrier.

use super::*;
use crate::template::operation::OperationExits;

/// Compile-time inputs and append-only state for one baseline operation.
pub(crate) struct OperationContext<'c, 'a> {
    pub(crate) ops: &'c mut Assembler,
    pub(crate) relocations: &'c mut RelocationCapture,
    pub(crate) return_sites: &'c mut Vec<abi::SafepointEntry>,
    pub(crate) call_safepoint: abi::SafepointId,
    pub(crate) transitions: &'a crate::entry::TransitionTable,
    pub(crate) view: &'a JitCompileSnapshot,
    pub(crate) plan: &'c TemplatePlan,
    pub(crate) labels: &'c BTreeMap<u32, DynamicLabel>,
    pub(crate) exits: OperationExits,
    pub(crate) frame_kind: abi::NativeFrameKind,
    pub(crate) shared_property: &'c mut super::shared_property::SharedPropertyProbes,
    pub(crate) direct_call_events: &'c mut Option<crate::template::DirectCallEvents>,
    pub(crate) code_map: &'c mut Option<CodeMapCapture>,
}

/// Emit one operation whose label and required canonical PC the caller bound.
pub(crate) fn emit_operation(
    context: OperationContext<'_, '_>,
    instruction: &crate::template::plan::TemplateInstr,
) -> Result<(), Unsupported> {
    let OperationContext {
        ops,
        relocations,
        return_sites,
        call_safepoint,
        transitions,
        view,
        plan,
        labels,
        exits,
        frame_kind,
        shared_property,
        direct_call_events,
        code_map,
    } = context;
    let mut call_source = crate::return_sites::ReturnSiteRecorder {
        entries: return_sites,
        safepoint_id: call_safepoint,
        logical_pc: instruction.pc,
    };
    let return_sites = &mut call_source;
    crate::template::mark_direct_call_site_reached(
        direct_call_events.as_mut(),
        instruction.byte_pc,
    );
    let OperationExits {
        type_mismatch_exit: type_mismatch,
        allocation_miss_exit: allocation_miss,
        unsupported_exit: unsupported,
        runtime_transition_exit: runtime_transition,
        backedge_relink_exit: backedge_relink,
        returned,
        committed_throw,
        threw,
        fatal,
    } = exits;
    match instruction.op {
        TemplateOp::LoadImmediate { dst, bits } => {
            emit_load_u64(ops, 0, bits);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::Move { dst, src } => {
            emit_load_reg(ops, 0, src);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::Jump { target, back_edge } => {
            if back_edge {
                emit_backedge_poll(
                    ops,
                    relocations,
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
            emit_load_reg(ops, 0, condition);
            emit_truthiness_bool(ops, type_mismatch);
            let fallthrough = ops.new_dynamic_label();
            let not_taken = if when_truthy { VALUE_FALSE } else { VALUE_TRUE };
            emit_cmp_rax_imm8(ops, not_taken);
            dynasm!(ops ; .arch x64 ; je =>fallthrough);
            if back_edge {
                emit_backedge_poll(
                    ops,
                    relocations,
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
            emit_load_reg(ops, 0, condition);
            let taken = ops.new_dynamic_label();
            let done = ops.new_dynamic_label();
            emit_load_u64(ops, 11, VALUE_NULL);
            dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>taken);
            emit_load_u64(ops, 11, VALUE_UNDEFINED);
            dynasm!(ops ; .arch x64 ; cmp rax, r11 ; jne =>done ; =>taken);
            if back_edge {
                emit_backedge_poll(
                    ops,
                    relocations,
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
            emit_load_reg(ops, 0, src);
            emit_truthiness_bool(ops, type_mismatch);
            if negate {
                emit_load_u64(ops, 11, VALUE_TRUE ^ VALUE_FALSE);
                dynasm!(ops ; .arch x64 ; xor rax, r11);
            }
            emit_store_reg(ops, 0, dst);
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
        } => emit_binary_arith(ops, relocations, dst, lhs, rhs, kind, type_mismatch),
        TemplateOp::AddGeneric {
            dst,
            lhs,
            rhs,
            concat_safepoint,
        } => emit_add_generic(
            ops,
            relocations,
            view,
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
        } => emit_compare(ops, relocations, dst, lhs, rhs, kind, type_mismatch, fatal),
        TemplateOp::LooseCompare {
            dst,
            lhs,
            rhs,
            negate,
        } => emit_loose_compare(ops, dst, lhs, rhs, negate, type_mismatch),
        TemplateOp::TestTypeOf { dst, src, test } => {
            emit_test_typeof(ops, relocations, dst, src, test, type_mismatch)
        }
        TemplateOp::IntBitwise {
            dst,
            lhs,
            rhs,
            kind,
        } => emit_bitwise(ops, dst, lhs, rhs, kind, type_mismatch),
        TemplateOp::UnsignedShiftRight { dst, lhs, rhs } => {
            emit_unsigned_shift(ops, dst, lhs, rhs, type_mismatch)
        }
        TemplateOp::Increment { dst, src, delta } => {
            emit_increment(ops, dst, src, delta, type_mismatch)
        }
        TemplateOp::Negate { dst, src } => emit_negate(ops, dst, src, type_mismatch),
        TemplateOp::BitwiseNot { dst, src } => {
            emit_load_reg(ops, 0, src);
            emit_to_int32(ops, 0, 10, type_mismatch);
            dynasm!(ops ; .arch x64 ; not r10d);
            emit_box_int32(ops, 10, 0, 11);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::ToNumeric { dst, src } => {
            emit_load_reg(ops, 0, src);
            emit_guard_number(ops, 0, type_mismatch);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::ToPrimitive { dst, src, .. } => {
            emit_load_reg(ops, 0, src);
            emit_load_u64(ops, 11, NOT_CELL_MASK);
            dynasm!(ops
                ; .arch x64
                ; mov r10, rax
                ; and r10, r11
                ; test r10, r10
                ; jz =>type_mismatch
            );
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::LoadThis { dst } => {
            dynasm!(ops ; .arch x64 ; mov rax, [r14 + NATIVE_FRAME_THIS_OFFSET as i32]);
            emit_load_u64(ops, 11, VALUE_HOLE);
            dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>type_mismatch);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::LoadSelfClosure { dst } => {
            dynasm!(ops ; .arch x64 ; mov rax, [r14 + NATIVE_FRAME_SELF_OFFSET as i32]);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::LoadClosureContext { dst } => {
            context::emit_load_closure_context(ops, view, dst)?;
        }
        TemplateOp::LoadContextSlot {
            dst,
            context,
            depth,
            slot,
        } => context::emit_load_context_slot(ops, view, dst, context, depth, slot)?,
        TemplateOp::StoreContextSlot {
            src,
            context,
            depth,
            slot,
        } => context::emit_store_context_slot(ops, relocations, view, src, context, depth, slot)?,
        TemplateOp::CreateContext {
            dst,
            parent,
            scope,
            safepoint,
        } => context::emit_context_allocation(
            ops,
            relocations,
            view,
            instruction.pc,
            instruction.byte_pc,
            dst,
            context::ContextAllocation::Create { parent, scope },
            safepoint,
            allocation_miss,
            fatal,
        )?,
        TemplateOp::CopyContext {
            dst,
            src,
            safepoint,
        } => context::emit_context_allocation(
            ops,
            relocations,
            view,
            instruction.pc,
            instruction.byte_pc,
            dst,
            context::ContextAllocation::Copy { source: src },
            safepoint,
            allocation_miss,
            fatal,
        )?,
        TemplateOp::ClassSuperConstructor { dst, class } => {
            emit_load_reg(ops, 6, class);
            dynasm!(ops ; .arch x64 ; mov rdi, r15);
            emit_load_runtime_stub(
                ops,
                relocations,
                transitions.variadic_entry(abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR),
                abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR,
            );
            emit_variadic_call(ops, abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR, 2);
            emit_load_u64(ops, 11, VALUE_HOLE);
            dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>runtime_transition);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::MakeFunction { dst, safepoint } => {
            context::emit_context_allocation(
                ops,
                relocations,
                view,
                instruction.pc,
                instruction.byte_pc,
                dst,
                context::ContextAllocation::Function,
                safepoint,
                allocation_miss,
                fatal,
            )?;
        }
        TemplateOp::NewObject { dst } => emit_value_packet_transition(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_NEW_OBJECT,
            &[],
            dst,
            committed_throw,
            fatal,
        )?,
        TemplateOp::CollectArguments { dst } => {
            emit_collect_arguments(ops, relocations, transitions, dst, threw, fatal)
        }
        TemplateOp::CallForwardArguments {
            dst,
            method,
            receiver,
            this_value,
        } => forward_call::emit_forward_call(
            ops,
            relocations,
            transitions,
            return_sites,
            view,
            [dst, method, receiver, this_value],
            committed_throw,
            threw,
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
                ops,
                relocations,
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
                ops,
                relocations,
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
                ops,
                relocations,
                transitions.variadic_entry(abi::STUB_JIT_DEFINE_DATA_PROPERTY),
                abi::STUB_JIT_DEFINE_DATA_PROPERTY,
            );
            emit_variadic_call(ops, abi::STUB_JIT_DEFINE_DATA_PROPERTY, 4);
            emit_status_word_result(ops, threw, fatal);
        }
        TemplateOp::DefineOwnProperty {
            target,
            key,
            descriptor,
        } => emit_define_own_property(
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
            transitions,
            return_sites,
            opcode,
            arg0,
            arg1,
            arg2,
            committed_throw,
            threw,
            fatal,
        )?,
        TemplateOp::DeleteOp {
            opcode,
            arg0,
            arg1,
            arg2,
        } => emit_opcode_transition(
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            emit_load_u64(ops, 6, packed_meta);
            emit_load_u64(ops, 2, packed_args);
            emit_load_runtime_stub(
                ops,
                relocations,
                transitions.variadic_entry(abi::STUB_JIT_BIND_FUNCTION),
                abi::STUB_JIT_BIND_FUNCTION,
            );
            emit_variadic_call(ops, abi::STUB_JIT_BIND_FUNCTION, 3);
            emit_side_exit_status_result(ops, runtime_transition, threw, fatal);
        }
        TemplateOp::LoadRegExp { dst, constant } => emit_constant_transition(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_LOAD_REGEXP,
            dst,
            constant,
            threw,
            fatal,
        ),
        TemplateOp::LoadBuiltinError { dst, constant } => emit_constant_transition(
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            context: closure_context,
            safepoint,
        } => {
            context::emit_context_allocation(
                ops,
                relocations,
                view,
                instruction.pc,
                instruction.byte_pc,
                dst,
                context::ContextAllocation::Closure {
                    context: closure_context,
                },
                safepoint,
                allocation_miss,
                fatal,
            )?;
        }
        TemplateOp::BindingValue {
            semantics,
            result,
            value0,
            value1,
            context_coord,
        } => binding::emit_binding_value(
            ops,
            relocations,
            transitions,
            view,
            semantics,
            result,
            value0,
            value1,
            context_coord,
            instruction.byte_pc,
            code_map.as_mut(),
            committed_throw,
            fatal,
        )?,
        TemplateOp::GlobalDeclarationValue { value0, value1, .. } => {
            emit_committed_value2(
                ops,
                relocations,
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
            ops,
            relocations,
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
                ops,
                relocations,
                11,
                target.cell_addr as u64,
                RelocationTarget::StringConstantCell {
                    function_id: view.code_block.id,
                    byte_pc: instruction.byte_pc,
                },
            );
            dynasm!(ops ; .arch x64 ; mov rax, [r11]);
            emit_store_reg(ops, 0, dst);
        }
        TemplateOp::LoadProperty { dst, object, .. } => emit_load_property(
            ops,
            relocations,
            shared_property,
            view,
            instruction.byte_pc,
            dst,
            object,
            committed_throw,
            fatal,
        )?,
        TemplateOp::StoreProperty { object, value, .. } => emit_store_property(
            ops,
            relocations,
            shared_property,
            view,
            instruction.byte_pc,
            object,
            value,
            committed_throw,
            fatal,
        )?,
        TemplateOp::LoadElement {
            dst,
            receiver,
            index,
        } => emit_load_element(
            ops,
            relocations,
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
            ops,
            relocations,
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
            if let Some(target) = view
                .native_calls
                .get(&byte_pc)
                .and_then(|target| target.leaf())
            {
                let name = abi::runtime_stub_name(target.leaf_stub_id);
                if native_leaf::supports_site(view, *target, arguments.len()) {
                    // A baseline call never deoptimizes: a callee-identity or
                    // leaf miss performs the ordinary call once.
                    let start = ops.offset().0;
                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    emit_load_reg(ops, 10, callee);
                    for (index, &argument) in arguments.iter().enumerate() {
                        emit_load_reg(ops, if index == 0 { 6 } else { 2 }, argument);
                    }
                    native_leaf::emit_guard(ops, view, target.builtin_native_ref, miss);
                    native_leaf::emit_tagged_call(
                        ops,
                        relocations,
                        target.leaf_stub_id,
                        target.argument_count,
                        miss,
                    )?;
                    emit_store_reg(ops, 0, dst);
                    dynasm!(ops ; .arch x64 ; jmp =>done ; =>miss);
                    let leaf_end = ops.offset().0;
                    calls::emit_call(
                        ops,
                        relocations,
                        transitions,
                        return_sites,
                        view,
                        Some(callee),
                        None,
                        calls::CallNewTarget::None,
                        &arguments,
                        None,
                        instruction.pc,
                        dst,
                        committed_throw,
                        threw,
                        fatal,
                    )?;
                    dynasm!(ops ; .arch x64 ; =>done);
                    if let Some(code_map) = code_map.as_mut() {
                        code_map.record(CodeRegion::static_native_structural(
                            "nativeLeafCall",
                            start,
                            leaf_end,
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
                                outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Generated,
                            },
                        );
                    }
                    return Ok(());
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
                ops,
                relocations,
                transitions,
                return_sites,
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
                fatal,
            )?;
            if let Some(target) = view
                .direct_callees
                .get(&byte_pc)
                .filter(|targets| targets.len() == 1)
                .and_then(|targets| targets.first())
            {
                crate::template::record_generated_direct_call(
                    direct_call_events.as_mut(),
                    otter_vm::JitDirectCallKind::Plain,
                    instruction.pc,
                    byte_pc,
                    target,
                    0,
                    1,
                );
            }
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
                ops,
                relocations,
                transitions,
                return_sites,
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
            let known = view
                .direct_constructs
                .get(&byte_pc)
                .filter(|target| target.plan.call_flags & abi::FUNCTION_CALL_CONSTRUCTIBLE != 0)
                .map(|target| target.plan);
            let start = ops.offset().0;
            calls::emit_call(
                ops,
                relocations,
                transitions,
                return_sites,
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
                fatal,
            )?;
            if let Some(target) = view
                .direct_constructs
                .get(&byte_pc)
                .filter(|target| target.plan.call_flags & abi::FUNCTION_CALL_CONSTRUCTIBLE != 0)
            {
                crate::template::record_generated_direct_call(
                    direct_call_events.as_mut(),
                    crate::template::construct_call_kind(
                        super_construct,
                        target.plan.is_derived_constructor,
                    ),
                    instruction.pc,
                    byte_pc,
                    target,
                    0,
                    1,
                );
            }
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
            byte_pc,
            ..
        } => {
            let argument_registers = plan.register_tail(arguments);
            calls::emit_method_call(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                shared_property,
                direct_call_events.as_mut(),
                code_map.as_mut(),
                instruction.pc,
                byte_pc,
                receiver,
                argument_registers,
                dst,
                committed_throw,
                threw,
                fatal,
            )?;
        }
        TemplateOp::Throw { src } => {
            emit_scalar_value(
                ops,
                relocations,
                transitions,
                src,
                Some(src),
                None,
                committed_throw,
                fatal,
            );
            emit_load_reg(ops, 0, src);
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
                abi::ScalarValueOp::LoadArgumentsLength | abi::ScalarValueOp::LoadArgumentsElement
            ) {
                if let Some(src) = value0 {
                    emit_load_reg(ops, 9, src);
                }
                dynasm!(ops ; .arch x64 ; mov r11, r14);
                crate::x86_64::arguments::emit(ops, value0.map(|_| 9), slow);
                emit_store_reg(ops, 8, result);
                dynasm!(ops ; .arch x64 ; jmp =>done);
            }
            dynasm!(ops ; .arch x64 ; =>slow);
            emit_scalar_value(
                ops,
                relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            ops,
            relocations,
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
            emit_load_reg(ops, 0, src);
            dynasm!(ops ; .arch x64 ; jmp =>returned);
        }
        TemplateOp::ReturnUndefined => {
            emit_load_u64(ops, 0, VALUE_UNDEFINED);
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
            emit_load_reg(ops, 0, value);
            emit_load_u64(ops, 11, VALUE_UNDEFINED);
            dynasm!(ops ; .arch x64 ; cmp rax, r11 ; jne =>runtime_transition);
            context::emit_read_context_slot_rax(ops, view, context, depth, slot)?;
            emit_load_u64(ops, 11, VALUE_HOLE);
            dynasm!(ops
                ; .arch x64
                ; cmp rax, r11
                ; je =>runtime_transition
                ; jmp =>returned
            );
        }
        TemplateOp::TailCall {
            dst,
            callee,
            argc,
            packed_args,
            byte_pc,
        } => {
            let arguments = plan.call_argument_registers(argc, packed_args);
            let known = view
                .direct_callees
                .get(&byte_pc)
                .filter(|targets| targets.len() == 1)
                .map(|targets| targets[0].plan);
            let start = ops.offset().0;
            calls::emit_tail_call(
                ops,
                relocations,
                transitions,
                return_sites,
                view,
                frame_kind,
                callee,
                &arguments,
                known,
                instruction.pc,
                dst,
                runtime_transition,
                committed_throw,
                threw,
                fatal,
            )?;
            if let Some(target) = view
                .direct_callees
                .get(&byte_pc)
                .filter(|targets| targets.len() == 1)
                .and_then(|targets| targets.first())
            {
                crate::template::record_generated_direct_call(
                    direct_call_events.as_mut(),
                    otter_vm::JitDirectCallKind::Plain,
                    instruction.pc,
                    byte_pc,
                    target,
                    0,
                    1,
                );
            }
            if let Some(code_map) = code_map.as_mut() {
                code_map.record(CodeRegion::call_structural(
                    "tailCall",
                    start,
                    ops.offset().0,
                    view.code_block.id,
                    instruction.pc,
                    byte_pc,
                    known.map(|plan| plan.function_id),
                ));
            }
        }
        TemplateOp::UnsupportedBail => dynasm!(ops ; .arch x64 ; jmp =>unsupported),
    }
    Ok(())
}

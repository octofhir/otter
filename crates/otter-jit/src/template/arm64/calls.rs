//! AArch64 call emitters for the template compiler.
//!
//! # Contents
//! - Compiler-generated monomorphic plain calls and bounded polymorphic method
//!   chains with a canonical generic-call continuation.
//! - Construct lowering through the current runtime transition.
//! - Guarded read-only numeric call/method splicing from VM-baked metadata.
//! - Guarded collection-method leaves before generated method linkage.
//!
//! # Invariants
//! - Call guard/setup failure is effect-free and deoptimizes at the original
//!   opcode; accepted inline misses never replay it.
//! - A generated call owns a rooted stack register window and enters the baked
//!   stable code-entry generation directly.
//! - A callee throw caught by the compiled caller publishes the selected
//!   catch/finally PC and exits through the shared bailout epilogue.
//! - A generated method chain re-reads each receiver/prototype/slot identity in
//!   feedback order, then carries the proven callable and exact receiver into
//!   the selected linkage.
//! - Inlined call and method bodies contain no call, allocation, branch, or
//!   mutation; every guard failure balances compact scratch storage and
//!   deoptimizes the original opcode before observable effects.
//! - `x19` remains the caller register base throughout an inline body. Callee
//!   virtual registers use explicit `sp`-relative compact slots initialized
//!   only for live entry values.
//! - A raw receiver pointer retained in `x17` is live only between the entry
//!   identity guard and the end of a call-free, safepoint-free inline body.
//! - A method target without one current native generation is omitted; a site
//!   matching no generated target completes through the VM's canonical
//!   `GetMethod + Call` transition without replaying the caller.
//!
//! # See also
//! - [`crate::arm64`] — shared generated call and method-guard emission.
//! - [`super::transitions`] — descriptor-resolved entries used here.

use std::collections::BTreeMap;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::native_abi as abi;
use otter_vm::{
    JitCompileSnapshot, JitInlineCallee, JitInlineMethod, closure::JS_CLOSURE_BODY_TYPE_TAG,
    value::tag as value_tag,
};

use super::collections::{
    MethodSite, emit_alloc_method_guarded_call, emit_leaf_method_guarded_call,
    emit_primitive_method_guarded_call,
};
use super::transitions::TransitionTable;
use super::values::{
    emit_box_double, emit_box_int32, emit_decompress_slot, emit_load_reg, emit_load_runtime_stub,
    emit_load_symbol_u64, emit_load_u64, emit_num_to_double, emit_slab_base, emit_store_reg,
};
use crate::arm64::{
    DirectCallForm, DirectCallSite, MethodGuardSite, StaticNativeCallSite, direct_call_artifact,
    direct_call_target_is_supported, emit_direct_call, emit_method_guard, emit_static_native_call,
    static_native_target_is_supported,
};
use crate::artifact::relocation::{
    RelocationCapture, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
};
use crate::artifact::{
    CodeMapCapture, CodeRegion, InlineScratchEntryArtifact, InlineScratchLayoutArtifact,
    InlineSiteArtifact,
};
use crate::entry::{
    MAX_METHOD_ARGS, NUMBER_TAG_HI16, Unsupported, VALUE_UNDEFINED, pack_method_arg_regs,
};
use crate::template::{
    ArithKind, InlineEntryValue, InlineLeafPlan, InlineScratchSlot, TemplateOp, TemplatePlan,
    TemplateTail,
};

fn inline_scratch_artifact(
    parameter_count: u16,
    register_count: u16,
    plan: &InlineLeafPlan<'_>,
) -> InlineScratchLayoutArtifact {
    let register_slots = (0..register_count)
        .map(|register| plan.register_slot(register).map(InlineScratchSlot::index))
        .collect();
    let entry_values = plan
        .entry_values()
        .iter()
        .map(|entry| match *entry {
            InlineEntryValue::Argument {
                argument,
                register,
                slot,
            } => InlineScratchEntryArtifact::Argument {
                argument,
                register,
                slot: slot.index(),
            },
            InlineEntryValue::Receiver { slot } => {
                InlineScratchEntryArtifact::Receiver { slot: slot.index() }
            }
            InlineEntryValue::Undefined { register, slot } => {
                InlineScratchEntryArtifact::Undefined {
                    register,
                    slot: slot.index(),
                }
            }
        })
        .collect();
    InlineScratchLayoutArtifact {
        parameter_count,
        virtual_register_count: register_count,
        scratch_slot_count: plan.slot_count(),
        slot_bytes: 8,
        stack_alignment_bytes: 16,
        scratch_bytes: plan.aligned_scratch_bytes(),
        offset_basis: "postAllocationSp",
        register_slots,
        receiver_slot: plan.receiver_slot().map(InlineScratchSlot::index),
        entry_values,
    }
}

/// Build the current template plan for one baked leaf body.
///
/// PORT NOTE: the deleted legacy baseline emitter decoded the method body a
/// second time. This port deliberately reuses the current typed TemplatePlan,
/// keeping operand validation and opcode shapes in one backend-neutral path.
fn inline_leaf_template_plan(
    view: &JitCompileSnapshot,
    code_block: &std::sync::Arc<otter_vm::CodeBlock>,
    instructions: &[otter_vm::JitInstructionMetadata],
) -> Result<TemplatePlan, Unsupported> {
    let mut leaf_view = view.clone();
    leaf_view.code_block = code_block.clone();
    leaf_view.instructions = instructions.to_vec();
    leaf_view.inline_callees.clear();
    leaf_view.direct_callees.clear();
    leaf_view.direct_methods.clear();
    leaf_view.inline_methods.clear();
    leaf_view.inline_poly_methods.clear();
    leaf_view.collection_leaf_methods.clear();
    leaf_view.collection_alloc_methods.clear();
    leaf_view.array_methods.clear();
    leaf_view.primitive_method_guards.clear();
    TemplatePlan::build(&leaf_view)
}

fn inline_method_template_plan(
    view: &JitCompileSnapshot,
    method: &JitInlineMethod,
) -> Result<TemplatePlan, Unsupported> {
    inline_leaf_template_plan(view, &method.code_block, &method.instructions)
}

fn inline_callee_template_plan(
    view: &JitCompileSnapshot,
    callee: &JitInlineCallee,
) -> Result<TemplatePlan, Unsupported> {
    inline_leaf_template_plan(view, &callee.code_block, &callee.instructions)
}

/// Load one compact inline slot without changing the caller register base.
fn emit_load_inline_slot(ops: &mut Assembler, target: u8, slot: InlineScratchSlot) {
    dynasm!(ops ; .arch aarch64 ; ldr X(target), [sp, slot.byte_offset()]);
}

/// Store one compact inline slot without changing the caller register base.
fn emit_store_inline_slot(ops: &mut Assembler, source: u8, slot: InlineScratchSlot) {
    dynasm!(ops ; .arch aarch64 ; str X(source), [sp, slot.byte_offset()]);
}

fn emit_inline_number_identity(
    ops: &mut Assembler,
    dst: InlineScratchSlot,
    src: InlineScratchSlot,
    miss: DynamicLabel,
) {
    emit_load_inline_slot(ops, 9, src);
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x15
        ; b.eq =>miss
    );
    if dst != src {
        emit_store_inline_slot(ops, 9, dst);
    }
}

fn emit_inline_numeric_add(
    ops: &mut Assembler,
    dst: InlineScratchSlot,
    lhs: InlineScratchSlot,
    rhs: InlineScratchSlot,
    miss: DynamicLabel,
) {
    emit_load_inline_slot(ops, 9, lhs);
    emit_load_inline_slot(ops, 10, rhs);
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
        ; adds w13, w9, w10
        ; b.vs =>float_path
    );
    emit_box_int32(ops, 13, 12);
    emit_store_inline_slot(ops, 13, dst);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, miss);
    emit_num_to_double(ops, 10, 1, miss);
    dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1);
    emit_box_double(ops, 2, 13);
    emit_store_inline_slot(ops, 13, dst);
    dynasm!(ops ; .arch aarch64 ; =>done);
}

fn emit_inline_numeric_binary(
    ops: &mut Assembler,
    dst: InlineScratchSlot,
    lhs: InlineScratchSlot,
    rhs: InlineScratchSlot,
    kind: ArithKind,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if matches!(kind, ArithKind::Pow) {
        return Err(Unsupported::Opcode(otter_bytecode::Op::Pow));
    }
    emit_load_inline_slot(ops, 9, lhs);
    emit_load_inline_slot(ops, 10, rhs);
    emit_num_to_double(ops, 9, 0, miss);
    emit_num_to_double(ops, 10, 1, miss);
    match kind {
        ArithKind::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
        ArithKind::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
        ArithKind::Div => dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1),
        ArithKind::Rem | ArithKind::Pow => {
            return Err(Unsupported::OperandShape(
                "inline numeric method remainder/pow",
            ));
        }
    }
    emit_box_double(ops, 2, 13);
    emit_store_inline_slot(ops, 13, dst);
    Ok(())
}

fn emit_inline_receiver_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    dst: InlineScratchSlot,
    value_byte: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    // `x17` holds the receiver's slab base, derived once from the exact
    // receiver body after its type/shape guard. Eligible inline bodies cannot
    // call, allocate, branch, or mutate, so neither the receiver nor its slab
    // can move or change before this load.
    dynasm!(ops ; .arch aarch64 ; ldr w9, [x17, value_byte]);
    emit_decompress_slot(ops, relocations, view.cage_base as u64, miss);
    emit_store_inline_slot(ops, 9, dst);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct InlineBodySpec<'a> {
    function_id: u32,
    parameter_count: u16,
    register_count: u16,
    arguments: [Option<u16>; 2],
    receiver: Option<u16>,
    method: Option<&'a JitInlineMethod>,
    inline_site: InlineSiteArtifact,
    body_region: &'static str,
    hit_epilogue_region: &'static str,
    deopt_teardown_region: &'static str,
}

/// Emit one already-guarded leaf body over compact scratch storage.
///
/// `body_deopt` balances scratch allocated after the identity guard, then
/// branches to the caller's exact pre-effect deopt exit. Keeping teardown in
/// one emitter prevents call and method splices from drifting in stack
/// discipline or code-map coverage.
#[allow(clippy::too_many_arguments)]
fn emit_inline_leaf_body(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: &InlineLeafPlan<'_>,
    spec: InlineBodySpec<'_>,
    dst: u16,
    mut code_map: Option<&mut CodeMapCapture>,
    done: DynamicLabel,
    deopt: DynamicLabel,
) -> Result<(), Unsupported> {
    let body_deopt = ops.new_dynamic_label();
    let inline_done = ops.new_dynamic_label();

    // Compact scratch is unobservable and contains no safepoint. `x19` stays
    // on the published caller window; only explicit `sp`-relative helpers may
    // touch callee values.
    let scratch_start = ops.offset().0;
    let scratch_bytes = plan.aligned_scratch_bytes();
    if scratch_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
    }
    let mut undefined_loaded = false;
    for &entry in plan.entry_values() {
        match entry {
            InlineEntryValue::Argument { argument, slot, .. } => {
                let caller_register = spec
                    .arguments
                    .get(usize::from(argument))
                    .copied()
                    .flatten()
                    .ok_or(Unsupported::OperandShape("inline leaf argument"))?;
                emit_load_reg(ops, 9, caller_register)?;
                emit_store_inline_slot(ops, 9, slot);
            }
            InlineEntryValue::Receiver { slot } => {
                let receiver = spec
                    .receiver
                    .ok_or(Unsupported::OperandShape("inline leaf receiver"))?;
                emit_load_reg(ops, 9, receiver)?;
                emit_store_inline_slot(ops, 9, slot);
            }
            InlineEntryValue::Undefined { slot, .. } => {
                if !undefined_loaded {
                    emit_load_u64(ops, 9, VALUE_UNDEFINED);
                    undefined_loaded = true;
                }
                emit_store_inline_slot(ops, 9, slot);
            }
        }
    }

    let scratch_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_scratch(
            scratch_start,
            scratch_end,
            spec.inline_site,
            spec.function_id,
            inline_scratch_artifact(spec.parameter_count, spec.register_count, plan),
        ));
    }

    let body_start = ops.offset().0;
    for (operation_index, instruction) in plan.instructions().iter().enumerate() {
        let instruction_start = ops.offset().0;
        match instruction.op {
            TemplateOp::LoadImmediate { dst, bits } => {
                emit_load_u64(ops, 9, bits);
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                emit_store_inline_slot(ops, 9, dst);
            }
            TemplateOp::Move { dst, src } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let src = plan
                    .register_slot(src)
                    .ok_or(Unsupported::OperandShape("inline scratch source"))?;
                if dst != src {
                    emit_load_inline_slot(ops, 9, src);
                    emit_store_inline_slot(ops, 9, dst);
                }
            }
            TemplateOp::LoadThis { dst } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let receiver = plan
                    .receiver_slot()
                    .ok_or(Unsupported::OperandShape("inline scratch receiver"))?;
                if dst != receiver {
                    emit_load_inline_slot(ops, 9, receiver);
                    emit_store_inline_slot(ops, 9, dst);
                }
            }
            TemplateOp::LoadProperty { dst, .. } => {
                let method = spec
                    .method
                    .ok_or(Unsupported::OperandShape("inline callee property load"))?;
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let value_byte = *method
                    .prop_offsets
                    .get(&instruction.byte_pc)
                    .ok_or(Unsupported::OperandShape("inline method property offset"))?;
                emit_inline_receiver_property(ops, relocations, view, dst, value_byte, body_deopt)?;
            }
            TemplateOp::ToPrimitive { dst, src, .. } | TemplateOp::ToNumeric { dst, src } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let src = plan
                    .register_slot(src)
                    .ok_or(Unsupported::OperandShape("inline scratch source"))?;
                emit_inline_number_identity(ops, dst, src, body_deopt);
            }
            TemplateOp::AddGeneric { dst, lhs, rhs, .. } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let lhs = plan
                    .register_slot(lhs)
                    .ok_or(Unsupported::OperandShape("inline scratch lhs"))?;
                let rhs = plan
                    .register_slot(rhs)
                    .ok_or(Unsupported::OperandShape("inline scratch rhs"))?;
                emit_inline_numeric_add(ops, dst, lhs, rhs, body_deopt);
            }
            TemplateOp::BinaryArith {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let lhs = plan
                    .register_slot(lhs)
                    .ok_or(Unsupported::OperandShape("inline scratch lhs"))?;
                let rhs = plan
                    .register_slot(rhs)
                    .ok_or(Unsupported::OperandShape("inline scratch rhs"))?;
                emit_inline_numeric_binary(ops, dst, lhs, rhs, kind, body_deopt)?;
            }
            TemplateOp::Return { src } => {
                let src = plan
                    .register_slot(src)
                    .ok_or(Unsupported::OperandShape("inline scratch return"))?;
                emit_load_inline_slot(ops, 9, src);
                dynasm!(ops ; .arch aarch64 ; b =>inline_done);
            }
            TemplateOp::ReturnUndefined => {
                emit_load_u64(ops, 9, VALUE_UNDEFINED);
                dynasm!(ops ; .arch aarch64 ; b =>inline_done);
            }
            _ => unreachable!("inline eligibility accepted unsupported template op"),
        }
        if let Some(code_map) = code_map.as_deref_mut() {
            code_map.record(CodeRegion::inline_instruction(
                instruction_start,
                ops.offset().0,
                spec.inline_site,
                spec.function_id,
                instruction.pc,
                instruction.byte_pc,
                u32::try_from(operation_index).unwrap_or(u32::MAX),
                format!("{:?}", instruction.op),
            ));
        }
    }
    let body_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_structural(
            spec.body_region,
            body_start,
            body_end,
            spec.inline_site,
            spec.function_id,
        ));
    }

    let hit_epilogue_start = ops.offset().0;
    dynasm!(ops ; .arch aarch64 ; =>inline_done);
    if scratch_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    }
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    let hit_epilogue_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_structural(
            spec.hit_epilogue_region,
            hit_epilogue_start,
            hit_epilogue_end,
            spec.inline_site,
            spec.function_id,
        ));
    }

    dynasm!(ops ; .arch aarch64 ; =>body_deopt);
    let deopt_teardown_start = ops.offset().0;
    if scratch_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    }
    dynasm!(ops ; .arch aarch64 ; b =>deopt);
    let deopt_teardown_end = ops.offset().0;
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::inline_structural(
            spec.deopt_teardown_region,
            deopt_teardown_start,
            deopt_teardown_end,
            spec.inline_site,
            spec.function_id,
        ));
    }
    Ok(())
}

/// Emit one exact receiver/method guard plus a deopt-safe scratch body.
#[allow(clippy::too_many_arguments)]
fn try_emit_inline_numeric_method(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    method: &JitInlineMethod,
    dst: u16,
    receiver: u16,
    argc: u16,
    arg0: Option<u16>,
    arg1: Option<u16>,
    call_logical_pc: u32,
    call_byte_pc: u32,
    mut code_map: Option<&mut CodeMapCapture>,
    done: DynamicLabel,
    bail: DynamicLabel,
) -> Result<bool, Unsupported> {
    let template_plan = inline_method_template_plan(view, method)?;
    let Some(plan) = (view.cage_base != 0)
        .then(|| InlineLeafPlan::build_method(method, &template_plan, usize::from(argc)))
        .flatten()
    else {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] template inline method skip fid={} argc={} params={} regs={} ops={:?}",
                method.guard.method_fid,
                argc,
                method.param_count,
                method.register_count,
                template_plan
                    .instructions
                    .iter()
                    .map(|instruction| instruction.op)
                    .collect::<Vec<_>>(),
            );
        }
        return Ok(false);
    };
    if std::env::var_os("OTTER_JIT_TRACE").is_some() {
        eprintln!(
            "[otter-jit] template inline method emit fid={} argc={} regs={} scratch_slots={} scratch_bytes={}",
            method.guard.method_fid,
            argc,
            method.register_count,
            plan.slot_count(),
            plan.aligned_scratch_bytes(),
        );
    }
    let inline_site = InlineSiteArtifact {
        caller_function_id: view.code_block.id,
        logical_pc: call_logical_pc,
        byte_pc: call_byte_pc,
        has_receiver_property: plan.has_receiver_property(),
    };
    let arguments = match (argc, arg0, arg1) {
        (0, _, _) => [None, None],
        (1, Some(first), _) => [Some(first), None],
        (2, Some(first), Some(second)) => [Some(first), Some(second)],
        _ => return Ok(false),
    };
    let guard_start = ops.offset().0;
    emit_method_guard(
        ops,
        relocations,
        view,
        MethodGuardSite {
            guard: &method.guard,
            receiver,
        },
        17,
        plan.has_receiver_property().then_some(16),
        bail,
    )?;
    if plan.has_receiver_property() {
        // Receiver-property offsets were baked from `recv_shape`, whose guard
        // already succeeded above. Materialize its slab once for every sealed
        // receiver-property load in the straight-line inline body.
        dynasm!(ops ; .arch aarch64 ; mov x13, x16);
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x13, =>bail
            ; mov x17, x13
        );
    }

    let guard_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_structural(
            "inlineMethodGuard",
            guard_start,
            guard_end,
            inline_site,
            method.guard.method_fid,
        ));
    }

    emit_inline_leaf_body(
        ops,
        relocations,
        view,
        &plan,
        InlineBodySpec {
            function_id: method.guard.method_fid,
            parameter_count: method.param_count,
            register_count: method.register_count,
            arguments,
            receiver: Some(receiver),
            method: Some(method),
            inline_site,
            body_region: "inlineMethodBody",
            hit_epilogue_region: "inlineMethodHitEpilogue",
            deopt_teardown_region: "inlineMethodDeoptTeardown",
        },
        dst,
        code_map,
        done,
        bail,
    )?;
    Ok(true)
}

fn inline_argument_registers(argc: u16, argument_registers: &[u16]) -> Option<[Option<u16>; 2]> {
    if argument_registers.len() != usize::from(argc) {
        return None;
    }
    match argc {
        0 => Some([None, None]),
        1 => Some([Some(argument_registers[0]), None]),
        2 => Some([Some(argument_registers[0]), Some(argument_registers[1])]),
        _ => None,
    }
}

/// Emit one exact plain-callee identity guard plus deopt-safe leaf body.
///
/// Function-id immediates and closure cells share the same body identity. A
/// closure additionally must not require runtime call setup; bound lexical
/// `this` remains eligible because the plain-callee planner rejects
/// `LoadThis`. Every guard failure deoptimizes the original `Call` before
/// effects.
#[allow(clippy::too_many_arguments)]
fn try_emit_inline_numeric_callee(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    callee: &JitInlineCallee,
    dst: u16,
    callee_register: u16,
    argc: u16,
    argument_registers: &[u16],
    call_logical_pc: u32,
    call_byte_pc: u32,
    mut code_map: Option<&mut CodeMapCapture>,
    done: DynamicLabel,
    bail: DynamicLabel,
) -> Result<bool, Unsupported> {
    let template_plan = inline_callee_template_plan(view, callee)?;
    let Some(plan) = InlineLeafPlan::build_callee(callee, &template_plan, usize::from(argc)) else {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] template inline call skip fid={} argc={} params={} regs={} ops={:?}",
                callee.function_id,
                argc,
                callee.param_count,
                callee.register_count,
                template_plan
                    .instructions
                    .iter()
                    .map(|instruction| instruction.op)
                    .collect::<Vec<_>>(),
            );
        }
        return Ok(false);
    };
    let Some(arguments) = inline_argument_registers(argc, argument_registers) else {
        return Ok(false);
    };
    if std::env::var_os("OTTER_JIT_TRACE").is_some() {
        eprintln!(
            "[otter-jit] template inline call emit fid={} argc={} regs={} scratch_slots={} scratch_bytes={}",
            callee.function_id,
            argc,
            callee.register_count,
            plan.slot_count(),
            plan.aligned_scratch_bytes(),
        );
    }

    let inline_site = InlineSiteArtifact {
        caller_function_id: view.code_block.id,
        logical_pc: call_logical_pc,
        byte_pc: call_byte_pc,
        has_receiver_property: false,
    };
    let closure = ops.new_dynamic_label();
    let guarded = ops.new_dynamic_label();
    let guard_start = ops.offset().0;

    emit_load_reg(ops, 9, callee_register)?;
    emit_load_u64(ops, 10, value_tag::box_function_id(callee.function_id));
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x10
        ; b.eq =>guarded
        ; cbz x9, =>bail
    );
    emit_load_u64(ops, 10, value_tag::NOT_CELL_MASK);
    dynasm!(ops
        ; .arch aarch64
        ; tst x9, x10
        ; b.eq =>closure
        ; b =>bail
        ; =>closure
        // Heap-cell Values already carry the full pointer. No cage relocation
        // belongs on this path.
        ; ldrb w11, [x9]
        ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32
        ; b.ne =>bail
    );
    let closure_flags_byte = view.closure_call_layout.flags_byte;
    let closure_fid_byte = view.closure_call_layout.function_id_byte;
    if view.closure_call_layout.runtime_setup_flags != 0 {
        dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, closure_flags_byte]);
        emit_load_u64(
            ops,
            12,
            u64::from(view.closure_call_layout.runtime_setup_flags),
        );
        dynasm!(ops
            ; .arch aarch64
            ; tst w11, w12
            ; b.ne =>bail
        );
    }
    dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, closure_fid_byte]);
    emit_load_u64(ops, 12, u64::from(callee.function_id));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w12
        ; b.ne =>bail
        ; =>guarded
    );

    let guard_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_structural(
            "inlineCallGuard",
            guard_start,
            guard_end,
            inline_site,
            callee.function_id,
        ));
    }

    emit_inline_leaf_body(
        ops,
        relocations,
        view,
        &plan,
        InlineBodySpec {
            function_id: callee.function_id,
            parameter_count: callee.param_count,
            register_count: callee.register_count,
            arguments,
            receiver: None,
            method: None,
            inline_site,
            body_region: "inlineCallBody",
            hit_epilogue_region: "inlineCallHitEpilogue",
            deopt_teardown_region: "inlineCallDeoptTeardown",
        },
        dst,
        code_map,
        done,
        bail,
    )?;
    Ok(true)
}

fn emit_packed_args(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    packed_args: u64,
    tail: Option<TemplateTail>,
    role: TemplateOperandRole,
) {
    if let Some(tail) = tail {
        emit_load_symbol_u64(
            ops,
            relocations,
            register,
            packed_args,
            RelocationTarget::TemplateOperandSlice {
                arena: TemplateOperandArena::Registers,
                role,
                start: u32::try_from(tail.start).expect("template operand offset fits u32"),
                len: u32::try_from(tail.len).expect("template operand length fits u32"),
            },
        );
    } else {
        emit_load_u64(ops, register, packed_args);
    }
}

/// Emit `dst = callee(args…)` (plain `Op::Call`).
///
/// A baked monomorphic pure leaf gets an exact guarded splice. Otherwise a
/// baked stable code-entry plan emits the complete rooted native call in
/// machine code. Missing plans and all pre-effect guard/setup failures
/// deoptimize the original opcode; plain calls never prepare, replay, or enter
/// a generic call transition.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    mut direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    mut code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    argc: u16,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let done = ops.new_dynamic_label();
    if let Some(target) = view.static_native_calls.get(&byte_pc) {
        let site = StaticNativeCallSite {
            target,
            caller_function_id: view.code_block.id,
            logical_pc,
            byte_pc,
            argc: usize::from(argc),
        };
        if let Some(&argument) = argument_registers.first()
            && static_native_target_is_supported(view, site)
        {
            emit_load_reg(ops, 9, callee)?;
            emit_load_reg(ops, 10, argument)?;
            emit_static_native_call(
                ops,
                relocations,
                view,
                site,
                9,
                10,
                code_map.as_deref_mut(),
                bail,
            )?;
            emit_store_reg(ops, 9, dst)?;
            if let Some(events) = direct_call_events.as_deref_mut() {
                events.insert(
                    (byte_pc, 0),
                    otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                        instruction_pc: logical_pc,
                        byte_pc,
                        target: target.kind,
                        outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Generated,
                    },
                );
            }
            dynasm!(ops ; .arch aarch64 ; b =>done ; =>done);
            return Ok(());
        }
        if let Some(events) = direct_call_events.as_deref_mut() {
            events.insert(
                (byte_pc, 0),
                otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                    instruction_pc: logical_pc,
                    byte_pc,
                    target: target.kind,
                    outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Rejected {
                        reason: if argument_registers.is_empty() {
                            otter_vm::JitStaticNativeCallLoweringRejectionReason::ArityUnsupported
                        } else {
                            otter_vm::JitStaticNativeCallLoweringRejectionReason::LayoutUnsupported
                        },
                    },
                },
            );
        }
        dynasm!(ops ; .arch aarch64 ; b =>bail);
        return Ok(());
    }
    let direct_target = view.direct_callees.get(&byte_pc);
    if let Some(candidate) = view.inline_callees.get(&byte_pc)
        && try_emit_inline_numeric_callee(
            ops,
            relocations,
            view,
            candidate,
            dst,
            callee,
            argc,
            argument_registers,
            logical_pc,
            byte_pc,
            code_map.as_deref_mut(),
            done,
            bail,
        )?
    {
        if let (Some(events), Some(target)) = (direct_call_events.as_deref_mut(), direct_target) {
            events.insert(
                (byte_pc, 0),
                direct_call_lowering_event(
                    otter_vm::JitDirectCallKind::Plain,
                    logical_pc,
                    byte_pc,
                    target,
                    0,
                    1,
                    otter_vm::JitDirectCallLoweringOutcome::Inlined,
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    if let Some(target) = direct_target.filter(|target| direct_call_target_is_supported(target)) {
        emit_direct_call(
            ops,
            relocations,
            view,
            DirectCallSite {
                target,
                caller_function_id: view.code_block.id,
                logical_pc,
                byte_pc,
                dst,
                form: DirectCallForm::Plain { callable: callee },
                arguments: argument_registers,
            },
            table.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
            table.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            code_map,
            bail,
            threw,
            done,
        )?;
        if let Some(events) = direct_call_events.as_deref_mut() {
            events.insert(
                (byte_pc, 0),
                direct_call_lowering_event(
                    otter_vm::JitDirectCallKind::Plain,
                    logical_pc,
                    byte_pc,
                    target,
                    0,
                    1,
                    otter_vm::JitDirectCallLoweringOutcome::Generated {
                        code_object_id: target.plan.code_object_id,
                        target_tier: direct_call_target_tier(target),
                        this_mode: target.plan.this_mode,
                    },
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    if let (Some(events), Some(target)) = (direct_call_events, direct_target) {
        events.insert(
            (byte_pc, 0),
            direct_call_lowering_event(
                otter_vm::JitDirectCallKind::Plain,
                logical_pc,
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
    Ok(())
}

fn direct_call_target_tier(target: &otter_vm::JitDirectCallee) -> otter_vm::JitDebugTier {
    match target.plan.tier {
        abi::NativeFrameKind::Baseline => otter_vm::JitDebugTier::Template,
        abi::NativeFrameKind::Optimizing => otter_vm::JitDebugTier::Optimizing,
        abi::NativeFrameKind::Interpreter => {
            unreachable!("interpreter has no entry-capable code generation")
        }
    }
}

fn direct_call_lowering_event(
    call_kind: otter_vm::JitDirectCallKind,
    logical_pc: u32,
    byte_pc: u32,
    target: &otter_vm::JitDirectCallee,
    target_index: u32,
    target_count: u32,
    outcome: otter_vm::JitDirectCallLoweringOutcome,
) -> otter_vm::JitCompilerDiagnostic {
    otter_vm::JitCompilerDiagnostic::DirectCallLowered {
        call_kind,
        instruction_pc: logical_pc,
        byte_pc,
        callee_function_id: target.plan.function_id,
        target_index,
        target_count,
        outcome,
    }
}

/// Emit `dst = new callee(args…)` (`Op::New`).
///
/// The construct opcode has no direct-call fast path: it completes through
/// the single generic in-place construct transition, which runs the
/// interpreter's own `Construct` synchronously and writes `dst`. Status `0`
/// continues the compiled caller, `1` throws, and `2` (a non-constructor
/// callee) takes the exact side exit so the interpreter owns the `TypeError`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_construct(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, callee as u32
        ; movz x3, argc as u32
    );
    emit_packed_args(
        ops,
        relocations,
        4,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::ConstructArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CONSTRUCT),
        abi::STUB_JIT_CONSTRUCT,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
    );
}

/// Emit `dst = recv.name(args…)` (`Op::CallMethodValue`).
///
/// Leaf/inlined collection layers run first. Otherwise a VM-baked bounded
/// method-target chain remains native: generated code walks exact
/// receiver/prototype/method-slot guards in feedback order, then builds the
/// selected rooted callee frame directly. Missing plans and guard-chain misses
/// deoptimize the original opcode before method lookup effects.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    mut direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    mut code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    receiver: u16,
    name: u32,
    argc: u16,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    arg0: Option<u16>,
    arg1: Option<u16>,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), crate::entry::Unsupported> {
    let done = ops.new_dynamic_label();
    let method_site = MethodSite {
        dst,
        receiver,
        argc,
        arg0,
        arg1,
    };
    let planned_methods = view.direct_methods.get(&byte_pc);
    if planned_methods.is_none_or(|methods| methods.len() == 1 && methods[0].target_count == 1)
        && let Some(method) = view.inline_methods.get(&byte_pc)
        && try_emit_inline_numeric_method(
            ops,
            relocations,
            view,
            method,
            dst,
            receiver,
            argc,
            arg0,
            arg1,
            logical_pc,
            byte_pc,
            code_map.as_deref_mut(),
            done,
            bail,
        )?
    {
        if let (Some(events), Some(target)) = (
            direct_call_events.as_deref_mut(),
            planned_methods.and_then(|methods| methods.first()),
        ) {
            events.insert(
                (byte_pc, target.target_index),
                direct_call_lowering_event(
                    otter_vm::JitDirectCallKind::Method,
                    logical_pc,
                    byte_pc,
                    &target.callee,
                    target.target_index,
                    target.target_count,
                    otter_vm::JitDirectCallLoweringOutcome::Inlined,
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }
    // Guarded monomorphic collection fast paths precede existing dispatch;
    // every guard miss lands on the next layer.
    if let Some(leaf) = view.collection_leaf_methods.get(&byte_pc) {
        let after_leaf = ops.new_dynamic_label();
        if emit_leaf_method_guarded_call(
            ops,
            relocations,
            view,
            leaf,
            byte_pc,
            &method_site,
            after_leaf,
            done,
        )? {
            dynasm!(ops ; .arch aarch64 ; =>after_leaf);
        }
    }
    if let Some(guard) = view.primitive_method_guards.get(&byte_pc) {
        let after_primitive = ops.new_dynamic_label();
        if emit_primitive_method_guarded_call(
            ops,
            relocations,
            view,
            guard,
            byte_pc,
            &method_site,
            after_primitive,
            done,
        )? {
            dynasm!(ops ; .arch aarch64 ; =>after_primitive);
        }
    }
    if let Some(alloc) = view.collection_alloc_methods.get(&byte_pc) {
        let after_alloc = ops.new_dynamic_label();
        if emit_alloc_method_guarded_call(
            ops,
            relocations,
            view,
            alloc,
            byte_pc,
            &method_site,
            after_alloc,
            done,
        )? {
            dynasm!(ops ; .arch aarch64 ; =>after_alloc);
        }
    }
    for method in planned_methods.into_iter().flatten() {
        if !direct_call_target_is_supported(&method.callee) {
            if let Some(events) = direct_call_events.as_deref_mut() {
                events.insert(
                    (byte_pc, method.target_index),
                    direct_call_lowering_event(
                        otter_vm::JitDirectCallKind::Method,
                        logical_pc,
                        byte_pc,
                        &method.callee,
                        method.target_index,
                        method.target_count,
                        otter_vm::JitDirectCallLoweringOutcome::Rejected {
                            reason:
                                otter_vm::JitDirectCallLoweringRejectionReason::LayoutUnsupported,
                        },
                    ),
                );
            }
            continue;
        }
        let next_target = ops.new_dynamic_label();
        let direct_site = DirectCallSite {
            target: &method.callee,
            caller_function_id: view.code_block.id,
            logical_pc,
            byte_pc,
            dst,
            form: DirectCallForm::Method {
                callable: 17,
                receiver,
            },
            arguments: argument_registers,
        };
        let direct_call = direct_call_artifact(view, direct_site)?;
        let guard_start = ops.offset().0;
        emit_method_guard(
            ops,
            relocations,
            view,
            MethodGuardSite {
                guard: &method.guard,
                receiver,
            },
            17,
            None,
            next_target,
        )?;
        if let Some(code_map) = code_map.as_deref_mut() {
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
            ops,
            relocations,
            view,
            direct_site,
            table.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
            table.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            code_map.as_deref_mut(),
            bail,
            threw,
            done,
        )?;
        if let Some(events) = direct_call_events.as_deref_mut() {
            events.insert(
                (byte_pc, method.target_index),
                direct_call_lowering_event(
                    otter_vm::JitDirectCallKind::Method,
                    logical_pc,
                    byte_pc,
                    &method.callee,
                    method.target_index,
                    method.target_count,
                    otter_vm::JitDirectCallLoweringOutcome::Generated {
                        code_object_id: method.callee.plan.code_object_id,
                        target_tier: direct_call_target_tier(&method.callee),
                        this_mode: otter_vm::JitDirectCallThisMode::MethodReceiver,
                    },
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>next_target);
    }
    if argument_registers.len() <= MAX_METHOD_ARGS {
        let packed_meta = u64::from(dst) | (u64::from(receiver) << 16) | (u64::from(argc) << 32);
        super::spread_call::emit_spread_call_op(
            ops,
            relocations,
            table,
            Op::CallMethodValue as u8,
            packed_meta,
            pack_method_arg_regs(argument_registers),
            u64::from(name),
            bail,
            threw,
        );
        dynasm!(ops ; .arch aarch64 ; b =>done);
    } else {
        dynasm!(ops ; .arch aarch64 ; b =>bail);
    }
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

//! AArch64 call emitters for the template compiler.
//!
//! # Contents
//! - Compiler-generated monomorphic plain calls and bounded polymorphic method
//!   chains with a canonical generic-call continuation.
//! - Fixed base construction through shared generated linkage, with exact
//!   non-reentrant receiver preparation and observable fallback.
//! - Guarded read-only numeric call/method splicing from VM-baked metadata.
//! - Guarded collection-method leaves before generated method linkage.
//!
//! # Invariants
//! - Call guard/setup failure is effect-free and deoptimizes at the original
//!   opcode; accepted inline misses never replay it.
//! - A caller pushes only actual arguments and loads the current generation
//!   from its stable function-entry cell. The entered callee owns and initializes
//!   its rooted register window, including every missing formal.
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

use std::collections::{BTreeMap, BTreeSet};

use super::value_packet::{PacketWord, emit_value_packet_transition};

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;
use otter_vm::{JitCompileSnapshot, JitInlineCallee, JitInlineMethod};

use super::ic_probe::{
    emit_guarded_method_call, emit_native_entry_call, emit_native_leaf_call,
    emit_native_leaf_guard, guarded_method_call_is_supported, native_leaf_call_is_supported,
    native_leaf_call_name,
};
use super::transitions::TransitionTable;
use super::values::{
    emit_box_double, emit_box_int32, emit_box_number, emit_field_base, emit_load_reg,
    emit_load_u64, emit_num_to_double, emit_store_reg,
};
use crate::arm64::{MethodGuardSite, emit_method_guard};
use crate::artifact::relocation::RelocationCapture;
use crate::artifact::{
    CodeMapCapture, CodeRegion, InlineScratchEntryArtifact, InlineScratchLayoutArtifact,
    InlineSiteArtifact,
};
use crate::entry::{NUMBER_TAG_HI16, Unsupported, VALUE_UNDEFINED};
use crate::template::{
    ACCUMULATOR_DREG, ArithKind, FusedArithKind, FusedChainStep, InlineEntryValue, InlineLeafPlan,
    InlineScratchSlot, TemplateOp, TemplatePlan,
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
fn inline_leaf_template_plan(body: &JitCompileSnapshot) -> Result<TemplatePlan, Unsupported> {
    // The spliced body is planned from its own baked inputs. Its call sites
    // stay calls: a leaf body owns no further splice.
    let mut leaf_view = body.clone();
    leaf_view.inline_callees.clear();
    leaf_view.direct_callees.clear();
    leaf_view.direct_methods.clear();
    leaf_view.inline_methods.clear();
    leaf_view.inline_poly_methods.clear();
    TemplatePlan::build(&leaf_view)
}

fn inline_method_template_plan(method: &JitInlineMethod) -> Result<TemplatePlan, Unsupported> {
    inline_leaf_template_plan(&method.body)
}

fn inline_callee_template_plan(callee: &JitInlineCallee) -> Result<TemplatePlan, Unsupported> {
    inline_leaf_template_plan(&callee.body)
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
        ArithKind::Add => {
            return Err(Unsupported::OperandShape(
                "inline numeric addition has its own operation",
            ));
        }
    }
    emit_box_double(ops, 2, 13);
    emit_store_inline_slot(ops, 13, dst);
    Ok(())
}

/// Emit a fused unboxed-double chain inside an inline leaf body.
///
/// Each leaf is loaded once from its compact scratch slot, number-guarded, and
/// unboxed into a vector register; a non-number leaf deopts to the real call
/// (which owns the full coercion). The chain then runs entirely in
/// floating-point through [`ACCUMULATOR_DREG`], and only the results that
/// outlive the chain are boxed representation-preservingly and written back to
/// their scratch slots. This keeps the inline body's arithmetic as cheap as the
/// standalone fused callee while eliminating the call frame.
fn emit_inline_fused_chain(
    ops: &mut Assembler,
    plan: &InlineLeafPlan<'_>,
    steps: &[FusedChainStep],
    leaves: &[u16],
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    for (index, &leaf) in leaves.iter().enumerate() {
        let dst_d = ACCUMULATOR_DREG + 1 + u8::try_from(index).expect("leaf count is bounded");
        let slot = plan
            .register_slot(leaf)
            .ok_or(Unsupported::OperandShape("inline fused chain leaf"))?;
        emit_load_inline_slot(ops, 9, slot);
        emit_num_to_double(ops, 9, dst_d, miss);
    }
    for step in steps {
        let acc = ACCUMULATOR_DREG;
        let (a, b) = (step.a_dreg, step.b_dreg);
        match step.kind {
            FusedArithKind::Add => dynasm!(ops ; .arch aarch64 ; fadd D(acc), D(a), D(b)),
            FusedArithKind::Sub => dynasm!(ops ; .arch aarch64 ; fsub D(acc), D(a), D(b)),
            FusedArithKind::Mul => dynasm!(ops ; .arch aarch64 ; fmul D(acc), D(a), D(b)),
            FusedArithKind::Div => dynasm!(ops ; .arch aarch64 ; fdiv D(acc), D(a), D(b)),
        }
        if step.store_result {
            emit_box_number(ops, acc, 13);
            let slot = plan
                .register_slot(step.dst)
                .ok_or(Unsupported::OperandShape("inline fused chain result"))?;
            emit_store_inline_slot(ops, 13, slot);
        }
    }
    Ok(())
}

/// Emit inline unary negation over compact scratch storage.
///
/// A non-number operand deopts to the real call, which owns the full
/// `ToNumeric` (object `valueOf`, BigInt). For a Number, `-x` is computed in
/// f64 and boxed as a double: this reproduces the standalone negate exactly,
/// including `-0` (from `+0`) and `2147483648` (from `-i32::MIN`).
fn emit_inline_negate(
    ops: &mut Assembler,
    dst: InlineScratchSlot,
    src: InlineScratchSlot,
    miss: DynamicLabel,
) {
    emit_load_inline_slot(ops, 9, src);
    emit_num_to_double(ops, 9, 0, miss);
    dynasm!(ops ; .arch aarch64 ; fneg d1, d0);
    emit_box_double(ops, 1, 13);
    emit_store_inline_slot(ops, 13, dst);
}

fn emit_inline_receiver_property(
    ops: &mut Assembler,
    dst: InlineScratchSlot,
    field: otter_vm::object::FieldLocation,
    view: &JitCompileSnapshot,
    relocations: &mut RelocationCapture,
) -> Result<(), Unsupported> {
    // `x17` keeps the shape-proven receiver header. This field chooses its
    // immutable bank; overflow does not relocate the inline prefix. Eligible
    // inline bodies cannot collect or mutate the receiver before this load.
    dynasm!(ops ; .arch aarch64 ; mov x13, x17);
    emit_field_base(ops, relocations, view, 13, 14, field);
    let byte = field.byte_offset();
    if byte <= 32760 {
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x13, byte]);
    } else {
        emit_load_u64(ops, 9, u64::from(byte));
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x13, x9]);
    }
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
    // A fused chain replaces the per-operation stream up to its jump target; the
    // inline body emits the fused computation once and skips those operations.
    let mut skip_until: Option<u32> = None;
    for (operation_index, instruction) in plan.instructions().iter().enumerate() {
        if let Some(limit) = skip_until {
            if instruction.pc < limit {
                continue;
            }
            skip_until = None;
        }
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
                let field = *method
                    .prop_fields
                    .get(&instruction.byte_pc)
                    .ok_or(Unsupported::OperandShape("inline method property offset"))?;
                emit_inline_receiver_property(ops, dst, field, &method.body, relocations)?;
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
            TemplateOp::Negate { dst, src } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let src = plan
                    .register_slot(src)
                    .ok_or(Unsupported::OperandShape("inline scratch source"))?;
                emit_inline_negate(ops, dst, src, body_deopt);
            }
            // Emit the fused chain once, then skip the per-operation stream it
            // stands in for (up to its jump target).
            TemplateOp::FusedNumericChain {
                steps,
                leaves,
                jump_target,
            } => {
                emit_inline_fused_chain(
                    ops,
                    plan,
                    plan.chain_steps(steps),
                    plan.chain_leaves(leaves),
                    body_deopt,
                )?;
                skip_until = Some(jump_target);
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
    guard_miss: DynamicLabel,
    bail: DynamicLabel,
) -> Result<bool, Unsupported> {
    let template_plan = inline_method_template_plan(method)?;
    let Some(plan) = (view.cage_base != 0)
        .then(|| InlineLeafPlan::build_method(method, &template_plan, usize::from(argc)))
        .flatten()
    else {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] template inline method skip fid={} argc={} params={} regs={} ops={:?}",
                method.guard.method_fid,
                argc,
                method.param_count(),
                method.register_count(),
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
            method.register_count(),
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
        guard_miss,
    )?;
    if plan.has_receiver_property() {
        // The receiver header remains stable throughout the call-free body;
        // each sealed field selects its own immutable prefix/suffix bank.
        dynasm!(ops ; .arch aarch64 ; mov x17, x16);
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
        &plan,
        InlineBodySpec {
            function_id: method.guard.method_fid,
            parameter_count: method.param_count(),
            register_count: method.register_count(),
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
    let template_plan = inline_callee_template_plan(callee)?;
    let Some(plan) = InlineLeafPlan::build_callee(callee, &template_plan, usize::from(argc)) else {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] template inline call skip fid={} argc={} params={} regs={} ops={:?}",
                callee.function_id(),
                argc,
                callee.param_count(),
                callee.register_count(),
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
            callee.function_id(),
            argc,
            callee.register_count(),
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
    let guard_start = ops.offset().0;
    emit_load_reg(ops, 9, callee_register)?;
    crate::arm64::inline_guard::emit_inline_identity(ops, view, callee.function_id(), bail);

    let guard_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_structural(
            "inlineCallGuard",
            guard_start,
            guard_end,
            inline_site,
            callee.function_id(),
        ));
    }

    emit_inline_leaf_body(
        ops,
        relocations,
        &plan,
        InlineBodySpec {
            function_id: callee.function_id(),
            parameter_count: callee.param_count(),
            register_count: callee.register_count(),
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

/// Emit `dst = callee(args…)` (plain `Op::Call`).
///
/// A baked monomorphic pure leaf gets an exact guarded splice. Otherwise a
/// baked stable code-entry plan emits the complete rooted native call in
/// machine code. Pre-effect guard/setup failures of a generated edge
/// deoptimize the original opcode; a site without a generated edge completes
/// in place through the variadic call transition, so a polymorphic or
/// never-planned call never leaves generated code on every execution.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    spliced_functions: &mut BTreeSet<u32>,
    direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    argc: u16,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_call_with_receiver(
        ops,
        relocations,
        table,
        return_sites,
        view,
        spliced_functions,
        direct_call_events,
        code_map,
        dst,
        callee,
        None,
        argc,
        argument_registers,
        logical_pc,
        byte_pc,
        bail,
        threw,
        throw_value,
        fatal,
    )
}

/// [`emit_call`] with an explicit receiver.
///
/// `receiver` is `None` for `Op::Call` (the callee sees the canonical
/// `undefined` `this`) and `Some(register)` for `Op::CallWithThis`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call_with_receiver(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    spliced_functions: &mut BTreeSet<u32>,
    mut direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    mut code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    receiver: Option<u16>,
    argc: u16,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let done = ops.new_dynamic_label();
    if let Some(receiver) = receiver
        && let Some(target) = view
            .native_calls
            .get(&byte_pc)
            .and_then(|target| target.leaf())
        && let Some(declaration) =
            otter_vm::jit_static_native::jit_leaf_builtin(target.leaf_stub_id)
        && view.native_call_layout.identity_byte != 0
        && usize::from(argc) == usize::from(declaration.argument_count)
        && otter_vm::runtime_stubs::leaf_entry_shape(target.leaf_stub_id)
            .is_some_and(|shape| declaration.operand_words() <= usize::from(shape.words))
    {
        // An explicit-receiver call owns its loaded callee and `this`, so the
        // declared leaf needs only the callee identity; the entry proves its
        // own receiver. A miss enters the ordinary call, which performs the
        // complete call once.
        let start = ops.offset().0;
        let miss = ops.new_dynamic_label();
        let hit = ops.new_dynamic_label();
        let this_word = u8::from(declaration.this_operand);
        emit_load_reg(ops, 9, callee)?;
        emit_native_leaf_guard(ops, view, target.builtin_native_ref, 9, miss)?;
        emit_native_entry_call(
            ops,
            relocations,
            target.leaf_stub_id,
            abi::NO_SAFEPOINT,
            this_word + declaration.argument_count,
            20,
            |ops, index, register| {
                if this_word == 1 && index == 0 {
                    return emit_load_reg(ops, register, receiver);
                }
                let source = argument_registers
                    .get(usize::from(index - this_word))
                    .copied()
                    .ok_or(Unsupported::OperandShape("native leaf call argument"))?;
                emit_load_reg(ops, register, source)
            },
            miss,
        )?;
        emit_store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>hit ; =>miss);
        let leaf_end = ops.offset().0;
        // Static-native feedback names no bytecode callee, so the ordinary
        // call is the generic transition, exactly as for an unsupported arity.
        emit_trampoline_call(
            ops,
            relocations,
            table,
            return_sites,
            view,
            None,
            view.code_block.id,
            logical_pc,
            byte_pc,
            CallCallee::Register(callee),
            Some(receiver),
            CallNewTarget::None,
            CallActuals::Fixed(argument_registers),
            None,
            dst,
            throw_value,
            threw,
            fatal,
        )?;
        dynasm!(ops ; .arch aarch64 ; =>hit);
        let name = native_leaf_call_name(target.leaf_stub_id);
        if let Some(code_map) = code_map.as_deref_mut() {
            code_map.record(CodeRegion::static_native_structural(
                "nativeLeafCall",
                start,
                leaf_end,
                view.code_block.id,
                logical_pc,
                byte_pc,
                name,
            ));
        }
        if let Some(events) = direct_call_events.as_deref_mut() {
            events.insert(
                (byte_pc, 0),
                otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                    instruction_pc: logical_pc,
                    byte_pc,
                    target: name,
                    outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Generated,
                },
            );
        }
        return Ok(());
    }
    if let Some(target) = view
        .native_calls
        .get(&byte_pc)
        .and_then(|target| target.leaf())
        .filter(|_| receiver.is_none())
    {
        let stub_id = target.leaf_stub_id;
        let name = native_leaf_call_name(stub_id);
        let start = ops.offset().0;
        if native_leaf_call_is_supported(view, stub_id, usize::from(argc)) {
            // A baseline call never deoptimizes: a callee-identity or leaf
            // miss (an input the leaf does not cover) performs the ordinary
            // call once, as the explicit-receiver form does.
            let miss = ops.new_dynamic_label();
            emit_load_reg(ops, 9, callee)?;
            emit_native_leaf_call(
                ops,
                relocations,
                view,
                stub_id,
                target.builtin_native_ref,
                9,
                20,
                |ops, index, register| {
                    let source = argument_registers
                        .get(usize::from(index))
                        .copied()
                        .ok_or(Unsupported::OperandShape("native leaf call argument"))?;
                    emit_load_reg(ops, register, source)
                },
                miss,
            )?;
            emit_store_reg(ops, 0, dst)?;
            dynasm!(ops ; .arch aarch64 ; b =>done ; =>miss);
            let leaf_end = ops.offset().0;
            emit_trampoline_call(
                ops,
                relocations,
                table,
                return_sites,
                view,
                None,
                view.code_block.id,
                logical_pc,
                byte_pc,
                CallCallee::Register(callee),
                None,
                CallNewTarget::None,
                CallActuals::Fixed(argument_registers),
                None,
                dst,
                throw_value,
                threw,
                fatal,
            )?;
            if let Some(code_map) = code_map.as_deref_mut() {
                code_map.record(CodeRegion::static_native_structural(
                    "nativeLeafCall",
                    start,
                    leaf_end,
                    view.code_block.id,
                    logical_pc,
                    byte_pc,
                    name,
                ));
            }
            if let Some(events) = direct_call_events.as_deref_mut() {
                events.insert(
                    (byte_pc, 0),
                    otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                        instruction_pc: logical_pc,
                        byte_pc,
                        target: name,
                        outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Generated,
                    },
                );
            }
            dynasm!(ops ; .arch aarch64 ; =>done);
            return Ok(());
        }
        if let Some(events) = direct_call_events.as_deref_mut() {
            events.insert(
                (byte_pc, 0),
                otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
                    instruction_pc: logical_pc,
                    byte_pc,
                    target: name,
                    outcome: otter_vm::JitStaticNativeCallLoweringOutcome::Rejected {
                        reason:
                            otter_vm::JitStaticNativeCallLoweringRejectionReason::ArityUnsupported,
                    },
                },
            );
        }
        return emit_trampoline_call(
            ops,
            relocations,
            table,
            return_sites,
            view,
            code_map,
            view.code_block.id,
            logical_pc,
            byte_pc,
            CallCallee::Register(callee),
            receiver,
            CallNewTarget::None,
            CallActuals::Fixed(argument_registers),
            None,
            dst,
            throw_value,
            threw,
            fatal,
        );
    }
    let direct_target = view
        .direct_callees
        .get(&byte_pc)
        .and_then(|targets| targets.first());
    if let Some(candidate) = view
        .inline_callees
        .get(&byte_pc)
        .filter(|_| receiver.is_none())
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
        spliced_functions.insert(candidate.function_id());
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
                    otter_vm::JitDirectCallLoweringOutcome::Inlined,
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    let generated_target = direct_target.filter(|_| {
        view.direct_callees
            .get(&byte_pc)
            .is_some_and(|targets| targets.len() == 1)
    });
    emit_trampoline_call(
        ops,
        relocations,
        table,
        return_sites,
        view,
        code_map,
        view.code_block.id,
        logical_pc,
        byte_pc,
        CallCallee::Register(callee),
        receiver,
        CallNewTarget::None,
        CallActuals::Fixed(argument_registers),
        generated_target.map(|target| target.plan),
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    if let Some(target) = generated_target {
        super::super::record_generated_direct_call(
            direct_call_events,
            otter_vm::JitDirectCallKind::Plain,
            logical_pc,
            byte_pc,
            target,
            0,
            1,
        );
    }
    Ok(())
}

/// Emit `return callee(args…)` from a strict tail position (§15.10.3).
///
/// A called record whose span holds the callee's actuals pushes them like a
/// call's and hands its place to the callee (see
/// [`crate::arm64::frame::emit_tail_transfer`]): a chain of tail calls runs
/// in constant native stack and every callee returns straight to the
/// chain's caller. A site with one proven bytecode target enters its current
/// generation behind the identity guard; any other callee goes through the
/// generic entry. Actuals that outgrow the record's span are staged as the
/// context's request, and the record retires with `Continue`; its caller
/// enters the request in its place. A constructing record calls like
/// [`emit_call`] into `dst`, and the return that follows completes it. A
/// tier-entered frame and a pending interrupt reach `leave`, an exact exit
/// at this instruction: the interpreter performs the replacement there.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_tail_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    spliced_functions: &mut BTreeSet<u32>,
    direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    argc: u16,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    leave: DynamicLabel,
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    saved_pairs: u8,
) -> Result<(), Unsupported> {
    use crate::arm64::js_call::CallTarget;
    let start = ops.offset().0;
    let ordinary = ops.new_dynamic_label();
    let outgrown = ops.new_dynamic_label();
    crate::arm64::frame::emit_tail_admission(ops, leave, ordinary);
    let known = view
        .direct_callees
        .get(&byte_pc)
        .filter(|targets| targets.len() == 1)
        .and_then(|targets| targets.first())
        .map(|target| target.plan);
    if let Some(plan) = known {
        let generic = ops.new_dynamic_label();
        emit_load_reg(ops, 9, callee)?;
        crate::arm64::inline_guard::emit_cached_identity(
            ops,
            relocations,
            view,
            plan,
            logical_pc,
            generic,
        );
        emit_tail_path(
            ops,
            relocations,
            table,
            callee,
            argument_registers,
            CallTarget::Known {
                entry_cell: plan.entry_cell,
                function_id: plan.function_id,
            },
            outgrown,
            saved_pairs,
        )?;
        dynasm!(ops ; .arch aarch64 ; =>generic);
    }
    if matches!(
        view.native_calls.get(&byte_pc),
        Some(otter_vm::JitNativeCall::Native)
    ) {
        let generic = ops.new_dynamic_label();
        emit_load_reg(ops, 9, callee)?;
        crate::arm64::js_call::emit_native_kind_guard(ops, 9, generic);
        emit_tail_path(
            ops,
            relocations,
            table,
            callee,
            argument_registers,
            CallTarget::Native,
            outgrown,
            saved_pairs,
        )?;
        dynasm!(ops ; .arch aarch64 ; =>generic);
    }
    emit_tail_path(
        ops,
        relocations,
        table,
        callee,
        argument_registers,
        CallTarget::Generic,
        outgrown,
        saved_pairs,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>outgrown);
    let mut words = vec![PacketWord::Register(callee)];
    words.extend(argument_registers.iter().copied().map(PacketWord::Register));
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_value_packet_transition(
        ops,
        relocations,
        table,
        abi::STUB_JIT_STAGE_TAIL_CALL,
        &words,
        None,
        throw_value,
        fatal,
    )?;
    crate::arm64::frame::emit_tail_return(ops, saved_pairs);
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::call_structural(
            "tailCall",
            start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            known.map(|plan| plan.function_id),
        ));
    }
    dynasm!(ops ; .arch aarch64 ; =>ordinary);
    emit_call(
        ops,
        relocations,
        table,
        return_sites,
        view,
        spliced_functions,
        direct_call_events,
        None,
        dst,
        callee,
        argc,
        argument_registers,
        logical_pc,
        byte_pc,
        bail,
        threw,
        throw_value,
        fatal,
    )
}

/// One target of [`emit_tail_call`]: check that the actuals fit the
/// record's span, push them, and hand the
/// record to `target`; actuals that outgrow the span reach `outgrown`.
#[allow(clippy::too_many_arguments)]
fn emit_tail_path(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    callee: u16,
    argument_registers: &[u16],
    target: crate::arm64::js_call::CallTarget,
    outgrown: DynamicLabel,
    saved_pairs: u8,
) -> Result<(), Unsupported> {
    let bytes = crate::call_linkage::pushed_argument_bytes(argument_registers.len())?;
    crate::arm64::frame::emit_tail_span_check(ops, bytes / 8, outgrown);
    crate::arm64::js_call::emit_push_arguments(
        ops,
        argument_registers.len(),
        |ops, index, register, _| {
            emit_load_reg(ops, register, argument_registers[index])?;
            Ok(register)
        },
    )?;
    emit_load_reg(ops, 13, callee)?;
    let count = u32::try_from(argument_registers.len())
        .map_err(|_| Unsupported::OperandShape("tail call actual count"))?;
    crate::arm64::frame::emit_tail_transfer(ops, relocations, table, bytes, count, target, saved_pairs);
    Ok(())
}

/// Deliver a call completion in `x0`/`x1`: success to `dst`, a throw to
/// `throw_value`, the callee's final failure to `fatal` unchanged and any
/// other parked error to `threw`. A callee that retired itself for a tail
/// call it staged returns `Continue`; that call is entered in its place and
/// its completion delivered the same way.
pub(super) fn emit_call_completion(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    dst: u16,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let completion = ops.new_dynamic_label();
    let abrupt = ops.new_dynamic_label();
    let error = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>completion ; cbnz x1, =>abrupt);
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops
        ; .arch aarch64
        ; b =>done
        ; =>abrupt
        ; cmp x1, abi::NativeResultStatus::Continue as u32
        ; b.ne =>error
    );
    // The callee retired itself for a tail call it staged: enter that call
    // in its place.
    return_sites.record(crate::arm64::js_call::emit_enter_staged(
        ops,
        relocations,
        table,
        20,
    ))?;
    dynasm!(ops
        ; .arch aarch64
        ; b =>completion
        ; =>error
    );
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; cmp x1, abi::NativeResultStatus::Fatal as u32
        ; b.eq =>fatal
        ; b =>threw
        ; =>done
    );
    Ok(())
}

/// Callee operand of a classified call.
#[derive(Debug, Clone, Copy)]
pub(super) enum CallCallee {
    /// A caller register.
    Register(u16),
    /// The callable a method guard left in `x17`.
    GuardedMethod,
    /// The callable the method resolution entry returned in `x0`.
    ResolvedMethod,
}

/// `new.target` operand of a classified call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallNewTarget {
    /// `[[Call]]`.
    None,
    /// `new C(...)`: the constructor itself.
    Callee,
    /// `super(...)`: the frame's `new.target`, or the parent outside a
    /// construct.
    Super,
}

/// Actual arguments of a classified call.
#[derive(Debug, Clone, Copy)]
pub(super) enum CallActuals<'a> {
    /// Caller registers, pushed in order.
    Fixed(&'a [u16]),
    /// A staging entry already wrote the span.
    Staged,
}

/// Call one callee and deliver its completion to `dst`.
///
/// A proven bytecode target is entered through its current generation after
/// its identity guard; every other callee through the generic entry. The
/// callee owns its activation, receiver binding, constructor completion and
/// deoptimization. A throw reaches `throw_value` with the exception in `x0`;
/// any other abrupt completion parks its error and reaches `threw`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_trampoline_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    code_map: Option<&mut CodeMapCapture>,
    caller_function_id: u32,
    logical_pc: u32,
    byte_pc: u32,
    callee: CallCallee,
    receiver: Option<u16>,
    new_target: CallNewTarget,
    actuals: CallActuals<'_>,
    known: Option<otter_vm::jit::JitDirectCallPlan>,
    dst: u16,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    use crate::arm64::js_call::{
        CallTarget, emit_call, emit_pop_arguments, emit_push_arguments, emit_staged_call,
    };
    let start = ops.offset().0;
    let done = ops.new_dynamic_label();
    // `[[Construct]]` enters a proven target directly only when it has the
    // internal method; classification throws otherwise.
    let known = known.filter(|plan| {
        new_target == CallNewTarget::None || plan.call_flags & abi::FUNCTION_CALL_CONSTRUCTIBLE != 0
    });
    let native = matches!(
        view.native_calls.get(&byte_pc),
        Some(otter_vm::JitNativeCall::Native)
    );
    for (plan, target) in known
        .map(|plan| {
            (
                Some(plan),
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            )
        })
        .into_iter()
        .chain(native.then_some((None, CallTarget::Native)))
    {
        let CallActuals::Fixed(arguments) = actuals else {
            break;
        };
        let generic = ops.new_dynamic_label();
        match callee {
            CallCallee::Register(callee) => emit_load_reg(ops, 9, callee)?,
            CallCallee::GuardedMethod => dynasm!(ops ; .arch aarch64 ; mov x9, x17),
            CallCallee::ResolvedMethod => dynasm!(ops ; .arch aarch64 ; mov x9, x0),
        }
        if let Some(plan) = plan {
            crate::arm64::inline_guard::emit_cached_identity(
                ops,
                relocations,
                view,
                plan,
                logical_pc,
                generic,
            );
        } else {
            crate::arm64::js_call::emit_native_kind_guard(ops, 9, generic);
        }
        // The proven method callable stays in `x12` through the span push.
        if !matches!(callee, CallCallee::Register(_)) {
            dynasm!(ops ; .arch aarch64 ; mov x12, x9);
        }
        let count = arguments.len();
        let bytes = emit_push_arguments(ops, count, |ops, index, register, _| {
            emit_load_reg(ops, register, arguments[index])?;
            Ok(register)
        })?;
        emit_trampoline_operands(ops, callee, receiver, new_target)?;
        let count =
            u32::try_from(count).map_err(|_| Unsupported::OperandShape("call actual count"))?;
        return_sites.record(emit_call(
            ops,
            relocations,
            table,
            20,
            12,
            receiver.map(|_| 13),
            (new_target != CallNewTarget::None).then_some(14),
            Some(count),
            target,
        ))?;
        emit_pop_arguments(ops, bytes);
        emit_call_completion(
            ops,
            relocations,
            table,
            return_sites,
            dst,
            throw_value,
            threw,
            fatal,
        )?;
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>generic);
    }
    // The method callable survives the span push: it clobbers only x15/x16.
    match callee {
        CallCallee::GuardedMethod => dynasm!(ops ; .arch aarch64 ; mov x12, x17),
        CallCallee::ResolvedMethod => dynasm!(ops ; .arch aarch64 ; mov x12, x0),
        CallCallee::Register(_) => {}
    }
    match actuals {
        CallActuals::Fixed(arguments) => {
            let bytes = emit_push_arguments(ops, arguments.len(), |ops, index, register, _| {
                emit_load_reg(ops, register, arguments[index])?;
                Ok(register)
            })?;
            let count = u32::try_from(arguments.len())
                .map_err(|_| Unsupported::OperandShape("call actual count"))?;
            emit_trampoline_operands(ops, callee, receiver, new_target)?;
            return_sites.record(emit_call(
                ops,
                relocations,
                table,
                20,
                12,
                receiver.map(|_| 13),
                (new_target != CallNewTarget::None).then_some(14),
                Some(count),
                CallTarget::Generic,
            ))?;
            emit_pop_arguments(ops, bytes);
        }
        CallActuals::Staged => {
            emit_trampoline_operands(ops, callee, receiver, new_target)?;
            return_sites.record(emit_staged_call(
                ops,
                relocations,
                table,
                20,
                12,
                receiver.map(|_| 13),
                (new_target != CallNewTarget::None).then_some(14),
            ))?;
        }
    }
    emit_call_completion(
        ops,
        relocations,
        table,
        return_sites,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::call_structural(
            "callTrampoline",
            start,
            ops.offset().0,
            caller_function_id,
            logical_pc,
            byte_pc,
            known.map(|plan| plan.function_id),
        ));
    }
    Ok(())
}

/// Load the callee into `x12`, the receiver into `x13` and `new.target` into
/// `x14`. A method callable already sits in `x12`.
fn emit_trampoline_operands(
    ops: &mut Assembler,
    callee: CallCallee,
    receiver: Option<u16>,
    new_target: CallNewTarget,
) -> Result<(), Unsupported> {
    if let CallCallee::Register(callee) = callee {
        emit_load_reg(ops, 12, callee)?;
    }
    if let Some(receiver) = receiver {
        emit_load_reg(ops, 13, receiver)?;
    }
    // Only this verified Super operation may create a synchronous origin.
    // Every ordinary/new path overwrites the mailbox, including guard misses.
    match new_target {
        CallNewTarget::Super => dynasm!(ops ; .arch aarch64
            ; str x21, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET]),
        CallNewTarget::Callee => dynasm!(ops ; .arch aarch64
            ; str xzr, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET]),
        CallNewTarget::None => {}
    }
    match new_target {
        CallNewTarget::None => {}
        CallNewTarget::Callee => dynasm!(ops ; .arch aarch64 ; mov x14, x12),
        CallNewTarget::Super => {
            let ready = ops.new_dynamic_label();
            emit_load_u64(ops, 15, VALUE_UNDEFINED);
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [x21, crate::entry::NATIVE_FRAME_NEW_TARGET_OFFSET]
                ; cmp x14, x15
                ; b.ne =>ready
                ; mov x14, x12
                ; =>ready
            );
        }
    }
    Ok(())
}

pub(super) fn direct_call_lowering_event(
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

/// Emit fixed-arity `new callee(args…)` or `super(args…)` through the call
/// trampoline, which classifies the constructor, creates or passes its
/// receiver and applies the constructor completion.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_construct(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    direct_call_events: Option<&mut crate::template::DirectCallEvents>,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    argument_registers: &[u16],
    super_construct: bool,
    logical_pc: u32,
    byte_pc: u32,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_trampoline_call(
        ops,
        relocations,
        table,
        return_sites,
        view,
        code_map,
        view.code_block.id,
        logical_pc,
        byte_pc,
        CallCallee::Register(callee),
        None,
        if super_construct {
            CallNewTarget::Super
        } else {
            CallNewTarget::Callee
        },
        CallActuals::Fixed(argument_registers),
        view.direct_constructs
            .get(&byte_pc)
            .map(|target| target.plan),
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    if let Some(target) = view
        .direct_constructs
        .get(&byte_pc)
        .filter(|target| target.plan.call_flags & abi::FUNCTION_CALL_CONSTRUCTIBLE != 0)
    {
        crate::template::record_generated_direct_call(
            direct_call_events,
            crate::template::construct_call_kind(
                super_construct,
                target.plan.is_derived_constructor,
            ),
            logical_pc,
            byte_pc,
            target,
            0,
            1,
        );
    }
    Ok(())
}

/// Emit `dst = recv.name(args…)` (`Op::CallMethodValue`).
///
/// Leaf/inlined collection layers run first. Otherwise a VM-baked bounded
/// method-target chain remains native: generated code walks exact
/// receiver/prototype/method-slot guards in feedback order, then builds the
/// selected rooted callee frame directly. Missing plans and final guard-chain
/// misses build one contiguous boxed-value packet and complete through the
/// canonical method-call boundary exactly once.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    shared_probes: &mut super::shared_property::SharedPropertyProbes,
    view: &JitCompileSnapshot,
    spliced_functions: &mut BTreeSet<u32>,
    mut direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    mut code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    receiver: u16,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    arg0: Option<u16>,
    arg1: Option<u16>,
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), crate::entry::Unsupported> {
    let argc = u16::try_from(argument_registers.len())
        .map_err(|_| Unsupported::OperandShape("template method argument count"))?;
    let done = ops.new_dynamic_label();
    // A guarded call straight into a declared entry precedes every other
    // layer; its guard miss lands on the next one.
    if let Some(call) = view
        .guarded_method_calls
        .get(&byte_pc)
        .filter(|call| usize::from(argc) == usize::from(call.argument_count))
        .filter(|call| guarded_method_call_is_supported(view, call))
    {
        let leaf_miss = ops.new_dynamic_label();
        let method_arguments = [arg0, arg1];
        emit_guarded_method_call(
            ops,
            relocations,
            view,
            call,
            receiver,
            byte_pc,
            |ops, index, register| {
                let source = method_arguments
                    .get(usize::from(index))
                    .copied()
                    .flatten()
                    .ok_or(Unsupported::OperandShape("guarded method argument"))?;
                emit_load_reg(ops, register, source)
            },
            leaf_miss,
        )?;
        emit_store_reg(ops, 0, dst)?;
        dynasm!(ops
            ; .arch aarch64
            ; b =>done
            ; =>leaf_miss
        );
    }
    let planned_methods = view.direct_methods.get(&byte_pc);
    // A monomorphic inlinable body keeps its receiver guard as a fall-through
    // to the layers below, exactly as the polymorphic chain does: an unseen
    // shape reaches the direct call or the generic packet instead of pinning
    // the site back to the interpreter. Only a bail raised inside the
    // accepted body is a real deopt.
    if planned_methods.is_none_or(|methods| methods.len() == 1 && methods[0].target_count == 1)
        && let Some(method) = view.inline_methods.get(&byte_pc)
    {
        let inline_miss = ops.new_dynamic_label();
        if try_emit_inline_numeric_method(
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
            inline_miss,
            bail,
        )? {
            spliced_functions.insert(method.guard.method_fid);
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
            dynasm!(ops ; .arch aarch64 ; =>inline_miss);
        }
    }
    // A site that observed several inlinable shapes emits one guarded body per
    // shape. Each guard miss falls through to the next candidate rather than
    // deoptimizing, so an unobserved shape reaches the ordinary dispatch below
    // instead of pinning the whole site back to the interpreter. Only a bail
    // raised inside an accepted body is a real deopt.
    for method in view.inline_poly_methods.get(&byte_pc).into_iter().flatten() {
        let next_candidate = ops.new_dynamic_label();
        if try_emit_inline_numeric_method(
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
            next_candidate,
            bail,
        )? {
            spliced_functions.insert(method.guard.method_fid);
            if let (Some(events), Some(target)) = (
                direct_call_events.as_deref_mut(),
                planned_methods
                    .into_iter()
                    .flatten()
                    .find(|target| target.guard == method.guard),
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
            dynasm!(ops ; .arch aarch64 ; =>next_candidate);
        }
    }
    // A planned method chain proves the callable from exact receiver and
    // holder identities; the trampoline then enters it like any callee.
    for method in planned_methods.into_iter().flatten() {
        let next_target = ops.new_dynamic_label();
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
                "methodGuard",
                guard_start,
                ops.offset().0,
                view.code_block.id,
                logical_pc,
                byte_pc,
                receiver,
                &method.guard,
            ));
        }
        emit_trampoline_call(
            ops,
            relocations,
            table,
            return_sites,
            view,
            code_map.as_deref_mut(),
            view.code_block.id,
            logical_pc,
            byte_pc,
            CallCallee::GuardedMethod,
            Some(receiver),
            CallNewTarget::None,
            CallActuals::Fixed(argument_registers),
            Some(method.callee.plan),
            dst,
            throw_value,
            threw,
            fatal,
        )?;
        super::super::record_generated_direct_call(
            direct_call_events.as_deref_mut(),
            otter_vm::JitDirectCallKind::Method,
            logical_pc,
            byte_pc,
            &method.callee,
            method.target_index,
            method.target_count,
        );
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>next_target);
    }
    // Every other receiver first probes the isolate's shared property-action
    // table, as a V8 megamorphic call site probes its stub cache; a callable
    // hit is called generically. A miss resolves through the committed
    // method resolution, which also records the site's call feedback.
    let resolved = ops.new_dynamic_label();
    let access = view
        .property_accesses
        .get(&byte_pc)
        .filter(|_| view.cage_base != 0);
    if let Some(access) = access {
        use super::shared_property::{METHOD_ATOM, METHOD_RECEIVER};
        let probe = shared_probes.method_label(ops);
        let resolve = ops.new_dynamic_label();
        emit_load_reg(ops, METHOD_RECEIVER, receiver)?;
        super::values::emit_load_u64(ops, METHOD_ATOM, u64::from(access.atom));
        dynasm!(ops ; .arch aarch64
            ; bl =>probe
            ; cbz x1, =>resolved
            ; =>resolve);
    }
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_value_packet_transition(
        ops,
        relocations,
        table,
        abi::STUB_JIT_RESOLVE_METHOD,
        &[PacketWord::Register(receiver)],
        None,
        throw_value,
        fatal,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>resolved);
    emit_trampoline_call(
        ops,
        relocations,
        table,
        return_sites,
        view,
        code_map,
        view.code_block.id,
        logical_pc,
        byte_pc,
        CallCallee::ResolvedMethod,
        Some(receiver),
        CallNewTarget::None,
        CallActuals::Fixed(argument_registers),
        None,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

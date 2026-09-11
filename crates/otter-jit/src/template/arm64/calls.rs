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
use otter_vm::native_abi as abi;
use otter_vm::{
    JitCompileSnapshot, JitInlineCallee, JitInlineMethod, closure::JS_CLOSURE_BODY_TYPE_TAG,
    value::tag as value_tag,
};

use super::ic_probe::{
    emit_guarded_method_call, emit_native_leaf_call, guarded_method_call_is_supported,
    native_leaf_call_is_supported, native_leaf_call_name,
};
use super::transitions::TransitionTable;
use super::values::{
    CellTest, emit_box_double, emit_box_int32, emit_box_number, emit_cell_test, emit_load_reg,
    emit_load_runtime_stub, emit_load_symbol_u64, emit_load_u64, emit_num_to_double,
    emit_slab_base, emit_store_reg,
};
use crate::arm64::{
    DirectCallArguments, DirectCallForm, DirectCallSite, MethodGuardSite, direct_call_artifact,
    direct_call_target_is_supported, emit_direct_call, emit_method_guard,
};
use crate::artifact::relocation::{
    RelocationCapture, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
};
use crate::artifact::{
    CodeMapCapture, CodeRegion, InlineScratchEntryArtifact, InlineScratchLayoutArtifact,
    InlineSiteArtifact,
};
use crate::entry::{NUMBER_TAG_HI16, Unsupported, VALUE_UNDEFINED};
use crate::template::{
    ACCUMULATOR_DREG, ArithKind, FusedArithKind, FusedChainStep, InlineEntryValue, InlineLeafPlan,
    InlineScratchSlot, TemplateOp, TemplatePlan, TemplateTail,
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
    value_byte: u32,
) -> Result<(), Unsupported> {
    // `x17` holds the receiver's slab base, derived once from the exact
    // receiver body after its type/shape guard. Eligible inline bodies cannot
    // call, allocate, branch, or mutate, so neither the receiver nor its slab
    // can move or change before this load.
    dynasm!(ops ; .arch aarch64 ; ldr x9, [x17, value_byte]);
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
                let value_byte = *method
                    .prop_offsets
                    .get(&instruction.byte_pc)
                    .ok_or(Unsupported::OperandShape("inline method property offset"))?;
                emit_inline_receiver_property(ops, dst, value_byte)?;
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
        false,
        guard_miss,
    )?;
    if plan.has_receiver_property() {
        // Receiver-property offsets were baked from `recv_shape`, whose guard
        // already succeeded above. Materialize its slab once for every sealed
        // receiver-property load in the straight-line inline body.
        dynasm!(ops ; .arch aarch64 ; mov x13, x16);
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x13, =>guard_miss
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
    let guarded = ops.new_dynamic_label();
    let guard_start = ops.offset().0;

    emit_load_reg(ops, 9, callee_register)?;
    emit_load_u64(ops, 10, value_tag::box_function_id(callee.function_id()));
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x10
        ; b.eq =>guarded
        ; cbz x9, =>bail
    );
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, bail);
    dynasm!(ops
        ; .arch aarch64
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
    // Frameless leaf inlining has no callee NativeFrame slot to carry a
    // closure's dynamic eval chain. A generated call can propagate this
    // handle; an inline candidate must prove it null before entering.
    dynasm!(ops
        ; .arch aarch64
        ; ldr w11, [x9, view.closure_call_layout.eval_env_byte]
        ; cbnz w11, =>bail
    );
    dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, closure_fid_byte]);
    emit_load_u64(ops, 12, u64::from(callee.function_id()));
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
            callee.function_id(),
        ));
    }

    emit_inline_leaf_body(
        ops,
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
/// machine code. Pre-effect guard/setup failures of a generated edge
/// deoptimize the original opcode; a site without a generated edge completes
/// in place through the variadic call transition, so a polymorphic or
/// never-planned call never leaves generated code on every execution.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
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
        view,
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
    view: &JitCompileSnapshot,
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
    if let Some(target) = view
        .static_native_calls
        .get(&byte_pc)
        .filter(|_| receiver.is_none())
    {
        let stub_id = target.leaf_stub_id;
        let name = native_leaf_call_name(stub_id);
        let start = ops.offset().0;
        if native_leaf_call_is_supported(view, stub_id, usize::from(argc)) {
            emit_load_reg(ops, 9, callee)?;
            emit_native_leaf_call(
                ops,
                relocations,
                view,
                stub_id,
                target.builtin_native_ref,
                9,
                |ops, index, register| {
                    let source = argument_registers
                        .get(usize::from(index))
                        .copied()
                        .ok_or(Unsupported::OperandShape("native leaf call argument"))?;
                    emit_load_reg(ops, register, source)
                },
                bail,
            )?;
            emit_store_reg(ops, 0, dst)?;
            if let Some(code_map) = code_map.as_deref_mut() {
                code_map.record(CodeRegion::static_native_structural(
                    "nativeLeafCall",
                    start,
                    ops.offset().0,
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
            dynasm!(ops ; .arch aarch64 ; b =>done ; =>done);
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
        return emit_generic_call_transition(
            ops,
            relocations,
            table,
            view,
            None,
            code_map,
            dst,
            callee,
            receiver,
            argument_registers,
            logical_pc,
            byte_pc,
            bail,
            threw,
            throw_value,
            fatal,
        );
    }
    let direct_target = view.direct_callees.get(&byte_pc);
    if let Some(candidate) = view
        .inline_callees
        .get(&byte_pc)
        .filter(|_| receiver.is_none())
        && try_emit_inline_numeric_callee(
            ops,
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
                target_index: 0,
                target_count: 1,
                caller_function_id: view.code_block.id,
                logical_pc,
                byte_pc,
                dst,
                form: match receiver {
                    None => DirectCallForm::Plain { callable: callee },
                    Some(receiver) => DirectCallForm::CallWithThis {
                        callable: callee,
                        receiver,
                    },
                },
                arguments: DirectCallArguments::Fixed(argument_registers),
            },
            table.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
            table.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            table.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
            code_map,
            bail,
            threw,
            throw_value,
            fatal,
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
                otter_vm::JitDirectCallLoweringOutcome::Rejected {
                    reason: otter_vm::JitDirectCallLoweringRejectionReason::LayoutUnsupported,
                },
            ),
        );
    }
    emit_generic_call_transition(
        ops,
        relocations,
        table,
        view,
        direct_call_events,
        code_map,
        dst,
        callee,
        receiver,
        argument_registers,
        logical_pc,
        byte_pc,
        bail,
        threw,
        throw_value,
        fatal,
    )
}

/// Number of 16-bit register lanes the in-place call transition carries in
/// each of its two argument words.
pub(super) const GENERIC_CALL_LANES_PER_WORD: usize = 4;

/// Pack up to eight argument registers into the two lane words of the
/// in-place call transition; `None` when the site has more.
pub(super) fn pack_generic_call_lanes(argument_registers: &[u16]) -> Option<(u64, u64)> {
    if argument_registers.len() > 2 * GENERIC_CALL_LANES_PER_WORD {
        return None;
    }
    let mut words = [0u64; 2];
    for (index, register) in argument_registers.iter().enumerate() {
        words[index / GENERIC_CALL_LANES_PER_WORD] |=
            u64::from(*register) << ((index % GENERIC_CALL_LANES_PER_WORD) * 16);
    }
    Some((words[0], words[1]))
}

/// Complete `Op::Call` / `Op::CallWithThis` in place through the variadic
/// call transition when the site owns no generated edge.
///
/// The transition reads the callee, receiver, and arguments from the
/// published register window, runs the canonical call, and writes the result
/// back; a throw reaches the frame's committed-throw router. Only a site
/// wider than the transition's eight argument lanes keeps the exact
/// pre-effect side exit.
#[allow(clippy::too_many_arguments)]
fn emit_generic_call_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    receiver: Option<u16>,
    argument_registers: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let Some((lanes_low, lanes_high)) = pack_generic_call_lanes(argument_registers) else {
        dynasm!(ops ; .arch aarch64 ; b =>bail);
        return Ok(());
    };
    let argc = u64::try_from(argument_registers.len())
        .map_err(|_| Unsupported::OperandShape("generic call argument count"))?;
    let (opcode, this_lane) = match receiver {
        Some(receiver) => (otter_bytecode::Op::CallWithThis, u64::from(receiver)),
        None => (otter_bytecode::Op::Call, 0),
    };
    super::spread_call::emit_spread_call_op(
        ops,
        relocations,
        table,
        view,
        direct_call_events,
        code_map,
        opcode as u8,
        u64::from(dst) | (u64::from(callee) << 16) | (this_lane << 32) | (argc << 48),
        lanes_low,
        lanes_high,
        logical_pc,
        byte_pc,
        bail,
        threw,
        throw_value,
        fatal,
    )
}

pub(super) fn direct_call_target_tier(
    target: &otter_vm::JitDirectCallee,
) -> otter_vm::JitDebugTier {
    match target.plan.tier {
        abi::NativeFrameKind::Baseline => otter_vm::JitDebugTier::Template,
        abi::NativeFrameKind::Optimizing => otter_vm::JitDebugTier::Optimizing,
        abi::NativeFrameKind::Interpreter => {
            unreachable!("interpreter has no entry-capable code generation")
        }
    }
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

/// Emit fixed-arity `new callee(args…)` or `super(args…)`.
///
/// A baked target uses the shared stack-owned generated linkage. An unplanned
/// site completes through the single generic in-place construct transition,
/// which runs the interpreter's own `Construct` synchronously and writes
/// `dst`. `Success` continues, `Throw` enters the parked-error epilogue, and
/// `SideExit` (a non-constructor callee) leaves before effects so the
/// interpreter owns the `TypeError`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_construct(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    direct_call_events: Option<&mut BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>>,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
    argument_registers: &[u16],
    super_construct: bool,
    logical_pc: u32,
    byte_pc: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    // Keep fixed `super()` on the in-place transition until template entry
    // tiering accounts for a generated superclass edge. Entering it directly
    // here starves the derived body of the feedback/hotness that currently
    // publishes its Machine IR super linkage. Ordinary fixed `new` has no such
    // tier-policy dependency and uses the shared generated path below.
    let direct_target = (!super_construct)
        .then(|| view.direct_constructs.get(&byte_pc))
        .flatten();
    if let Some(target) = direct_target.filter(|target| direct_call_target_is_supported(target)) {
        let done = ops.new_dynamic_label();
        let kind = match (super_construct, target.plan.is_derived_constructor) {
            (false, false) => otter_vm::JitDirectCallKind::Construct,
            (false, true) => otter_vm::JitDirectCallKind::DerivedConstruct,
            (true, false) => otter_vm::JitDirectCallKind::SuperConstruct,
            (true, true) => otter_vm::JitDirectCallKind::DerivedSuperConstruct,
        };
        let form = match kind {
            otter_vm::JitDirectCallKind::Construct => DirectCallForm::Construct {
                callable: callee,
                receiver: dst,
            },
            otter_vm::JitDirectCallKind::DerivedConstruct => {
                DirectCallForm::DerivedConstruct { callable: callee }
            }
            otter_vm::JitDirectCallKind::SuperConstruct => DirectCallForm::SuperConstruct {
                callable: callee,
                receiver: dst,
            },
            otter_vm::JitDirectCallKind::DerivedSuperConstruct => {
                DirectCallForm::DerivedSuperConstruct { callable: callee }
            }
            _ => unreachable!("fixed construct kind"),
        };
        crate::arm64::emit_direct_call_with_access(
            ops,
            relocations,
            view,
            DirectCallSite {
                target,
                target_index: 0,
                target_count: 1,
                caller_function_id: view.code_block.id,
                logical_pc,
                byte_pc,
                dst,
                form,
                arguments: DirectCallArguments::Fixed(argument_registers),
            },
            table.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
            table.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            table.entry(abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT),
            table.entry(abi::STUB_JIT_PREPARE_BASE_CONSTRUCT),
            table.entry(abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT),
            0,
            table.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
            code_map,
            bail,
            threw,
            throw_value,
            fatal,
            done,
            20,
            |ops, source, target, _| emit_load_reg(ops, target, source),
            |ops, destination, source, _| emit_store_reg(ops, source, destination),
            |_| Ok(()),
            |ops, source, _| emit_store_reg(ops, source, dst),
            |_, _| Ok(()),
        )?;
        if let Some(events) = direct_call_events {
            events.insert(
                (byte_pc, 0),
                direct_call_lowering_event(
                    kind,
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
                if super_construct {
                    otter_vm::JitDirectCallKind::SuperConstruct
                } else {
                    otter_vm::JitDirectCallKind::Construct
                },
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
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, callee as u32
    );
    emit_load_u64(ops, 3, u64::from(argc) | (u64::from(super_construct) << 63));
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
    dynasm!(ops ; .arch aarch64 ; blr x16);
    super::transitions::emit_status_word_result(ops, Some(bail), threw, fatal);
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
    view: &JitCompileSnapshot,
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
            dynasm!(ops ; .arch aarch64 ; =>next_candidate);
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
            target_index: method.target_index,
            target_count: method.target_count,
            caller_function_id: view.code_block.id,
            logical_pc,
            byte_pc,
            dst,
            form: DirectCallForm::Method {
                callable: 17,
                receiver,
            },
            arguments: DirectCallArguments::Fixed(argument_registers),
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
            true,
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
            table.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
            code_map.as_deref_mut(),
            bail,
            threw,
            throw_value,
            fatal,
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
    let packet_words = argument_registers
        .len()
        .checked_add(1)
        .and_then(|words| u32::try_from(words).ok())
        .ok_or(Unsupported::OperandShape(
            "template method value-packet word count",
        ))?;
    let packet_bytes = packet_words
        .checked_mul(8)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape(
            "template method value-packet frame",
        ))?;
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, packet_bytes);
    emit_load_reg(ops, 9, receiver)?;
    dynasm!(ops ; .arch aarch64 ; str x9, [sp]);
    for (index, &argument) in argument_registers.iter().enumerate() {
        emit_load_reg(ops, 9, argument)?;
        let offset = u32::try_from(index + 1)
            .ok()
            .and_then(|word| word.checked_mul(8))
            .ok_or(Unsupported::OperandShape(
                "template method value-packet offset",
            ))?;
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, offset]);
    }
    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; mov x1, sp ; movz w2, packet_words);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CALL_METHOD_VALUE),
        abi::STUB_JIT_CALL_METHOD_VALUE,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; add sp, sp, packet_bytes
        ; cbz x1, >method_value_completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; method_value_completed:
    );
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

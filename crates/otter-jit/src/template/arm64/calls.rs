//! AArch64 call emitters for the template compiler.
//!
//! # Contents
//! - Plain-call, method-call, and construct lowering through prepare and
//!   generic transitions.
//! - Guarded read-only numeric method splicing from VM-baked inline metadata.
//! - Guarded collection-method dispatch before ordinary method resolution.
//!
//! # Invariants
//! - The caller's canonical PC is stamped before the prepare transition; a
//!   bailed callee reifies at its exact PC through the finish helpers and the
//!   caller's published frame survives untouched.
//! - A callee throw caught by the compiled caller publishes the selected
//!   catch/finally PC and exits through the shared bailout epilogue.
//! - Prepared callees enter through the common owned AArch64 call trampoline,
//!   which owns frame publication and cleanup for every native tier.
//! - Inlined method bodies contain no call, allocation, branch, or mutation;
//!   every guard failure balances compact scratch storage and completes the
//!   original method call through the ordinary bridge.
//! - `x19` remains the caller register base throughout an inline body. Callee
//!   virtual registers use explicit `sp`-relative compact slots initialized
//!   only for live entry values.
//! - A raw receiver pointer retained in `x17` is live only between the entry
//!   identity guard and the end of a call-free, safepoint-free inline body.
//! - Ineligible call resolutions complete through the descriptor-classified
//!   generic in-place transitions; only receivers whose opcode semantics the
//!   interpreter dispatches through bespoke branches take an exact side exit.
//!
//! # See also
//! - [`crate::arm64::calls`] — shared compiled-to-compiled call emission.
//! - [`super::transitions`] — descriptor-resolved entries used here.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;
use otter_vm::{JitCompileSnapshot, JitInlineMethod, closure::JS_CLOSURE_BODY_TYPE_TAG};

use super::collections::{
    MethodSite, emit_alloc_method_guarded_call, emit_leaf_method_guarded_call,
};
use super::transitions::TransitionTable;
use super::values::{
    emit_box_double, emit_box_int32, emit_decompress_slot, emit_load_reg, emit_load_runtime_stub,
    emit_load_symbol_u64, emit_load_u64, emit_num_to_double, emit_slab_base, emit_store_reg,
};
use crate::arm64::{CallTrampoline, emit_prepared_call};
use crate::artifact::relocation::{
    RelocationCapture, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
};
use crate::artifact::{
    CodeMapCapture, CodeRegion, InlineScratchEntryArtifact, InlineScratchLayoutArtifact,
    InlineSiteArtifact,
};
use crate::entry::{NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, Unsupported, VALUE_UNDEFINED};
use crate::template::{
    ArithKind, InlineEntryValue, InlineMethodPlan, InlineScratchSlot, TemplateOp, TemplatePlan,
    TemplateTail,
};

#[derive(Debug, Clone, Copy)]
struct InlineMethodEmission {
    miss_replay_start: usize,
    inline_site: InlineSiteArtifact,
    function_id: u32,
}

fn inline_scratch_artifact(
    method: &JitInlineMethod,
    plan: &InlineMethodPlan<'_>,
) -> InlineScratchLayoutArtifact {
    let register_slots = (0..method.register_count)
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
        parameter_count: method.param_count,
        virtual_register_count: method.register_count,
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

/// Build the current template plan for one baked method body.
///
/// PORT NOTE: the deleted legacy baseline emitter decoded the method body a
/// second time. This port deliberately reuses the current typed TemplatePlan,
/// keeping operand validation and opcode shapes in one backend-neutral path.
fn inline_method_template_plan(
    view: &JitCompileSnapshot,
    method: &JitInlineMethod,
) -> Result<TemplatePlan, Unsupported> {
    let mut method_view = view.clone();
    method_view.code_block = method.code_block.clone();
    method_view.instructions = method.instructions.clone();
    method_view.inline_callees.clear();
    method_view.inline_methods.clear();
    method_view.inline_poly_methods.clear();
    method_view.collection_leaf_methods.clear();
    method_view.collection_alloc_methods.clear();
    method_view.array_methods.clear();
    method_view.primitive_method_guards.clear();
    TemplatePlan::build(&method_view)
}

pub(super) fn has_emit_eligible_inline_method(view: &JitCompileSnapshot) -> bool {
    if view.inline_methods.is_empty() {
        return false;
    }
    let Ok(caller_plan) = TemplatePlan::build(view) else {
        return false;
    };
    caller_plan.instructions.iter().any(|instruction| {
        let TemplateOp::MethodCall { argc, byte_pc, .. } = instruction.op else {
            return false;
        };
        let Some(method) = view.inline_methods.get(&byte_pc) else {
            return false;
        };
        inline_method_template_plan(view, method).is_ok_and(|method_plan| {
            view.cage_base != 0
                && InlineMethodPlan::build(method, &method_plan, usize::from(argc)).is_some()
        })
    })
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

/// Emit one exact receiver/method guard plus a replay-safe scratch body.
///
/// A miss label is bound at the end so the caller can append the existing
/// collection/direct/generic method dispatch without duplicating it.
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
) -> Result<Option<InlineMethodEmission>, Unsupported> {
    let template_plan = inline_method_template_plan(view, method)?;
    let Some(plan) = (view.cage_base != 0)
        .then(|| InlineMethodPlan::build(method, &template_plan, usize::from(argc)))
        .flatten()
    else {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] template inline method skip fid={} argc={} params={} regs={} ops={:?}",
                method.method_fid,
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
        return Ok(None);
    };
    if std::env::var_os("OTTER_JIT_TRACE").is_some() {
        eprintln!(
            "[otter-jit] template inline method emit fid={} argc={} regs={} scratch_slots={} scratch_bytes={}",
            method.method_fid,
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
        _ => return Ok(None),
    };
    let miss = ops.new_dynamic_label();
    let body_miss = ops.new_dynamic_label();
    let inline_done = ops.new_dynamic_label();
    let method_closure = ops.new_dynamic_label();
    let method_guarded = ops.new_dynamic_label();
    let guard_start = ops.offset().0;

    // Receiver tag/type/shape guard.
    emit_load_reg(ops, 9, receiver)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_symbol_u64(
        ops,
        relocations,
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
        ; ldr w14, [x13, view.object_shape_byte]
    );
    emit_load_u64(ops, 15, u64::from(method.recv_shape));
    dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.ne =>miss);
    if plan.has_receiver_property() {
        // Preserve the exact shape-guarded receiver body while `x13` chases
        // the possibly distinct prototype holder of the method slot.
        dynasm!(ops ; .arch aarch64 ; mov x17, x13);
    }

    // Chase the baked direct prototype chain, guarding each holder shape.
    for &hop_shape in &method.proto_chain {
        dynasm!(ops
            ; .arch aarch64
            ; ldr w9, [x13, view.jit_proto_byte]
            ; cbz w9, =>miss
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            12,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x12, x9
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x13, view.object_shape_byte]
        );
        emit_load_u64(ops, 15, u64::from(hop_shape));
        dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.ne =>miss);
    }

    // Re-read the method slot. A closure is accepted only when its function id
    // matches and no bound/runtime-setup state can alter ordinary method-call
    // receiver semantics.
    emit_slab_base(ops, view, 13, 14);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x13, =>miss
        ; ldr w9, [x13, method.method_value_byte]
    );
    use otter_vm::value::compressed as cslot;
    debug_assert_eq!(cslot::TAG_MASK, 0b111);
    debug_assert_eq!(cslot::TAG_FUNCTION_ID, 0b110);
    dynasm!(ops
        ; .arch aarch64
        ; and w10, w9, cslot::TAG_MASK
        ; cbz w10, =>method_closure
    );
    if let Some(compressed_fid) = (method.method_fid <= u32::MAX >> 3)
        .then_some((method.method_fid << 3) | cslot::TAG_FUNCTION_ID)
    {
        emit_load_u64(ops, 10, u64::from(compressed_fid));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w9, w10
            ; b.eq =>method_guarded
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; b =>miss
        ; =>method_closure
        ; cbz w9, =>miss
        ; mov w12, w9
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    let closure_fid_byte = view.closure_call_layout.function_id_byte;
    let closure_flags_byte = view.closure_call_layout.flags_byte;
    let incompatible_call_flags =
        view.closure_call_layout.runtime_setup_flags | view.closure_call_layout.bound_this_flag;
    dynasm!(ops
        ; .arch aarch64
        ; add x11, x11, x12
        ; ldrb w14, [x11]
        ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG as u32
        ; b.ne =>miss
        ; ldr w14, [x11, closure_flags_byte]
    );
    emit_load_u64(ops, 15, u64::from(incompatible_call_flags));
    dynasm!(ops
        ; .arch aarch64
        ; tst w14, w15
        ; b.ne =>miss
        ; ldr w14, [x11, closure_fid_byte]
    );
    emit_load_u64(ops, 15, u64::from(method.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w14, w15
        ; b.ne =>miss
        ; =>method_guarded
    );
    if plan.has_receiver_property() {
        // Receiver-property offsets were baked from `recv_shape`, whose guard
        // already succeeded above. Materialize its slab once for every sealed
        // receiver-property load in the straight-line inline body.
        dynasm!(ops ; .arch aarch64 ; mov x13, x17);
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x13, =>miss
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
            method.method_fid,
        ));
    }

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
                let caller_register = arguments
                    .get(usize::from(argument))
                    .copied()
                    .flatten()
                    .expect("validated inline method argument");
                emit_load_reg(ops, 9, caller_register)?;
                emit_store_inline_slot(ops, 9, slot);
            }
            InlineEntryValue::Receiver { slot } => {
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
            inline_site,
            method.method_fid,
            inline_scratch_artifact(method, &plan),
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
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let value_byte = *method
                    .prop_offsets
                    .get(&instruction.byte_pc)
                    .ok_or(Unsupported::OperandShape("inline method property offset"))?;
                emit_inline_receiver_property(ops, relocations, view, dst, value_byte, body_miss)?;
            }
            TemplateOp::ToPrimitive { dst, src, .. } | TemplateOp::ToNumeric { dst, src } => {
                let dst = plan
                    .register_slot(dst)
                    .ok_or(Unsupported::OperandShape("inline scratch destination"))?;
                let src = plan
                    .register_slot(src)
                    .ok_or(Unsupported::OperandShape("inline scratch source"))?;
                emit_inline_number_identity(ops, dst, src, body_miss);
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
                emit_inline_numeric_add(ops, dst, lhs, rhs, body_miss);
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
                emit_inline_numeric_binary(ops, dst, lhs, rhs, kind, body_miss)?;
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
                inline_site,
                method.method_fid,
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
            "inlineMethodBody",
            body_start,
            body_end,
            inline_site,
            method.method_fid,
        ));
    }

    let hit_epilogue_start = ops.offset().0;
    dynasm!(ops ; .arch aarch64 ; =>inline_done);
    if scratch_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    }
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>body_miss);
    let hit_epilogue_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::inline_structural(
            "inlineMethodHitEpilogue",
            hit_epilogue_start,
            hit_epilogue_end,
            inline_site,
            method.method_fid,
        ));
    }
    let miss_replay_start = ops.offset().0;
    if scratch_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    let miss_entry = ops.offset().0;
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::inline_structural(
            "inlineMethodMissTeardown",
            miss_replay_start,
            miss_entry,
            inline_site,
            method.method_fid,
        ));
    }
    Ok(Some(InlineMethodEmission {
        miss_replay_start: miss_entry,
        inline_site,
        function_id: method.method_fid,
    }))
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
/// The prepare transition resolves the callee against installed code and
/// stages the callee window/identity in the entry context (`0`), throws
/// (`1`), or reports an ineligible callee (`2`), which then completes
/// through the generic in-place call transition — the compiled caller keeps
/// running for every callable. Only a non-callable value takes the exact
/// side exit so the interpreter owns the thrown error.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    call_trampoline: &CallTrampoline,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    let generic = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, callee as u32
        ; movz x2, argc as u32
    );
    emit_packed_args(
        ops,
        relocations,
        3,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::CallArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_PREPARE_DIRECT_CALL),
        abi::STUB_JIT_PREPARE_DIRECT_CALL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>generic
    );
    emit_prepared_call(ops, relocations, call_trampoline, dst, bail, threw, done);

    // Ineligible callee: complete the whole opcode through the generic
    // in-place call transition; only its non-callable report (`2`)
    // side-exits to normal dispatch.
    dynasm!(ops
        ; .arch aarch64
        ; =>generic
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
        TemplateOperandRole::CallArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CALL_GENERIC),
        abi::STUB_JIT_CALL_GENERIC,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
        ; =>done
    );
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
/// Layered dispatch: the collection-method IC transition completes hot
/// collection methods in place (`0`), throws (`1`), or misses (`2`); a miss
/// falls through to the direct-method prepare; an ineligible resolution
/// (polymorphic, native, accessor, or cold method) then completes through
/// the generic in-place method transition, so the compiled caller keeps
/// running for every ordinary receiver, including missing/non-callable
/// resolutions after an observable getter or proxy trap. Only receivers the
/// interpreter dispatches through bespoke opcode branches (generators,
/// iterators, pending bind continuations) take the exact side exit before
/// resolution begins.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    call_trampoline: &CallTrampoline,
    view: &JitCompileSnapshot,
    mut code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    receiver: u16,
    name: u32,
    site: u64,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
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
    let mut inline_emission = None;
    if let Some(method) = view.inline_methods.get(&byte_pc) {
        inline_emission = try_emit_inline_numeric_method(
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
        )?;
    }
    // Guarded monomorphic collection fast paths precede the shared bridge;
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
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, receiver as u32
    );
    emit_load_u64(ops, 3, site);
    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
    emit_packed_args(
        ops,
        relocations,
        5,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::MethodArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_COLLECTION_METHOD_IC),
        abi::STUB_JIT_COLLECTION_METHOD_IC,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cbz x0, =>done
    );

    let generic = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, receiver as u32
    );
    emit_load_u64(ops, 2, u64::from(name));
    emit_load_u64(ops, 3, site);
    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
    emit_packed_args(
        ops,
        relocations,
        5,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::MethodArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL),
        abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>generic
    );
    emit_prepared_call(ops, relocations, call_trampoline, dst, bail, threw, done);

    // Ineligible direct resolution: complete the whole opcode through the
    // generic in-place method transition; only its exotic-receiver report
    // (`2`) side-exits to normal dispatch.
    dynasm!(ops
        ; .arch aarch64
        ; =>generic
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, receiver as u32
    );
    emit_load_u64(ops, 3, u64::from(name));
    emit_load_u64(ops, 4, site);
    dynasm!(ops ; .arch aarch64 ; movz x5, argc as u32);
    emit_packed_args(
        ops,
        relocations,
        6,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::MethodArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CALL_METHOD_GENERIC),
        abi::STUB_JIT_CALL_METHOD_GENERIC,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
        ; =>done
    );
    if let (Some(emission), Some(code_map)) = (inline_emission, code_map) {
        code_map.record(CodeRegion::inline_structural(
            "inlineMethodMissReplay",
            emission.miss_replay_start,
            ops.offset().0,
            emission.inline_site,
            emission.function_id,
        ));
    }
    Ok(())
}

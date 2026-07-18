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
//!   every guard failure restores the caller register base and completes the
//!   original method call through the ordinary bridge.
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
use crate::entry::{
    FUNCTION_ID_TAG, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, Unsupported, VALUE_UNDEFINED,
};
use crate::template::{ArithKind, TemplateOp, TemplatePlan, TemplateTail};

const INLINE_METHOD_MAX_REGISTERS: u16 = 24;
const INLINE_METHOD_MAX_INSTRUCTIONS: usize = 48;
const INLINE_METHOD_MAX_ARGUMENTS: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineValueKind {
    Unknown,
    This,
}

/// Build the current template plan for one baked method body.
///
/// PORT NOTE: the deleted legacy baseline emitter decoded the method body a
/// second time. This port deliberately reuses the current typed TemplatePlan,
/// keeping operand validation and opcode shapes in one backend-neutral path.
fn inline_method_plan(
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

/// Accept only straight-line, read-only bodies whose guarded misses can replay
/// the original method call without duplicating an observable effect.
fn inline_numeric_method_eligible(
    method: &JitInlineMethod,
    plan: &TemplatePlan,
    argc: usize,
) -> bool {
    if argc != usize::from(method.param_count)
        || argc > INLINE_METHOD_MAX_ARGUMENTS
        || method.register_count > INLINE_METHOD_MAX_REGISTERS
        || plan.instructions.len() > INLINE_METHOD_MAX_INSTRUCTIONS
        || plan.register_count != method.register_count
    {
        return false;
    }
    let mut kinds = vec![InlineValueKind::Unknown; usize::from(method.register_count)];
    let mut saw_return = false;
    for (index, instruction) in plan.instructions.iter().enumerate() {
        if saw_return {
            return false;
        }
        match instruction.op {
            TemplateOp::LoadImmediate { dst, .. } => {
                kinds[usize::from(dst)] = InlineValueKind::Unknown;
            }
            TemplateOp::Move { dst, src } => {
                kinds[usize::from(dst)] = kinds[usize::from(src)];
            }
            TemplateOp::LoadThis { dst } => {
                kinds[usize::from(dst)] = InlineValueKind::This;
            }
            TemplateOp::LoadProperty { dst, object, .. } => {
                if kinds[usize::from(object)] != InlineValueKind::This
                    || !method.prop_offsets.contains_key(&instruction.byte_pc)
                    || method.prop_shapes.contains_key(&instruction.byte_pc)
                {
                    return false;
                }
                kinds[usize::from(dst)] = InlineValueKind::Unknown;
            }
            TemplateOp::ToPrimitive { dst, .. }
            | TemplateOp::ToNumeric { dst, .. }
            | TemplateOp::AddGeneric { dst, .. } => {
                kinds[usize::from(dst)] = InlineValueKind::Unknown;
            }
            TemplateOp::BinaryArith { dst, kind, .. } => {
                if matches!(kind, ArithKind::Rem | ArithKind::Pow) {
                    return false;
                }
                kinds[usize::from(dst)] = InlineValueKind::Unknown;
            }
            TemplateOp::Return { .. } | TemplateOp::ReturnUndefined => {
                saw_return = true;
                if index + 1 != plan.instructions.len() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    saw_return
}

pub(super) fn has_emit_eligible_inline_method(view: &JitCompileSnapshot) -> bool {
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
        inline_method_plan(view, method).is_ok_and(|method_plan| {
            view.cage_base != 0
                && inline_numeric_method_eligible(method, &method_plan, usize::from(argc))
        })
    })
}

fn emit_inline_number_identity(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, src)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x15
        ; b.eq =>miss
    );
    emit_store_reg(ops, 9, dst)
}

fn emit_inline_numeric_add(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
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
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, miss);
    emit_num_to_double(ops, 10, 1, miss);
    dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1);
    emit_box_double(ops, 2, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

fn emit_inline_numeric_binary(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: ArithKind,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if matches!(kind, ArithKind::Pow) {
        return Err(Unsupported::Opcode(otter_bytecode::Op::Pow));
    }
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
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
    emit_store_reg(ops, 13, dst)
}

fn emit_inline_receiver_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    dst: u16,
    object: u16,
    value_byte: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, object)?;
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
    );
    emit_slab_base(ops, view, 13, 14);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x13, =>miss
        ; ldr w9, [x13, value_byte]
    );
    emit_decompress_slot(ops, relocations, view.cage_base as u64, miss);
    emit_store_reg(ops, 9, dst)
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
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    let plan = inline_method_plan(view, method)?;
    if view.cage_base == 0 || !inline_numeric_method_eligible(method, &plan, usize::from(argc)) {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] template inline method skip fid={} argc={} params={} regs={} ops={:?}",
                method.method_fid,
                argc,
                method.param_count,
                method.register_count,
                plan.instructions
                    .iter()
                    .map(|instruction| instruction.op)
                    .collect::<Vec<_>>(),
            );
        }
        return Ok(false);
    }
    if std::env::var_os("OTTER_JIT_TRACE").is_some() {
        eprintln!(
            "[otter-jit] template inline method emit fid={} argc={} regs={}",
            method.method_fid, argc, method.register_count,
        );
    }
    let arguments = match (argc, arg0, arg1) {
        (0, _, _) => [None, None],
        (1, Some(first), _) => [Some(first), None],
        (2, Some(first), Some(second)) => [Some(first), Some(second)],
        _ => return Ok(false),
    };
    let miss = ops.new_dynamic_label();
    let body_miss = ops.new_dynamic_label();
    let inline_done = ops.new_dynamic_label();
    let method_closure = ops.new_dynamic_label();
    let method_guarded = ops.new_dynamic_label();

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
    emit_decompress_slot(ops, relocations, view.cage_base as u64, miss);
    emit_load_u64(
        ops,
        10,
        FUNCTION_ID_TAG | (u64::from(method.method_fid) << 16),
    );
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x10
        ; b.eq =>method_guarded
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2
        ; tst x9, x11
        ; b.eq =>method_closure
        ; b =>miss
        ; =>method_closure
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

    // Scratch registers are unobservable and contain no safepoint. The
    // receiver occupies one extra slot used by LoadThis.
    let this_slot = method.register_count;
    let scratch_slots = u32::from(method.register_count) + 1;
    let scratch_bytes = (scratch_slots * 8).next_multiple_of(16);
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
    for (slot, argument) in arguments.iter().take(usize::from(argc)).enumerate() {
        emit_load_reg(ops, 9, argument.expect("validated inline method argument"))?;
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
    }
    emit_load_reg(ops, 9, receiver)?;
    dynasm!(ops ; .arch aarch64 ; str x9, [sp, u32::from(this_slot) * 8]);
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    for slot in usize::from(argc)..usize::from(method.register_count) {
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
    }
    dynasm!(ops ; .arch aarch64 ; mov x19, sp);

    for instruction in &plan.instructions {
        match instruction.op {
            TemplateOp::LoadImmediate { dst, bits } => {
                emit_load_u64(ops, 9, bits);
                emit_store_reg(ops, 9, dst)?;
            }
            TemplateOp::Move { dst, src } => {
                emit_load_reg(ops, 9, src)?;
                emit_store_reg(ops, 9, dst)?;
            }
            TemplateOp::LoadThis { dst } => {
                emit_load_reg(ops, 9, this_slot)?;
                emit_store_reg(ops, 9, dst)?;
            }
            TemplateOp::LoadProperty { dst, object, .. } => {
                let value_byte = *method
                    .prop_offsets
                    .get(&instruction.byte_pc)
                    .ok_or(Unsupported::OperandShape("inline method property offset"))?;
                emit_inline_receiver_property(
                    ops,
                    relocations,
                    view,
                    dst,
                    object,
                    value_byte,
                    body_miss,
                )?;
            }
            TemplateOp::ToPrimitive { dst, src, .. } | TemplateOp::ToNumeric { dst, src } => {
                emit_inline_number_identity(ops, dst, src, body_miss)?;
            }
            TemplateOp::AddGeneric { dst, lhs, rhs, .. } => {
                emit_inline_numeric_add(ops, dst, lhs, rhs, body_miss)?;
            }
            TemplateOp::BinaryArith {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_inline_numeric_binary(ops, dst, lhs, rhs, kind, body_miss)?;
            }
            TemplateOp::Return { src } => {
                emit_load_reg(ops, 9, src)?;
                dynasm!(ops ; .arch aarch64 ; b =>inline_done);
            }
            TemplateOp::ReturnUndefined => {
                emit_load_u64(ops, 9, VALUE_UNDEFINED);
                dynasm!(ops ; .arch aarch64 ; b =>inline_done);
            }
            _ => unreachable!("inline eligibility accepted unsupported template op"),
        }
    }

    dynasm!(ops
        ; .arch aarch64
        ; =>inline_done
        ; add sp, sp, scratch_bytes
        ; ldr x10, [x20, crate::entry::NATIVE_FRAME_OFFSET]
        ; ldr x19, [x10, crate::entry::NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops
        ; .arch aarch64
        ; b =>done
        ; =>body_miss
        ; add sp, sp, scratch_bytes
        ; ldr x10, [x20, crate::entry::NATIVE_FRAME_OFFSET]
        ; ldr x19, [x10, crate::entry::NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; =>miss
    );
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
    dst: u16,
    receiver: u16,
    name: u32,
    site: u64,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
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
    if let Some(method) = view.inline_methods.get(&byte_pc) {
        try_emit_inline_numeric_method(
            ops,
            relocations,
            view,
            method,
            dst,
            receiver,
            argc,
            arg0,
            arg1,
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
    Ok(())
}

//! AArch64 dynasm backend for the template compiler.
//!
//! # Contents
//! - Code-buffer ownership, per-PC labels, and branch fixups.
//! - Prologue/epilogue establishing the shared compiled-entry ABI.
//! - One emit dispatch per [`TemplateOp`], including inline tagged truthiness.
//! - Cooperative back-edge interrupt/fuel polling.
//! - [`values`] — tagged encode/decode primitives.
//! - [`arith`] — numeric, comparison, and bitwise emitters.
//!
//! # Invariants
//! - Every instruction stamps its canonical resume PC into the published
//!   native frame before any observable work; exact side exits, returns, and
//!   the throw status are the only exits.
//! - Tagged truthiness decides immediate primitives inline and delegates heap
//!   cells to the total leaf helper without allocating or re-entering JS.
//! - Boxed-double falsiness is decided by exact bit patterns (`+0.0`, `-0.0`,
//!   the canonical NaN); the VM's NaN-purification invariant makes this
//!   complete.
//! - Emitted code bakes only offsets from the shared entry-ABI module and the
//!   frozen value-tag contract; no Rust container layout is probed.
//!
//! # See also
//! - [`super::plan`] — the validated operation stream consumed here.
//! - [`super::code`] — the owner of the finalized mapping.

// dynasm 5 normalizes dynamic AArch64 register operands through `Into<u8>`;
// when our register ids are already `u8`, that macro-generated conversion is
// intentionally redundant and outside the source-level emitter's control.
#![allow(clippy::useless_conversion)]

mod arith;
mod calls;
mod collections;
mod construct;
mod delete;
mod exceptions;
mod functions;
mod globals;
mod iterators;
mod private_access;
mod properties;
mod protocol;
mod scalar;
mod structural;
mod super_access;
mod transitions;
mod value_load;
mod values;

use std::collections::BTreeMap;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::JitCompileSnapshot;

use self::arith::{
    emit_add_generic, emit_binary_arith, emit_bitwise_not, emit_coercion_slow_paths, emit_compare,
    emit_increment, emit_int_bitwise, emit_loose_compare, emit_negate, emit_numeric_slow_paths,
    emit_to_numeric, emit_to_primitive, emit_unsigned_shift_right,
};
use self::values::{emit_load_reg, emit_load_u64, emit_store_reg};
use super::{TemplateCode, TemplateOp, TemplatePlan};
use crate::CompiledCode;
use crate::entry::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
    NUMBER_TAG_HI16, SELF_CLOSURE_OFFSET, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW,
    THIS_VALUE_OFFSET, THREAD_OFFSET, Unsupported, VALUE_FALSE, VALUE_HOLE, VALUE_NULL, VALUE_TRUE,
    VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
    VM_THREAD_INTERRUPT_CELL_OFFSET, reg_offset,
};
use otter_vm::native_abi as abi;

/// Boolean/nullish immediates as 32-bit `dynasm` operands.
const VALUE_TRUE_IMM: u32 = VALUE_TRUE as u32;
const VALUE_FALSE_IMM: u32 = VALUE_FALSE as u32;
const VALUE_NULL_IMM: u32 = VALUE_NULL as u32;
const VALUE_UNDEFINED_IMM: u32 = VALUE_UNDEFINED as u32;

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
) -> Result<TemplateCode, Unsupported> {
    let plan = TemplatePlan::build(view)?;
    let poll_entry = transitions.entry(abi::STUB_JIT_BACKEDGE_POLL);
    let code_block_id = view.code_block.id;
    // Self-patching property IC cells: allocated address-stable before any
    // pointer is baked, consumed strictly in emission order, owned by the
    // finalized code object.
    let mut load_ic_cells =
        vec![crate::entry::WhiskerIcCell::default(); plan.load_property_count].into_boxed_slice();
    let mut store_ic_cells =
        vec![crate::entry::WhiskerIcCell::default(); plan.store_property_count].into_boxed_slice();
    let mut next_load_ic = 0usize;
    let mut next_store_ic = 0usize;
    let mut coercion_slow_paths = Vec::new();
    let mut numeric_slow_paths = Vec::new();
    let mut ops = Assembler::new().expect("assembler alloc");
    let bail = ops.new_dynamic_label();
    let returned = ops.new_dynamic_label();
    let threw = ops.new_dynamic_label();
    let labels: BTreeMap<u32, DynamicLabel> = plan
        .instructions
        .iter()
        .map(|instr| (instr.pc, ops.new_dynamic_label()))
        .collect();

    let entry = ops.offset();
    emit_prologue(&mut ops);
    for instr in &plan.instructions {
        let label = labels[&instr.pc];
        dynasm!(ops ; .arch aarch64 ; =>label);
        // Publish this op's logical PC before any observable work; the
        // published NativeFrame PC is the one canonical logical PC every bail
        // path and diagnostic reads.
        emit_stamp_pc(&mut ops, instr.pc);
        match instr.op {
            TemplateOp::LoadImmediate { dst, bits } => {
                emit_load_u64(&mut ops, 9, bits);
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::Move { dst, src } => {
                emit_load_reg(&mut ops, 9, src)?;
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::Jump { target, back_edge } => {
                let tgt = labels[&target];
                if back_edge {
                    emit_backedge_poll(&mut ops, poll_entry, threw);
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
                emit_load_reg(&mut ops, 9, condition)?;
                emit_truthiness_bool(&mut ops, bail);
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
                    emit_backedge_poll(&mut ops, poll_entry, threw);
                    dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
                } else if when_truthy {
                    dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
                } else {
                    dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
                }
            }
            TemplateOp::Truthiness { dst, src, negate } => {
                emit_load_reg(&mut ops, 9, src)?;
                emit_truthiness_bool(&mut ops, bail);
                if negate {
                    // VALUE_TRUE and VALUE_FALSE differ exactly in bit 0.
                    dynasm!(ops ; .arch aarch64 ; eor x9, x9, #1);
                }
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::BinaryArith {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_binary_arith(&mut ops, dst, lhs, rhs, kind, &mut numeric_slow_paths)?;
            }
            TemplateOp::Compare {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_compare(&mut ops, dst, lhs, rhs, kind, bail, &mut numeric_slow_paths)?;
            }
            TemplateOp::LooseCompare {
                dst,
                lhs,
                rhs,
                negate,
            } => {
                emit_loose_compare(&mut ops, transitions, dst, lhs, rhs, negate, bail, threw)?;
            }
            TemplateOp::IntBitwise {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_int_bitwise(&mut ops, dst, lhs, rhs, kind, &mut numeric_slow_paths)?;
            }
            TemplateOp::UnsignedShiftRight { dst, lhs, rhs } => {
                emit_unsigned_shift_right(&mut ops, dst, lhs, rhs, &mut numeric_slow_paths)?;
            }
            TemplateOp::Increment { dst, src, delta } => {
                emit_increment(&mut ops, dst, src, delta, &mut numeric_slow_paths)?;
            }
            TemplateOp::Negate { dst, src } => {
                emit_negate(&mut ops, dst, src, &mut numeric_slow_paths)?;
            }
            TemplateOp::BitwiseNot { dst, src } => {
                emit_bitwise_not(&mut ops, dst, src, &mut numeric_slow_paths)?;
            }
            TemplateOp::ToNumeric { dst, src } => {
                emit_to_numeric(
                    &mut ops,
                    dst,
                    src,
                    view.code_block.id,
                    &mut coercion_slow_paths,
                )?;
            }
            TemplateOp::ToPrimitive { dst, src, hint } => {
                emit_to_primitive(
                    &mut ops,
                    dst,
                    src,
                    hint,
                    view.code_block.id,
                    &mut coercion_slow_paths,
                )?;
            }
            TemplateOp::AddGeneric {
                dst,
                lhs,
                rhs,
                concat_safepoint,
            } => {
                emit_add_generic(
                    &mut ops,
                    transitions,
                    dst,
                    lhs,
                    rhs,
                    concat_safepoint,
                    threw,
                )?;
            }
            TemplateOp::LoadThis { dst } => {
                dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, THIS_VALUE_OFFSET]);
                emit_load_u64(&mut ops, 12, VALUE_HOLE);
                // A derived-ctor `this`-before-`super` hole resolves in the
                // interpreter.
                dynasm!(ops ; .arch aarch64 ; cmp x9, x12 ; b.eq =>bail);
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::LoadSelfClosure { dst } => {
                dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, SELF_CLOSURE_OFFSET]);
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::MakeFunction { dst, constant } => {
                transitions::emit_make_function(&mut ops, transitions, dst, constant, threw);
            }
            TemplateOp::MakeClosure {
                dst,
                function,
                parents,
            } => {
                transitions::emit_make_closure(
                    &mut ops,
                    transitions,
                    code_block_id,
                    dst,
                    function,
                    plan.index_tail(parents),
                    threw,
                );
            }
            TemplateOp::LoadString { dst, constant } => {
                transitions::emit_load_string(
                    &mut ops,
                    transitions,
                    code_block_id,
                    dst,
                    constant,
                    threw,
                );
            }
            TemplateOp::LoadRegExp { dst, constant } => {
                transitions::emit_load_regexp(&mut ops, transitions, dst, constant, threw);
            }
            TemplateOp::LoadGlobal { dst, name } => {
                transitions::emit_load_global(
                    &mut ops,
                    transitions,
                    dst,
                    name,
                    code_block_id,
                    threw,
                );
            }
            TemplateOp::LoadBuiltinError { dst, constant } => {
                transitions::emit_load_builtin_error(&mut ops, transitions, dst, constant, threw);
            }
            TemplateOp::NewObject { dst } => {
                transitions::emit_new_object(&mut ops, transitions, dst, threw);
            }
            TemplateOp::NewArray { dst, elements } => {
                transitions::emit_new_array(
                    &mut ops,
                    transitions,
                    dst,
                    plan.register_tail(elements),
                    threw,
                );
            }
            TemplateOp::MathCall {
                dst,
                method,
                arguments,
            } => {
                transitions::emit_math_call(
                    &mut ops,
                    transitions,
                    dst,
                    method,
                    plan.register_tail(arguments),
                    threw,
                )?;
            }
            TemplateOp::FreshUpvalue { index } => {
                transitions::emit_fresh_upvalue(&mut ops, transitions, index, threw);
            }
            TemplateOp::DefineDataProperty { object, key, value } => {
                transitions::emit_define_data_property(
                    &mut ops,
                    transitions,
                    object,
                    key,
                    value,
                    threw,
                );
            }
            TemplateOp::DefineOwnProperty {
                target,
                key,
                descriptor,
            } => {
                transitions::emit_define_own_property(
                    &mut ops,
                    transitions,
                    target,
                    key,
                    descriptor,
                    threw,
                );
            }
            TemplateOp::LoadElement {
                dst,
                receiver,
                index,
            } => {
                transitions::emit_load_element(&mut ops, transitions, dst, receiver, index, threw);
            }
            TemplateOp::StoreElement {
                receiver,
                index,
                value,
                scratch,
            } => {
                transitions::emit_store_element(
                    &mut ops,
                    transitions,
                    receiver,
                    index,
                    value,
                    scratch,
                    threw,
                );
            }
            TemplateOp::LoadUpvalue { dst, index } => {
                transitions::emit_load_upvalue(&mut ops, transitions, dst, index, threw);
            }
            TemplateOp::StoreUpvalue { src, index } => {
                transitions::emit_store_upvalue(&mut ops, transitions, src, index, false, threw);
            }
            TemplateOp::StoreUpvalueChecked { src, index } => {
                transitions::emit_store_upvalue(&mut ops, transitions, src, index, true, threw);
            }
            TemplateOp::LoadProperty {
                dst,
                object,
                name,
                site,
                array_length,
            } => {
                let cell = &mut load_ic_cells[next_load_ic];
                next_load_ic += 1;
                let cell_addr = cell as *mut crate::entry::WhiskerIcCell as usize;
                properties::emit_load_property(
                    &mut ops,
                    transitions,
                    view,
                    dst,
                    object,
                    name,
                    site,
                    array_length,
                    cell_addr,
                    threw,
                )?;
            }
            TemplateOp::StoreProperty {
                object,
                name,
                value,
                site,
            } => {
                let cell = &mut store_ic_cells[next_store_ic];
                next_store_ic += 1;
                let cell_addr = cell as *mut crate::entry::WhiskerIcCell as usize;
                properties::emit_store_property(
                    &mut ops,
                    transitions,
                    view,
                    object,
                    name,
                    value,
                    site,
                    cell_addr,
                    threw,
                )?;
            }
            TemplateOp::Call {
                dst,
                callee,
                argc,
                packed_args,
            } => {
                calls::emit_call(
                    &mut ops,
                    transitions,
                    dst,
                    callee,
                    argc,
                    packed_args,
                    bail,
                    threw,
                );
            }
            TemplateOp::Construct {
                dst,
                callee,
                argc,
                packed_args,
            } => {
                calls::emit_construct(
                    &mut ops,
                    transitions,
                    dst,
                    callee,
                    argc,
                    packed_args,
                    bail,
                    threw,
                );
            }
            TemplateOp::MethodCall {
                dst,
                receiver,
                name,
                site,
                argc,
                packed_args,
                byte_pc,
                arg0,
                arg1,
            } => {
                calls::emit_method_call(
                    &mut ops,
                    transitions,
                    view,
                    dst,
                    receiver,
                    name,
                    site,
                    argc,
                    packed_args,
                    byte_pc,
                    arg0,
                    arg1,
                    bail,
                    threw,
                )?;
            }
            TemplateOp::EnterTry {
                catch_pc,
                finally_pc,
                exception_register,
            } => {
                exceptions::emit_exception_op(
                    &mut ops,
                    transitions,
                    Op::EnterTry as u8,
                    u64::from(catch_pc.unwrap_or(u32::MAX)),
                    u64::from(finally_pc.unwrap_or(u32::MAX)),
                    u64::from(exception_register),
                    bail,
                    returned,
                    threw,
                );
            }
            TemplateOp::LeaveTry => {
                exceptions::emit_exception_op(
                    &mut ops,
                    transitions,
                    Op::LeaveTry as u8,
                    0,
                    0,
                    0,
                    bail,
                    returned,
                    threw,
                );
            }
            TemplateOp::Throw { src } => {
                exceptions::emit_exception_op(
                    &mut ops,
                    transitions,
                    Op::Throw as u8,
                    u64::from(src),
                    0,
                    0,
                    bail,
                    returned,
                    threw,
                );
            }
            TemplateOp::EndFinally => {
                exceptions::emit_exception_op(
                    &mut ops,
                    transitions,
                    Op::EndFinally as u8,
                    0,
                    0,
                    0,
                    bail,
                    returned,
                    threw,
                );
            }
            TemplateOp::PopParkedFinally { count } => {
                exceptions::emit_exception_op(
                    &mut ops,
                    transitions,
                    Op::PopParkedFinally as u8,
                    u64::from(count),
                    0,
                    0,
                    bail,
                    returned,
                    threw,
                );
            }
            TemplateOp::JumpViaFinally { target, floor } => {
                exceptions::emit_exception_op(
                    &mut ops,
                    transitions,
                    Op::JumpViaFinally as u8,
                    u64::from(target),
                    u64::from(floor),
                    0,
                    bail,
                    returned,
                    threw,
                );
            }
            TemplateOp::IteratorNext {
                value_dst,
                done_dst,
                iterator,
            } => {
                iterators::emit_iterator_op(
                    &mut ops,
                    transitions,
                    Op::IteratorNext as u8,
                    u64::from(value_dst),
                    u64::from(done_dst),
                    u64::from(iterator),
                    bail,
                    threw,
                );
            }
            TemplateOp::IteratorClose { iterator } => {
                iterators::emit_iterator_op(
                    &mut ops,
                    transitions,
                    Op::IteratorClose as u8,
                    u64::from(iterator),
                    0,
                    0,
                    bail,
                    threw,
                );
            }
            TemplateOp::IteratorCloseStart { iterator } => {
                iterators::emit_iterator_op(
                    &mut ops,
                    transitions,
                    Op::IteratorCloseStart as u8,
                    u64::from(iterator),
                    0,
                    0,
                    bail,
                    threw,
                );
            }
            TemplateOp::IteratorCloseEnd { iterator } => {
                iterators::emit_iterator_op(
                    &mut ops,
                    transitions,
                    Op::IteratorCloseEnd as u8,
                    u64::from(iterator),
                    0,
                    0,
                    bail,
                    threw,
                );
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
                    &mut ops,
                    transitions,
                    packed_meta,
                    packed_args,
                    bail,
                    threw,
                );
            }
            TemplateOp::GlobalOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                globals::emit_global_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::ObjectProtocolOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                protocol::emit_object_protocol_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::DeleteOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                delete::emit_delete_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::ScalarOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                scalar::emit_scalar_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::SuperOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                super_access::emit_super_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::PrivateOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                private_access::emit_private_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::ValueLoadOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                value_load::emit_value_load_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::ConstructOp {
                opcode,
                arg0,
                arg1,
                arg2,
            } => {
                construct::emit_construct_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    arg2,
                    bail,
                    threw,
                );
            }
            TemplateOp::StructuralOp { opcode, arg0, arg1 } => {
                structural::emit_structural_op(
                    &mut ops,
                    transitions,
                    opcode,
                    arg0,
                    arg1,
                    bail,
                    threw,
                );
            }
            TemplateOp::NoOp => {}
            TemplateOp::GetIterator { dst, src } => {
                iterators::emit_iterator_op(
                    &mut ops,
                    transitions,
                    Op::GetIterator as u8,
                    u64::from(dst),
                    u64::from(src),
                    0,
                    bail,
                    threw,
                );
            }
            TemplateOp::GetAsyncIterator { dst, src } => {
                iterators::emit_iterator_op(
                    &mut ops,
                    transitions,
                    Op::GetAsyncIterator as u8,
                    u64::from(dst),
                    u64::from(src),
                    0,
                    bail,
                    threw,
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
                emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
                dynasm!(ops ; .arch aarch64 ; b =>returned);
            }
            TemplateOp::UnsupportedBail => {
                dynasm!(ops ; .arch aarch64 ; b =>bail);
            }
        }
    }

    // Preserve the old end-of-stream exact exit while keeping every coercion
    // transition out of line. Each cold continuation returns to the label
    // immediately after its source operation's inline fast path.
    if !coercion_slow_paths.is_empty() || !numeric_slow_paths.is_empty() {
        dynasm!(ops ; .arch aarch64 ; b =>bail);
        emit_numeric_slow_paths(&mut ops, transitions, numeric_slow_paths, bail, threw);
        emit_coercion_slow_paths(&mut ops, transitions, coercion_slow_paths, bail, threw);
    }

    // Shared normal-return epilogue. `x0` already carries the boxed value.
    dynasm!(ops
        ; .arch aarch64
        ; =>returned
        ; movz x1, STATUS_RETURNED as u32
    );
    emit_epilogue(&mut ops);

    // Shared exact-side-exit epilogue: status = bailed, value = 0. The frame
    // PC stamped at the exiting instruction names the uncommitted opcode.
    dynasm!(ops
        ; .arch aarch64
        ; =>bail
        ; movz x0, #0
        ; movz x1, STATUS_BAILED as u32
    );
    emit_epilogue(&mut ops);
    // Shared throw epilogue: the poll stub parked the error in the context.
    dynasm!(ops
        ; .arch aarch64
        ; =>threw
        ; movz x0, #0
        ; movz x1, STATUS_THREW as u32
    );
    emit_epilogue(&mut ops);

    assert_eq!(next_load_ic, load_ic_cells.len(), "LoadProperty IC count");
    assert_eq!(
        next_store_ic,
        store_ic_cells.len(),
        "StoreProperty IC count"
    );

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
        emit_prologue(&mut ops);
        dynasm!(ops ; .arch aarch64 ; b =>target);
        osr_entries.insert(header_pc, offset);
    }

    let buf = ops.finalize().expect("finalize");
    let TemplatePlan {
        register_count,
        register_operands,
        index_operands,
        mut safepoint_records,
        osr_only,
        ..
    } = plan;
    safepoint_records.sort_by_key(|record| record.id);
    Ok(TemplateCode::from_emission(
        CompiledCode::new(buf, entry),
        code_object_id,
        view.code_block.id,
        register_count,
        register_operands,
        index_operands,
        load_ic_cells,
        store_ic_cells,
        safepoint_records.into_boxed_slice(),
        osr_entries,
        osr_only,
    ))
}

/// Emit the function prologue: save fp/lr + callee-saved bases, then set
/// `x20 = ctx` (arg in `x0`) and `x19 = ctx.regs` (the frame register base) —
/// the shared compiled-entry ABI.
fn emit_prologue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-32]!
        ; stp x19, x20, [sp, #16]
        ; mov x29, sp
        ; mov x20, x0
        ; ldr x19, [x20]
    );
}

/// Emit the function epilogue (restore callee-saved + frame, return). `x0`
/// (value) and `x1` (status) must already be set.
fn emit_epilogue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #32
        ; ret
    );
}

/// Publish the canonical instruction-index PC into the active native frame.
fn emit_stamp_pc(ops: &mut Assembler, pc: u32) {
    emit_load_u64(ops, 9, u64::from(pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
    );
}

/// Reduce the tagged `Value` in `x9` to `VALUE_TRUE` / `VALUE_FALSE` in `x9`
/// per `ToBoolean`. Numbers (int32 and boxed double), booleans, `null`, and
/// `undefined` decide inline; a heap cell or any other encoding (the hole,
/// function-id immediates) branches to `bail` for the exact side exit.
/// Clobbers `x14`/`x15`.
fn emit_truthiness_bool(ops: &mut Assembler, bail: DynamicLabel) {
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
    emit_load_u64(
        ops,
        16,
        otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
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
/// descriptor-resolved poll transition; a nonzero status branches to the
/// throw epilogue.
fn emit_backedge_poll(ops: &mut Assembler, poll_entry: u64, threw: DynamicLabel) {
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
        ; mov x0, x20
    );
    emit_load_u64(ops, 16, poll_entry);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; =>cont
    );
}

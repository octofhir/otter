//! Scalar value-query/coercion transition emission.
//!
//! # Contents
//! - Native guarded fast paths for string length, array length, and
//!   `Array.isArray`.
//! - Reentrant fallback to the VM-owned scalar register helper.
//! - Uniform success, throw, and exact pre-effect bailout routing.
//!
//! # Invariants
//! - Native hits only inspect guarded tags and VM-owned length fields, never
//!   allocate, and commit the destination after every guard succeeds.
//! - Proxy, realm-identity, wide-length, and wrong-type cases retain the exact
//!   VM helper semantics through the shared fallback.
//! - The VM helper commits every supported scalar opcode before returning
//!   success, so generated code only falls through once.
//! - A missing published activation is the sole bailout case and occurs before
//!   any observable coercion hook or wrapper allocation.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_scalar_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::values::{
    emit_box_int32, emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg,
};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{
    NUMBER_TAG_HI16, STATUS_BAILED, STATUS_THREW, Unsupported, VALUE_FALSE, VALUE_TRUE,
};

/// Decompress one heap-cell value into `x13`.
///
/// Non-cell values branch to `non_cell`; the caller decides whether that is a
/// semantic miss or an immediate negative result. Clobbers `x9`, `x11`, `x13`.
fn emit_cell_header(
    ops: &mut Assembler,
    src: u16,
    non_cell: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, src)?;
    dynasm!(ops
        ; .arch aarch64
        ; cbz x9, =>non_cell
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2       // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>non_cell
        ; mov x13, x9              // Value stores the full GcHeader pointer
    );
    Ok(())
}

fn emit_load_length_fast(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    src: u16,
    slow: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_cell_header(ops, src, slow)?;
    let string_tag = u32::from(view.string_layout.string_type_tag);
    let length_byte = view.string_layout.string_len_byte;
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [x13]
        ; cmp w14, string_tag
        ; b.ne =>slow
        ; ldr w9, [x13, length_byte]
    );
    emit_box_int32(ops, 9, 11);
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

fn emit_array_length_fast(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    src: u16,
    slow: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_cell_header(ops, src, slow)?;
    let array_tag = u32::from(view.array_layout.type_tag);
    let length_byte = view.array_layout.length_byte;
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [x13]
        ; cmp w14, array_tag
        ; b.ne =>slow
        ; ldr x9, [x13, length_byte]
    );
    emit_load_u64(ops, 11, i32::MAX as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x11
        ; b.hi =>slow
    );
    emit_box_int32(ops, 9, 11);
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

fn emit_is_array_fast(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    src: u16,
    slow: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let primitive = ops.new_dynamic_label();
    emit_cell_header(ops, src, primitive)?;
    let array_tag = u32::from(view.array_layout.type_tag);
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [x13]
        ; cmp w14, array_tag
        ; b.ne =>slow
    );
    emit_load_u64(ops, 9, VALUE_TRUE);
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>primitive);
    emit_load_u64(ops, 9, VALUE_FALSE);
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_scalar_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if view.cage_base != 0 {
        let dst = arg0 as u16;
        let src = arg1 as u16;
        match opcode {
            value if value == Op::LoadLength as u8 => {
                emit_load_length_fast(ops, view, dst, src, slow, done)?;
            }
            value if value == Op::ArrayLength as u8 => {
                emit_array_length_fast(ops, view, dst, src, slow, done)?;
            }
            value if value == Op::IsArray as u8 => {
                emit_is_array_fast(ops, view, dst, src, slow, done)?;
            }
            _ => {}
        }
    }
    dynasm!(ops ; .arch aarch64 ; =>slow ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, arg1);
    emit_load_u64(ops, 4, arg2);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_SCALAR_OP),
        abi::STUB_JIT_SCALAR_OP,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x0, =>done
        ; cmp x0, STATUS_BAILED as u32
        ; b.eq =>bail
        ; cmp x0, STATUS_THREW as u32
        ; b.eq =>threw
        ; b =>threw
        ; =>done
    );
    Ok(())
}

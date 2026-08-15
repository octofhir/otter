//! Scalar value-query/coercion transition emission.
//!
//! # Contents
//! - Prepared address-stable string-cell leaf loads.
//! - Native guarded fast paths for string length, array length, and
//!   `Array.isArray`.
//! - Fixed boxed-value fallback to the VM-owned typed scalar boundary.
//! - Normal-result commit and rooted JavaScript-throw routing.
//!
//! # Invariants
//! - Native hits only inspect guarded tags and VM-owned length fields, never
//!   allocate, and commit the destination after every guard succeeds.
//! - Proxy, realm-identity, wide-length, and wrong-type cases retain the exact
//!   VM helper semantics through the shared fallback.
//! - The VM decodes the authoritative operation from function/PC; no opcode,
//!   destination, or register index crosses the ABI.
//! - Once semantic entry begins, the VM returns `Ok(value)` or
//!   `Throw(exception)`. Pre-entry `Fatal` bypasses local JS handlers.
//!
//! # See also
//! - `otter_vm::RuntimeCall::scalar_values`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::values::{
    CellTest, emit_box_int32, emit_cell_test, emit_load_reg, emit_load_runtime_stub,
    emit_load_symbol_u64, emit_load_u64, emit_store_reg,
};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{Unsupported, VALUE_FALSE, VALUE_TRUE, VALUE_UNDEFINED};

/// Load one eagerly prepared primitive-string literal through its traced cell.
pub(super) fn emit_string_constant(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    result: u16,
) -> Result<(), Unsupported> {
    let target = view
        .string_constant_cells
        .get(&byte_pc)
        .ok_or(Unsupported::OperandShape("prepared LoadString stable cell"))?;
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        target.cell_addr as u64,
        RelocationTarget::StringConstantCell {
            function_id: view.code_block.id,
            byte_pc,
        },
    );
    dynasm!(ops ; .arch aarch64 ; ldr x9, [x13]);
    emit_store_reg(ops, 9, result)
}

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
    );
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, non_cell);
    dynasm!(ops
        ; .arch aarch64
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
pub(super) fn emit_scalar_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    operation: otter_vm::ScalarValueOp,
    result: u16,
    value0: Option<u16>,
    value1: Option<u16>,
    committed_throw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if view.cage_base != 0 {
        match (operation, value0) {
            (otter_vm::ScalarValueOp::LoadLength, Some(src)) => {
                emit_load_length_fast(ops, view, result, src, slow, done)?;
            }
            (otter_vm::ScalarValueOp::ArrayLength, Some(src)) => {
                emit_array_length_fast(ops, view, result, src, slow, done)?;
            }
            (otter_vm::ScalarValueOp::IsArray, Some(src)) => {
                emit_is_array_fast(ops, view, result, src, slow, done)?;
            }
            _ => {}
        }
    }
    dynasm!(ops ; .arch aarch64 ; =>slow ; mov x0, x20);
    if let Some(value0) = value0 {
        emit_load_reg(ops, 1, value0)?;
    } else {
        emit_load_u64(ops, 1, VALUE_UNDEFINED);
    }
    if let Some(value1) = value1 {
        emit_load_reg(ops, 2, value1)?;
    } else {
        emit_load_u64(ops, 2, VALUE_UNDEFINED);
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_JIT_SCALAR_VALUE),
        abi::STUB_JIT_SCALAR_VALUE,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x15, x1
        ; cbz x15, >normal
        ; cmp x15, abi::NativeResultStatus::Throw as u32
        ; b.eq =>committed_throw
        ; b =>fatal
        ; normal:
    );
    emit_store_reg(ops, 0, result)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynasmrt::{AssemblyOffset, ExecutableBuffer};

    const STRING_TAG: u8 = 0x20;

    #[repr(C, align(8))]
    struct FakeString {
        header: [u8; 8],
        length: u32,
    }

    impl FakeString {
        fn new(length: u32) -> Self {
            let mut header = [0; 8];
            header[0] = STRING_TAG;
            Self { header, length }
        }
    }

    fn load_length_program() -> (ExecutableBuffer, AssemblyOffset) {
        let mut view = JitCompileSnapshot::without_feedback(0, 0, 2, Vec::new());
        view.string_layout.string_type_tag = STRING_TAG;
        view.string_layout.string_len_byte = std::mem::offset_of!(FakeString, length) as u32;

        let mut ops = Assembler::new().expect("assembler");
        let entry = ops.offset();
        let slow = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        let exit = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; stp x19, x30, [sp, #-16]!
            ; mov x19, x0
        );
        emit_load_length_fast(&mut ops, &view, 0, 1, slow, done).expect("encodable registers");
        dynasm!(ops
            ; .arch aarch64
            ; =>slow
            ; movz x0, #0
            ; b =>exit
            ; =>done
            ; ldr x0, [x19]
            ; =>exit
            ; ldp x19, x30, [sp], #16
            ; ret
        );
        let buffer = ops.finalize().expect("finalize");
        (buffer, entry)
    }

    fn run_load_length(length: u32) -> u64 {
        let string = FakeString::new(length);
        let mut regs = [0, std::ptr::addr_of!(string) as u64];
        let (buffer, entry) = load_length_program();
        // SAFETY: the emitted leaf matches `extern "C" fn(*mut u64) -> u64`,
        // preserves its callee-saved register, and `buffer`/`regs`/`string`
        // outlive the call.
        let load: extern "C" fn(*mut u64) -> u64 =
            unsafe { std::mem::transmute(buffer.ptr(entry)) };
        load(regs.as_mut_ptr())
    }

    #[test]
    fn load_length_wide_u32_uses_canonical_slow_continuation() {
        assert_eq!(
            run_load_length(i32::MAX as u32),
            otter_vm::value::tag::box_int32(i32::MAX)
        );
        assert_eq!(run_load_length(i32::MAX as u32 + 1), 0);
        assert_eq!(run_load_length(u32::MAX), 0);
    }
}

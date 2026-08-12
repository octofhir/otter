//! Scalar value-query/coercion transition emission.
//!
//! # Contents
//! - Native guarded fast paths for string length, array length, and
//!   `Array.isArray`.
//! - Reentrant fallback to the VM-owned typed scalar boundary.
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
//! - `otter_vm::RuntimeCall::scalar_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::values::{
    CellTest, emit_box_int32, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_u64,
    emit_store_reg,
};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{STATUS_BAILED, STATUS_THREW, Unsupported, VALUE_FALSE, VALUE_TRUE};

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

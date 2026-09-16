//! System V x86-64 lowering for guarded static-native leaves.
//!
//! # Contents
//! - [`emit_guard`] proves the exact bootstrap native-function identity.
//! - [`emit_int32`] replaces proven `Math.abs` / `max` / `min` leaves.
//! - [`emit_tagged_call`] enters the shared no-allocation leaf ABI.
//!
//! # Invariants
//! - The identity guard completes before an intrinsic or native call has an
//!   observable effect.
//! - Tagged leaf calls receive `(heap, value0, value1)` in the System V
//!   integer argument registers and return the shared two-register pair.
//! - No path allocates, collects, throws, or publishes a safepoint.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot, JitStaticNativeCall, Value,
    native_abi::{STUB_MATH_ABS_LEAF, STUB_MATH_MAX_LEAF, STUB_MATH_MIN_LEAF},
};

use crate::{
    Unsupported,
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{THREAD_OFFSET, VM_THREAD_GC_HEAP_OFFSET},
};

const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;

/// Prove that `r10` still names the exact bootstrap native callable.
pub(crate) fn emit_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    builtin_native_ref: u32,
    miss: DynamicLabel,
) {
    load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov rax, r10
        ; and rax, r11
        ; jnz =>miss
        ; test r10, r10
        ; jz =>miss
        ; cmp BYTE [r10], view.collection_layout.native_function_type_tag as i8
        ; jne =>miss
        ; cmp DWORD [r10 + view.native_ref_byte as i32], builtin_native_ref as i32
        ; jne =>miss
    );
}

/// Emit the proven Int32 `Math.abs`, `Math.max`, or `Math.min` replacement.
pub(crate) fn emit_int32(
    ops: &mut Assembler,
    target: JitStaticNativeCall,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let done = ops.new_dynamic_label();
    if target.leaf_stub_id == STUB_MATH_ABS_LEAF.id {
        dynasm!(ops
            ; .arch x64
            ; mov eax, esi
            ; test eax, eax
            ; jns =>done
            ; neg eax
            ; jo =>miss
            ; =>done
        );
    } else if target.leaf_stub_id == STUB_MATH_MAX_LEAF.id {
        dynasm!(ops
            ; .arch x64
            ; mov eax, esi
            ; cmp esi, edx
            ; cmovl eax, edx
        );
    } else if target.leaf_stub_id == STUB_MATH_MIN_LEAF.id {
        dynasm!(ops
            ; .arch x64
            ; mov eax, esi
            ; cmp esi, edx
            ; cmovg eax, edx
        );
    } else {
        return Err(Unsupported::OperandShape(
            "x86-64 static-native Int32 declaration",
        ));
    }
    Ok(())
}

/// Call the shared no-allocation leaf entry after [`emit_guard`] succeeded.
pub(crate) fn emit_tagged_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    target: JitStaticNativeCall,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let Some(stub) = otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id(target.leaf_stub_id)
        .filter(|stub| stub.is_valid())
    else {
        return Err(Unsupported::OperandShape("x86-64 native leaf entry"));
    };
    if target.argument_count < 2 {
        load_u64(ops, 2, Value::undefined().to_bits());
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
    );
    let start = ops.offset().0;
    load_u64(ops, 11, stub.entry_addr() as u64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(stub.descriptor),
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; jne =>miss
    );
    Ok(())
}

#[allow(clippy::useless_conversion)]
fn load_u64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
}

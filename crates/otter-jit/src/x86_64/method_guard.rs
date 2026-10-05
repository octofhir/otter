//! Live x86-64 method target proofs.
//!
//! # Contents
//! - [`emit`] checks an ordinary receiver and its current method slot, leaving
//!   the exact callable ready for the shared generated call ABI.
//!
//! # Invariants
//! - All mutable receiver, prototype and callable facts are read before effects.
//! - A miss leaves resolution and the call to the committed method boundary.
//! - The callable in `r9` is a full tagged value; no moving address is retained.
//! - Only `r8..r11` and flags are clobbered; no allocation or safepoint occurs.
//! - Inherited holders are read through retained root shapes and validity cells.
//!
//! # See also
//! - `otter_vm::jit::JitMethodGuard` for the owned proof inputs.
//! - [`super::js_call`] for current-generation linkage.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, jit::JitMethodGuard, value::tag};

use super::values::emit_load_symbol_u64;

use crate::{
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{OBJECT_BODY_TYPE_TAG, Unsupported},
    x86_64::fields::emit_field_base,
};

/// Prove a method on the full receiver value in `r8`, returning its callable
/// in `r9`. Branch to `miss` before effects on any failed proof.
pub(crate) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    guard: &JitMethodGuard,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if view.cage_base == 0 {
        return Err(Unsupported::OperandShape("x86-64 method guard cage base"));
    }
    let method_byte = i32::try_from(guard.method_field.byte_offset())
        .map_err(|_| Unsupported::OperandShape("x86-64 method field displacement"))?;
    dynasm!(ops
        ; .arch x64
        ; test r8, r8
        ; jz =>miss
        ; mov r10, QWORD tag::NOT_CELL_MASK as i64
        ; test r8, r10
        ; jnz =>miss
        ; cmp BYTE [r8], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
        ; cmp DWORD [r8 + view.object_shape_byte as i32], guard.recv_shape as i32
        ; jne =>miss
    );
    // The compile boundary admitted this finalized ordinary shape. Its exact
    // identity fixes lookup state and descriptors; no header byte can override it.
    if let Some(validity) = guard.prototype_validity {
        emit_load_symbol_u64(
            ops,
            relocations,
            10,
            validity.address as u64,
            RelocationTarget::PrototypeValidityCell {
                identity: validity.identity,
            },
        );
        // Ordinary loads provide acquire ordering on x86-64.
        dynasm!(ops ; .arch x64 ; cmp DWORD [r10], 0 ; je =>miss);
        emit_load_symbol_u64(
            ops,
            relocations,
            11,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops
            ; .arch x64
            ; mov r10d, guard.holder_root as i32
            ; add r10, r11
            ; mov r8d, [r10 + view.shape_prototype_byte as i32]
            ; test r8d, r8d
            ; jz =>miss
            ; add r8, r11
        );
    }
    emit_field_base(ops, relocations, view, 8, 8, 10, guard.method_field);
    dynasm!(ops
        ; .arch x64
        ; mov r9, [r8 + method_byte]
    );
    let guarded = ops.new_dynamic_label();
    let incompatible =
        view.closure_call_layout.runtime_setup_flags | view.closure_call_layout.bound_this_flag;
    dynasm!(ops
        ; .arch x64
        ; mov r10, QWORD tag::box_function_id(guard.method_fid) as i64
        ; cmp r9, r10
        ; je =>guarded
        ; test r9, r9
        ; jz =>miss
        ; mov r10, QWORD tag::NOT_CELL_MASK as i64
        ; test r9, r10
        ; jnz =>miss
        ; cmp BYTE [r9], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
        ; jne =>miss
        ; test DWORD [r9 + view.closure_call_layout.flags_byte as i32], incompatible as i32
        ; jnz =>miss
        ; cmp DWORD [r9 + view.closure_call_layout.function_id_byte as i32], guard.method_fid as i32
        ; jne =>miss
        ; =>guarded
    );
    Ok(())
}

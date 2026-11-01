//! `OrdinaryHasInstance` (§7.3.22) in x86-64 generated code, shared by the
//! baseline and optimizing tiers.
//!
//! # Contents
//! - [`emit_routine`] — the baseline tier's one shared `instanceof` routine.
//! - [`emit_ordinary_has_instance`] — an ordinary closure target without an
//!   own `@@hasInstance`, its `prototype` an ordinary object, and the value's
//!   prototype chain walked up to
//!   [`crate::graph::INSTANCEOF_CHAIN_BOUND`] links.
//!
//! # Invariants
//! - Every outcome is decided before any effect but the site's `instanceof`
//!   cell: a caching write of the proved target and prototype, with the
//!   target's rare-record flag that asks the VM to empty the cell before the
//!   closure changes.
//! - A non-cell or primitive value answers `false`; an opaque link, an exotic
//!   link, a long chain or any other target jumps to `miss`, whose caller
//!   completes the operation in the runtime.
//! - Writes only the five temporaries, `r10` and `r11`; `value` and `target`
//!   survive.
//!
//! # See also
//! - `otter_vm::closure` owns the closure layout and named-lookup byte.
//! - `crate::graph::x86_64` and `crate::template::x86_64` call it.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::object::ShapeState;

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::x86_64::values::{emit_load_symbol_u64, emit_load_u64};

/// Jump to `miss` unless `value` is a cell.
fn emit_cell_guard(ops: &mut Assembler, value: u8, miss: DynamicLabel) {
    emit_load_u64(ops, 10, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64 ; test Rq(value), r10 ; jnz =>miss ; test Rq(value), Rq(value) ; jz =>miss);
}

/// The immutable shape state of the object in `receiver`, in `r10d`.
fn emit_shape_state(ops: &mut Assembler, view: &JitCompileSnapshot, receiver: u8) {
    emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(receiver)
        ; mov r10d, [Rq(receiver) + view.object_shape_byte as i32]
        ; add r10, r11
        ; movzx r10d, BYTE [r10 + view.shape_state_byte as i32]);
}

/// Routine input: the value `instanceof` tests.
pub(crate) const ROUTINE_VALUE: u8 = 6;
/// Routine input: the target.
pub(crate) const ROUTINE_TARGET: u8 = 2;

/// The baseline `instanceof` routine: [`emit_ordinary_has_instance`] of
/// `Rq(ROUTINE_VALUE)` and `Rq(ROUTINE_TARGET)` without a site cell.
/// Returns `r11 == 0` with the boolean in `rax`, or `r11 != 0` for a case
/// the caller completes in the runtime. Writes `rax`, `rcx`, `rdi`, `r8`–`r11`;
/// both inputs survive.
pub(crate) fn emit_routine(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) -> crate::routines::RoutineOffsets {
    let entry = ops.offset().0;
    let [yes, no, miss] = [
        ops.new_dynamic_label(),
        ops.new_dynamic_label(),
        ops.new_dynamic_label(),
    ];
    emit_ordinary_has_instance(
        ops,
        relocations,
        view,
        None,
        [ROUTINE_VALUE, ROUTINE_TARGET],
        [0, 1, 7, 8, 9],
        [yes, no, miss],
    );
    dynasm!(ops ; .arch x64 ; =>yes);
    emit_load_u64(ops, 0, crate::entry::VALUE_TRUE);
    dynasm!(ops ; .arch x64 ; xor r11d, r11d ; ret ; =>no);
    emit_load_u64(ops, 0, crate::entry::VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; xor r11d, r11d ; ret ; =>miss ; mov r11d, 1 ; ret);
    crate::routines::RoutineOffsets {
        entry,
        second: entry,
    }
}

/// Jump to `yes` when the value in `Rq(value)` is an instance of the target
/// in `Rq(target)` by `OrdinaryHasInstance`, to `no` when it is not, and to
/// `miss` for any case this walk does not decide. `cell` is the site's
/// `instanceof` cell, when it has one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_ordinary_has_instance(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    cell: Option<(u64, RelocationTarget)>,
    [value, target]: [u8; 2],
    [cage, rare, prototype, cursor, budget]: [u8; 5],
    [yes, no, miss]: [DynamicLabel; 3],
) {
    let layout = view.closure_call_layout;
    let symbols_absent = ops.new_dynamic_label();
    let proved = ops.new_dynamic_label();
    let validate = ops.new_dynamic_label();
    let walk = ops.new_dynamic_label();
    let step = ops.new_dynamic_label();
    let target_byte = otter_vm::jit::JIT_INSTANCEOF_CELL_TARGET_OFFSET as i32;
    let prototype_byte = otter_vm::jit::JIT_INSTANCEOF_CELL_PROTOTYPE_OFFSET as i32;
    if view.cage_base == 0 || layout.prototype_byte == 0 {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
    } else {
        emit_load_symbol_u64(
            ops,
            relocations,
            cage,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        if let Some((address, relocation)) = cell.clone() {
            // The target this site last proved, and its prototype.
            emit_load_symbol_u64(ops, relocations, 10, address, relocation);
            dynasm!(ops ; .arch x64
                ; cmp Rq(target), [r10 + target_byte] ; jne =>validate
                ; mov Rq(prototype), [r10 + prototype_byte] ; jmp =>proved
                ; =>validate);
        }
        emit_cell_guard(ops, target, miss);
        dynasm!(ops ; .arch x64
            ; mov Rd(rare), Rd(target) ; add Rq(rare), Rq(cage)
            ; cmp BYTE [Rq(rare)], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8 ; jne =>miss
            ; movzx r10d, BYTE [Rq(rare) + otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE as i32]
            ; and r10d, !u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS) as i32
            ; cmp r10d, otter_vm::closure::CLOSURE_LOOKUP_ORDINARY as i32 ; jne =>miss
            ; mov Rd(rare), [Rq(rare) + layout.rare_byte as i32] ; test Rd(rare), Rd(rare) ; jz =>miss
            ; add Rq(rare), Rq(cage)
            // Any `prototype` but an ordinary object completes in the runtime.
            ; cmp BYTE [Rq(rare) + layout.prototype_ordinary_byte as i32], 0 ; je =>miss
            ; mov Rq(prototype), [Rq(rare) + layout.prototype_byte as i32]
            ; mov r10d, [Rq(rare) + layout.own_props_byte as i32] ; test r10d, r10d ; jz =>symbols_absent
            ; add r10, Rq(cage)
            // A symbol key in the bag's hidden class or side table may
            // be `@@hasInstance`.
            ; mov r11d, [r10 + view.object_shape_byte as i32] ; add r11, Rq(cage)
            ; cmp BYTE [r11 + otter_vm::object::SHAPE_SYMBOL_KEYS_BYTE as i32], 0 ; jne =>miss
            ; mov r10d, [r10 + view.object_exotic_handle_byte as i32]
            ; test r10d, r10d ; jz =>proved ; add r10, Rq(cage)
            ; cmp DWORD [r10 + otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE as i32], 0 ; jne =>miss
            ; jmp =>proved
            ; =>symbols_absent);
        if let Some((address, relocation)) = cell {
            // A closure without own properties is cached; the flag asks
            // the VM to empty the cell before the closure changes.
            emit_load_symbol_u64(ops, relocations, 10, address, relocation);
            dynasm!(ops ; .arch x64
                ; mov [r10 + target_byte], Rq(target)
                ; mov [r10 + prototype_byte], Rq(prototype)
                ; mov BYTE [Rq(rare) + layout.instanceof_cached_byte as i32], 1);
        }
        dynasm!(ops ; .arch x64 ; =>proved);
        emit_cell_guard(ops, value, no);
        dynasm!(ops ; .arch x64 ; mov Rd(cursor), Rd(value) ; add Rq(cursor), Rq(cage)
            ; movzx r10d, BYTE [Rq(cursor)] ; cmp r10d, crate::entry::OBJECT_BODY_TYPE_TAG as i32 ; je =>walk);
        for primitive in view.primitive_cell_type_tags {
            dynasm!(ops ; .arch x64 ; cmp r10d, primitive as i32 ; je =>no);
        }
        dynasm!(ops ; .arch x64 ; jmp =>miss
            ; =>walk ; mov Rd(budget), crate::graph::INSTANCEOF_CHAIN_BOUND as i32
            ; =>step);
        emit_shape_state(ops, view, cursor);
        dynasm!(ops ; .arch x64 ; test r10b, ShapeState::OPAQUE_LOOKUP_MASK as i8 ; jnz =>miss);
        crate::x86_64::fields::emit_load_prototype(ops, view, 10, cursor, cage);
        dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>no
            ; lea Rq(cursor), [Rq(cage) + r10] ; cmp Rq(cursor), Rq(prototype) ; je =>yes
            ; cmp BYTE [Rq(cursor)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss
            ; dec Rd(budget) ; jnz =>step ; jmp =>miss);
    }
}

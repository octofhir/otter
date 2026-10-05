//! Schema-owned binding and global-declaration emission for Template AArch64.
//!
//! # Contents
//! - Shared physical global reads and guards, with binding policy here.
//! - Checked context-slot accesses complete inline unless the slot is a TDZ
//!   `hole`; lookup and eval-extension accesses complete through the
//!   committed cold boundary. The context register is one of its boxed inputs.
//! - One fixed boxed-value committed cold boundary per semantic family.
//! - Family-level guard/hit/cold/join artifact regions.
//!
//! # Invariants
//! - The published function/PC and opcode schema own names, indices, flags,
//!   and missing-binding policy; generated calls pass only boxed values.
//! - A generated write runs the canonical write barrier for a cell value.
//! - An eligible ordinary global's exact immutable shape fixes lookup state,
//!   descriptor attributes and prototype role. Dictionary hits prove their
//!   current state kind and watched descriptor layout instead; stores also
//!   refuse a live prototype-role shape before effects.
//! - Every guard miss enters the committed cold sibling. It never deoptimizes
//!   or replays an accessor, proxy, TDZ, const, eval-chain, or unresolved case.
//! - Cold JavaScript failure carries its pure exception value to the shared
//!   Template throw router; the binding ABI has no side-exit result.
//!
//! # See also
//! - `otter_bytecode::opcode_schema::BindingSemantics` — semantic authority.
//! - `crate::arm64::binding` — physical cell, object and field-bank proof.
//! - `crate::entry::runtime_ops::reentry` — fixed committed entry functions.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::ContextCoord;
use otter_bytecode::opcode_schema::{
    BindingRead, BindingSemantics, BindingWrite, GlobalDeclarationSemantics,
};
use otter_vm::{JitCompileSnapshot, jit::BindingHitProof, native_abi as abi, object::ShapeState};

use super::context::{CheckedContextAccess, emit_checked_context_slot};
use super::transitions::TransitionTable;
use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64,
    emit_load_u64, emit_store_reg, emit_write_barrier,
};
use crate::arm64::binding as native_binding;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::artifact::{CodeMapCapture, CodeRegion};
use crate::entry::{GLOBAL_THIS_OFFSET_PTR_OFFSET, Unsupported, VALUE_TRUE, VALUE_UNDEFINED};

fn record_region(
    capture: &mut Option<&mut CodeMapCapture>,
    kind: &'static str,
    start: usize,
    end: usize,
    byte_pc: u32,
) {
    if let Some(capture) = capture.as_deref_mut() {
        capture.record(CodeRegion::structural_at_byte_pc(kind, start, end, byte_pc));
    }
}

/// Dictionary layout identity does not fix the object's current role.
fn emit_global_dictionary_write_role_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) {
    super::values::emit_load_shape_state(ops, relocations, view, 13, 14, 11);
    dynasm!(ops ; .arch aarch64
        ; tbnz w14, ShapeState::PROTOTYPE_MASK.trailing_zeros(), =>miss);
}

fn emit_cell_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    parent: u8,
    address: u8,
    byte_offset: u32,
    source: u16,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let store = |ops: &mut Assembler| {
        if byte_offset <= 32760 && byte_offset.is_multiple_of(8) {
            dynasm!(ops ; .arch aarch64 ; str x9, [X(address), byte_offset]);
        } else {
            emit_load_u64(ops, 11, u64::from(byte_offset));
            dynasm!(ops ; .arch aarch64 ; str x9, [X(address), x11]);
        }
    };
    let primitive = ops.new_dynamic_label();
    emit_load_reg(ops, 9, source)?;
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, primitive);
    store(ops);
    emit_write_barrier(ops, relocations, view, parent, 9);
    dynasm!(ops
        ; .arch aarch64
        ; b =>done
        ; =>primitive
    );
    store(ops);
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_binding_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    semantics: BindingSemantics,
    result: Option<u16>,
    value0: Option<u16>,
    value1: Option<u16>,
    context_coord: Option<ContextCoord>,
    byte_pc: u32,
    mut code_map: Option<&mut CodeMapCapture>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let guard_start = ops.offset().0;
    let mut guard_end = guard_start;
    let mut hit_end = guard_start;

    match semantics {
        BindingSemantics::Read(BindingRead::ContextSlot { .. })
        | BindingSemantics::Write(BindingWrite::ContextSlot { .. }) => {
            let coord =
                context_coord.ok_or(Unsupported::OperandShape("checked context coordinate"))?;
            let (access, context) = match semantics {
                BindingSemantics::Read(_) => (
                    CheckedContextAccess::Load {
                        dst: result.ok_or(Unsupported::OperandShape("context read result"))?,
                    },
                    value0,
                ),
                _ => (
                    CheckedContextAccess::Store {
                        src: value0.ok_or(Unsupported::OperandShape("context write value"))?,
                    },
                    value1,
                ),
            };
            let context = context.ok_or(Unsupported::OperandShape("checked context register"))?;
            guard_end = emit_checked_context_slot(
                ops,
                relocations,
                view,
                access,
                context,
                coord.depth,
                coord.slot,
                miss,
                done,
            )?;
            hit_end = ops.offset().0;
        }
        BindingSemantics::Read(BindingRead::GlobalThis { .. }) if view.cage_base != 0 => {
            let dst = result.ok_or(Unsupported::OperandShape("globalThis read result"))?;
            native_binding::emit_global_realm_guard(ops, view, miss);
            guard_end = ops.offset().0;
            // The isolate publishes the global object as a compressed cage
            // offset; a Value is the full address, so rebase before it can
            // reach a register (bit-compared by `===` and traced by GC).
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET]
                ; ldr w9, [x14]
            );
            emit_load_symbol_u64(
                ops,
                relocations,
                13,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops ; .arch aarch64 ; add x9, x9, x13);
            emit_store_reg(ops, 9, dst)?;
            dynasm!(ops ; .arch aarch64 ; b =>done);
            hit_end = ops.offset().0;
        }
        BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Exists { .. }) => {
            let dst = result.ok_or(Unsupported::OperandShape("binding read result"))?;
            if let Some(proof) = view.binding_hit_proofs.get(&byte_pc).copied() {
                if matches!(
                    semantics,
                    BindingSemantics::Read(BindingRead::Global { .. })
                ) {
                    guard_end = native_binding::emit_global_read(
                        ops,
                        relocations,
                        view,
                        proof,
                        byte_pc,
                        9,
                        [13, 14],
                        miss,
                    )?;
                    emit_store_reg(ops, 9, dst)?;
                    dynasm!(ops ; .arch aarch64 ; b =>done);
                    hit_end = ops.offset().0;
                } else {
                    match proof {
                        BindingHitProof::GlobalLexical { cell_offset, .. } => {
                            if native_binding::emit_global_cell_address(
                                ops,
                                relocations,
                                view,
                                cell_offset,
                                byte_pc,
                                13,
                                miss,
                            ) {
                                guard_end = ops.offset().0;
                                emit_load_u64(ops, 9, VALUE_TRUE);
                                emit_store_reg(ops, 9, dst)?;
                                dynasm!(ops ; .arch aarch64 ; b =>done);
                                hit_end = ops.offset().0;
                            }
                        }
                        BindingHitProof::GlobalObject {
                            shape,
                            dictionary,
                            global_lexical_epoch,
                            ..
                        } if view.cage_base != 0 => {
                            native_binding::emit_global_object_guard(
                                ops,
                                relocations,
                                view,
                                shape,
                                dictionary,
                                global_lexical_epoch,
                                [13, 14],
                                miss,
                            );
                            guard_end = ops.offset().0;
                            emit_load_u64(ops, 9, VALUE_TRUE);
                            emit_store_reg(ops, 9, dst)?;
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                            hit_end = ops.offset().0;
                        }
                        _ => {}
                    }
                }
            }
        }
        BindingSemantics::Write(
            BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. },
        ) => {
            let source = value0.ok_or(Unsupported::OperandShape("global write value"))?;
            if let BindingSemantics::Write(BindingWrite::GlobalChecked { .. }) = semantics {
                let exists = value1.ok_or(Unsupported::OperandShape("checked-global exists"))?;
                emit_load_reg(ops, 9, exists)?;
                emit_load_u64(ops, 11, VALUE_TRUE);
                dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.ne =>miss);
            }
            if let Some(proof) = view.binding_hit_proofs.get(&byte_pc).copied() {
                match proof {
                    BindingHitProof::GlobalLexical {
                        cell_offset,
                        writable: true,
                    } => {
                        if native_binding::emit_global_lexical_guard(
                            ops,
                            relocations,
                            view,
                            cell_offset,
                            byte_pc,
                            13,
                            miss,
                        ) {
                            guard_end = ops.offset().0;
                            emit_cell_store(
                                ops,
                                relocations,
                                view,
                                13,
                                13,
                                view.global_lexical_value_byte,
                                source,
                                done,
                            )?;
                            hit_end = ops.offset().0;
                        }
                    }
                    BindingHitProof::GlobalObject {
                        shape,
                        dictionary,
                        field,
                        global_lexical_epoch,
                        writable: true,
                    } if view.cage_base != 0 => {
                        native_binding::emit_global_object_guard(
                            ops,
                            relocations,
                            view,
                            shape,
                            dictionary,
                            global_lexical_epoch,
                            [13, 14],
                            miss,
                        );
                        if dictionary {
                            // Layout identity does not fix dictionary shape
                            // state, so current prototype role remains dynamic.
                            emit_global_dictionary_write_role_guard(ops, relocations, view, miss);
                        }
                        native_binding::emit_global_field_bank(
                            ops,
                            relocations,
                            view,
                            13,
                            14,
                            field,
                            miss,
                        );
                        guard_end = ops.offset().0;
                        emit_cell_store(
                            ops,
                            relocations,
                            view,
                            13,
                            14,
                            field.byte_offset(),
                            source,
                            done,
                        )?;
                        hit_end = ops.offset().0;
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // A checked-global precondition may be emitted even when no physical hit
    // proof is usable. Attribute that pre-cold work to the guard and keep the
    // hit region empty instead of folding it into the committed call.
    if hit_end == guard_start && ops.offset().0 != guard_start {
        guard_end = ops.offset().0;
        hit_end = guard_end;
    }

    record_region(
        &mut code_map,
        "templateBindingGuard",
        guard_start,
        guard_end,
        byte_pc,
    );
    record_region(
        &mut code_map,
        "templateBindingHit",
        guard_end,
        hit_end,
        byte_pc,
    );

    let cold_start = ops.offset().0;
    dynasm!(ops ; .arch aarch64 ; =>miss ; mov x0, x20);
    match value0 {
        Some(register) => emit_load_reg(ops, 1, register)?,
        None => emit_load_u64(ops, 1, VALUE_UNDEFINED),
    }
    match value1 {
        Some(register) => emit_load_reg(ops, 2, register)?,
        None => emit_load_u64(ops, 2, VALUE_UNDEFINED),
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_BINDING_VALUE),
        abi::STUB_JIT_BINDING_VALUE,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x1, >binding_completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; binding_completed:
    );
    if let Some(result) = result {
        emit_store_reg(ops, 0, result)?;
    }
    let cold_end = ops.offset().0;
    dynasm!(ops ; .arch aarch64 ; =>done);
    let join = ops.offset().0;
    record_region(
        &mut code_map,
        "templateBindingCold",
        cold_start,
        cold_end,
        byte_pc,
    );
    record_region(&mut code_map, "templateBindingJoin", join, join, byte_pc);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_global_declaration_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    _semantics: GlobalDeclarationSemantics,
    value0: Option<u16>,
    value1: Option<u16>,
    byte_pc: u32,
    mut code_map: Option<&mut CodeMapCapture>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let cold_start = ops.offset().0;
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    match value0 {
        Some(register) => emit_load_reg(ops, 1, register)?,
        None => emit_load_u64(ops, 1, VALUE_UNDEFINED),
    }
    match value1 {
        Some(register) => emit_load_reg(ops, 2, register)?,
        None => emit_load_u64(ops, 2, VALUE_UNDEFINED),
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_GLOBAL_DECLARATION_VALUE),
        abi::STUB_JIT_GLOBAL_DECLARATION_VALUE,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x1, >global_declaration_completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; global_declaration_completed:
    );
    let cold_end = ops.offset().0;
    record_region(
        &mut code_map,
        "templateGlobalDeclarationCold",
        cold_start,
        cold_end,
        byte_pc,
    );
    record_region(
        &mut code_map,
        "templateGlobalDeclarationJoin",
        cold_end,
        cold_end,
        byte_pc,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{THREAD_OFFSET, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET};

    fn word(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
    }

    #[test]
    fn native_global_guard_reads_exact_identity_and_every_dictionary_state() {
        const OBJECT: u32 = 64;
        const SHAPE: u32 = 128;
        const EXOTIC: u32 = 192;
        const LAYOUT: u32 = 69;
        let mut cage = vec![0_u8; 256].into_boxed_slice();
        let mut thread = vec![
            0_u8;
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET
                .max(crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET)
                as usize
                + 8
        ]
        .into_boxed_slice();
        let mut context = vec![0_u8; THREAD_OFFSET.max(GLOBAL_THIS_OFFSET_PTR_OFFSET) as usize + 8]
            .into_boxed_slice();
        let global = OBJECT;
        let mut epoch = 0_u64;
        let realm = 0_u32;
        word(&mut context, THREAD_OFFSET as usize, thread.as_ptr() as u64);
        word(
            &mut context,
            GLOBAL_THIS_OFFSET_PTR_OFFSET as usize,
            (&global as *const u32) as u64,
        );
        word(
            &mut thread,
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
            (&mut epoch as *mut u64) as u64,
        );
        word(
            &mut thread,
            crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
            (&realm as *const u32) as u64,
        );
        let mut view = JitCompileSnapshot::without_feedback(7, 0, 1, vec![]);
        view.cage_base = cage.as_ptr() as usize;
        view.object_shape_byte = 8;
        view.object_exotic_handle_byte = 16;
        view.shape_state_byte = 8;
        view.exotic_dictionary_layout_byte = 8;
        cage[OBJECT as usize + 8..OBJECT as usize + 12].copy_from_slice(&SHAPE.to_ne_bytes());
        cage[OBJECT as usize + 16..OBJECT as usize + 20].copy_from_slice(&EXOTIC.to_ne_bytes());
        word(&mut cage, EXOTIC as usize + 8, u64::from(LAYOUT));
        for dictionary in [false, true] {
            for write in [false, true] {
                for expected_epoch in [0_u64, 1, 0xdead_beef_89ab_cdef, u64::MAX] {
                    let mut ops = Assembler::new().unwrap();
                    let entry = ops.offset();
                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    let mut relocations = RelocationCapture::new(false);
                    dynasm!(ops ; .arch aarch64 ; stp x20, x30, [sp, #-16]! ; mov x20, x0);
                    native_binding::emit_global_object_guard(
                        &mut ops,
                        &mut relocations,
                        &view,
                        u64::from(if dictionary { LAYOUT } else { SHAPE }),
                        dictionary,
                        expected_epoch,
                        [13, 14],
                        miss,
                    );
                    if dictionary && write {
                        emit_global_dictionary_write_role_guard(
                            &mut ops,
                            &mut relocations,
                            &view,
                            miss,
                        );
                    }
                    dynasm!(ops ; .arch aarch64
                        ; mov w0, 1 ; b =>done
                        ; =>miss ; mov w0, wzr
                        ; =>done ; ldp x20, x30, [sp], #16 ; ret);
                    let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
                    // SAFETY: the guard reads only live owned byte arenas and
                    // scalar cells, preserves x20/LR/SP and never calls or publishes
                    // a VM/GC frame. All pointers outlive the executable mapping.
                    let run: unsafe extern "C" fn(*const u8) -> u32 =
                        unsafe { std::mem::transmute(code.entry_ptr()) };
                    epoch = expected_epoch;
                    if dictionary {
                        // Layout identity can survive a state-only shape change.
                        // Exercise all possible physical state bytes at its live
                        // owner, including every opaque/provisional combination.
                        for state in u8::MIN..=u8::MAX {
                            cage[SHAPE as usize + 8] = state;
                            let expected = state & ShapeState::DICTIONARY_MASK != 0
                                && state
                                    & (ShapeState::OPAQUE_LOOKUP_MASK
                                        | ShapeState::PROVISIONAL_MASK)
                                    == 0
                                && (!write || state & ShapeState::PROTOTYPE_MASK == 0);
                            let before = cage.to_vec();
                            // SAFETY: same retained mapping/arena/scalar contract.
                            assert_eq!(
                                unsafe { run(context.as_ptr()) },
                                u32::from(expected),
                                "state={state:#x}, write={write}, epoch={expected_epoch}"
                            );
                            assert_eq!(
                                cage.as_ref(),
                                before.as_slice(),
                                "guard has no heap effects"
                            );
                            assert_eq!(epoch, expected_epoch);
                        }
                        cage[SHAPE as usize + 8] = ShapeState::DICTIONARY_MASK;
                    } else {
                        cage[SHAPE as usize + 8] = ShapeState::ORDINARY.bits();
                        // SAFETY: same live owned guard fixture.
                        assert_eq!(unsafe { run(context.as_ptr()) }, 1);
                        let other = SHAPE + 1;
                        cage[OBJECT as usize + 8..OBJECT as usize + 12]
                            .copy_from_slice(&other.to_ne_bytes());
                        // SAFETY: a mismatch exits before following any shape token.
                        assert_eq!(unsafe { run(context.as_ptr()) }, 0);
                        cage[OBJECT as usize + 8..OBJECT as usize + 12]
                            .copy_from_slice(&SHAPE.to_ne_bytes());
                    }
                    epoch = expected_epoch ^ 1;
                    // SAFETY: the changed scalar remains at the same owned address.
                    assert_eq!(
                        unsafe { run(context.as_ptr()) },
                        0,
                        "full-word lexical epoch mismatch"
                    );
                    assert_eq!(epoch, expected_epoch ^ 1, "guard cannot alter epoch");
                }
            }
        }
    }
}

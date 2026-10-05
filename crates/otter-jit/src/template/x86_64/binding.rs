//! Schema-owned binding hits and committed misses for x86-64 native tiers.
//!
//! # Contents
//! - Stable global lexical cells and guarded global object slot accesses.
//! - Checked context slots and direct global-this reads.
//! - Exact guard/hit/cold/join ranges for binding operations.
//!
//! # Invariants
//! - The opcode schema and published semantic source own every binding name,
//!   coordinate, assignment flag, and missing-binding policy.
//! - Generated hits read the live cell or slot. TDZ, const, descriptor, realm
//!   epoch, ordinary lookup-state, dictionary and watched-prototype misses enter
//!   the committed owner once before any store or user-visible effect.
//! - A cell-valued store uses the existing noncollecting write barrier.
//! - r13/r14/r15 retain the window, activation and context on every path;
//!   derived addresses stay in volatile scratch registers between operations.
//! - Eligible global reads share the physical guard/bank owner with Graph;
//!   schema-only operations retain their canonical Template window.
//!
//! # See also
//! - `crate::x86_64::binding` owns shared global read/guard/bank geometry.
//! - `super::context` owns checked context-slot geometry.
//! - `crate::template::arm64::binding` implements the peer target contract.

use super::*;
use otter_bytecode::{
    ContextCoord,
    opcode_schema::{BindingRead, BindingSemantics, BindingWrite},
};
use otter_vm::{jit::BindingHitProof, object::ShapeState};

use crate::entry::GLOBAL_THIS_OFFSET_PTR_OFFSET;
use crate::x86_64::binding::{
    emit_global_cell_address, emit_global_field_bank, emit_global_lexical_guard,
    emit_global_object_guard, emit_global_read, emit_global_realm_guard,
};

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

/// A dictionary layout token does not fix the object's current prototype role.
/// The preceding global guard leaves the live header in r8 and cage in r11.
fn emit_global_dictionary_write_role_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) {
    dynasm!(ops ; .arch x64
        ; mov r10d, [r8 + view.object_shape_byte as i32]
        ; test BYTE [r11 + r10 + view.shape_state_byte as i32], ShapeState::PROTOTYPE_MASK as i8
        ; jnz =>miss
    );
}

fn emit_cell_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    offset: u32,
    source: u16,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let offset = i32::try_from(offset)
        .map_err(|_| Unsupported::OperandShape("binding cell displacement"))?;
    emit_load_reg(ops, 2, source);
    dynasm!(ops ; .arch x64 ; mov [r9 + offset], rdx);
    emit_template_value_barrier(ops, relocations, view, 8, 2);
    dynasm!(ops ; .arch x64 ; jmp =>done);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_binding_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    semantics: BindingSemantics,
    result: Option<u16>,
    value0: Option<u16>,
    value1: Option<u16>,
    coord: Option<ContextCoord>,
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
            let coord = coord.ok_or(Unsupported::OperandShape("checked context coordinate"))?;
            let (access, context) = match semantics {
                BindingSemantics::Read(_) => (
                    context::CheckedContextAccess::Load {
                        dst: result.ok_or(Unsupported::OperandShape("context read result"))?,
                    },
                    value0,
                ),
                _ => (
                    context::CheckedContextAccess::Store {
                        src: value0.ok_or(Unsupported::OperandShape("context write value"))?,
                    },
                    value1,
                ),
            };
            guard_end = context::emit_checked_context_slot(
                ops,
                relocations,
                view,
                access,
                context.ok_or(Unsupported::OperandShape("checked context register"))?,
                coord.depth,
                coord.slot,
                miss,
            )?;
            dynasm!(ops ; .arch x64 ; jmp =>done);
            hit_end = ops.offset().0;
        }
        BindingSemantics::Read(BindingRead::GlobalThis { .. }) if view.cage_base != 0 => {
            let dst = result.ok_or(Unsupported::OperandShape("global-this result"))?;
            emit_global_realm_guard(ops, view, miss);
            guard_end = ops.offset().0;
            dynasm!(ops ; .arch x64
                ; mov r9, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
                ; mov eax, [r9]
            );
            emit_load_symbol_u64(
                ops,
                relocations,
                11,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops ; .arch x64 ; add rax, r11);
            emit_store_reg(ops, 0, dst);
            dynasm!(ops ; .arch x64 ; jmp =>done);
            hit_end = ops.offset().0;
        }
        BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Exists { .. }) => {
            let dst = result.ok_or(Unsupported::OperandShape("binding read result"))?;
            if let Some(proof) = view.binding_hit_proofs.get(&byte_pc).copied() {
                if matches!(
                    semantics,
                    BindingSemantics::Read(BindingRead::Global { .. })
                ) {
                    guard_end =
                        emit_global_read(ops, relocations, view, proof, byte_pc, 0, [8, 9], miss)?;
                    emit_store_reg(ops, 0, dst);
                    dynasm!(ops ; .arch x64 ; jmp =>done);
                    hit_end = ops.offset().0;
                } else {
                    let admitted = match proof {
                        BindingHitProof::GlobalLexical { cell_offset, .. } => {
                            emit_global_cell_address(
                                ops,
                                relocations,
                                view,
                                cell_offset,
                                byte_pc,
                                9,
                                miss,
                            )
                        }
                        BindingHitProof::GlobalObject {
                            shape,
                            dictionary,
                            global_lexical_epoch,
                            ..
                        } if view.cage_base != 0 => {
                            emit_global_object_guard(
                                ops,
                                relocations,
                                view,
                                shape,
                                dictionary,
                                global_lexical_epoch,
                                [8, 9],
                                miss,
                            );
                            true
                        }
                        _ => false,
                    };
                    if admitted {
                        // A declared lexical cell exists even while its value is HOLE.
                        guard_end = ops.offset().0;
                        emit_load_u64(ops, 0, VALUE_TRUE);
                        emit_store_reg(ops, 0, dst);
                        dynasm!(ops ; .arch x64 ; jmp =>done);
                        hit_end = ops.offset().0;
                    }
                }
            }
        }
        BindingSemantics::Write(
            BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. },
        ) => {
            let src = value0.ok_or(Unsupported::OperandShape("global write value"))?;
            if matches!(
                semantics,
                BindingSemantics::Write(BindingWrite::GlobalChecked { .. })
            ) {
                let exists = value1.ok_or(Unsupported::OperandShape("checked global exists"))?;
                emit_load_reg(ops, 0, exists);
                emit_load_u64(ops, 11, VALUE_TRUE);
                dynasm!(ops ; .arch x64 ; cmp rax, r11 ; jne =>miss);
            }
            if let Some(proof) = view.binding_hit_proofs.get(&byte_pc).copied() {
                match proof {
                    BindingHitProof::GlobalLexical {
                        cell_offset,
                        writable: true,
                    } => {
                        if emit_global_cell_address(
                            ops,
                            relocations,
                            view,
                            cell_offset,
                            byte_pc,
                            9,
                            miss,
                        ) {
                            emit_global_lexical_guard(ops, view, 9, miss);
                            dynasm!(ops ; .arch x64 ; mov r8, r9);
                            guard_end = ops.offset().0;
                            emit_cell_store(
                                ops,
                                relocations,
                                view,
                                view.global_lexical_value_byte,
                                src,
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
                        emit_global_object_guard(
                            ops,
                            relocations,
                            view,
                            shape,
                            dictionary,
                            global_lexical_epoch,
                            [8, 9],
                            miss,
                        );
                        if dictionary {
                            // r11 retains the cage base from the global guard.
                            emit_global_dictionary_write_role_guard(ops, view, miss);
                        }
                        emit_global_field_bank(ops, relocations, view, 8, 9, field, miss);
                        guard_end = ops.offset().0;
                        emit_cell_store(ops, relocations, view, field.byte_offset(), src, done)?;
                        hit_end = ops.offset().0;
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
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
    let cold = ops.offset().0;
    dynasm!(ops ; .arch x64 ; =>miss);
    emit_committed_value2(
        ops,
        relocations,
        transitions,
        abi::STUB_JIT_BINDING_VALUE,
        result,
        value0,
        value1,
        throw_value,
        fatal,
    );
    let cold_end = ops.offset().0;
    dynasm!(ops ; .arch x64 ; =>done);
    let join = ops.offset().0;
    record_region(
        &mut code_map,
        "templateBindingCold",
        cold,
        cold_end,
        byte_pc,
    );
    record_region(&mut code_map, "templateBindingJoin", join, join, byte_pc);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{
        VM_THREAD_ACTIVE_REALM_CELL_OFFSET, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
    };

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    #[test]
    fn native_global_guard_uses_the_live_epoch_layout_and_dictionary_owner() {
        const OBJECT: u32 = 64;
        const SHAPE: u32 = 128;
        const EXOTIC: u32 = 192;
        const LAYOUT: u32 = 69;
        let mut cage = vec![0_u8; 256].into_boxed_slice();
        let mut thread = vec![
            0_u8;
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET.max(VM_THREAD_ACTIVE_REALM_CELL_OFFSET)
                as usize
                + 8
        ]
        .into_boxed_slice();
        let mut ctx = vec![0_u8; THREAD_OFFSET.max(GLOBAL_THIS_OFFSET_PTR_OFFSET) as usize + 8]
            .into_boxed_slice();
        let global = OBJECT;
        let mut epoch = 0_u64;
        write_u64(&mut ctx, THREAD_OFFSET as usize, thread.as_ptr() as u64);
        write_u64(
            &mut ctx,
            GLOBAL_THIS_OFFSET_PTR_OFFSET as usize,
            (&global as *const u32) as u64,
        );
        write_u64(
            &mut thread,
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
            (&mut epoch as *mut u64) as u64,
        );
        let mut view = JitCompileSnapshot::without_feedback(7, 0, 1, vec![]);
        view.cage_base = cage.as_ptr() as usize;
        view.object_shape_byte = 8;
        view.object_exotic_handle_byte = 16;
        view.shape_state_byte = 8;
        view.exotic_dictionary_layout_byte = 8;
        let source_realm = view.literal_allocations.realm_id;
        write_u64(
            &mut thread,
            VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
            (&source_realm as *const u32) as u64,
        );
        for dictionary in [false, true] {
            for write in [false, true] {
                for expected_epoch in [0_u64, 1, 0xdead_beef_89ab_cdef, u64::MAX] {
                    let mut ops = Assembler::new().unwrap();
                    let mut relocations = RelocationCapture::new(false);
                    let entry = ops.offset();
                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    dynasm!(ops ; .arch x64 ; push r15 ; mov r15, rdi);
                    emit_global_object_guard(
                        &mut ops,
                        &mut relocations,
                        &view,
                        u64::from(if dictionary { LAYOUT } else { SHAPE }),
                        dictionary,
                        expected_epoch,
                        [8, 9],
                        miss,
                    );
                    if dictionary && write {
                        emit_global_dictionary_write_role_guard(&mut ops, &view, miss);
                    }
                    dynasm!(ops ; .arch x64
                        ; mov eax, 1
                        ; jmp =>done
                        ; =>miss
                        ; xor eax, eax
                        ; =>done
                        ; pop r15
                        ; ret
                    );
                    let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
                    // SAFETY: this private System V fixture reads only the owned
                    // stable byte arenas and scalar pointers written above. It
                    // preserves r15, has no call/GC boundary, and returns u32.
                    let run: extern "sysv64" fn(*const u8) -> u32 =
                        unsafe { std::mem::transmute(code.entry_ptr()) };
                    let cases = if dictionary { 6 } else { 4 };
                    for case in 0..cases {
                        write_u32(&mut cage, OBJECT as usize + 8, SHAPE);
                        write_u32(&mut cage, OBJECT as usize + 16, EXOTIC);
                        cage[SHAPE as usize + view.shape_state_byte as usize] = if dictionary {
                            ShapeState::DICTIONARY_MASK
                        } else {
                            ShapeState::ORDINARY.bits()
                        };
                        write_u32(&mut cage, EXOTIC as usize + 8, LAYOUT);
                        epoch = expected_epoch;
                        write_u64(
                            &mut thread,
                            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
                            (&mut epoch as *mut u64) as u64,
                        );
                        match case {
                            0 => {}
                            1 => epoch = expected_epoch ^ 1,
                            2 => write_u64(
                                &mut thread,
                                VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
                                0,
                            ),
                            3 if dictionary => cage[SHAPE as usize + 8] = 0,
                            3 => write_u32(&mut cage, OBJECT as usize + 8, SHAPE + 1),
                            4 => write_u32(&mut cage, OBJECT as usize + 16, 0),
                            5 => write_u32(&mut cage, EXOTIC as usize + 8, LAYOUT + 1),
                            _ => unreachable!(),
                        }
                        assert_eq!(
                            run(ctx.as_ptr()),
                            u32::from(case == 0),
                            "dictionary={dictionary}, epoch={expected_epoch}, case={case}"
                        );
                        assert_eq!(
                            epoch,
                            if case == 1 {
                                expected_epoch ^ 1
                            } else {
                                expected_epoch
                            },
                            "the generated proof cannot write its scalar owner"
                        );
                    }
                    if dictionary {
                        // A dictionary layout token does not fix its shape state;
                        // every possible state byte exercises the actual kind test.
                        write_u32(&mut cage, OBJECT as usize + 8, SHAPE);
                        write_u32(&mut cage, OBJECT as usize + 16, EXOTIC);
                        write_u32(&mut cage, EXOTIC as usize + 8, LAYOUT);
                        epoch = expected_epoch;
                        write_u64(
                            &mut thread,
                            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
                            (&mut epoch as *mut u64) as u64,
                        );
                        for state in u8::MIN..=u8::MAX {
                            cage[SHAPE as usize + view.shape_state_byte as usize] = state;
                            let expected = state & ShapeState::DICTIONARY_MASK != 0
                                && state
                                    & (ShapeState::OPAQUE_LOOKUP_MASK
                                        | ShapeState::PROVISIONAL_MASK)
                                    == 0
                                && (!write || state & ShapeState::PROTOTYPE_MASK == 0);
                            assert_eq!(
                                run(ctx.as_ptr()),
                                u32::from(expected),
                                "dictionary state, write={write}, epoch={expected_epoch}, state={state:#x}"
                            );
                            assert_eq!(
                                cage[SHAPE as usize + view.shape_state_byte as usize],
                                state,
                                "the generated proof cannot mutate shape state"
                            );
                        }
                    }
                }
            }
        }
    }
}

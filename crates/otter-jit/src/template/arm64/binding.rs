//! Schema-owned binding and global-declaration emission for Template AArch64.
//!
//! # Contents
//! - Direct stable-cell, captured-cell, and guarded global-object hits.
//! - One fixed boxed-value committed cold boundary per semantic family.
//! - Family-level guard/hit/cold/join artifact regions.
//!
//! # Invariants
//! - The published function/PC and opcode schema own names, indices, flags,
//!   and missing-binding policy; generated calls pass only boxed values.
//! - A generated write runs the canonical write barrier for a cell value.
//! - Every guard miss enters the committed cold sibling. It never deoptimizes
//!   or replays an accessor, proxy, TDZ, const, eval-chain, or unresolved case.
//! - Cold JavaScript failure carries its pure exception value to the shared
//!   Template throw router; the binding ABI has no side-exit result.
//!
//! # See also
//! - `otter_bytecode::opcode_schema::BindingSemantics` — semantic authority.
//! - `crate::entry::runtime_ops::reentry` — fixed committed entry functions.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::opcode_schema::{
    BindingRead, BindingSemantics, BindingWrite, BindingWriteCheck, GlobalDeclarationSemantics,
};
use otter_vm::{JitCompileSnapshot, jit::BindingHitProof, native_abi as abi};

use super::transitions::TransitionTable;
use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64,
    emit_load_u64, emit_slab_base, emit_store_reg, emit_write_barrier,
};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::artifact::{CodeMapCapture, CodeRegion};
use crate::entry::{
    GLOBAL_THIS_OFFSET_PTR_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET, THREAD_OFFSET, Unsupported,
    VALUE_HOLE, VALUE_TRUE, VALUE_UNDEFINED, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
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

fn instruction_imm32(view: &JitCompileSnapshot, byte_pc: u32, operand: u8) -> Option<i32> {
    view.instructions
        .iter()
        .find(|instruction| instruction.byte_pc == byte_pc)
        .and_then(|instruction| instruction.imm32(&view.code_block, usize::from(operand)))
}

fn emit_global_object_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    shape: u64,
    dictionary: bool,
    global_lexical_epoch: u64,
    miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x14, [x20, THREAD_OFFSET]
        ; ldr x14, [x14, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET]
        ; cbz x14, =>miss
        ; ldr x15, [x14]
    );
    emit_load_u64(ops, 11, global_lexical_epoch);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x15, x11
        ; b.ne =>miss
        ; ldr x14, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET]
        ; ldr w12, [x14]
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        14,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x14, x12
        ; ldr w14, [x13, view.object_shape_byte]
    );
    if dictionary {
        dynasm!(ops
            ; .arch aarch64
            ; cbnz w14, =>miss
            ; ldr x14, [x13, view.object_dictionary_shape_id_byte]
        );
        emit_load_u64(ops, 11, shape);
        dynasm!(ops ; .arch aarch64 ; cmp x14, x11 ; b.ne =>miss);
    } else {
        emit_load_u64(ops, 11, shape);
        dynasm!(ops ; .arch aarch64 ; cmp w14, w11 ; b.ne =>miss);
    }
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
    let primitive = ops.new_dynamic_label();
    emit_load_reg(ops, 9, source)?;
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, primitive);
    dynasm!(ops ; .arch aarch64 ; str x9, [X(address), byte_offset]);
    emit_write_barrier(ops, relocations, view, parent, 9);
    dynasm!(ops
        ; .arch aarch64
        ; b =>done
        ; =>primitive
        ; str x9, [X(address), byte_offset]
        ; b =>done
    );
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
    let mut cold_reachable = true;

    match semantics {
        BindingSemantics::Read(BindingRead::GlobalThis { .. }) => {
            let dst = result.ok_or(Unsupported::OperandShape("globalThis read result"))?;
            cold_reachable = false;
            guard_end = ops.offset().0;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET]
                ; ldr w9, [x14]
            );
            emit_store_reg(ops, 9, dst)?;
            dynasm!(ops ; .arch aarch64 ; b =>done);
            hit_end = ops.offset().0;
        }
        BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Exists { .. }) => {
            let dst = result.ok_or(Unsupported::OperandShape("binding read result"))?;
            if let Some(proof) = view.binding_hit_proofs.get(&byte_pc).copied() {
                match proof {
                    BindingHitProof::GlobalLexical { cell_offset, .. } => {
                        if view.cage_base != 0
                            && let Some(cell_addr) =
                                view.cage_base.checked_add(cell_offset as usize)
                        {
                            emit_load_symbol_u64(
                                ops,
                                relocations,
                                13,
                                cell_addr as u64,
                                RelocationTarget::GlobalLexicalCell {
                                    function_id: view.code_block.id,
                                    byte_pc,
                                },
                            );
                            if matches!(
                                semantics,
                                BindingSemantics::Read(BindingRead::Global { .. })
                            ) {
                                dynasm!(ops
                                    ; .arch aarch64
                                    ; ldr x9, [x13, view.upvalue_value_byte]
                                );
                                emit_load_u64(ops, 11, VALUE_HOLE);
                                dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
                                guard_end = ops.offset().0;
                                emit_store_reg(ops, 9, dst)?;
                            } else {
                                cold_reachable = false;
                                guard_end = ops.offset().0;
                                emit_load_u64(ops, 9, VALUE_TRUE);
                                emit_store_reg(ops, 9, dst)?;
                            }
                            dynasm!(ops ; .arch aarch64 ; b =>done);
                            hit_end = ops.offset().0;
                        }
                    }
                    BindingHitProof::GlobalObject {
                        shape,
                        dictionary,
                        value_byte,
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
                            miss,
                        );
                        if matches!(
                            semantics,
                            BindingSemantics::Read(BindingRead::Global { .. })
                        ) {
                            emit_slab_base(ops, view, 13, 14);
                            dynasm!(ops ; .arch aarch64 ; cbz x13, =>miss);
                            guard_end = ops.offset().0;
                            dynasm!(ops ; .arch aarch64 ; ldr x9, [x13, value_byte]);
                        } else {
                            guard_end = ops.offset().0;
                            emit_load_u64(ops, 9, VALUE_TRUE);
                        }
                        emit_store_reg(ops, 9, dst)?;
                        dynasm!(ops ; .arch aarch64 ; b =>done);
                        hit_end = ops.offset().0;
                    }
                    _ => {}
                }
            }
        }
        BindingSemantics::Read(BindingRead::Upvalue { index, .. }) => {
            let dst = result.ok_or(Unsupported::OperandShape("upvalue read result"))?;
            if let Some(index) = instruction_imm32(view, byte_pc, index)
                && index >= 0
                && view.cage_base != 0
            {
                let spine_offset = (index as u32) * 4;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x9, [x21, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
                    ; cbz x9, =>miss
                    ; ldr w9, [x9, spine_offset]
                    ; cbz w9, =>miss
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
                    ; add x13, x13, x9
                    ; ldr x9, [x13, view.upvalue_value_byte]
                );
                emit_load_u64(ops, 11, VALUE_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
                guard_end = ops.offset().0;
                emit_store_reg(ops, 9, dst)?;
                dynasm!(ops ; .arch aarch64 ; b =>done);
                hit_end = ops.offset().0;
            }
        }
        BindingSemantics::Write(BindingWrite::Upvalue { index, check, .. }) => {
            let source = value0.ok_or(Unsupported::OperandShape("upvalue write value"))?;
            if let Some(index) = instruction_imm32(view, byte_pc, index)
                && index >= 0
                && view.cage_base != 0
            {
                let spine_offset = (index as u32) * 4;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x9, [x21, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
                    ; cbz x9, =>miss
                    ; ldr w9, [x9, spine_offset]
                    ; cbz w9, =>miss
                );
                emit_load_symbol_u64(
                    ops,
                    relocations,
                    13,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops ; .arch aarch64 ; add x13, x13, x9);
                if check == BindingWriteCheck::Checked {
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x13, view.upvalue_value_byte]);
                    emit_load_u64(ops, 11, VALUE_HOLE);
                    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
                }
                guard_end = ops.offset().0;
                emit_cell_store(
                    ops,
                    relocations,
                    view,
                    13,
                    13,
                    view.upvalue_value_byte,
                    source,
                    done,
                )?;
                hit_end = ops.offset().0;
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
                        if view.cage_base != 0
                            && let Some(cell_addr) =
                                view.cage_base.checked_add(cell_offset as usize)
                        {
                            emit_load_symbol_u64(
                                ops,
                                relocations,
                                13,
                                cell_addr as u64,
                                RelocationTarget::GlobalLexicalCell {
                                    function_id: view.code_block.id,
                                    byte_pc,
                                },
                            );
                            dynasm!(ops ; .arch aarch64 ; ldr x9, [x13, view.upvalue_value_byte]);
                            emit_load_u64(ops, 11, VALUE_HOLE);
                            dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
                            guard_end = ops.offset().0;
                            emit_cell_store(
                                ops,
                                relocations,
                                view,
                                13,
                                13,
                                view.upvalue_value_byte,
                                source,
                                done,
                            )?;
                            hit_end = ops.offset().0;
                        }
                    }
                    BindingHitProof::GlobalObject {
                        shape,
                        dictionary,
                        value_byte,
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
                            miss,
                        );
                        dynasm!(ops ; .arch aarch64 ; mov x12, x13);
                        emit_slab_base(ops, view, 13, 14);
                        dynasm!(ops ; .arch aarch64 ; cbz x13, =>miss);
                        guard_end = ops.offset().0;
                        emit_cell_store(ops, relocations, view, 12, 13, value_byte, source, done)?;
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

    if !cold_reachable {
        let cold = ops.offset().0;
        dynasm!(ops ; .arch aarch64 ; =>done);
        let join = ops.offset().0;
        record_region(&mut code_map, "templateBindingCold", cold, cold, byte_pc);
        record_region(&mut code_map, "templateBindingJoin", join, join, byte_pc);
        return Ok(());
    }

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

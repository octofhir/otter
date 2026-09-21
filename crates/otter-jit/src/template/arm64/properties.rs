//! AArch64 named-property IC probes for the template compiler.
//!
//! # Contents
//! - Inline guarded own-data loads/stores from immutable CacheIR snapshots
//!   cells, with the Array exotic `length` fast path.
//! - Fixed-value misses that resolve full load/`[[Set]]` semantics and
//!   self-patch cacheable sites.
//!
//! # Invariants
//! - Inline sequences neither allocate nor call, so they carry no safepoint;
//!   the receiver pointer is recomputed from the rooted frame slot on every
//!   access and never survives one.
//! - The slab base derives from the fresh header (inline slab) or the stable
//!   out-of-line `values_ptr` — never a cached body pointer that the moving
//!   collector could dangle.
//! - Pointer-valued stores run the generational write barrier; primitive
//!   stores skip it. Every slot stores the complete runtime `Value` word.
//! - The active frame already publishes and traces the complete register
//!   window. Misses pass boxed values directly, so setters, proxies,
//!   exceptions, reentry, and moving GC complete without replay.
//! - Cage bases, IC cells, and transition entries carry semantic relocation
//!   identities; IC ordinals are assigned before emission in ownership order.
//!
//! # See also
//! - [`super::values`] — slot compression/decompression primitives.
//! - `crates/otter-jit/src/entry/runtime_ops/vm_ops.rs` — fixed-value
//!   transitions and the authoritative cell layout.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::ic_probe;
use super::transitions::TransitionTable;
use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64,
    emit_store_reg, emit_write_barrier,
};
use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget};
use crate::entry::{Unsupported, reg_offset};

/// Emit `dst = obj.name` from transpiled CacheIR and one committed cold edge.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_load_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    object: u16,
    byte_pc: u32,
    _name: u32,
    _site: u64,
    array_length: bool,
    cell_addr: usize,
    cell_ordinal: u32,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let cage_base = view.cage_base;
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    if cage_base != 0 && array_length {
        let obj_off = reg_offset(object)?;
        let dst_off = reg_offset(dst)?;
        let have_length = ops.new_dynamic_label();
        let not_length = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, obj_off]);
        ic_probe::emit_exotic_length_fast(ops, relocations, view, have_length, not_length);
        dynasm!(ops
            ; .arch aarch64
            ; =>have_length
            ; str x9, [x19, dst_off]
            ; b =>done
            ; =>not_length
        );
    }

    // Inline guarded own-data load through the self-patching cell: guard tag
    // + GC type tag + cell shape, then read the value slab slot at the cell's
    // byte offset. Shape `0` is the empty-cell sentinel, so live shape-0
    // receivers deliberately miss to the transition.
    if cage_base != 0 {
        let obj_off = reg_offset(object)?;
        let dst_off = reg_offset(dst)?;
        ic_probe::emit_property_ic_load(
            ops,
            relocations,
            view,
            programs,
            byte_pc,
            |ops, register| {
                dynasm!(ops ; .arch aarch64 ; ldr X(register), [x19, obj_off]);
                Ok(())
            },
            cell_addr,
            cell_ordinal,
            miss,
        )?;
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [x19, dst_off]
            ; b =>done
        );
    }

    // Miss / no cage base: pass the boxed receiver directly. The published
    // native frame supplies function/PC/name/site identity, and the stable IC
    // pointer lets the canonical completion patch this code object's probe.
    dynasm!(ops
        ; .arch aarch64
        ; =>miss
        ; mov x0, x20
    );
    emit_load_reg(ops, 1, object)?;
    emit_load_symbol_u64(
        ops,
        relocations,
        2,
        cell_addr as u64,
        RelocationTarget::PropertySourceCell {
            access: PropertySourceAccess::Load,
            ordinal: cell_ordinal,
        },
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_LOAD_PROPERTY),
        abi::STUB_JIT_LOAD_PROPERTY,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x15, x1
        ; cbz x15, >property_load_completed
        ; cmp x15, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; property_load_completed:
    );
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit `obj.name = value` from transpiled CacheIR and one committed cold edge.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    object: u16,
    _name: u32,
    value: u16,
    _site: u64,
    cell_addr: usize,
    cell_ordinal: u32,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let cage_base = view.cage_base;
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    // Inline guarded existing-own-data store through the self-patching cell,
    // then a value-tag-gated write barrier (primitive stores skip it).
    if cage_base != 0 {
        let obj_off = reg_offset(object)?;
        let src_off = reg_offset(value)?;
        ic_probe::emit_property_ic_store_guard(
            ops,
            relocations,
            view,
            programs,
            |ops, register| {
                dynasm!(ops ; .arch aarch64 ; ldr X(register), [x19, obj_off]);
                Ok(())
            },
            cell_addr,
            cell_ordinal,
            miss,
        )?;
        let store_prim = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, src_off]);
        emit_cell_test(ops, 9, 11, CellTest::IsNotCell, store_prim);
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [x13, x17]
        );
        ic_probe::emit_property_transition_shape_barrier(ops, relocations, view, 20);
        emit_write_barrier(ops, relocations, view, 12, 9);
        dynasm!(ops
            ; .arch aarch64
            ; b =>done
            ; =>store_prim
            ; str x9, [x13, x17]
        );
        ic_probe::emit_property_transition_shape_barrier(ops, relocations, view, 20);
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }

    // Miss / no cage base: receiver and value are read from the published,
    // traced window immediately before the fixed-value call.
    dynasm!(ops
        ; .arch aarch64
        ; =>miss
        ; mov x0, x20
    );
    emit_load_reg(ops, 1, object)?;
    emit_load_reg(ops, 2, value)?;
    emit_load_symbol_u64(
        ops,
        relocations,
        3,
        cell_addr as u64,
        RelocationTarget::PropertySourceCell {
            access: PropertySourceAccess::Store,
            ordinal: cell_ordinal,
        },
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_STORE_PROPERTY),
        abi::STUB_JIT_STORE_PROPERTY,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x15, x1
        ; cbz x15, =>done
        ; cmp x15, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; =>done
    );
    Ok(())
}

//! AArch64 named-property IC probes for the template compiler.
//!
//! # Contents
//! - One immutable monomorphic CacheIR program inline, then a `bl` to the code
//!   object's shared property subroutine ([`super::shared_property`]): the
//!   live shared property-action probe and the committed runtime miss.
//! - Exotic length reads and existing shape/value barriers after committed stores.
//! - Source-owned cold entries complete full load/`[[Set]]` semantics once.
//!
//! # Invariants
//! - Inline sequences neither allocate nor call, so they carry no safepoint;
//!   the receiver pointer is recomputed from the rooted frame slot on every
//!   access and never survives one.
//! - The slot base derives from the fresh header (in-object slots) or the
//!   out-of-line slab handle — never a cached body pointer that the moving
//!   collector could dangle.
//! - Pointer-valued stores run the generational write barrier; primitive
//!   stores skip it. Every slot stores the complete runtime `Value` word.
//! - The active frame already publishes and traces the complete register
//!   window. Misses pass boxed values directly, so setters, proxies,
//!   exceptions, reentry, and moving GC complete without replay.
//! - A failed PIC reloads inputs from the rooted window before the shared
//!   probe. Independent read/store facts select their own holder and slot.
//! - Every miss precedes effects. Published stores have only barrier/completion
//!   edges; their child shape and value are each barriered by the existing owner.
//! - Cache tables and source cells carry semantic relocation identities;
//!   source ordinals are assigned before emission in ownership order.
//!
//! # See also
//! - [`super::values`] — slot compression/decompression primitives.
//! - `crates/otter-jit/src/entry/runtime_ops/vm_ops.rs` — fixed-value
//!   transitions and the authoritative cell layout.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::ic_probe;
use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64,
    emit_store_reg, emit_write_barrier,
};
use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget};
use crate::entry::TransitionTable;
use crate::entry::{Unsupported, reg_offset};

/// Route the shared subroutine's `NativeResultPair` status in `x1`.
fn emit_shared_status(ops: &mut Assembler, throw_value: DynamicLabel, fatal: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; cbz x1, >completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; completed:
    );
}

/// Only a monomorphic site keeps its CacheIR program inline; every other
/// receiver is served by the shared action probe.
fn inline_programs(
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
) -> Option<&[otter_vm::JitCacheIrProgram]> {
    programs.filter(|programs| programs.len() == 1)
}

/// Emit `dst = obj.name` from CacheIR, the shared table and one committed miss.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_load_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    shared_probes: &mut super::shared_property::SharedPropertyProbes,
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
    let shared = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    if cage_base != 0 && array_length {
        let obj_off = reg_offset(object)?;
        let dst_off = reg_offset(dst)?;
        let have_length = ops.new_dynamic_label();
        let not_length = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, obj_off]);
        ic_probe::emit_exotic_length_fast(ops, view, have_length, not_length);
        dynasm!(ops
            ; .arch aarch64
            ; =>have_length
            ; str x9, [x19, dst_off]
            ; b =>done
            ; =>not_length
        );
    }

    // Immutable CacheIR proves the exact receiver/holder before the live
    // slot read. A miss remains pre-effect and reaches the shared probe.
    if cage_base != 0 {
        let obj_off = reg_offset(object)?;
        let dst_off = reg_offset(dst)?;
        ic_probe::emit_property_ic_load(
            ops,
            relocations,
            view,
            inline_programs(programs),
            byte_pc,
            |ops, register| {
                dynasm!(ops ; .arch aarch64 ; ldr X(register), [x19, obj_off]);
                Ok(())
            },
            cell_addr,
            cell_ordinal,
            shared,
        )?;
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [x19, dst_off]
            ; b =>done
        );
    }

    // The immutable PIC may miss for a peer shape recorded after this
    // generation was installed. Reload the actual receiver and call the
    // shared probe of the same current table the optimizing tier and VM use.
    dynasm!(ops ; .arch aarch64 ; =>shared);
    let access = view
        .property_accesses
        .get(&byte_pc)
        .filter(|_| cage_base != 0);
    if let Some(access) = access {
        use super::shared_property::{LOAD_ATOM, LOAD_ORDINAL, LOAD_RECEIVER};
        let subroutine = shared_probes.label(ops, false);
        emit_load_reg(ops, LOAD_RECEIVER, object)?;
        super::values::emit_load_u64(ops, LOAD_ATOM, u64::from(access.atom));
        super::values::emit_load_u64(ops, LOAD_ORDINAL, u64::from(cell_ordinal));
        dynasm!(ops ; .arch aarch64 ; bl =>subroutine);
        emit_shared_status(ops, throw_value, fatal);
        emit_store_reg(ops, 0, dst)?;
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    // No key / no cage base: pass the boxed receiver directly. The published
    // native frame and retained source cell supply function/PC/name/site
    // identity. Canonical completion publishes actions for subsequent probes.
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

/// Emit `obj.name = value` from CacheIR, the shared table and one committed miss.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    shared_probes: &mut super::shared_property::SharedPropertyProbes,
    view: &JitCompileSnapshot,
    object: u16,
    _name: u32,
    value: u16,
    byte_pc: u32,
    _site: u64,
    cell_addr: usize,
    cell_ordinal: u32,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let cage_base = view.cage_base;
    let shared = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    // Immutable CacheIR resolves a guarded own slot or append and commits
    // through the existing child/value barrier owners.
    if cage_base != 0 {
        let obj_off = reg_offset(object)?;
        let src_off = reg_offset(value)?;
        ic_probe::emit_property_ic_store_guard(
            ops,
            relocations,
            view,
            inline_programs(programs),
            |ops, register| {
                dynasm!(ops ; .arch aarch64 ; ldr X(register), [x19, obj_off]);
                Ok(())
            },
            cell_addr,
            cell_ordinal,
            shared,
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

    // The PIC miss is still pre-effect. Its scratch can be discarded;
    // the published window supplies collector-current inputs to the shared
    // action probe. It preserves x12/x9 and returns the child in w11.
    dynasm!(ops ; .arch aarch64 ; =>shared);
    let access = view
        .property_accesses
        .get(&byte_pc)
        .filter(|_| cage_base != 0);
    if let Some(access) = access {
        use super::shared_property::{STORE_ATOM, STORE_ORDINAL, STORE_RECEIVER, STORE_VALUE};
        let subroutine = shared_probes.label(ops, true);
        emit_load_reg(ops, STORE_RECEIVER, object)?;
        emit_load_reg(ops, STORE_VALUE, value)?;
        super::values::emit_load_u64(ops, STORE_ATOM, u64::from(access.atom));
        super::values::emit_load_u64(ops, STORE_ORDINAL, u64::from(cell_ordinal));
        dynasm!(ops ; .arch aarch64 ; bl =>subroutine);
        emit_shared_status(ops, throw_value, fatal);
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    // No key / no cage base: receiver and value are read from the published,
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

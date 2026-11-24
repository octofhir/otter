//! x86-64 guards for generational and incremental-marking edges.
//!
//! # Contents
//! - `emit_young_edge` resolves old edges and falls through for nursery edges.
//!
//! # Invariants
//! - The child is a live, non-null cell and the store has already committed.
//! - Only an old white child can need insertion during marking; no collection
//!   or change of marking phase can occur between the store and this guard.
//!
//! # See also
//! - `otter_gc::barrier` owns the actual remembered set and insertion effects.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::jit::JitGcBarrierLayout;

use crate::entry::{THREAD_OFFSET, VM_THREAD_MARKING_FLAG_CELL_OFFSET};

/// Fall through for a young child. Old marked children go to `done`, as do
/// old white children outside a marking cycle. Clobbers `r10`.
pub(crate) fn emit_young_edge(
    ops: &mut Assembler,
    layout: JitGcBarrierLayout,
    child: u8,
    [done, slow]: [DynamicLabel; 2],
) {
    debug_assert_ne!(child, 10);
    let young = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; movzx r10d, BYTE [Rq(child) + layout.header_flags_byte as i32]
        ; test r10b, layout.young_flag as i8 ; jnz =>young
        ; test r10b, layout.mark_color_bits as i8 ; jnz =>done
        ; mov r10, [r15 + THREAD_OFFSET as i32]
        ; mov r10, [r10 + VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
        ; cmp BYTE [r10], 0 ; jne =>slow
        ; jmp =>done
        ; =>young
    );
}

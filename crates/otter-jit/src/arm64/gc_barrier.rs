//! AArch64 guards for generational and incremental-marking edges.
//!
//! # Contents
//! - `emit_young_edge` leaves nursery edges to the caller's dirty/remembered
//!   bookkeeping and resolves every old edge.
//!
//! # Invariants
//! - The child is a live, non-null cell. The store has already committed.
//! - Old cells never require a remembered-parent entry. An old white cell
//!   requires insertion only while marking runs. No collection or change of
//!   marking phase can occur between the store and this guard.
//! - The concurrent marker can only shade an old child, so a change to the
//!   observed color can at worst send an already shaded cell to the barrier.
//!
//! # See also
//! - `otter_gc::barrier` owns the actual remembered set and insertion effects.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::JitGcBarrierLayout;

use crate::entry::{THREAD_OFFSET, VM_THREAD_MARKING_FLAG_CELL_OFFSET};

/// Fall through for a young child. Old marked children go to `done`, as do
/// old white children outside a marking cycle. Clobbers only `scratch`.
pub(crate) fn emit_young_edge(
    ops: &mut Assembler,
    layout: JitGcBarrierLayout,
    [context, child, scratch]: [u8; 3],
    [done, slow]: [DynamicLabel; 2],
) {
    debug_assert_ne!(scratch, child);
    debug_assert_ne!(scratch, context);
    let young = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldrb W(scratch), [X(child), layout.header_flags_byte]
        ; tbnz W(scratch), layout.young_flag.trailing_zeros(), =>young
        ; tst W(scratch), layout.mark_color_bits
        ; b.ne =>done
        ; ldr X(scratch), [X(context), THREAD_OFFSET]
        ; ldr X(scratch), [X(scratch), VM_THREAD_MARKING_FLAG_CELL_OFFSET]
        ; ldrb W(scratch), [X(scratch)]
        ; cbnz W(scratch), =>slow
        ; b =>done
        ; =>young
    );
}

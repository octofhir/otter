//! No-call proof for `value instanceof target` over an ordinary closure.
//!
//! # Contents
//! - Target proof: an ordinary closure (plain kind, no `[[Prototype]]`
//!   override) whose own bag carries no symbol properties, so
//!   `@@hasInstance` resolves to the non-writable, non-configurable
//!   `%Function.prototype%[@@hasInstance]`, i.e. OrdinaryHasInstance.
//! - `target.prototype` read from the closure's rare-record slot, the same
//!   slot generated construction reads.
//! - A bounded walk of an ordinary object's `[[Prototype]]` chain; primitive
//!   values answer `false`.
//!
//! # Invariants
//! - No allocation, VM transition, deopt or user code occurs inside the probe.
//!   Proxies, opaque chain links, exotic receivers, an unallocated prototype
//!   and chains longer than [`MAX_CHAIN`] miss into the committed operation.
//! - Both inputs are copied into the reserved `x15`/`x16` before any scratch
//!   register is written; outputs are written last.
//!
//! # See also
//! - `machine::committed_probe` — cold call, catch edge and SSA result join.
//! - `otter_vm::closure::CLOSURE_LOOKUP_ORDINARY` — the named-lookup byte.

use super::emit_load_symbolic_u64;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::OBJECT_BODY_TYPE_TAG;
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};
use dynasmrt::aarch64::Assembler;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{JitCompileSnapshot, Value};

/// Bit index of the chain-link opacity flag for `tbnz`.
const CHAIN_LINK_OPAQUE_BIT: u32 =
    otter_vm::jit::JIT_OBJECT_FLAG_CHAIN_LINK_OPAQUE.trailing_zeros();

/// Prototype links the probe follows before handing the walk to the VM.
pub(super) const MAX_CHAIN: u32 = 32;

pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    inputs: [u8; 2],
    outputs: [u8; 2],
) {
    let [value, target] = inputs;
    let [result, hit] = outputs;
    let yes = ops.new_dynamic_label();
    let no = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let walk = ops.new_dynamic_label();
    let step = ops.new_dynamic_label();
    // x15/x16 lie outside the allocation file, so neither input can be there.
    dynasm!(ops ; .arch aarch64 ; mov x15, X(value) ; mov x16, X(target));
    if view.cage_base == 0 || view.closure_call_layout.prototype_byte == 0 {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
    } else {
        let layout = view.closure_call_layout;
        let named_lookup = otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE;
        emit_load_symbolic_u64(
            ops,
            relocations,
            11,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        // Target: an ordinary closure; a bag may exist but no override.
        emit_cell_test(ops, 16, 9, CellTest::IsNotCell, miss);
        dynasm!(ops ; .arch aarch64
            ; cbz x16, =>miss
            ; mov w9, w16 ; add x16, x11, x9
            ; ldrb w9, [x16]
            ; cmp w9, u32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG)
            ; b.ne =>miss
            ; ldrb w9, [x16, named_lookup]
            ; and w9, w9, !u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
            ; cmp w9, u32::from(otter_vm::closure::CLOSURE_LOOKUP_ORDINARY)
            ; b.ne =>miss
            // `prototype` lives in the rare record's slot (`x16` from here);
            // without a record, or while the slot holds the hole, the
            // committed operation allocates the default object.
            ; ldr w13, [x16, layout.rare_byte]
            ; cbz w13, =>miss
            ; add x16, x11, x13
            // Symbol-keyed own properties live in the bag's exotic sidecar
            // table; none may exist, so the function cannot own
            // `@@hasInstance`.
            ; ldr w10, [x16, layout.own_props_byte]
            ; cbz w10, >symbols_absent
            ; add x10, x11, x10
            ; ldr w9, [x10, view.object_exotic_handle_byte]
            ; cbz w9, >symbols_absent
            ; add x9, x11, x9
            ; ldr w9, [x9, otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE]
            ; cbnz w9, =>miss
            ; symbols_absent:
            ; ldr x12, [x16, layout.prototype_byte]
        );
        // The prototype must be an ordinary object; anything else throws.
        emit_cell_test(ops, 12, 9, CellTest::IsNotCell, miss);
        dynasm!(ops ; .arch aarch64
            ; cbz x12, =>miss
            ; mov w9, w12 ; add x12, x11, x9
            ; ldrb w9, [x12]
            ; cmp w9, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
        );
        // Value: a non-cell answers false; a primitive cell answers false; an
        // ordinary object is walked; every other cell misses.
        emit_cell_test(ops, 15, 9, CellTest::IsNotCell, no);
        dynasm!(ops ; .arch aarch64
            ; cbz x15, =>miss
            ; mov w9, w15 ; add x13, x11, x9
            ; ldrb w9, [x13]
            ; cmp w9, OBJECT_BODY_TYPE_TAG
            ; b.eq =>walk
        );
        for primitive_tag in view.primitive_cell_type_tags {
            dynasm!(ops ; .arch aarch64 ; cmp w9, u32::from(primitive_tag) ; b.eq =>no);
        }
        dynasm!(ops ; .arch aarch64
            ; b =>miss
            ; =>walk
            ; movz w14, MAX_CHAIN
            ; =>step
            ; ldrb w9, [x13, view.object_flags_byte]
            ; tbnz w9, CHAIN_LINK_OPAQUE_BIT, =>miss
        );
        crate::template::arm64::values::emit_load_prototype(ops, view, 9, 13, 11);
        dynasm!(ops ; .arch aarch64
            ; cbz w9, =>no
            ; add x13, x11, x9
            ; cmp x13, x12
            ; b.eq =>yes
            ; ldrb w9, [x13]
            ; cmp w9, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; subs w14, w14, #1
            ; b.ne =>step
            ; b =>miss
        );
    }
    dynasm!(ops ; .arch aarch64 ; =>yes);
    emit_load_u64(ops, result, Value::boolean(true).to_bits());
    dynasm!(ops ; .arch aarch64 ; mov W(hit), #1 ; b =>done ; =>no);
    emit_load_u64(ops, result, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch aarch64 ; mov W(hit), #1 ; b =>done ; =>miss);
    emit_load_u64(ops, result, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch aarch64 ; mov W(hit), wzr ; =>done);
}

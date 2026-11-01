//! `OrdinaryHasInstance` (§7.3.22) in AArch64 generated code, shared by the
//! baseline and optimizing tiers.
//!
//! # Contents
//! - [`emit_routine`] — the baseline tier's one shared `instanceof` routine.
//! - [`emit_ordinary_has_instance`] — an ordinary closure target without an
//!   own `@@hasInstance`, its `prototype` an ordinary object, and the value's
//!   prototype chain walked up to [`INSTANCEOF_CHAIN_BOUND`] links.
//!
//! # Invariants
//! - Every outcome is decided before any effect but the site's `instanceof`
//!   cell: a caching write of the proved target and prototype, with the
//!   target's rare-record flag that asks the VM to empty the cell before the
//!   closure changes.
//! - A non-cell or primitive value answers `false`; an opaque link, an exotic
//!   link, a long chain or any other target branches to `miss`, whose caller
//!   completes the operation in the runtime.
//! - Writes only the five temporaries, `x16` and `x17`; `value` and `target`
//!   survive.
//!
//! # See also
//! - `otter_vm::closure` owns the closure layout and named-lookup byte.
//! - `crate::graph::arm64` and `crate::template::arm64` call it.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::object::ShapeState;

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::graph::INSTANCEOF_CHAIN_BOUND;
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_symbol_u64};

/// Routine input: the value `instanceof` tests.
pub(crate) const ROUTINE_VALUE: u8 = 9;
/// Routine input: the target.
pub(crate) const ROUTINE_TARGET: u8 = 10;

/// The baseline `instanceof` routine: [`emit_ordinary_has_instance`] of
/// `X(ROUTINE_VALUE)` and `X(ROUTINE_TARGET)` without a site cell. Returns
/// `x17 == 0` with the boolean in `x16`, or `x17 != 0` for a case the caller
/// completes in the runtime. Writes `x11`–`x17`; both inputs survive.
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
        [11, 12, 13, 14, 15],
        [yes, no, miss],
    );
    dynasm!(ops ; .arch aarch64 ; =>yes);
    crate::template::arm64::values::emit_load_u64(ops, 16, crate::entry::VALUE_TRUE);
    dynasm!(ops ; .arch aarch64 ; mov x17, xzr ; ret ; =>no);
    crate::template::arm64::values::emit_load_u64(ops, 16, crate::entry::VALUE_FALSE);
    dynasm!(ops ; .arch aarch64 ; mov x17, xzr ; ret ; =>miss ; movz x17, 1 ; ret);
    crate::routines::RoutineOffsets {
        entry,
        second: entry,
    }
}

/// Branch to `yes` when the value in `X(value)` is an instance of the
/// target in `X(target)` by `OrdinaryHasInstance`, to `no` when it is not,
/// and to `miss` for any case this walk does not decide. `cell` is the
/// site's `instanceof` cell, when it has one.
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
    let step = ops.new_dynamic_label();
    let not_object = ops.new_dynamic_label();
    let proved = ops.new_dynamic_label();
    let validate = ops.new_dynamic_label();
    let symbols_absent = ops.new_dynamic_label();
    if view.cage_base == 0 || layout.prototype_byte == 0 {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
    } else {
        if let Some((address, relocation)) = cell.clone() {
            // The target this site last proved, and its prototype.
            emit_load_symbol_u64(ops, relocations, 16, address, relocation);
            dynasm!(ops
                ; .arch aarch64
                ; ldp x17, X(prototype), [x16, otter_vm::jit::JIT_INSTANCEOF_CELL_TARGET_OFFSET as i32]
                ; cmp X(target), x17
                ; b.ne =>validate
                ; and XSP(cage), X(target), #0xffff_ffff_0000_0000
                ; b =>proved
                ; =>validate
            );
        }
        let named_lookup = otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE;
        // Target: an ordinary closure whose own properties hold no
        // `@@hasInstance` override. A cell value is its header's full
        // address, and its cage is the address's high half.
        emit_cell_test(ops, target, CellTest::IsNotCell, miss);
        dynasm!(ops
            ; .arch aarch64
            ; cbz X(target), =>miss
            ; and XSP(cage), X(target), #0xffff_ffff_0000_0000
            ; ldrb w16, [X(target)]
            ; cmp w16, u32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG)
            ; b.ne =>miss
            ; ldrb w16, [X(target), named_lookup]
            ; and w16, w16, !u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
            ; cmp w16, u32::from(otter_vm::closure::CLOSURE_LOOKUP_ORDINARY)
            ; b.ne =>miss
            // `prototype` lives in the rare record; without one, or
            // while it holds the hole, the runtime allocates it. Any
            // value but an ordinary object completes in the runtime.
            ; ldr W(rare), [X(target), layout.rare_byte]
            ; cbz W(rare), =>miss
            ; add X(rare), X(cage), X(rare)
            ; ldrb w16, [X(rare), layout.prototype_ordinary_byte]
            ; cbz w16, =>miss
            ; ldr X(prototype), [X(rare), layout.prototype_byte]
            ; ldr w16, [X(rare), layout.own_props_byte]
            ; cbz w16, =>symbols_absent
            ; add x16, X(cage), x16
            // A symbol key in the bag's hidden class or side table may
            // be `@@hasInstance`.
            ; ldr w17, [x16, view.object_shape_byte]
            ; add x17, X(cage), x17
            ; ldrb w17, [x17, otter_vm::object::SHAPE_SYMBOL_KEYS_BYTE]
            ; cbnz w17, =>miss
            ; ldr w16, [x16, view.object_exotic_handle_byte]
            ; cbz w16, =>proved
            ; add x16, X(cage), x16
            ; ldr w16, [x16, otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE]
            ; cbnz w16, =>miss
            ; b =>proved
            ; =>symbols_absent
        );
        if let Some((address, relocation)) = cell {
            // A closure without own properties is cached; the flag asks
            // the VM to empty the cell before the closure changes.
            emit_load_symbol_u64(ops, relocations, 16, address, relocation);
            dynasm!(ops
                ; .arch aarch64
                ; stp X(target), X(prototype), [x16, otter_vm::jit::JIT_INSTANCEOF_CELL_TARGET_OFFSET as i32]
                ; mov w16, #1
                ; strb w16, [X(rare), layout.instanceof_cached_byte]
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>proved);
        // Value: a non-cell answers false, a primitive cell answers
        // false, an ordinary object is walked, any other cell misses.
        emit_cell_test(ops, value, CellTest::IsNotCell, no);
        dynasm!(ops
            ; .arch aarch64
            ; cbz X(value), =>miss
            ; ldrb w16, [X(value)]
            ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
            ; b.ne =>not_object
            ; mov X(cursor), X(value)
            ; movz W(budget), INSTANCEOF_CHAIN_BOUND
            // One link: the cursor's shape fixes its prototype unless
            // its lookup is opaque.
            ; =>step
            ; ldr w16, [X(cursor), view.object_shape_byte]
            ; add x16, X(cage), x16
            ; ldrb w17, [x16, view.shape_state_byte]
            ; tst w17, u32::from(ShapeState::OPAQUE_LOOKUP_MASK)
            ; b.ne =>miss
            ; ldr w16, [x16, view.shape_prototype_byte]
            ; cbz w16, =>no
            ; add X(cursor), X(cage), x16
            ; cmp X(cursor), X(prototype)
            ; b.eq =>yes
            ; ldrb w16, [X(cursor)]
            ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; subs W(budget), WSP(budget), #1
            ; b.ne =>step
            ; b =>miss
            ; =>not_object
        );
        for primitive_tag in view.primitive_cell_type_tags {
            dynasm!(ops ; .arch aarch64 ; cmp w16, u32::from(primitive_tag) ; b.eq =>no);
        }
        dynasm!(ops ; .arch aarch64 ; b =>miss);
    }
}

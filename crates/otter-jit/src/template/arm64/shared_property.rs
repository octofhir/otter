//! Per-code-object shared named-property subroutines for baseline sites.
//!
//! # Contents
//! - [`SharedPropertyProbes`] names one load, one store and one method
//!   subroutine, requested by sites and emitted once after the body.
//! - The load and store subroutines run the isolate's shared property-action
//!   probe with the key in a register, then the committed runtime miss.
//! - The method subroutine runs the same load probe and returns only a
//!   callable hit; its sites resolve every miss through their own committed
//!   method resolution, which also records call feedback.
//!
//! # Invariants
//! - A site passes its receiver (and value) boxed from the rooted window plus
//!   its atom and source-cell ordinal in fixed registers, and calls with `bl`. The subroutine returns the canonical
//!   `NativeResultPair`: `x1 == 0` with the loaded value in `x0`, or a
//!   Throw/Fatal status the site routes through its own exits.
//! - Probes neither allocate nor collect; only the miss calls the runtime.
//! - Every input register except the receiver/value survives no call, matching
//!   the caller-saved contract of the inline miss these subroutines replace.
//!   The subroutine preserves `x19`–`x21` and returns with `sp` unchanged.
//!
//! # See also
//! - `crate::arm64::property_actions` owns the probe of the shared table.
//! - [`super::properties`] owns the per-site CacheIR fast path and routing.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::values::{CellTest, emit_cell_test, emit_load_runtime_stub, emit_load_symbol_u64};
use crate::arm64::property_actions::{self, AtomOperand};
use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget};

/// Load subroutine inputs.
pub(crate) const LOAD_RECEIVER: u8 = 9;
pub(crate) const LOAD_ATOM: u8 = 10;
pub(crate) const LOAD_ORDINAL: u8 = 11;

/// Method subroutine inputs: the load subroutine's receiver and atom.
pub(crate) const METHOD_RECEIVER: u8 = LOAD_RECEIVER;
pub(crate) const METHOD_ATOM: u8 = LOAD_ATOM;

/// Store subroutine inputs.
pub(crate) const STORE_RECEIVER: u8 = 12;
pub(crate) const STORE_VALUE: u8 = 9;
pub(crate) const STORE_ATOM: u8 = 3;
pub(crate) const STORE_ORDINAL: u8 = 4;

const CELL_BYTES: u32 = std::mem::size_of::<crate::entry::PropertySourceCell>() as u32;

/// The shared property subroutines one code object's sites requested.
#[derive(Default)]
pub(crate) struct SharedPropertyProbes {
    load: Option<DynamicLabel>,
    store: Option<DynamicLabel>,
    method: Option<DynamicLabel>,
}

impl SharedPropertyProbes {
    /// The subroutine serving store or load sites.
    pub(crate) fn label(&mut self, ops: &mut Assembler, store: bool) -> DynamicLabel {
        let slot = if store {
            &mut self.store
        } else {
            &mut self.load
        };
        *slot.get_or_insert_with(|| ops.new_dynamic_label())
    }

    /// The probe-only subroutine serving method-call sites.
    pub(crate) fn method_label(&mut self, ops: &mut Assembler) -> DynamicLabel {
        *self.method.get_or_insert_with(|| ops.new_dynamic_label())
    }

    /// Emit every requested subroutine once, after the body.
    pub(crate) fn emit(
        self,
        ops: &mut Assembler,
        relocations: &mut RelocationCapture,
        transitions: &crate::entry::TransitionTable,
        view: &JitCompileSnapshot,
        load_cells: usize,
        store_cells: usize,
    ) {
        if let Some(label) = self.load {
            emit_load(ops, relocations, transitions, view, label, load_cells);
        }
        if let Some(label) = self.store {
            emit_store(ops, relocations, transitions, view, label, store_cells);
        }
        if let Some(label) = self.method {
            emit_method(ops, relocations, view, label);
        }
    }
}

/// `X(destination) = cells + W(ordinal) * size`.
fn emit_cell_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    access: PropertySourceAccess,
    cells: usize,
    ordinal: u8,
    destination: u8,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        destination,
        cells as u64,
        RelocationTarget::PropertySourceCell { access, ordinal: 0 },
    );
    dynasm!(ops ; .arch aarch64
        ; movz w16, CELL_BYTES
        ; umaddl X(destination), W(ordinal), w16, X(destination));
}

fn emit_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
    cells: usize,
) {
    let miss = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    property_actions::emit_load(
        ops,
        relocations,
        view,
        AtomOperand::Register(LOAD_ATOM),
        LOAD_RECEIVER,
        [13, 14, 15, 3],
        0,
        miss,
    );
    dynasm!(ops ; .arch aarch64 ; mov x1, xzr ; ret ; =>miss
        ; stp x29, x30, [sp, #-16]!);
    emit_cell_address(
        ops,
        relocations,
        PropertySourceAccess::Load,
        cells,
        LOAD_ORDINAL,
        2,
    );
    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; mov x1, X(LOAD_RECEIVER));
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_JIT_LOAD_PROPERTY),
        abi::STUB_JIT_LOAD_PROPERTY,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; ldp x29, x30, [sp], #16 ; ret);
}

/// `x1 == 0` with a closure or native function in `x0`, else `x1 == 1`.
/// Leaf code: no call, no allocation, `x30` untouched.
fn emit_method(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let miss = ops.new_dynamic_label();
    let callable = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    property_actions::emit_load(
        ops,
        relocations,
        view,
        AtomOperand::Register(METHOD_ATOM),
        METHOD_RECEIVER,
        [13, 14, 15, 3],
        0,
        miss,
    );
    emit_cell_test(ops, 0, 16, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz x0, =>miss
        ; ldrb w16, [x0]
        ; cmp w16, u32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG)
        ; b.eq =>callable
        ; cmp w16, u32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG)
        ; b.ne =>miss
        ; =>callable
        ; mov x1, xzr
        ; ret
        ; =>miss
        ; movz x1, 1
        ; ret
    );
}

fn emit_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
    cells: usize,
) {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // The committed barriers' slow paths call out, so the return address is
    // saved for the whole subroutine.
    dynasm!(ops ; .arch aarch64 ; =>label ; stp x29, x30, [sp, #-16]!);
    property_actions::emit_store(
        ops,
        relocations,
        view,
        AtomOperand::Register(STORE_ATOM),
        [STORE_RECEIVER, STORE_VALUE],
        [10, 11, 13, 15],
        miss,
    );
    dynasm!(ops ; .arch aarch64 ; mov w16, w11);
    super::ic_probe::emit_property_transition_shape_barrier(ops, relocations, view, 20);
    emit_cell_test(ops, STORE_VALUE, 11, CellTest::IsNotCell, done);
    super::values::emit_write_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops ; .arch aarch64 ; =>done ; mov x1, xzr ; ldp x29, x30, [sp], #16 ; ret ; =>miss);
    emit_cell_address(
        ops,
        relocations,
        PropertySourceAccess::Store,
        cells,
        STORE_ORDINAL,
        3,
    );
    dynasm!(ops ; .arch aarch64
        ; mov x0, x20 ; mov x1, X(STORE_RECEIVER) ; mov x2, X(STORE_VALUE));
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_JIT_STORE_PROPERTY),
        abi::STUB_JIT_STORE_PROPERTY,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; ldp x29, x30, [sp], #16 ; ret);
}

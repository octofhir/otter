//! Per-code-object named-property IC routines for baseline sites: V8's
//! `LoadIC` / `StoreIC` builtins over the site's native feedback slot.
//!
//! # Contents
//! - [`SharedPropertyProbes`] names one load, one store and one method
//!   routine, requested by sites and emitted once after the body.
//! - Load: the receiver shape selects an entry of the site's
//!   [`otter_vm::jit::PropertyIcLayout`] slot (V8 `TryMonomorphicCase` /
//!   `HandlePolymorphicCase`), whose handler is dispatched in machine code:
//!   own field, prototype field under its chain proof, nonexistent key.
//!   A megamorphic slot probes the isolate's shared action table
//!   (`TryProbeStubCache`). Anything else is the committed runtime miss,
//!   which also updates the slot.
//! - Store: the same selection for own-field stores and add-transitions
//!   (field write, shape publication, both barriers), then the shared table,
//!   then the committed miss.
//! - Method: the load selection without a miss call; it returns only a
//!   callable hit, and its site resolves every other receiver through its own
//!   committed method resolution, which also records call feedback.
//! - Concat: the nursery string-concatenation fit every `Add` site shares
//!   instead of inlining it.
//!
//! # Invariants
//! - A site passes its receiver (and value) boxed from the rooted window plus
//!   its slot address in fixed registers, and calls with `bl`. The routine
//!   returns the canonical `NativeResultPair`: `x1 == 0` with the loaded value
//!   in `x0`, or a Throw/Fatal status the site routes through its own exits.
//! - Selection and handlers neither allocate nor collect; only the miss calls
//!   the runtime. Every handler check precedes the first effect.
//! - Inputs other than the receiver/value survive no call, matching the
//!   caller-saved contract of the inline miss. The routines preserve
//!   `x19`–`x21` and return with `sp` unchanged.
//! - Feedback is read, never baked: a miss's slot update serves the next
//!   execution of already-published code.
//!
//! # See also
//! - `otter_vm::property_ic` owns the slot, handler kinds and state machine.
//! - `crate::arm64::property_actions` owns the shared-table probe.
//! - [`super::properties`] owns the per-site inline own-field path.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
use otter_vm::native_abi as abi;

use super::values::{CellTest, emit_cell_test, emit_load_runtime_stub};
use crate::arm64::property_actions::{self, AtomOperand};
use crate::arm64::property_ic;
use crate::artifact::relocation::RelocationCapture;

/// Load routine inputs.
pub(crate) const LOAD_RECEIVER: u8 = 9;
pub(crate) const LOAD_SLOT: u8 = 10;

/// Method routine inputs: the load routine's receiver and slot.
pub(crate) const METHOD_RECEIVER: u8 = LOAD_RECEIVER;
pub(crate) const METHOD_SLOT: u8 = LOAD_SLOT;

/// Store routine inputs.
pub(crate) const STORE_RECEIVER: u8 = 12;
pub(crate) const STORE_VALUE: u8 = 9;
pub(crate) const STORE_SLOT: u8 = 10;

/// The shared routines one code object's sites requested.
#[derive(Default)]
pub(crate) struct SharedPropertyProbes {
    load: Option<DynamicLabel>,
    store: Option<DynamicLabel>,
    method: Option<DynamicLabel>,
    concat: Option<DynamicLabel>,
}

impl SharedPropertyProbes {
    /// The routine serving store or load sites.
    pub(crate) fn label(&mut self, ops: &mut Assembler, store: bool) -> DynamicLabel {
        let slot = if store {
            &mut self.store
        } else {
            &mut self.load
        };
        *slot.get_or_insert_with(|| ops.new_dynamic_label())
    }

    /// The probe-only routine serving method-call sites.
    pub(crate) fn method_label(&mut self, ops: &mut Assembler) -> DynamicLabel {
        *self.method.get_or_insert_with(|| ops.new_dynamic_label())
    }

    /// The string-concatenation fit serving `Add` sites.
    pub(crate) fn concat_label(&mut self, ops: &mut Assembler) -> DynamicLabel {
        *self.concat.get_or_insert_with(|| ops.new_dynamic_label())
    }

    /// Emit every requested routine once, after the body.
    pub(crate) fn emit(
        self,
        ops: &mut Assembler,
        relocations: &mut RelocationCapture,
        transitions: &crate::entry::TransitionTable,
        view: &JitCompileSnapshot,
    ) {
        if let Some(label) = self.load {
            emit_load(ops, relocations, transitions, view, label);
        }
        if let Some(label) = self.store {
            emit_store(ops, relocations, transitions, view, label);
        }
        if let Some(label) = self.method {
            emit_method(ops, relocations, view, label);
        }
        if let Some(label) = self.concat {
            emit_concat(ops, view, label);
        }
    }
}

/// Concat routine inputs: the two operands.
pub(crate) const CONCAT_LHS: u8 = 9;
pub(crate) const CONCAT_RHS: u8 = 10;
/// Concat routine result on a fit.
pub(crate) const CONCAT_RESULT: u8 = 12;

/// The nursery fit of `CONCAT_LHS + CONCAT_RHS` for two strings: `x1` is
/// zero with the new string in `CONCAT_RESULT`, or one when it does not fit
/// and nothing was published. Neither collects nor calls Rust.
fn emit_concat(ops: &mut Assembler, view: &JitCompileSnapshot, label: DynamicLabel) {
    let miss = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    crate::arm64::allocation::emit_concat(
        ops,
        20,
        view.string_layout,
        [
            crate::allocation::AllocationValue::Register(CONCAT_LHS),
            crate::allocation::AllocationValue::Register(CONCAT_RHS),
        ],
        crate::allocation::LabRegisters {
            buffer: 11,
            candidate: CONCAT_RESULT,
            end: 13,
            scratch: 15,
            size: 17,
        },
        miss,
    );
    dynasm!(ops
        ; .arch aarch64
        ; mov x1, xzr
        ; ret
        ; =>miss
        ; mov x1, #1
        ; ret
    );
}

/// The load dispatch shared by the load and method routines: on a hit `x0`
/// holds the value and control jumps to `loaded`.
fn emit_load_handlers(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    loaded: DynamicLabel,
    miss: DynamicLabel,
) {
    let megamorphic = ops.new_dynamic_label();
    property_ic::emit_slot_load(
        ops,
        view,
        LOAD_RECEIVER,
        LOAD_SLOT,
        [13, 14, 15],
        0,
        megamorphic,
        miss,
        loaded,
    );
    dynasm!(ops ; .arch aarch64
        ; =>megamorphic
        ; ldr w11, [X(LOAD_SLOT), IC.atom_byte]
    );
    property_actions::emit_load(
        ops,
        relocations,
        view,
        AtomOperand::Register(11),
        LOAD_RECEIVER,
        [13, 14, 15, 3],
        0,
        miss,
    );
    dynasm!(ops ; .arch aarch64 ; b =>loaded);
}

fn emit_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let loaded = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    emit_load_handlers(ops, relocations, view, loaded, miss);
    dynasm!(ops ; .arch aarch64
        ; =>loaded
        ; mov x1, xzr
        ; ret
        ; =>miss
        ; stp x29, x30, [sp, #-16]!
        ; mov x0, x20
        ; mov x1, X(LOAD_RECEIVER)
        ; mov x2, X(LOAD_SLOT)
    );
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
    let loaded = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let callable = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    emit_load_handlers(ops, relocations, view, loaded, miss);
    dynasm!(ops ; .arch aarch64 ; =>loaded);
    emit_cell_test(ops, 0, CellTest::IsNotCell, miss);
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
) {
    let megamorphic = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let barriers = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    // The committed barriers' slow paths call out, so the return address is
    // saved for the whole routine.
    dynasm!(ops ; .arch aarch64 ; =>label ; stp x29, x30, [sp, #-16]!);
    property_ic::emit_slot_store(
        ops,
        view,
        STORE_RECEIVER,
        STORE_VALUE,
        STORE_SLOT,
        [13, 14, 11],
        megamorphic,
        miss,
        stored,
    );
    dynasm!(ops ; .arch aarch64
        ; =>megamorphic
        ; ldr w3, [X(STORE_SLOT), IC.atom_byte]
    );
    property_actions::emit_store(
        ops,
        relocations,
        view,
        AtomOperand::Register(3),
        [STORE_RECEIVER, STORE_VALUE],
        [15, 11, 13, 14],
        miss,
    );
    // Both paths leave the compressed child (zero for an overwrite) in w11.
    dynasm!(ops ; .arch aarch64
        ; =>stored
        ; mov w16, w11
        ; =>barriers);
    super::ic_probe::emit_property_transition_shape_barrier(ops, relocations, view, 20);
    emit_cell_test(ops, STORE_VALUE, CellTest::IsNotCell, primitive);
    super::values::emit_write_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops ; .arch aarch64
        ; =>primitive
        ; mov x1, xzr
        ; ldp x29, x30, [sp], #16
        ; ret
        ; =>miss
        ; mov x0, x20
        ; mov x1, X(STORE_RECEIVER)
        ; mov x2, X(STORE_VALUE)
        ; mov x3, X(STORE_SLOT)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_JIT_STORE_PROPERTY),
        abi::STUB_JIT_STORE_PROPERTY,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; ldp x29, x30, [sp], #16 ; ret);
}

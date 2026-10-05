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
use otter_vm::jit::{PROPERTY_IC_LAYOUT as IC, PropertyIcHandlerKind as Kind};
use otter_vm::native_abi as abi;

use super::values::{CellTest, emit_cell_test, emit_load_runtime_stub, emit_load_u64};
use crate::arm64::property_actions::{self, AtomOperand};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{OBJECT_BODY_TYPE_TAG, VALUE_UNDEFINED};

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

/// The shared property routines one code object's sites requested.
#[derive(Default)]
pub(crate) struct SharedPropertyProbes {
    load: Option<DynamicLabel>,
    store: Option<DynamicLabel>,
    method: Option<DynamicLabel>,
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
    }
}

/// Prove `X(receiver)` is an ordinary object and select the entry of its
/// shape in the slot at `X(slot)`. On a match `x13` addresses the entry; a
/// megamorphic slot branches to `megamorphic`; anything else to `miss`.
/// Clobbers `w11`, `w14`, `x13`, `x16`, `w17`; never the store receiver `x12`.
fn emit_select_entry(
    ops: &mut Assembler,
    receiver: u8,
    slot: u8,
    megamorphic: DynamicLabel,
    miss: DynamicLabel,
    object_shape_byte: u32,
) {
    let scan = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    emit_cell_test(ops, receiver, 16, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)]
        ; cmp w16, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w11, [X(receiver), object_shape_byte]
        ; ldr w17, [X(slot), IC.state_byte]
        ; tbnz w17, IC.megamorphic_bit.trailing_zeros(), =>megamorphic
        ; and w17, w17, #IC.count_mask
        ; cbz w17, =>miss
        ; add x13, XSP(slot), #IC.entries_byte
        ; =>scan
        ; ldr w14, [x13, IC.entry_shape_byte]
        ; cmp w14, w11
        ; b.eq =>found
        ; subs w17, w17, #1
        ; b.eq =>miss
        ; add x13, x13, #IC.entry_bytes
        ; b =>scan
        ; =>found
    );
}

/// Branch to `miss` unless the entry at `x13` carries no proof or a valid one.
/// `proof_required` refuses an entry without a proof. Clobbers `x14`.
fn emit_entry_proof(ops: &mut Assembler, proof_required: bool, miss: DynamicLabel) {
    let held = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; ldr x14, [x13, IC.entry_validity_byte]);
    if proof_required {
        dynasm!(ops ; .arch aarch64 ; cbz x14, =>miss);
    } else {
        dynasm!(ops ; .arch aarch64 ; cbz x14, =>held);
    }
    dynasm!(ops ; .arch aarch64
        ; ldar w14, [x14]
        ; cbz w14, =>miss
        ; =>held
    );
}

/// `X(base)` = first word of the bank the field key in `w14` selects on the
/// object at `X(holder)`; `x14` becomes the bank-relative index. A missing
/// overflow slab branches to `miss`. Clobbers `x17`.
fn emit_field_bank(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    base: u8,
    miss: DynamicLabel,
) {
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let layout = view.field_layout;
    dynasm!(ops ; .arch aarch64
        ; tbnz w14, 31, =>inline
        ; ldr W(base), [X(holder), layout.slab_handle_byte]
        ; cbz W(base), =>miss
        ; and x17, X(holder), #0xffff_ffff_0000_0000
        ; add X(base), x17, X(base)
        ; add XSP(base), XSP(base), #layout.slab_words_byte
        ; b =>ready
        ; =>inline
        ; and w14, w14, #0x7fff_ffff
        ; add XSP(base), XSP(holder), #layout.inline_values_byte
        ; =>ready
    );
}

/// The load handler dispatch shared by the load and method routines: on a
/// hit `x0` holds the value and control falls through to `loaded`.
fn emit_load_handlers(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    loaded: DynamicLabel,
    miss: DynamicLabel,
) {
    let megamorphic = ops.new_dynamic_label();
    let own = ops.new_dynamic_label();
    let prototype = ops.new_dynamic_label();
    let field = ops.new_dynamic_label();
    emit_select_entry(
        ops,
        LOAD_RECEIVER,
        LOAD_SLOT,
        megamorphic,
        miss,
        view.object_shape_byte,
    );
    dynasm!(ops ; .arch aarch64
        ; ldrb w14, [x13, IC.entry_kind_byte]
        ; cmp w14, Kind::OwnField as u32
        ; b.eq =>own
        ; cmp w14, Kind::PrototypeField as u32
        ; b.eq =>prototype
        ; cmp w14, Kind::NonExistent as u32
        ; b.ne =>miss
    );
    emit_entry_proof(ops, false, miss);
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; b =>loaded ; =>prototype);
    emit_entry_proof(ops, true, miss);
    dynasm!(ops ; .arch aarch64
        ; and x16, X(LOAD_RECEIVER), #0xffff_ffff_0000_0000
        ; ldr w14, [x13, IC.entry_aux_byte]
        ; add x14, x16, x14
        ; ldr w15, [x14, view.shape_prototype_byte]
        ; cbz w15, =>miss
        ; add x15, x16, x15
        ; b =>field
        ; =>own
        ; mov x15, X(LOAD_RECEIVER)
        ; =>field
        ; ldr w14, [x13, IC.entry_field_byte]
    );
    emit_field_bank(ops, view, 15, 16, miss);
    dynasm!(ops ; .arch aarch64
        ; ldr x0, [x16, x14, lsl #3]
        ; b =>loaded
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
) {
    let megamorphic = ops.new_dynamic_label();
    let own = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let layout = view.field_layout;
    // The committed barriers' slow paths call out, so the return address is
    // saved for the whole routine.
    dynasm!(ops ; .arch aarch64 ; =>label ; stp x29, x30, [sp, #-16]!);
    emit_select_entry(
        ops,
        STORE_RECEIVER,
        STORE_SLOT,
        megamorphic,
        miss,
        view.object_shape_byte,
    );
    dynasm!(ops ; .arch aarch64
        ; ldrb w14, [x13, IC.entry_kind_byte]
        ; cmp w14, Kind::StoreField as u32
        ; b.eq =>own
        ; cmp w14, Kind::StoreTransition as u32
        ; b.ne =>miss
    );
    // Append: chain proof (absent for a `null` prototype, which the receiver
    // shape fixes), a resident target layout and storage, then the write and
    // the target shape's publication.
    emit_entry_proof(ops, false, miss);
    let inline = ops.new_dynamic_label();
    let publish = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldr w15, [x13, IC.entry_aux_byte]
        ; cbz w15, =>miss
        ; ldr w14, [x13, IC.entry_field_byte]
        ; tbnz w14, 31, =>inline
        ; ldr w16, [X(STORE_RECEIVER), layout.slab_handle_byte]
        ; cbz w16, =>miss
        ; and x17, X(STORE_RECEIVER), #0xffff_ffff_0000_0000
        ; add x16, x17, x16
        ; ldr w17, [x16, layout.slab_capacity_byte]
        ; cmp w14, w17
        ; b.hs =>miss
        ; add x16, x16, #layout.slab_words_byte
        ; str X(STORE_VALUE), [x16, x14, lsl #3]
        ; b =>publish
        ; =>inline
        ; and w14, w14, #0x7fff_ffff
        ; add x16, XSP(STORE_RECEIVER), #layout.inline_values_byte
        ; str X(STORE_VALUE), [x16, x14, lsl #3]
        ; =>publish
        ; str w15, [X(STORE_RECEIVER), view.object_shape_byte]
        ; mov w16, w15
        ; b =>stored
        ; =>own
        ; ldr w14, [x13, IC.entry_field_byte]
    );
    emit_field_bank(ops, view, STORE_RECEIVER, 16, miss);
    dynasm!(ops ; .arch aarch64
        ; str X(STORE_VALUE), [x16, x14, lsl #3]
        ; mov w16, wzr
        ; b =>stored
        ; =>megamorphic
        ; ldr w3, [X(STORE_SLOT), IC.atom_byte]
    );
    property_actions::emit_store(
        ops,
        relocations,
        view,
        AtomOperand::Register(3),
        [STORE_RECEIVER, STORE_VALUE],
        [11, 14, 13, 15],
        miss,
    );
    dynasm!(ops ; .arch aarch64 ; mov w16, w14 ; =>stored);
    super::ic_probe::emit_property_transition_shape_barrier(ops, relocations, view, 20);
    emit_cell_test(ops, STORE_VALUE, 11, CellTest::IsNotCell, primitive);
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

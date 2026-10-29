//! Named-property IC routines shared by every code object of one compiler,
//! over a site's native feedback slot.
//!
//! # Contents
//! - [`emit_routine`] emits one routine: load, store, method, concat, keyed
//!   load or keyed store, with probe entries of load and store for optimized
//!   sites. [`emit_call`] calls one of them.
//! - Load: the receiver shape selects an entry of the site's
//!   [`otter_vm::jit::PropertyIcLayout`] slot, whose handler is dispatched in
//!   machine code: own field, prototype field under its chain proof,
//!   nonexistent key. A megamorphic slot probes the isolate's shared action
//!   table. Anything else is the committed runtime miss, which also updates
//!   the slot.
//! - Store: the same selection for own-field stores and add-transitions
//!   (field write, shape publication, both barriers), then the shared table,
//!   then the committed miss.
//! - Method: the load selection without a miss call; it returns only a
//!   callable hit, and its site resolves every other receiver through its own
//!   committed method resolution, which also records call feedback.
//! - Concat: the nursery string-concatenation fit every `Add` site shares
//!   instead of inlining it.
//! - Keyed load and store, name case: a string key probes the shared action
//!   table under its atom, recorded in the string or found by the interning
//!   leaf and then recorded. A dictionary-mode receiver, which the table
//!   never holds, and a load the table misses run the generic own-data walk
//!   leaf; any other key, a shaped receiver's or a store's table miss, or a
//!   walk the leaf cannot finish returns to the element site, which
//!   completes through its committed element runtime (which records the
//!   table entry).
//!
//! # Invariants
//! - A site passes its receiver (and value) boxed from the rooted window plus
//!   its slot address in fixed registers, and calls with `blr x16`. The routine
//!   returns the canonical `NativeResultPair`: `x1 == 0` with the loaded value
//!   in `x0`, or a Throw/Fatal status the site routes through its own exits.
//! - Selection and handlers neither allocate nor collect; only the miss calls
//!   the runtime. Every handler check precedes the first effect.
//! - A probe entry completes no miss and calls nothing that collects: the
//!   each writes only its declared clobbers, its result, `x16` and `x17`.
//!   The optimized site enters the committed runtime operation on a miss.
//! - Routines embed only isolate-wide layouts and addresses; nothing about
//!   the calling code object.
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

use super::values::{CellTest, emit_cell_test, emit_load_runtime_stub, emit_load_symbol_u64};
use crate::arm64::property_actions;
use crate::arm64::property_ic;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::routines::{Routine, RoutineObject, RoutineOffsets, RoutineRequest};

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

/// Keyed load routine inputs: the load routine's receiver, and the key.
pub(crate) const KEYED_LOAD_RECEIVER: u8 = LOAD_RECEIVER;
pub(crate) const KEYED_LOAD_KEY: u8 = 10;

/// Keyed store routine inputs: the store routine's receiver and value, and
/// the key.
pub(crate) const KEYED_STORE_RECEIVER: u8 = STORE_RECEIVER;
pub(crate) const KEYED_STORE_VALUE: u8 = STORE_VALUE;
pub(crate) const KEYED_STORE_KEY: u8 = 10;

/// Call `routine` of the compiler's routine set: its inputs are in their
/// fixed registers; `x16` and `x30` are clobbered by the call itself.
pub(crate) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    routines: &RoutineRequest<'_>,
    routine: Routine,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        routines.entry(routine),
        RelocationTarget::Routine {
            routine: routine.name(),
        },
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

/// Emit the routine `object` holds, returning its entry offsets.
pub(crate) fn emit_routine(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    object: RoutineObject,
) -> RoutineOffsets {
    let label = ops.new_dynamic_label();
    let entry = ops.offset().0;
    match object {
        RoutineObject::Load => emit_load(ops, relocations, transitions, view, label),
        RoutineObject::Store => emit_store(ops, relocations, transitions, view, label),
        RoutineObject::Method => emit_method(ops, relocations, view, label),
        RoutineObject::Concat => emit_concat(ops, view, label),
        RoutineObject::KeyedLoad => emit_keyed_load(ops, relocations, view, label),
        RoutineObject::KeyedStore => emit_keyed_store(ops, relocations, view, label),
        RoutineObject::StoreTail => emit_store_tail(ops, relocations, view, label),
        RoutineObject::Truthy => unreachable!("the truthiness routine is the graph tier's"),
        RoutineObject::FieldObserve => {
            unreachable!("the field-observation routine is the graph tier's")
        }
    }
    let second = ops.offset().0;
    match object {
        RoutineObject::Load => emit_load_probe(ops, relocations, view),
        RoutineObject::Store => emit_store_probe(ops, relocations, view),
        RoutineObject::KeyedLoad => emit_keyed_load_probe(ops, relocations, view),
        _ => {
            return RoutineOffsets {
                entry,
                second: entry,
            };
        }
    }
    RoutineOffsets { entry, second }
}

/// Store tail inputs: the receiver, the stored value and the compressed
/// defining shape whose field summary observes it (zero for none).
pub(crate) const TAIL_RECEIVER: u8 = STORE_RECEIVER;
pub(crate) const TAIL_VALUE: u8 = STORE_VALUE;
pub(crate) const TAIL_OWNER: u8 = STORE_SLOT;

/// After a baseline site's inline own-field store: the field summary of
/// `W(TAIL_OWNER)` observes `X(TAIL_VALUE)`, then the value barrier of
/// `X(TAIL_RECEIVER)`. The site holds nothing else in registers.
fn emit_store_tail(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let primitive = ops.new_dynamic_label();
    // The barrier's slow path calls out.
    dynasm!(ops ; .arch aarch64 ; =>label ; stp x29, x30, [sp, #-16]! ; mov x29, sp);
    crate::arm64::field_type::emit_observe(ops, relocations, view, TAIL_OWNER, TAIL_VALUE);
    emit_cell_test(ops, TAIL_VALUE, CellTest::IsNotCell, primitive);
    super::values::emit_write_barrier(ops, relocations, view, TAIL_RECEIVER, TAIL_VALUE);
    dynasm!(ops ; .arch aarch64
        ; =>primitive
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

/// Probe entry inputs: the receiver and the site's slot address.
pub(crate) const PROBE_RECEIVER: u8 = LOAD_RECEIVER;
pub(crate) const PROBE_SLOT: u8 = LOAD_SLOT;
/// The registers the load probe writes besides `x16`, `x17` and its result
/// in `x0`: its slot input and its temporaries.
pub(crate) const PROBE_LOAD_CLOBBERS: [u8; 6] = [3, PROBE_SLOT, 11, 13, 14, 15];

/// The load selection for an optimized site, receiver in [`PROBE_RECEIVER`]
/// and slot address in [`PROBE_SLOT`]: `x17 == 0` with the value in `x0`,
/// else `x17 != 0`. The receiver survives; only [`PROBE_LOAD_CLOBBERS`],
/// `x0`, `x16` and `x17` are written and nothing is called.
fn emit_load_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) {
    let loaded = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    emit_load_handlers(ops, relocations, view, loaded, miss);
    dynasm!(ops ; .arch aarch64
        ; =>loaded
        ; mov x17, xzr
        ; ret
        ; =>miss
        ; movz x17, 1
        ; ret
    );
}

/// Store probe inputs: the receiver, the value and the site's slot address.
pub(crate) const PROBE_STORE_RECEIVER: u8 = STORE_RECEIVER;
pub(crate) const PROBE_STORE_VALUE: u8 = STORE_VALUE;
pub(crate) const PROBE_STORE_SLOT: u8 = STORE_SLOT;
/// The registers the store probe writes besides `x16` and `x17`: its slot
/// input and its temporaries.
pub(crate) const PROBE_STORE_CLOBBERS: [u8; 6] = [3, PROBE_STORE_SLOT, 11, 13, 14, 15];

/// The store selection for an optimized site: `x17 == 0` once the value is
/// stored with both barriers, else `x17 != 0` with nothing written. The
/// receiver and value survive; [`PROBE_STORE_CLOBBERS`], `x16` and `x17` are
/// written and every other register is preserved. Nothing collects: the
/// barriers' slow paths save the volatile registers around their leaves.
fn emit_store_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) {
    let megamorphic = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    property_ic::emit_slot_store(
        ops,
        relocations,
        view,
        PROBE_STORE_RECEIVER,
        PROBE_STORE_VALUE,
        PROBE_STORE_SLOT,
        [13, 14, 11],
        megamorphic,
        miss,
        stored,
    );
    dynasm!(ops ; .arch aarch64
        ; =>megamorphic
        ; ldr w3, [X(PROBE_STORE_SLOT), IC.atom_byte]
    );
    property_actions::emit_site_store(
        ops,
        relocations,
        view,
        3,
        [PROBE_STORE_RECEIVER, PROBE_STORE_VALUE],
        [15, 11, 13, 14],
        false,
        Some(property_actions::SiteFill::Named(PROBE_STORE_SLOT)),
        miss,
    );
    // Both paths leave the compressed child (zero for an overwrite) in
    // `w11` and the storage owner in the slot register.
    dynasm!(ops ; .arch aarch64
        ; mov X(PROBE_STORE_SLOT), X(PROBE_STORE_RECEIVER)
        ; =>stored
        ; cbz w11, >value
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64 ; add x13, x13, x11);
    super::values::emit_write_barrier_preserving(ops, relocations, view, PROBE_STORE_SLOT, 13);
    dynasm!(ops ; .arch aarch64 ; value:);
    emit_cell_test(ops, PROBE_STORE_VALUE, CellTest::IsNotCell, primitive);
    super::values::emit_write_barrier_preserving(
        ops,
        relocations,
        view,
        PROBE_STORE_SLOT,
        PROBE_STORE_VALUE,
    );
    dynasm!(ops ; .arch aarch64
        ; =>primitive
        ; mov x17, xzr
        ; ret
        ; =>miss
        ; movz x17, 1
        ; ret
    );
}

/// Keyed load probe result on a hit.
pub(crate) const PROBE_KEYED_RESULT: u8 = 0;
/// Keyed load probe input: the site's slot address, or zero; the result
/// word carries it in.
pub(crate) const PROBE_KEYED_SITE: u8 = PROBE_KEYED_RESULT;
/// The registers the keyed load probe writes besides `x16` and `x17`: its
/// result and its temporaries.
pub(crate) const PROBE_KEYED_LOAD_CLOBBERS: [u8; 7] = [PROBE_KEYED_RESULT, 3, 11, 12, 13, 14, 15];

/// The keyed load's shaped table hit for an optimized site, receiver in
/// [`KEYED_LOAD_RECEIVER`] and key in [`KEYED_LOAD_KEY`]: `x17 == 0` with the
/// value in [`PROBE_KEYED_RESULT`], else `x17 != 0` and the site takes the
/// full routine. A key whose atom is unrecorded, a dictionary-mode receiver
/// and a table miss all decline; nothing is called. Both inputs survive and
/// only [`PROBE_KEYED_LOAD_CLOBBERS`], `x16` and `x17` are written.
fn emit_keyed_load_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) {
    let miss = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let symbol = ops.new_dynamic_label();
    let key = KEYED_LOAD_KEY;
    // The key's recorded atom, or a decline.
    emit_cell_test(ops, key, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz X(key), =>miss
        ; ldrb w16, [X(key)]
        ; cmp w16, u32::from(view.string_layout.string_type_tag)
        ; b.ne =>symbol
        // The body records the atom plus one; zero is unrecorded.
        ; ldr w11, [X(key), view.string_layout.atom_byte]
        ; cbz w11, =>miss
        ; sub w11, w11, #1
        ; b =>ready
        ; =>symbol
        ; cmp w16, u32::from(otter_vm::symbol::SYMBOL_BODY_TYPE_TAG)
        ; b.ne =>miss
        ; ldr w11, [X(key), otter_vm::symbol::SYMBOL_ATOM_BYTE]
        ; cmn w11, #1
        ; b.eq =>miss
        ; =>ready
        ; mov x12, X(KEYED_LOAD_RECEIVER)
    );
    // The table holds no dictionary-mode object's keys, so its probe misses
    // such a receiver by shape identity; an Array reads its named bag.
    emit_array_bag(ops, view, 12, object, miss);
    property_actions::emit_load(
        ops,
        relocations,
        view,
        11,
        12,
        [13, 14, 15, 3],
        PROBE_KEYED_RESULT,
        true,
        miss,
    );
    dynasm!(ops ; .arch aarch64 ; mov x17, xzr ; ret ; =>object);
    // An object's own hit also fills the site's cache, whose slot address
    // the caller leaves in the result word (zero for none).
    property_actions::emit_site_load(
        ops,
        relocations,
        view,
        11,
        12,
        [13, 14, 15, 3],
        PROBE_KEYED_RESULT,
        false,
        Some(property_actions::SiteFill::Keyed {
            site: PROBE_KEYED_SITE,
            atom: 11,
        }),
        miss,
    );
    dynasm!(ops ; .arch aarch64
        ; mov x17, xzr
        ; ret
        ; =>miss
        ; movz x17, 1
        ; ret
    );
}

/// Leave the atom of the string or symbol `key` in `W(atom)`, or branch to
/// `miss` for any other key, to `uninterned` for a string no property key
/// has interned or a symbol that never keyed a slot. A string whose atom is
/// not yet recorded asks the interning leaf and records the answer in its
/// atom word, so the same string probes directly next time; a symbol carries
/// its atom from the slot it first keyed. The routine inputs, `x9`, `x10`
/// and `x12`, survive the leaf call.
fn emit_key_atom(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    key: u8,
    atom: u8,
    uninterned: DynamicLabel,
    miss: DynamicLabel,
) {
    let unrecorded = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let symbol = ops.new_dynamic_label();
    let atom_byte = view.string_layout.atom_byte;
    emit_cell_test(ops, key, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz X(key), =>miss
        ; ldrb w16, [X(key)]
        ; cmp w16, u32::from(view.string_layout.string_type_tag)
        ; b.ne =>symbol
        // The body records the atom plus one; zero is unrecorded.
        ; ldr W(atom), [X(key), atom_byte]
        ; cbz W(atom), =>unrecorded
        ; sub WSP(atom), WSP(atom), #1
        ; b =>ready
        ; =>unrecorded
        ; stp x30, x9, [sp, #-32]!
        ; stp x10, x12, [sp, #16]
        ; ldr x16, [x20, crate::entry::THREAD_OFFSET]
        ; ldr x0, [x16, crate::entry::VM_THREAD_GC_HEAP_OFFSET]
        ; mov x1, X(key)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::STRING_KEY_ATOM_LEAF.entry_addr() as u64,
        abi::STUB_STRING_KEY_ATOM_LEAF,
    );
    dynasm!(ops ; .arch aarch64
        ; blr x16
        ; ldp x10, x12, [sp, #16]
        ; ldp x30, x9, [sp], #32
        ; cbnz x1, =>miss
        // Content never interned is no shaped object's property; the
        // string records nothing.
        ; cmn w0, #1
        ; b.eq =>uninterned
        ; mov W(atom), w0
        ; add w16, w0, #1
        ; str w16, [X(key), atom_byte]
        ; b =>ready
        ; =>symbol
        ; cmp w16, u32::from(otter_vm::symbol::SYMBOL_BODY_TYPE_TAG)
        ; b.ne =>miss
        ; ldr W(atom), [X(key), otter_vm::symbol::SYMBOL_ATOM_BYTE]
        ; cmn WSP(atom), #1
        ; b.eq =>uninterned
        ; =>ready
    );
}

/// Replace an Array receiver in `X(receiver)` with its named bag, the
/// storage its own named properties live in; branch to `object` for any
/// other receiver (the register untouched) and to `miss` for an Array
/// without a bag. Clobbers `x16`, `x17`.
fn emit_array_bag(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    object: DynamicLabel,
    miss: DynamicLabel,
) {
    emit_cell_test(ops, receiver, CellTest::IsNotCell, object);
    dynasm!(ops ; .arch aarch64
        ; cbz X(receiver), =>object
        ; ldrb w16, [X(receiver)]
        ; cmp w16, u32::from(view.array_layout.type_tag)
        ; b.ne =>object
        ; ldr w16, [X(receiver), view.array_layout.expando_byte]
        ; cbz w16, =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; add X(receiver), x17, x16
    );
}

/// `x1 == 0` with the value in `x0`, else `x1 != 0` and nothing observed.
/// Nothing allocates; the only calls are the interning leaf and, for a
/// dictionary-mode receiver the table does not hold or a key the interner
/// never saw, the generic own-data walk.
fn emit_keyed_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let miss = ops.new_dynamic_label();
    let generic = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    let shaped = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    // The table never holds a dictionary-mode object's keys: such a
    // receiver goes straight to the own-data walk, which reads the key's
    // spelling without an atom.
    emit_cell_test(ops, KEYED_LOAD_RECEIVER, CellTest::IsNotCell, shaped);
    dynasm!(ops ; .arch aarch64
        ; cbz X(KEYED_LOAD_RECEIVER), =>shaped
        ; ldrb w16, [X(KEYED_LOAD_RECEIVER)]
        ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
        ; b.ne =>shaped
        ; ldr w16, [X(KEYED_LOAD_RECEIVER), view.object_shape_byte]
        ; cbz w16, =>shaped
        ; and x17, X(KEYED_LOAD_RECEIVER), #0xffff_ffff_0000_0000
        ; add x16, x17, x16
        ; ldrb w16, [x16, view.shape_state_byte]
        ; tbnz w16, otter_vm::object::ShapeState::DICTIONARY_MASK.trailing_zeros(), =>generic
        ; =>shaped
    );
    emit_key_atom(ops, relocations, view, KEYED_LOAD_KEY, 11, generic, miss);
    emit_array_bag(ops, view, KEYED_LOAD_RECEIVER, object, miss);
    property_actions::emit_load(
        ops,
        relocations,
        view,
        11,
        KEYED_LOAD_RECEIVER,
        [13, 14, 15, 3],
        0,
        true,
        miss,
    );
    dynasm!(ops ; .arch aarch64 ; mov x1, xzr ; ret ; =>object);
    property_actions::emit_load(
        ops,
        relocations,
        view,
        11,
        KEYED_LOAD_RECEIVER,
        [13, 14, 15, 3],
        0,
        false,
        generic,
    );
    dynasm!(ops ; .arch aarch64
        ; mov x1, xzr
        ; ret
        // A name the table does not hold: the generic own-data walk, whose
        // pair is the routine's result.
        ; =>generic
        ; str x30, [sp, #-16]!
        ; mov x1, X(KEYED_LOAD_RECEIVER)
        ; mov x2, X(KEYED_LOAD_KEY)
        ; ldr x0, [x20, crate::entry::THREAD_OFFSET]
        ; ldr x0, [x0, crate::entry::VM_THREAD_GC_HEAP_OFFSET]
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::KEYED_NAMED_LOAD_LEAF.entry_addr() as u64,
        abi::STUB_KEYED_NAMED_LOAD_LEAF,
    );
    dynasm!(ops ; .arch aarch64
        ; blr x16
        ; ldr x30, [sp], #16
        ; ret
        ; =>miss
        ; movz x1, 1
        ; ret
    );
}

/// `x1 == 0` once the value is stored with its barriers, else `x1 == 1`
/// and nothing written.
fn emit_keyed_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let stored = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label ; stp x29, x30, [sp, #-16]! ; mov x29, sp);
    emit_key_atom(ops, relocations, view, KEYED_STORE_KEY, 3, miss, miss);
    // An Array's own named data lives in its bag, written in place.
    emit_array_bag(ops, view, KEYED_STORE_RECEIVER, object, miss);
    property_actions::emit_store(
        ops,
        relocations,
        view,
        3,
        [STORE_RECEIVER, STORE_VALUE],
        [15, 11, 13, 14],
        true,
        miss,
    );
    // The overwrite left zero in w11; the bag takes the barriers.
    dynasm!(ops ; .arch aarch64
        ; mov X(STORE_SLOT), X(STORE_RECEIVER)
        ; b =>stored
        ; =>object);
    emit_table_store(ops, relocations, view, stored, miss);
    dynasm!(ops ; .arch aarch64
        ; =>miss
        ; movz x1, 1
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

/// The shared-table store under the atom in `w3`, then (also from `stored`,
/// with the compressed child or zero in `w11`) both barriers and the
/// success return. Expects the routine's saved `x29`/`x30` on the stack.
fn emit_table_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    stored: DynamicLabel,
    miss: DynamicLabel,
) {
    let primitive = ops.new_dynamic_label();
    property_actions::emit_store(
        ops,
        relocations,
        view,
        3,
        [STORE_RECEIVER, STORE_VALUE],
        [15, 11, 13, 14],
        false,
        miss,
    );
    // Both paths leave the compressed child (zero for an overwrite) in w11.
    dynasm!(ops ; .arch aarch64
        ; mov X(STORE_SLOT), X(STORE_RECEIVER)
        ; =>stored
        ; mov X(STORE_RECEIVER), X(STORE_SLOT)
        ; mov w16, w11);
    super::ic_probe::emit_property_transition_shape_barrier(ops, relocations, view, 20);
    emit_cell_test(ops, STORE_VALUE, CellTest::IsNotCell, primitive);
    super::values::emit_write_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops ; .arch aarch64
        ; =>primitive
        ; mov x1, xzr
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

/// Concat routine inputs: the two operands.
pub(crate) const CONCAT_LHS: u8 = 9;
pub(crate) const CONCAT_RHS: u8 = 10;
/// Concat routine result on a fit.
pub(crate) const CONCAT_RESULT: u8 = 12;
/// Concat routine input: the calling site's allocation-site record address,
/// or zero.
pub(crate) const CONCAT_SITE: u8 = 14;

/// The fit of `CONCAT_LHS + CONCAT_RHS` for two strings, placed by the
/// `CONCAT_SITE` record: `x1` is
/// zero with the new string in `CONCAT_RESULT`, or one when it does not fit
/// and nothing was published. Neither collects nor calls Rust.
fn emit_concat(ops: &mut Assembler, view: &JitCompileSnapshot, label: DynamicLabel) {
    let miss = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>label);
    // A record places by its run-time decision, never in old space.
    let old_cons = crate::arm64::allocation::emit_concat(
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
        crate::allocation::ConcatPlacement::Record(CONCAT_SITE),
        miss,
    );
    debug_assert!(old_cons.is_none());
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
    property_actions::emit_site_load(
        ops,
        relocations,
        view,
        11,
        LOAD_RECEIVER,
        [13, 14, 15, 3],
        0,
        false,
        Some(property_actions::SiteFill::Named(LOAD_SLOT)),
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
        ; mov x29, sp
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
    let miss = ops.new_dynamic_label();
    // The committed barriers' slow paths call out, so the return address is
    // saved for the whole routine.
    dynasm!(ops ; .arch aarch64 ; =>label ; stp x29, x30, [sp, #-16]! ; mov x29, sp);
    property_ic::emit_slot_store(
        ops,
        relocations,
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
    emit_table_store(ops, relocations, view, stored, miss);
    dynasm!(ops ; .arch aarch64
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

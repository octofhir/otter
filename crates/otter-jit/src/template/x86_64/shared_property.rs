//! Named-property IC routines shared by every x86-64 code object of one
//! compiler, over a site's native feedback slot.
//!
//! # Contents
//! - [`emit_routine`] emits one routine: load, store, method, concat, keyed
//!   load or keyed store. [`emit_call`] calls one of them.
//! - Load: the receiver shape selects an entry of the site's slot whose
//!   handler runs in machine code: own field, prototype field under its chain
//!   proof, nonexistent key. A megamorphic slot probes the isolate's shared
//!   action table. Anything else is the committed runtime miss, which also
//!   updates the slot.
//! - Store: own-field stores and add-transitions (field write, shape
//!   publication, both barriers), then the shared table, then the miss.
//! - Method: the load selection without a miss call; it returns only a
//!   callable hit.
//! - Concat: the nursery string-concatenation fit every `Add` site shares
//!   instead of inlining it.
//! - Keyed load and store, name case: a string key probes the shared action
//!   table under its atom, recorded in the string or found by the interning
//!   leaf and then recorded. A load the table misses on a dictionary-mode
//!   receiver, which the table never holds, runs the generic own-data walk
//!   leaf; any other key, a shaped receiver's or a store's table miss, or a
//!   walk the leaf cannot finish returns to the element site, which
//!   completes through its committed element runtime (which records the
//!   table entry).
//!
//! # Invariants
//! - Inputs arrive in the System V argument registers the miss passes on:
//!   load `rsi` receiver, `rdx` slot; store `rsi` receiver, `rdx` value,
//!   `rcx` slot. Results follow `NativeResultPair`: `rax` value, `rdx`
//!   status (zero on a hit).
//! - Sites call with a 16-byte aligned stack, so every routine that calls
//!   out first realigns by one word and restores it before returning.
//! - Selection and handlers neither allocate nor collect; every handler check
//!   precedes the first effect. Only barriers and the miss call out.
//! - Feedback is read, never baked: a miss's slot update serves the next
//!   execution of already-published code.
//! - The load and store inputs are the committed runtime ABI's, so an
//!   optimized site calls them like the runtime stub, with every live value in
//!   its home and the safepoint stamped.
//! - Routines embed only isolate-wide layouts and addresses.
//!
//! # See also
//! - `otter_vm::property_ic` owns the slot, handler kinds and state machine.
//! - `crate::x86_64::property_actions` owns the shared-table probe.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
use otter_vm::native_abi as abi;

use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget};
use crate::routines::{Routine, RoutineObject, RoutineOffsets, RoutineRequest};
use crate::x86_64::call_abi::emit_runtime_call;
use crate::x86_64::property_actions::emit_action_probe;
use crate::x86_64::property_ic;
use crate::x86_64::values::{emit_load_runtime_stub, emit_load_symbol_u64, emit_load_u64};

/// Load routine inputs.
pub(crate) const LOAD_RECEIVER: u8 = 6;
pub(crate) const LOAD_SLOT: u8 = 2;

/// Store routine inputs.
pub(crate) const STORE_RECEIVER: u8 = 6;
pub(crate) const STORE_VALUE: u8 = 2;
pub(crate) const STORE_SLOT: u8 = 1;

/// Keyed load routine inputs: the load routine's receiver, and the key.
pub(crate) const KEYED_LOAD_RECEIVER: u8 = LOAD_RECEIVER;
pub(crate) const KEYED_LOAD_KEY: u8 = 1;

/// Keyed store routine inputs: the store routine's receiver and value, and
/// the key.
pub(crate) const KEYED_STORE_RECEIVER: u8 = STORE_RECEIVER;
pub(crate) const KEYED_STORE_VALUE: u8 = STORE_VALUE;
pub(crate) const KEYED_STORE_KEY: u8 = 1;

const CAGE_MASK: u64 = 0xffff_ffff_0000_0000;

/// Concat routine inputs: the two operands.
pub(crate) const CONCAT_LHS: u8 = 0;
pub(crate) const CONCAT_RHS: u8 = 8;
/// Concat routine result on a fit.
pub(crate) const CONCAT_RESULT: u8 = 7;
/// Concat routine input: the calling site's allocation-site record address,
/// or zero.
pub(crate) const CONCAT_SITE: u8 = 2;

/// Call `routine` of the compiler's routine set: its inputs are in their
/// fixed registers; `r11` is clobbered by the call itself.
pub(crate) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    routines: &RoutineRequest<'_>,
    routine: Routine,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        routines.entry(routine),
        RelocationTarget::Routine {
            routine: routine.name(),
        },
    );
    dynasm!(ops ; .arch x64 ; call r11);
}

/// Emit the routine `object` holds, returning its entry offsets. The load
/// and store routines already take the committed runtime ABI, so their
/// committed entries are the routines themselves.
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
/// `Rd(TAIL_OWNER)` observes `Rq(TAIL_VALUE)`, then the value barrier of
/// `Rq(TAIL_RECEIVER)`. The site holds nothing else in registers.
fn emit_store_tail(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    // Sites call with an aligned stack; both steps may call out.
    dynasm!(ops ; .arch x64 ; =>label ; sub rsp, 8);
    crate::x86_64::field_type::emit_observe(ops, relocations, view, TAIL_OWNER, TAIL_VALUE);
    super::emit_template_value_barrier(ops, relocations, view, TAIL_RECEIVER, TAIL_VALUE);
    dynasm!(ops ; .arch x64 ; add rsp, 8 ; ret);
}

/// Probe entry inputs: the receiver and the site's slot address.
pub(crate) const PROBE_RECEIVER: u8 = LOAD_RECEIVER;
pub(crate) const PROBE_SLOT: u8 = 10;
/// Probe entry status: zero on a hit.
pub(crate) const PROBE_STATUS: u8 = 11;

/// The load selection for an optimized site, receiver in `rsi` and slot
/// address in `r10`: `r11 == 0` with the value in `rax`, else `r11 != 0`.
/// Every register but `rax`, `r10` and `r11` is preserved; nothing is called.
fn emit_load_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) {
    let loaded = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    debug_assert_eq!(PROBE_RECEIVER, LOAD_RECEIVER);
    dynasm!(ops ; .arch x64
        ; push rdx ; push rcx ; push rdi ; push r8 ; push r9
        ; mov Rq(LOAD_SLOT), Rq(PROBE_SLOT)
    );
    emit_load_handlers(ops, relocations, view, loaded, miss);
    dynasm!(ops ; .arch x64
        ; =>loaded
        ; xor Rd(PROBE_STATUS), Rd(PROBE_STATUS)
        ; jmp =>done
        ; =>miss
        ; mov Rd(PROBE_STATUS), 1
        ; =>done
        ; pop r9 ; pop r8 ; pop rdi ; pop rcx ; pop rdx
        ; ret
    );
}

/// Leave the atom of the string or symbol `key` in `Rd(atom)`, or branch to
/// `miss` for any other key, to `uninterned` for a string no property key has
/// interned or a symbol that never keyed a slot. A string whose atom is not
/// yet recorded asks the interning leaf and records the answer in its atom
/// word. The routine inputs, `rsi`, `rcx` and `rdx`,
/// survive the leaf call; `aligned` says whether `rsp` is 16-byte aligned
/// here.
fn emit_key_atom(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    key: u8,
    atom: u8,
    aligned: bool,
    uninterned: DynamicLabel,
    miss: DynamicLabel,
) {
    let ready = ops.new_dynamic_label();
    let symbol = ops.new_dynamic_label();
    let atom_byte = view.string_layout.atom_byte as i32;
    emit_load_u64(ops, 11, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(key), r11 ; jnz =>miss
        ; test Rq(key), Rq(key) ; jz =>miss
        ; movzx r11d, BYTE [Rq(key)]
        ; cmp r11d, i32::from(view.string_layout.string_type_tag) ; jne =>symbol
        // The body records the atom plus one; zero is unrecorded.
        ; mov Rd(atom), [Rq(key) + atom_byte]
        ; sub Rd(atom), 1 ; jae =>ready
        ; push rsi ; push rcx ; push rdx
    );
    if aligned {
        dynasm!(ops ; .arch x64 ; sub rsp, 8);
    }
    dynasm!(ops ; .arch x64
        ; mov r11, [r15 + crate::entry::THREAD_OFFSET as i32]
        ; mov rdi, [r11 + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
        ; mov rsi, Rq(key)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::STRING_KEY_ATOM_LEAF.entry_addr() as u64,
        abi::STUB_STRING_KEY_ATOM_LEAF,
    );
    emit_runtime_call(ops, abi::STUB_STRING_KEY_ATOM_LEAF);
    dynasm!(ops ; .arch x64 ; mov r10, rdx ; mov Rd(atom), eax);
    if aligned {
        dynasm!(ops ; .arch x64 ; add rsp, 8);
    }
    dynasm!(ops ; .arch x64
        ; pop rdx ; pop rcx ; pop rsi
        ; test r10, r10 ; jnz =>miss
        // Content never interned is no shaped object's property; the
        // string records nothing.
        ; cmp Rd(atom), -1 ; je =>uninterned
        ; lea r11d, [Rq(atom) + 1]
        ; mov [Rq(key) + atom_byte], r11d
        ; jmp =>ready
        // A symbol carries the atom of the slot it first keyed, all ones
        // before.
        ; =>symbol
        ; cmp r11d, i32::from(otter_vm::symbol::SYMBOL_BODY_TYPE_TAG) ; jne =>miss
        ; mov Rd(atom), [Rq(key) + otter_vm::symbol::SYMBOL_ATOM_BYTE as i32]
        ; cmp Rd(atom), -1 ; je =>uninterned
        ; =>ready
    );
}

/// `rdx == 0` with the value in `rax`, else `rdx != 0` and nothing observed.
/// Nothing allocates; the only calls are the interning leaf and, for a
/// dictionary-mode receiver the table does not hold or a key the interner
/// never saw, the generic own-data walk.
fn emit_keyed_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let loaded = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let generic = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>label);
    // Entered by `call`: `rsp` is eight bytes past alignment.
    emit_key_atom(
        ops,
        relocations,
        view,
        KEYED_LOAD_KEY,
        9,
        false,
        generic,
        miss,
    );
    // An Array's own named data lives in its bag.
    emit_array_bag(ops, view, KEYED_LOAD_RECEIVER, object, miss);
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        9,
        PropertySourceAccess::Load,
        KEYED_LOAD_RECEIVER,
        None,
        [1, 7, 8, 0],
        Some(0),
        true,
        miss,
        loaded,
        loaded,
    );
    dynasm!(ops ; .arch x64 ; =>object);
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        9,
        PropertySourceAccess::Load,
        KEYED_LOAD_RECEIVER,
        None,
        [1, 7, 8, 0],
        Some(0),
        false,
        generic,
        loaded,
        loaded,
    );
    // The generic own-data walk: the leaf's pair is the routine's result.
    // The receiver is already `rsi`.
    const _: () = assert!(KEYED_LOAD_RECEIVER == 6);
    dynasm!(ops ; .arch x64
        ; =>loaded
        ; xor edx, edx
        ; ret
        ; =>generic
        ; mov rdx, Rq(KEYED_LOAD_KEY)
        ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
        ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
        ; sub rsp, 8
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::KEYED_NAMED_LOAD_LEAF.entry_addr() as u64,
        abi::STUB_KEYED_NAMED_LOAD_LEAF,
    );
    emit_runtime_call(ops, abi::STUB_KEYED_NAMED_LOAD_LEAF);
    dynasm!(ops ; .arch x64
        ; add rsp, 8
        ; ret
        ; =>miss
        ; mov edx, 1
        ; ret);
}

/// Replace an Array receiver in `Rq(receiver)` with its named bag, the
/// storage its own named properties live in; jump to `object` for any other
/// receiver (the register untouched) and to `miss` for an Array without a
/// bag. Clobbers `r10`, `r11`.
fn emit_array_bag(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    object: DynamicLabel,
    miss: DynamicLabel,
) {
    emit_load_u64(ops, 11, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(receiver), r11 ; jnz =>object
        ; test Rq(receiver), Rq(receiver) ; jz =>object
        ; cmp BYTE [Rq(receiver)], view.array_layout.type_tag as i8 ; jne =>object
        ; mov r11d, [Rq(receiver) + view.array_layout.expando_byte as i32]
        ; test r11d, r11d ; jz =>miss
    );
    emit_load_u64(ops, 10, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r10, Rq(receiver)
        ; add r11, r10
        ; mov Rq(receiver), r11
    );
}

/// `rdx == 0` once the value is stored with its barriers, else `rdx == 1`
/// and nothing written.
fn emit_keyed_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let appended = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    // Realign for the barrier calls.
    let object = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>label ; sub rsp, 8);
    emit_key_atom(ops, relocations, view, KEYED_STORE_KEY, 9, true, miss, miss);
    // An Array's own named data lives in its bag, written in place.
    emit_array_bag(ops, view, KEYED_STORE_RECEIVER, object, miss);
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        9,
        PropertySourceAccess::Store,
        KEYED_STORE_RECEIVER,
        Some(KEYED_STORE_VALUE),
        [0, 7, 8, 1],
        None,
        true,
        miss,
        stored,
        appended,
    );
    dynasm!(ops ; .arch x64 ; =>object);
    // The probe's identity temporary (`rdi`) carries an appended child.
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        9,
        PropertySourceAccess::Store,
        KEYED_STORE_RECEIVER,
        Some(KEYED_STORE_VALUE),
        [0, 7, 8, 1],
        None,
        false,
        miss,
        stored,
        appended,
    );
    dynasm!(ops ; .arch x64
        ; test edi, edi
        ; jz =>stored
        ; =>appended);
    emit_child_barrier(ops, relocations, view);
    dynasm!(ops ; .arch x64 ; =>stored);
    super::emit_template_value_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops ; .arch x64
        ; xor edx, edx
        ; add rsp, 8
        ; ret
        ; =>miss
        ; mov edx, 1
        ; add rsp, 8
        ; ret);
}

/// The fit of `CONCAT_LHS + CONCAT_RHS` for two strings, placed by the
/// `CONCAT_SITE` record: `edx` is
/// zero with the new string in `CONCAT_RESULT`, or one when it does not fit
/// and nothing was published. Neither collects nor calls out.
fn emit_concat(ops: &mut Assembler, view: &JitCompileSnapshot, label: DynamicLabel) {
    let miss = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>label);
    // A record places by its run-time decision, never in old space.
    let old_cons = crate::x86_64::allocation::emit_concat(
        ops,
        15,
        view.string_layout,
        [
            crate::allocation::AllocationValue::Register(CONCAT_LHS),
            crate::allocation::AllocationValue::Register(CONCAT_RHS),
        ],
        crate::allocation::LabRegisters {
            buffer: 6,
            candidate: CONCAT_RESULT,
            end: 9,
            scratch: 1,
            size: 11,
        },
        crate::allocation::ConcatPlacement::Record(CONCAT_SITE),
        miss,
    );
    debug_assert!(old_cons.is_none());
    dynasm!(ops
        ; .arch x64
        ; xor edx, edx
        ; ret
        ; =>miss
        ; mov edx, 1
        ; ret
    );
}

/// Load dispatch shared by the load and method routines; a hit leaves the
/// value in `rax` and jumps to `loaded`.
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
        [8, 9, 1],
        0,
        megamorphic,
        miss,
        loaded,
    );
    dynasm!(ops ; .arch x64
        ; =>megamorphic
        ; mov r9d, [Rq(LOAD_SLOT) + IC.atom_byte as i32]);
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        9,
        PropertySourceAccess::Load,
        LOAD_RECEIVER,
        None,
        [1, 7, 8, 0],
        Some(0),
        false,
        miss,
        loaded,
        loaded,
    );
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
    dynasm!(ops ; .arch x64 ; =>label);
    emit_load_handlers(ops, relocations, view, loaded, miss);
    dynasm!(ops ; .arch x64
        ; =>loaded
        ; xor edx, edx
        ; ret
        ; =>miss
        ; sub rsp, 8
        ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_LOAD_PROPERTY),
        abi::STUB_JIT_LOAD_PROPERTY,
    );
    emit_runtime_call(ops, abi::STUB_JIT_LOAD_PROPERTY);
    dynasm!(ops ; .arch x64 ; add rsp, 8 ; ret);
}

/// `rdx == 0` with a closure or native function in `rax`, else `rdx == 1`.
/// Leaf code: no call, no allocation.
fn emit_method(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let loaded = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let callable = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>label);
    emit_load_handlers(ops, relocations, view, loaded, miss);
    dynasm!(ops ; .arch x64 ; =>loaded);
    emit_load_u64(ops, 11, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test rax, r11 ; jnz =>miss
        ; test rax, rax ; jz =>miss
        ; movzx r11d, BYTE [rax]
        ; cmp r11d, i32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG) ; je =>callable
        ; cmp r11d, i32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG)
        ; jne =>miss
        ; =>callable
        ; xor edx, edx
        ; ret
        ; =>miss
        ; mov edx, 1
        ; ret);
}

/// The child-shape edge barrier for a published compressed shape in `edi`,
/// preserving the receiver and value around the collecting-free call.
fn emit_child_barrier(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) {
    emit_load_u64(ops, 11, CAGE_MASK);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(STORE_RECEIVER) ; mov r8d, edi ; add r8, r11
        ; sub rsp, 16
        ; mov [rsp], Rq(STORE_RECEIVER)
        ; mov [rsp + 8], Rq(STORE_VALUE));
    super::emit_template_value_barrier(ops, relocations, view, STORE_RECEIVER, 8);
    dynasm!(ops ; .arch x64
        ; mov Rq(STORE_RECEIVER), [rsp]
        ; mov Rq(STORE_VALUE), [rsp + 8]
        ; add rsp, 16);
}

fn emit_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    label: DynamicLabel,
) {
    let megamorphic = ops.new_dynamic_label();
    let slot_stored = ops.new_dynamic_label();
    let appended = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    // Realign for the barrier and miss calls; the slot survives in the pad
    // word because the megamorphic probe reuses its register.
    dynasm!(ops ; .arch x64
        ; =>label
        ; sub rsp, 8
        ; mov [rsp], Rq(STORE_SLOT));
    property_ic::emit_slot_store(
        ops,
        relocations,
        view,
        STORE_RECEIVER,
        STORE_VALUE,
        STORE_SLOT,
        [8, 9, 7],
        megamorphic,
        miss,
        slot_stored,
    );
    dynasm!(ops ; .arch x64
        ; =>megamorphic
        ; mov r9d, [Rq(STORE_SLOT) + IC.atom_byte as i32]);
    // The probe's identity temporary (`rdi`) carries an appended child,
    // matching the slot's child register.
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        9,
        PropertySourceAccess::Store,
        STORE_RECEIVER,
        Some(STORE_VALUE),
        [0, 7, 8, 1],
        None,
        false,
        miss,
        stored,
        appended,
    );
    dynasm!(ops ; .arch x64
        ; =>slot_stored
        ; mov Rq(STORE_RECEIVER), Rq(STORE_SLOT)
        ; test edi, edi
        ; jz =>stored
        ; =>appended);
    emit_child_barrier(ops, relocations, view);
    dynasm!(ops ; .arch x64 ; =>stored);
    super::emit_template_value_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops ; .arch x64
        ; xor edx, edx
        ; add rsp, 8
        ; ret
        ; =>miss
        ; mov Rq(STORE_SLOT), [rsp]
        ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_STORE_PROPERTY),
        abi::STUB_JIT_STORE_PROPERTY,
    );
    emit_runtime_call(ops, abi::STUB_JIT_STORE_PROPERTY);
    dynasm!(ops ; .arch x64 ; add rsp, 8 ; ret);
}

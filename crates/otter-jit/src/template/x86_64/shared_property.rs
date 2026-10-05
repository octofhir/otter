//! Per-code-object named-property IC routines for x86-64 baseline sites:
//! V8's `LoadIC` / `StoreIC` builtins over the site's native feedback slot.
//!
//! # Contents
//! - [`SharedPropertyProbes`] names one load, one store and one method
//!   routine, requested by sites and emitted once after the body.
//! - Load: the receiver shape selects an entry of the site's slot
//!   (V8 `TryMonomorphicCase` / `HandlePolymorphicCase`) whose handler runs in
//!   machine code: own field, prototype field under its chain proof,
//!   nonexistent key. A megamorphic slot probes the isolate's shared action
//!   table. Anything else is the committed runtime miss, which also updates
//!   the slot.
//! - Store: own-field stores and add-transitions (field write, shape
//!   publication, both barriers), then the shared table, then the miss.
//! - Method: the load selection without a miss call; it returns only a
//!   callable hit.
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
//!
//! # See also
//! - `otter_vm::property_ic` owns the slot, handler kinds and state machine.
//! - `crate::x86_64::property_actions` owns the shared-table probe.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{PROPERTY_IC_LAYOUT as IC, PropertyIcHandlerKind as Kind};
use otter_vm::native_abi as abi;

use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture};
use crate::entry::{OBJECT_BODY_TYPE_TAG, VALUE_UNDEFINED};
use crate::x86_64::call_abi::emit_runtime_call;
use crate::x86_64::property_actions::{AtomOperand, emit_action_probe};
use crate::x86_64::values::{emit_load_runtime_stub, emit_load_u64};

/// Load routine inputs.
pub(crate) const LOAD_RECEIVER: u8 = 6;
pub(crate) const LOAD_SLOT: u8 = 2;

/// Store routine inputs.
pub(crate) const STORE_RECEIVER: u8 = 6;
pub(crate) const STORE_VALUE: u8 = 2;
pub(crate) const STORE_SLOT: u8 = 1;

const CAGE_MASK: u64 = 0xffff_ffff_0000_0000;

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

/// Prove `Rq(receiver)` an ordinary object and select its shape's entry in
/// the slot at `Rq(slot)`: `r10` addresses it. Clobbers `r8`..`r11`.
fn emit_select_entry(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    slot: u8,
    megamorphic: DynamicLabel,
    miss: DynamicLabel,
) {
    let scan = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    emit_load_u64(ops, 10, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(receiver), r10 ; jnz =>miss
        ; test Rq(receiver), Rq(receiver) ; jz =>miss
        ; cmp BYTE [Rq(receiver)], OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss
        ; mov r8d, [Rq(receiver) + view.object_shape_byte as i32]
        ; mov r9d, [Rq(slot) + IC.state_byte as i32]
        ; test r9d, IC.megamorphic_bit as i32 ; jnz =>megamorphic
        ; and r9d, IC.count_mask as i32 ; jz =>miss
        ; lea r10, [Rq(slot) + IC.entries_byte as i32]
        ; =>scan
        ; cmp r8d, [r10 + IC.entry_shape_byte as i32] ; je =>found
        ; dec r9d ; jz =>miss
        ; add r10, IC.entry_bytes as i32 ; jmp =>scan
        ; =>found
    );
}

/// Branch to `miss` unless the entry at `r10` holds no proof (when allowed)
/// or a valid one. Clobbers `rax`.
fn emit_entry_proof(ops: &mut Assembler, proof_required: bool, miss: DynamicLabel) {
    let held = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov rax, [r10 + IC.entry_validity_byte as i32]
        ; test rax, rax);
    if proof_required {
        dynasm!(ops ; .arch x64 ; jz =>miss);
    } else {
        dynasm!(ops ; .arch x64 ; jz =>held);
    }
    dynasm!(ops ; .arch x64 ; cmp DWORD [rax], 0 ; je =>miss ; =>held);
}

/// `Rq(base)` = first word of the bank the field key in `eax` selects on the
/// object at `Rq(holder)`; `rax` becomes the bank-relative index. A missing
/// slab branches to `miss`. Clobbers `r11`.
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
    dynasm!(ops ; .arch x64
        ; test eax, eax ; js =>inline
        ; mov Rd(base), [Rq(holder) + layout.slab_handle_byte as i32]
        ; test Rd(base), Rd(base) ; jz =>miss);
    emit_load_u64(ops, 11, CAGE_MASK);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(holder) ; add Rq(base), r11
        ; add Rq(base), layout.slab_words_byte as i32 ; jmp =>ready
        ; =>inline
        ; and eax, 0x7fff_ffff
        ; lea Rq(base), [Rq(holder) + layout.inline_values_byte as i32]
        ; =>ready);
}

/// Load handler dispatch shared by the load and method routines; a hit
/// leaves the value in `rax` and jumps to `loaded`.
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
    emit_select_entry(ops, view, LOAD_RECEIVER, LOAD_SLOT, megamorphic, miss);
    dynasm!(ops ; .arch x64
        ; movzx eax, BYTE [r10 + IC.entry_kind_byte as i32]
        ; cmp eax, Kind::OwnField as i32 ; je =>own
        ; cmp eax, Kind::PrototypeField as i32 ; je =>prototype
        ; cmp eax, Kind::NonExistent as i32 ; jne =>miss);
    emit_entry_proof(ops, false, miss);
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; jmp =>loaded ; =>prototype);
    emit_entry_proof(ops, true, miss);
    emit_load_u64(ops, 11, CAGE_MASK);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(LOAD_RECEIVER)
        ; mov r9d, [r10 + IC.entry_aux_byte as i32] ; add r9, r11
        ; mov r9d, [r9 + view.shape_prototype_byte as i32]
        ; test r9d, r9d ; jz =>miss ; add r9, r11
        ; jmp =>field
        ; =>own
        ; mov r9, Rq(LOAD_RECEIVER)
        ; =>field
        ; mov eax, [r10 + IC.entry_field_byte as i32]);
    emit_field_bank(ops, view, 9, 8, miss);
    dynasm!(ops ; .arch x64
        ; mov rax, [r8 + rax * 8]
        ; jmp =>loaded
        ; =>megamorphic
        ; mov r9d, [Rq(LOAD_SLOT) + IC.atom_byte as i32]);
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        Some(AtomOperand::Register(9)),
        PropertySourceAccess::Load,
        LOAD_RECEIVER,
        None,
        [1, 7, 8, 0],
        Some(0),
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
    let own = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let publish = ops.new_dynamic_label();
    let appended = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let layout = view.field_layout;
    // Realign for the barrier and miss calls; the slot survives in the pad
    // word because the megamorphic probe reuses its register.
    dynasm!(ops ; .arch x64
        ; =>label
        ; sub rsp, 8
        ; mov [rsp], Rq(STORE_SLOT));
    emit_select_entry(ops, view, STORE_RECEIVER, STORE_SLOT, megamorphic, miss);
    dynasm!(ops ; .arch x64
        ; movzx eax, BYTE [r10 + IC.entry_kind_byte as i32]
        ; cmp eax, Kind::StoreField as i32 ; je =>own
        ; cmp eax, Kind::StoreTransition as i32 ; jne =>miss);
    emit_entry_proof(ops, false, miss);
    dynasm!(ops ; .arch x64
        ; mov edi, [r10 + IC.entry_aux_byte as i32]
        ; test edi, edi ; jz =>miss
        ; mov eax, [r10 + IC.entry_field_byte as i32]
        ; test eax, eax ; js =>inline
        ; mov r8d, [Rq(STORE_RECEIVER) + layout.slab_handle_byte as i32]
        ; test r8d, r8d ; jz =>miss);
    emit_load_u64(ops, 11, CAGE_MASK);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(STORE_RECEIVER) ; add r8, r11
        ; cmp eax, [r8 + layout.slab_capacity_byte as i32] ; jae =>miss
        ; mov [r8 + rax * 8 + layout.slab_words_byte as i32], Rq(STORE_VALUE)
        ; jmp =>publish
        ; =>inline
        ; and eax, 0x7fff_ffff
        ; mov [Rq(STORE_RECEIVER) + rax * 8 + layout.inline_values_byte as i32], Rq(STORE_VALUE)
        ; =>publish
        ; mov [Rq(STORE_RECEIVER) + view.object_shape_byte as i32], edi
        ; =>appended);
    emit_child_barrier(ops, relocations, view);
    dynasm!(ops ; .arch x64
        ; jmp =>stored
        ; =>own
        ; mov eax, [r10 + IC.entry_field_byte as i32]);
    emit_field_bank(ops, view, STORE_RECEIVER, 8, miss);
    dynasm!(ops ; .arch x64
        ; mov [r8 + rax * 8], Rq(STORE_VALUE)
        ; jmp =>stored
        ; =>megamorphic
        ; mov r9d, [Rq(STORE_SLOT) + IC.atom_byte as i32]);
    // The probe's identity temporary (`rdi`) carries an appended child,
    // matching the transition path above.
    emit_action_probe(
        ops,
        relocations,
        view,
        view.property_action_cache,
        Some(AtomOperand::Register(9)),
        PropertySourceAccess::Store,
        STORE_RECEIVER,
        Some(STORE_VALUE),
        [0, 7, 8, 1],
        None,
        miss,
        stored,
        appended,
    );
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

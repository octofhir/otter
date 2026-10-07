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
//! - Concat: the nursery string-concatenation fit every `Add` site shares
//!   instead of inlining it.
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
use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
use otter_vm::native_abi as abi;

use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture};
use crate::x86_64::call_abi::emit_runtime_call;
use crate::x86_64::property_actions::{AtomOperand, emit_action_probe};
use crate::x86_64::property_ic;
use crate::x86_64::values::{emit_load_runtime_stub, emit_load_u64};

/// Load routine inputs.
pub(crate) const LOAD_RECEIVER: u8 = 6;
pub(crate) const LOAD_SLOT: u8 = 2;

/// Store routine inputs.
pub(crate) const STORE_RECEIVER: u8 = 6;
pub(crate) const STORE_VALUE: u8 = 2;
pub(crate) const STORE_SLOT: u8 = 1;

const CAGE_MASK: u64 = 0xffff_ffff_0000_0000;

/// Concat routine inputs: the two operands.
pub(crate) const CONCAT_LHS: u8 = 0;
pub(crate) const CONCAT_RHS: u8 = 8;
/// Concat routine result on a fit.
pub(crate) const CONCAT_RESULT: u8 = 7;

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

/// The nursery fit of `CONCAT_LHS + CONCAT_RHS` for two strings: `edx` is
/// zero with the new string in `CONCAT_RESULT`, or one when it does not fit
/// and nothing was published. Neither collects nor calls out.
fn emit_concat(ops: &mut Assembler, view: &JitCompileSnapshot, label: DynamicLabel) {
    let miss = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>label);
    crate::x86_64::allocation::emit_concat(
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
        miss,
    );
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
    dynasm!(ops ; .arch x64
        ; =>slot_stored
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

//! Generic element stores: V8's `KeyedStoreIC_Megamorphic` for a site the
//! optimizer does not specialize.
//!
//! # Contents
//! - [`Codegen::emit_store_keyed_cached`] stores into an Array's dense
//!   storage of any kind inline, a name key through the code object's keyed
//!   table routine, and every other store in the runtime.
//!
//! # Invariants
//! - Every guard precedes the one store; a failed guard has written nothing
//!   and takes the next path, never a deopt.
//! - A hole is written only while the realm's array-index accessor protector
//!   holds and the array has no exotic sidecar: no setter, proxy or exotic
//!   own state can then observe the addition.
//! - A store into numeric storage fills its hole as the VM does: the bit is
//!   cleared and counted, and the last one makes the storage packed. A
//!   tagged cell value takes the element write barrier.
//! - The keyed table routine neither allocates nor collects; the runtime
//!   operation grows storage and records the site's feedback.
//!
//! # See also
//! - [`super::Codegen::emit_store_element`] for the speculative store.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use super::{Codegen, CommittedArgument, DynamicLabel, NodeId, THREAD_OFFSET, VALUE_HOLE, abi};
use crate::Unsupported;
use crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::jit::{JitArrayStorage, JitHoleBitmap};

/// Bytes the slow path parks the three operands in.
const PARKED_BYTES: u32 = 32;

impl Codegen<'_> {
    /// `X(receiver)[X(key)] = X(value)`; `length` and `base` are temporaries.
    pub(super) fn emit_store_keyed_cached(
        &mut self,
        node: NodeId,
        [receiver, key, value]: [u8; 3],
        [length, base]: [u8; 2],
    ) -> Result<(), Unsupported> {
        let storage = JitArrayStorage::current();
        let numeric = storage.numeric;
        let other = self.ops.new_dynamic_label();
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let tagged = self.ops.new_dynamic_label();
        let holey = self.ops.new_dynamic_label();
        let store_double = self.ops.new_dynamic_label();
        let store_tagged = self.ops.new_dynamic_label();
        self.load_immediate(16, super::NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(receiver), x16
            ; b.ne =>other
            ; ldrb w17, [X(receiver)]
            ; cmp w17, u32::from(storage.type_tag)
            ; b.ne =>other
            ; ldrb w17, [X(receiver), storage.own_guard_byte]
            ; cbnz w17, =>runtime
            // A non-negative int32 key below the live dense length.
            ; asr x16, X(key), super::INT32_TAG_SHIFT
            ; cmn x16, 1
            ; b.ne =>runtime
            ; tbnz W(key), 31, =>runtime
            ; ldr W(length), [X(receiver), storage.length_byte]
            ; cmp W(length), W(key)
            ; b.ls =>runtime
            ; ldr X(base), [X(receiver), storage.base_byte]
            ; ldrb w17, [X(receiver), numeric.kind_byte]
            ; cmp w17, u32::from(storage.tagged_kind)
            ; b.eq =>tagged
            ; cmp w17, u32::from(numeric.holey_kind)
            ; b.eq =>holey
            ; cmp w17, u32::from(numeric.packed_kind)
            ; b.ne =>runtime
        );
        // Packed numeric storage has no hole below its length.
        self.emit_tagged_to_float(value, 31, runtime);
        dynasm!(self.ops ; .arch aarch64 ; b =>store_double ; =>holey);
        self.emit_tagged_to_float(value, 31, runtime);
        self.emit_hole_bit(numeric, base, key);
        dynasm!(self.ops ; .arch aarch64 ; b.eq =>store_double);
        self.emit_hole_store_permit(storage, receiver, runtime);
        // The length is dead past the bounds check.
        self.emit_fill_hole(numeric, receiver, base, key, length);
        dynasm!(self.ops
            ; .arch aarch64
            ; =>store_double
            ; str d31, [X(base), W(key), uxtw #3]
            ; b =>done
            ; =>tagged
        );
        self.load_immediate(17, VALUE_HOLE);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr x16, [X(base), W(key), uxtw #3]
            ; cmp x16, x17
            ; b.ne =>store_tagged
        );
        self.emit_hole_store_permit(storage, receiver, runtime);
        dynasm!(self.ops
            ; .arch aarch64
            ; =>store_tagged
            ; str X(value), [X(base), W(key), uxtw #3]
        );
        self.emit_element_write_barrier(node, base, key, value);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>other);
        self.emit_keyed_table_store(node, [receiver, key, value], runtime, done);
        dynasm!(self.ops ; .arch aarch64 ; =>runtime);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_STORE_ELEMENT,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Value(key),
                CommittedArgument::Value(value),
            ],
            None,
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// A name key's store through the code object's keyed table routine:
    /// to `stored` when it stored, else to `missed` with every register as
    /// it was. Only a string key is offered.
    fn emit_keyed_table_store(
        &mut self,
        node: NodeId,
        [receiver, key, value]: [u8; 3],
        missed: DynamicLabel,
        stored: DynamicLabel,
    ) {
        use crate::template::arm64::shared_property::{
            KEYED_STORE_KEY, KEYED_STORE_RECEIVER, KEYED_STORE_VALUE,
        };
        let string_tag = self.view.string_layout.string_type_tag;
        self.load_immediate(16, super::NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(key), x16
            ; b.ne =>missed
            ; ldrb w16, [X(key)]
            ; cmp w16, u32::from(string_tag)
            ; b.ne =>missed
        );
        let routine = self.shared_property.keyed_label(&mut self.ops, true);
        let live = self.allocation.node(node).live_registers.clone();
        // The operands may sit in registers the routine's inputs name: park
        // them below the saved registers and load the inputs from there.
        dynasm!(self.ops
            ; .arch aarch64
            ; sub sp, sp, PARKED_BYTES
            ; str X(receiver), [sp]
            ; str X(key), [sp, #8]
            ; str X(value), [sp, #16]
        );
        let saved = self.emit_save_registers(&live);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr X(KEYED_STORE_RECEIVER), [sp, saved]
            ; ldr X(KEYED_STORE_KEY), [sp, saved + 8]
            ; ldr X(KEYED_STORE_VALUE), [sp, saved + 16]
            ; bl =>routine
            ; mov x17, x1
        );
        self.emit_restore_registers(&live, saved);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr X(receiver), [sp]
            ; ldr X(key), [sp, #8]
            ; ldr X(value), [sp, #16]
            ; add sp, sp, PARKED_BYTES
            ; cbz x17, =>stored
            ; b =>missed
        );
    }

    /// Branch to `refused` unless a store into a hole of the Array in
    /// `X(receiver)` is an ordinary own addition: no indexed accessor exists
    /// on the realm's element prototypes and the array has no exotic sidecar.
    fn emit_hole_store_permit(
        &mut self,
        storage: JitArrayStorage,
        receiver: u8,
        refused: DynamicLabel,
    ) {
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET]
            ; ldrb w16, [x16]
            ; cbnz w16, =>refused
            ; ldr w16, [X(receiver), storage.exotic_byte]
            ; cbnz w16, =>refused
        );
    }

    /// Clear the set hole bit of `W(index)` in the numeric storage at
    /// `X(base)` and count the hole filled; the last one makes the storage,
    /// and the cached kind of the Array in `X(receiver)`, packed, as the
    /// VM's own store does. Clobbers `x16`, `x17` and `X(scratch)`.
    fn emit_fill_hole(
        &mut self,
        holes: JitHoleBitmap,
        receiver: u8,
        base: u8,
        index: u8,
        scratch: u8,
    ) {
        let still_holey = self.ops.new_dynamic_label();
        let (capacity, count, storage_kind) = (
            holes.capacity_byte,
            holes.hole_count_byte,
            holes.storage_kind_byte,
        );
        dynasm!(self.ops
            ; .arch aarch64
            ; ldur w16, [X(base), capacity]
            ; add x16, X(base), x16, lsl #3
            ; lsr w17, W(index), #6
            ; add x16, x16, x17, lsl #3
            ; ldr x17, [x16]
            // The shift reads the index's low six bits: its bit in the word.
            ; mov X(scratch), #1
            ; lsl X(scratch), X(scratch), X(index)
            ; bic x17, x17, X(scratch)
            ; str x17, [x16]
            ; ldur w16, [X(base), count]
            ; subs w16, w16, #1
            ; stur w16, [X(base), count]
            ; b.ne =>still_holey
            ; mov w16, u32::from(holes.packed_kind)
            ; sturb w16, [X(base), storage_kind]
            ; strb w16, [X(receiver), holes.kind_byte]
            ; =>still_holey
        );
    }
}

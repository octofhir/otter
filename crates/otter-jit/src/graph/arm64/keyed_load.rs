//! Generic element loads for a site the optimizer does not specialize.
//!
//! # Contents
//! - [`Codegen::emit_load_keyed_cached`] reads an Array's dense storage of
//!   any kind inline, a name key through the code object's keyed table
//!   routine, and every other load in the runtime.
//!
//! # Invariants
//! - Inline paths read only: an element present in dense storage is an own
//!   data property while the array's own-state guard byte reads zero.
//! - A hole or an index past the dense length reads `undefined` inline
//!   only while the realm's array-index protector holds and the array has
//!   no sidecar: no index key then exists on the default prototype chain.
//!   Otherwise it, a non-Array receiver with a non-string key, and a table
//!   miss complete in the runtime, which walks the prototype chain, runs
//!   accessors and records the site's feedback.
//! - Both operands stay intact until the result is written; the keyed table
//!   routine neither allocates nor collects.
//!
//! # See also
//! - [`super::keyed_store`] for the generic keyed store.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use super::{
    Codegen, CommittedArgument, DynamicLabel, NodeId, THREAD_OFFSET, VALUE_HOLE, VALUE_UNDEFINED,
    abi,
};
use crate::Unsupported;
use crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::jit::JitArrayStorage;

/// Bytes the table path parks the two operands in.
const PARKED_BYTES: u32 = 16;

impl Codegen<'_> {
    /// `X(destination)` = `X(receiver)[X(key)]`; `length` and `base` are
    /// temporaries, `double` a floating temporary.
    pub(super) fn emit_load_keyed_cached(
        &mut self,
        node: NodeId,
        [receiver, key]: [u8; 2],
        [length, base, index]: [u8; 3],
        double: u8,
        destination: u8,
    ) -> Result<(), Unsupported> {
        let storage = JitArrayStorage::current();
        let numeric = storage.numeric;
        let other = self.ops.new_dynamic_label();
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let tagged = self.ops.new_dynamic_label();
        let holey = self.ops.new_dynamic_label();
        let boxed = self.ops.new_dynamic_label();
        let absent = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(receiver), #0xfffe_0000_0000_0007
            ; b.ne =>other
            ; ldrb w17, [X(receiver)]
            ; cmp w17, u32::from(storage.type_tag)
            ; b.ne =>other
            ; ldrb w17, [X(receiver), storage.own_guard_byte]
            ; tbnz w17, storage.own_exotic_bit, =>runtime
        );
        crate::arm64::index::emit_number_index(&mut self.ops, key, index, double, runtime);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr W(length), [X(receiver), storage.length_byte]
            ; cmp W(length), W(index)
            ; b.ls =>absent
            ; ldr X(base), [X(receiver), storage.base_byte]
            ; ldrb w17, [X(receiver), numeric.kind_byte]
            ; cmp w17, u32::from(storage.tagged_kind)
            ; b.eq =>tagged
            ; cmp w17, u32::from(numeric.holey_kind)
            ; b.eq =>holey
            ; cmp w17, u32::from(numeric.packed_kind)
            ; b.ne =>runtime
            // Packed numeric storage has no hole below its length.
            ; ldr D(double), [X(base), W(index), uxtw #3]
            ; b =>boxed
            ; =>holey
        );
        self.emit_hole_bit(numeric, base, index);
        dynasm!(self.ops
            ; .arch aarch64
            ; b.ne =>absent
            ; ldr D(double), [X(base), W(index), uxtw #3]
            ; =>boxed
        );
        self.emit_box_float64(double, destination);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>tagged
            ; ldr x16, [X(base), W(index), uxtw #3]
        );
        self.load_immediate(17, VALUE_HOLE);
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp x16, x17
            ; b.eq =>absent
            ; mov X(destination), x16
            ; b =>done
            // No own element at the index: with no index key anywhere on the
            // realm's default chain and no sidecar to replace the array's
            // prototype, the ordinary [[Get]] answers `undefined`.
            ; =>absent
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET]
            ; ldrb w16, [x16]
            ; cbnz w16, =>runtime
            ; ldr w16, [X(receiver), storage.exotic_byte]
            ; cbnz w16, =>runtime
        );
        self.load_immediate(destination, VALUE_UNDEFINED);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>other
        );
        self.emit_keyed_table_load(node, [receiver, key], destination, runtime, done);
        dynasm!(self.ops ; .arch aarch64 ; =>runtime);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_LOAD_ELEMENT,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Value(key),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// A name key's load through the code object's keyed table routine:
    /// the value in `X(destination)` and on to `loaded` on a hit, else to
    /// `missed` with every register as it was. Only a string key is offered.
    fn emit_keyed_table_load(
        &mut self,
        node: NodeId,
        [receiver, key]: [u8; 2],
        destination: u8,
        missed: DynamicLabel,
        loaded: DynamicLabel,
    ) {
        use crate::template::arm64::shared_property::{KEYED_LOAD_KEY, KEYED_LOAD_RECEIVER};
        let string_tag = self.view.string_layout.string_type_tag;
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(key), #0xfffe_0000_0000_0007
            ; b.ne =>missed
            ; ldrb w16, [X(key)]
            ; cmp w16, u32::from(string_tag)
            ; b.ne =>missed
        );
        let live = self.allocation.node(node).live_registers.clone();
        // The operands may sit in registers the routine's inputs name: park
        // them below the saved registers and load the inputs from there.
        dynasm!(self.ops
            ; .arch aarch64
            ; sub sp, sp, PARKED_BYTES
            ; stp X(receiver), X(key), [sp]
        );
        let saved = self.emit_save_registers(&live);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr X(KEYED_LOAD_RECEIVER), [sp, saved]
            ; ldr X(KEYED_LOAD_KEY), [sp, saved + 8]
        );
        crate::template::arm64::shared_property::emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.shared_property,
            crate::routines::PropertyRoutine::KeyedLoad,
        );
        dynasm!(self.ops
            ; .arch aarch64
            ; mov x16, x0
            ; mov x17, x1
        );
        self.emit_restore_registers(&live, saved);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldp X(receiver), X(key), [sp]
            ; add sp, sp, PARKED_BYTES
            ; cbnz x17, =>missed
            ; mov X(destination), x16
            ; b =>loaded
        );
    }
}

//! Generic element loads: V8's `KeyedLoadIC_Megamorphic` for a site the
//! optimizer does not specialize.
//!
//! # Contents
//! - [`Codegen::emit_load_keyed_cached`] reads an Array's dense storage of
//!   any kind inline, a name key through the code object's keyed table
//!   routine, and every other load in the runtime.
//!
//! # Invariants
//! - Inline paths read only: an element present in dense storage is an own
//!   data property while the array's own-state guard byte reads zero.
//! - A hole, an index past the dense length, a non-Array receiver with a
//!   non-string key, and a table miss complete in the runtime, which walks
//!   the prototype chain, runs accessors and records the site's feedback.
//! - Both operands stay intact until the result is written; the keyed table
//!   routine neither allocates nor collects.
//!
//! # See also
//! - [`super::keyed_store`] for the generic keyed store.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use super::{Codegen, CommittedArgument, DynamicLabel, NOT_CELL_MASK, NodeId, VALUE_HOLE, abi};
use crate::Unsupported;
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
        [length, base]: [u8; 2],
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
        self.load_immediate(16, NOT_CELL_MASK);
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
            // Packed numeric storage has no hole below its length.
            ; ldr D(double), [X(base), W(key), uxtw #3]
            ; b =>boxed
            ; =>holey
        );
        self.emit_hole_bit(numeric, base, key);
        dynasm!(self.ops
            ; .arch aarch64
            ; b.ne =>runtime
            ; ldr D(double), [X(base), W(key), uxtw #3]
            ; =>boxed
        );
        self.emit_box_float64(double, destination);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>tagged
            ; ldr x16, [X(base), W(key), uxtw #3]
        );
        self.load_immediate(17, VALUE_HOLE);
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp x16, x17
            ; b.eq =>runtime
            ; mov X(destination), x16
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
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(key), x16
            ; b.ne =>missed
            ; ldrb w16, [X(key)]
            ; cmp w16, u32::from(string_tag)
            ; b.ne =>missed
        );
        let routine = self.shared_property.keyed_label(&mut self.ops, false);
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
            ; bl =>routine
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

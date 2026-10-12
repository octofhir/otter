//! Generic element stores for a site the optimizer does not specialize.
//!
//! # Contents
//! - [`Codegen::emit_store_keyed_cached`] stores into an Array's dense
//!   storage of any kind inline, appends at its length through the VM's
//!   push leaf, stores a Number into a typed view of any non-BigInt kind
//!   inline, a name key through the code object's keyed table routine, and
//!   every other store in the runtime.
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
//! - An append is an ordinary own addition under the same permit as a
//!   hole store; the push leaf completes it only within the storage's
//!   capacity and its own dense-range guards, so it never allocates.
//! - A typed view is stored into inline only through its cached element
//!   base while the detach protector holds, at an int32 index below its
//!   length, and with a value that is already a Number: the conversion to
//!   the element runs no user code, and the store is the whole effect.
//! - The keyed table routine neither allocates nor collects; the runtime
//!   operation grows storage and records the site's feedback.
//!
//! # See also
//! - [`super::Codegen::emit_store_element`] for the speculative store.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use super::{
    Codegen, CommittedArgument, DETACH_PROTECTOR, DOUBLE_OFFSET, DynamicLabel, INT32_TAG_SHIFT,
    NUMBER_TAG, NodeId, RelocationTarget, THREAD_OFFSET, VALUE_HOLE, VM_THREAD_GC_HEAP_OFFSET, abi,
    emit_load_symbol_u64,
};
use crate::Unsupported;
use crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::binary::TypedArrayKind;
use otter_vm::jit::{JitArrayStorage, JitHoleBitmap, JitTypedArrayStorage};

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
        let in_bounds = self.ops.new_dynamic_label();
        let not_array = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(receiver), #0xfffe_0000_0000_0007
            ; b.ne =>other
            ; ldrb w17, [X(receiver)]
            ; cmp w17, u32::from(storage.type_tag)
            ; b.ne =>not_array
            ; ldrb w17, [X(receiver), storage.own_guard_byte]
            ; cbnz w17, =>runtime
            // A non-negative int32 key below the live dense length.
            ; asr x16, X(key), super::INT32_TAG_SHIFT
            ; cmn x16, 1
            ; b.ne =>runtime
            ; tbnz W(key), 31, =>runtime
            ; ldr W(length), [X(receiver), storage.length_byte]
            ; cmp W(length), W(key)
            ; b.hi =>in_bounds
            ; b.ne =>runtime
        );
        // A key at the length appends.
        self.emit_hole_store_permit(storage, receiver, runtime);
        self.emit_array_push_leaf(node, [receiver, key, value], runtime, done);
        dynasm!(self.ops
            ; .arch aarch64
            ; =>in_bounds
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
        self.emit_element_write_barrier(node, base, key, value, &[]);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>not_array
            ; cmp w17, u32::from(JitTypedArrayStorage::current().type_tag)
            ; b.ne =>other
        );
        self.emit_typed_view_store([receiver, key, value], [length, base], runtime, done);
        dynasm!(self.ops ; .arch aarch64 ; =>other);
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

    /// Store the Number in `X(value)` at int32 `X(key)` of the typed view in
    /// `X(receiver)`, converted to its element kind: to `stored` when it
    /// stored, else to `refused` with nothing written. `kind` and `base` are
    /// temporaries; `x16`, `x17` and `d31` are clobbered.
    fn emit_typed_view_store(
        &mut self,
        [receiver, key, value]: [u8; 3],
        [kind, base]: [u8; 2],
        refused: DynamicLabel,
        stored: DynamicLabel,
    ) {
        let view = JitTypedArrayStorage::current();
        let code = |kind: TypedArrayKind| kind as u32;
        let double = self.ops.new_dynamic_label();
        let widen = self.ops.new_dynamic_label();
        let clamped = self.ops.new_dynamic_label();
        let integer = self.ops.new_dynamic_label();
        let half = self.ops.new_dynamic_label();
        let word = self.ops.new_dynamic_label();
        let float = self.ops.new_dynamic_label();
        let float64 = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; asr x16, X(key), INT32_TAG_SHIFT
            ; cmn x16, 1
            ; b.ne =>refused
            ; tbnz W(key), 31, =>refused
            // The cached base stands for the buffer's liveness and extent
            // only while no buffer was ever detached.
            ; ldr X(base), [X(receiver), view.data_byte]
            ; cbz X(base), =>refused
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, DETACH_PROTECTOR]
            ; cbz x16, =>refused
            ; ldrb w16, [x16]
            ; cbnz w16, =>refused
            ; ldr X(kind), [X(receiver), view.length_byte]
            ; cmp XSP(kind), W(key), uxtw
            ; b.ls =>refused
            ; ldr W(kind), [X(receiver), view.kind_byte]
            ; asr x16, X(value), INT32_TAG_SHIFT
            ; cmn x16, 1
            ; b.ne =>double
            // An int32: an integer element keeps its low bits.
            ; mov w16, W(value)
            ; cmp WSP(kind), code(TypedArrayKind::Uint32)
            ; b.hi =>widen
            ; cmp WSP(kind), code(TypedArrayKind::Uint8Clamped)
            ; b.ne =>integer
            ; cmp w16, #0
            ; csel w16, wzr, w16, lt
            ; mov w17, #255
            ; cmp w16, w17
            ; csel w16, w17, w16, gt
            ; b =>clamped
            ; =>widen
            ; scvtf d31, W(value)
            ; b =>float
            ; =>double
            ; tst X(value), NUMBER_TAG
            ; b.eq =>refused
        );
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; sub x16, X(value), x17
            ; fmov d31, x16
            ; cmp WSP(kind), code(TypedArrayKind::Uint32)
            ; b.hi =>float
            ; cmp WSP(kind), code(TypedArrayKind::Uint8Clamped)
            ; b.ne >modular
            // §7.1.12 ToUint8Clamp of a double: round half to even, NaN and
            // negatives to 0, saturated to 255.
            ; fcvtnu w16, d31
            ; mov w17, #255
            ; cmp w16, w17
            ; csel w16, w17, w16, hi
            ; b =>clamped
            ; modular:
        );
        if crate::arm64::has_javascript_conversion() {
            crate::arm64::emit_fjcvtzs(&mut self.ops, 31, 16);
            dynasm!(self.ops ; .arch aarch64 ; b =>integer);
        } else {
            dynasm!(self.ops ; .arch aarch64 ; b =>refused);
        }
        dynasm!(self.ops
            ; .arch aarch64
            ; =>clamped
            ; strb w16, [X(base), W(key), uxtw]
            ; b =>stored
            // `w16` holds the int32 whose low bits the element keeps.
            ; =>integer
            ; cmp WSP(kind), code(TypedArrayKind::Uint8)
            ; b.hi =>half
            ; strb w16, [X(base), W(key), uxtw]
            ; b =>stored
            ; =>half
            ; cmp WSP(kind), code(TypedArrayKind::Uint16)
            ; b.hi =>word
            ; strh w16, [X(base), W(key), uxtw #1]
            ; b =>stored
            ; =>word
            ; str w16, [X(base), W(key), uxtw #2]
            ; b =>stored
            // `d31` holds the Number; BigInt and half-precision views refuse.
            ; =>float
            ; cmp WSP(kind), code(TypedArrayKind::Float32)
            ; b.ne =>float64
            ; fcvt s31, d31
            ; str s31, [X(base), W(key), uxtw #2]
            ; b =>stored
            ; =>float64
            ; cmp WSP(kind), code(TypedArrayKind::Float64)
            ; b.ne =>refused
            ; str d31, [X(base), W(key), uxtw #3]
            ; b =>stored
        );
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
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(key), #0xfffe_0000_0000_0007
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

    /// Append `X(value)` to the Array in `X(receiver)` through the VM's push
    /// leaf: to `stored` when it appended, else to `missed` with every
    /// register as it was. The leaf refuses a full storage or a range its
    /// own guards do not admit, and never allocates.
    fn emit_array_push_leaf(
        &mut self,
        node: NodeId,
        [receiver, key, value]: [u8; 3],
        missed: DynamicLabel,
        stored: DynamicLabel,
    ) {
        let leaf = otter_vm::runtime_stubs::ARRAY_PUSH_LEAF;
        let live = self.allocation.node(node).live_registers.clone();
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
            ; ldr x1, [sp, saved]
            ; ldr x2, [sp, saved + 16]
            ; ldr x0, [x20, THREAD_OFFSET]
            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        );
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            leaf.entry_addr() as u64,
            RelocationTarget::runtime_stub(leaf.descriptor),
        );
        dynasm!(self.ops ; .arch aarch64 ; blr x16 ; mov x17, x1);
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

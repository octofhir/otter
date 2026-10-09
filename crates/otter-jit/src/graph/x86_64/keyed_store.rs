//! x86-64 generic element stores: V8's `KeyedStoreIC_Megamorphic` for a
//! site the optimizer does not specialize.
//!
//! # Contents
//! - [`Codegen::emit_keyed_store`] stores into an Array's dense storage of
//!   any kind inline, a name key through the code object's keyed table
//!   routine, and every other store in the runtime.
//!
//! # Invariants
//! - Every guard precedes the one store; a failed guard has written nothing
//!   and takes the next path, never a deopt.
//! - A hole is written only while the realm's array-index accessor protector
//!   holds and the array has no exotic sidecar.
//! - A store into numeric storage fills its hole as the VM does: the bit is
//!   cleared and counted, and the last one makes the storage packed. A
//!   tagged cell value takes the element write barrier.
//! - The keyed table routine neither allocates nor collects; the runtime
//!   operation grows storage and records the site's feedback.
//!
//! # See also
//! - `crate::graph::arm64::keyed_store` for the same operation on AArch64.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::jit::{JitArrayStorage, JitHoleBitmap};
use otter_vm::value::tag;

use super::scalar::conversions::emit_tagged_number_to_float;
use super::{Codegen, CommittedArgument, abi};
use crate::{
    Unsupported,
    graph::ir::{Kind, NodeId},
};

/// Bytes the slow path parks the three operands in.
const PARKED_BYTES: u32 = 32;

impl Codegen<'_> {
    pub(super) fn emit_keyed_store(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        if !matches!(self.graph.node(node).kind, Kind::StoreKeyedCached { .. }) {
            return Ok(false);
        }
        let assigned = self.allocation.node(node).clone();
        let input = |i| Self::gp(assigned.inputs[i]);
        let (receiver, key, value) = (input(0), input(1), input(2));
        let (length, base) = (assigned.gp_temps[0], assigned.gp_temps[1]);
        let storage = JitArrayStorage::current();
        let numeric = storage.numeric;
        let other = self.ops.new_dynamic_label();
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let tagged = self.ops.new_dynamic_label();
        let holey = self.ops.new_dynamic_label();
        let store_double = self.ops.new_dynamic_label();
        let store_tagged = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(receiver), r10 ; jnz =>other
            ; cmp BYTE [Rq(receiver)], storage.type_tag as i8 ; jne =>other
            ; cmp BYTE [Rq(receiver) + storage.own_guard_byte as i32], 0 ; jne =>runtime
            // A non-negative int32 key below the live dense length.
            ; mov r10, Rq(key) ; sar r10, 49 ; cmp r10, -1 ; jne =>runtime
            ; test Rd(key), Rd(key) ; js =>runtime
            ; mov Rd(length), [Rq(receiver) + storage.length_byte as i32]
            ; cmp Rd(length), Rd(key) ; jbe =>runtime
            ; mov Rq(base), [Rq(receiver) + storage.base_byte as i32]
            ; movzx r10d, BYTE [Rq(receiver) + numeric.kind_byte as i32]
            ; cmp r10d, i32::from(storage.tagged_kind) ; je =>tagged
            ; cmp r10d, i32::from(numeric.holey_kind) ; je =>holey
            ; cmp r10d, i32::from(numeric.packed_kind) ; jne =>runtime
        );
        // Packed numeric storage has no hole below its length.
        emit_tagged_number_to_float(&mut self.ops, value, 15, runtime);
        dynasm!(self.ops ; .arch x64 ; jmp =>store_double ; =>holey);
        emit_tagged_number_to_float(&mut self.ops, value, 15, runtime);
        self.emit_hole_bit(numeric, base, key);
        dynasm!(self.ops ; .arch x64 ; jnc =>store_double);
        self.emit_hole_store_permit(storage, receiver, runtime);
        // The length is dead past the bounds check.
        self.emit_fill_hole(numeric, receiver, base, key, length);
        dynasm!(self.ops ; .arch x64
            ; =>store_double
            ; mov r10d, Rd(key) ; movsd [Rq(base) + r10 * 8], xmm15
            ; jmp =>done
            ; =>tagged
        );
        self.load_immediate(11, tag::VALUE_HOLE);
        dynasm!(self.ops ; .arch x64
            ; mov r10d, Rd(key) ; cmp [Rq(base) + r10 * 8], r11 ; jne =>store_tagged
        );
        self.emit_hole_store_permit(storage, receiver, runtime);
        dynasm!(self.ops ; .arch x64
            ; =>store_tagged
            ; mov r10d, Rd(key) ; mov [Rq(base) + r10 * 8], Rq(value)
        );
        self.emit_element_write_barrier(node, base, key, value);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>other);
        self.emit_keyed_table_store(node, [receiver, key, value], runtime, done);
        dynasm!(self.ops ; .arch x64 ; =>runtime);
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
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(true)
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
        use crate::template::x86_64::shared_property::{
            KEYED_STORE_KEY, KEYED_STORE_RECEIVER, KEYED_STORE_VALUE,
        };
        let string_tag = self.view.string_layout.string_type_tag;
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(key), r10 ; jnz =>missed
            ; cmp BYTE [Rq(key)], string_tag as i8 ; jne =>missed
        );
        let routine = self.shared_property.keyed_label(&mut self.ops, true);
        let live = self.allocation.node(node).live_registers.clone();
        // The operands may sit in registers the routine's inputs name: park
        // them below the saved registers and load the inputs from there.
        dynasm!(self.ops ; .arch x64
            ; sub rsp, PARKED_BYTES as i32
            ; mov [rsp], Rq(receiver) ; mov [rsp + 8], Rq(key) ; mov [rsp + 16], Rq(value)
        );
        self.sp_delta += PARKED_BYTES;
        let saved = self.emit_save_registers(&live) as i32;
        dynasm!(self.ops ; .arch x64
            ; mov Rq(KEYED_STORE_RECEIVER), [rsp + saved]
            ; mov Rq(KEYED_STORE_KEY), [rsp + saved + 8]
            ; mov Rq(KEYED_STORE_VALUE), [rsp + saved + 16]
            ; call =>routine
            ; mov r11, rdx
        );
        self.emit_restore_registers(&live, saved as u32);
        dynasm!(self.ops ; .arch x64
            ; mov Rq(receiver), [rsp] ; mov Rq(key), [rsp + 8] ; mov Rq(value), [rsp + 16]
            ; add rsp, PARKED_BYTES as i32
            ; test r11, r11 ; jz =>stored ; jmp =>missed
        );
        self.sp_delta -= PARKED_BYTES;
    }

    /// Branch to `refused` unless a store into a hole of the Array in
    /// `Rq(receiver)` is an ordinary own addition: no indexed accessor exists
    /// on the realm's element prototypes and the array has no exotic sidecar.
    fn emit_hole_store_permit(
        &mut self,
        storage: JitArrayStorage,
        receiver: u8,
        refused: DynamicLabel,
    ) {
        dynasm!(self.ops ; .arch x64
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET as i32]
            ; cmp BYTE [r10], 0 ; jne =>refused
            ; cmp DWORD [Rq(receiver) + storage.exotic_byte as i32], 0 ; jne =>refused
        );
    }

    /// Clear the set hole bit of `Rd(index)` in the numeric storage at
    /// `Rq(base)` and count the hole filled; the last one makes the storage,
    /// and the cached kind of the Array in `Rq(receiver)`, packed, as the
    /// VM's own store does. Clobbers r10, r11 and `Rq(scratch)`.
    fn emit_fill_hole(
        &mut self,
        holes: JitHoleBitmap,
        receiver: u8,
        base: u8,
        index: u8,
        scratch: u8,
    ) {
        let still_holey = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; mov r11d, [Rq(base) + holes.capacity_byte]
            ; lea r11, [Rq(base) + r11 * 8]
            ; mov Rd(scratch), Rd(index)
            ; btr QWORD [r11], Rq(scratch)
            ; sub DWORD [Rq(base) + holes.hole_count_byte], 1
            ; jnz =>still_holey
            ; mov BYTE [Rq(base) + holes.storage_kind_byte], holes.packed_kind as i8
            ; mov BYTE [Rq(receiver) + holes.kind_byte as i32], holes.packed_kind as i8
            ; =>still_holey
        );
    }
}

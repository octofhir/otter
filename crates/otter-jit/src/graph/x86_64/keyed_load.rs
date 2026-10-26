//! x86-64 generic element loads for a site the optimizer does not
//! specialize.
//!
//! # Contents
//! - [`Codegen::emit_keyed_load`] reads an Array's dense storage of any kind
//!   inline, a name key through the code object's keyed table routine, and
//!   every other load in the runtime.
//!
//! # Invariants
//! - Inline paths read only: an element present in dense storage is an own
//!   data property while the array's own-state guard byte reads zero.
//! - A hole or an index past the dense length reads `undefined` inline
//!   only while the realm's array-index protector holds and the array has
//!   no sidecar. Otherwise it, a non-Array receiver with a non-string key,
//!   and a table miss complete in the runtime.
//! - Both operands stay intact until the result is written; the keyed table
//!   routine neither allocates nor collects.
//!
//! # See also
//! - `crate::graph::arm64::keyed_load` for the same operation on AArch64.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::jit::JitArrayStorage;
use otter_vm::value::tag;

use super::{Codegen, CommittedArgument, abi};
use crate::{
    Unsupported,
    graph::ir::{Kind, NodeId},
};

/// Bytes the table path parks the two operands in.
const PARKED_BYTES: u32 = 16;

impl Codegen<'_> {
    pub(super) fn emit_keyed_load(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        if !matches!(self.graph.node(node).kind, Kind::LoadKeyedCached { .. }) {
            return Ok(false);
        }
        let assigned = self.allocation.node(node).clone();
        let (receiver, key) = (Self::gp(assigned.inputs[0]), Self::gp(assigned.inputs[1]));
        let destination = Self::gp(assigned.result.expect("a keyed load result"));
        let (length, base, index) = (
            assigned.gp_temps[0],
            assigned.gp_temps[1],
            assigned.gp_temps[2],
        );
        let double = assigned.fp_temps[0];
        let storage = JitArrayStorage::current();
        let numeric = storage.numeric;
        let other = self.ops.new_dynamic_label();
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let tagged = self.ops.new_dynamic_label();
        let holey = self.ops.new_dynamic_label();
        let boxed = self.ops.new_dynamic_label();
        let absent = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(receiver), r10 ; jnz =>other
            ; cmp BYTE [Rq(receiver)], storage.type_tag as i8 ; jne =>other
            // A name key reads the array's named bag through the table.
            ; test Rq(key), r10 ; jz =>other
            ; test BYTE [Rq(receiver) + storage.own_guard_byte as i32], (1 << storage.own_exotic_bit) as i8 ; jne =>runtime
        );
        crate::x86_64::index::emit_number_index(&mut self.ops, key, index, double, runtime);
        dynasm!(self.ops ; .arch x64
            ; mov Rd(length), [Rq(receiver) + storage.length_byte as i32]
            ; cmp Rd(length), Rd(index) ; jbe =>absent
            ; mov Rq(base), [Rq(receiver) + storage.base_byte as i32]
            ; movzx r10d, BYTE [Rq(receiver) + numeric.kind_byte as i32]
            ; cmp r10d, i32::from(storage.tagged_kind) ; je =>tagged
            ; cmp r10d, i32::from(numeric.holey_kind) ; je =>holey
            ; cmp r10d, i32::from(numeric.packed_kind) ; jne =>runtime
            // Packed numeric storage has no hole below its length.
            ; mov r10d, Rd(index) ; movsd Rx(double), [Rq(base) + r10 * 8]
            ; jmp =>boxed
            ; =>holey
        );
        self.emit_hole_bit(numeric, base, index);
        dynasm!(self.ops ; .arch x64
            ; jc =>absent
            ; mov r10d, Rd(index) ; movsd Rx(double), [Rq(base) + r10 * 8]
            ; =>boxed
        );
        self.emit_box_float64(node, destination, double);
        dynasm!(self.ops ; .arch x64
            ; jmp =>done
            ; =>tagged
            ; mov r10d, Rd(index) ; mov r10, [Rq(base) + r10 * 8]
        );
        self.load_immediate(11, tag::VALUE_HOLE);
        dynasm!(self.ops ; .arch x64
            ; cmp r10, r11 ; je =>absent
            ; mov Rq(destination), r10 ; jmp =>done
            // No own element at the index: with no index key anywhere on the
            // realm's default chain and no sidecar to replace the array's
            // prototype, the ordinary [[Get]] answers `undefined`.
            ; =>absent
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET as i32]
            ; cmp BYTE [r10], 0 ; jne =>runtime
            ; cmp DWORD [Rq(receiver) + storage.exotic_byte as i32], 0 ; jne =>runtime
        );
        self.load_immediate(destination, tag::VALUE_UNDEFINED);
        dynasm!(self.ops ; .arch x64
            ; jmp =>done
            ; =>other
        );
        self.emit_keyed_table_load(node, [receiver, key], destination, runtime, done);
        dynasm!(self.ops ; .arch x64 ; =>runtime);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_LOAD_ELEMENT,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Value(key),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(true)
    }

    /// A name key's load through the code object's keyed table routine: the
    /// value in `Rq(destination)` and on to `loaded` on a hit, else to
    /// `missed` with every register as it was. Only a string or symbol key is
    /// offered.
    fn emit_keyed_table_load(
        &mut self,
        node: NodeId,
        [receiver, key]: [u8; 2],
        destination: u8,
        missed: DynamicLabel,
        loaded: DynamicLabel,
    ) {
        use crate::template::x86_64::shared_property::{KEYED_LOAD_KEY, KEYED_LOAD_RECEIVER};
        let string_tag = self.view.string_layout.string_type_tag;
        let name = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(key), r10 ; jnz =>missed
            ; cmp BYTE [Rq(key)], string_tag as i8 ; je =>name
            ; cmp BYTE [Rq(key)], otter_vm::symbol::SYMBOL_BODY_TYPE_TAG as i8 ; jne =>missed
            ; =>name
        );
        let live = self.allocation.node(node).live_registers.clone();
        // The operands may sit in registers the routine's inputs name: park
        // them below the saved registers and load the inputs from there.
        dynasm!(self.ops ; .arch x64
            ; sub rsp, PARKED_BYTES as i32
            ; mov [rsp], Rq(receiver) ; mov [rsp + 8], Rq(key)
        );
        self.sp_delta += PARKED_BYTES;
        let saved = self.emit_save_registers(&live) as i32;
        dynasm!(self.ops ; .arch x64
            ; mov Rq(KEYED_LOAD_RECEIVER), [rsp + saved]
            ; mov Rq(KEYED_LOAD_KEY), [rsp + saved + 8]
        );
        crate::template::x86_64::shared_property::emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.routines,
            crate::routines::Routine::KeyedLoad,
        );
        dynasm!(self.ops ; .arch x64 ; mov r10, rax ; mov r11, rdx);
        self.emit_restore_registers(&live, saved as u32);
        dynasm!(self.ops ; .arch x64
            ; mov Rq(receiver), [rsp] ; mov Rq(key), [rsp + 8]
            ; add rsp, PARKED_BYTES as i32
        );
        self.sp_delta -= PARKED_BYTES;
        dynasm!(self.ops ; .arch x64
            ; test r11, r11 ; jnz =>missed
            ; mov Rq(destination), r10 ; jmp =>loaded
        );
    }
}

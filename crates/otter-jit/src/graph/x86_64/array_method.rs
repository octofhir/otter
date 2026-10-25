//! x86-64 inline `Array.prototype.push` and `pop` at a guarded method call.
//!
//! # Contents
//! - [`Codegen::emit_array_method`] appends to tagged storage, growing it
//!   when full, or removes a present tagged last element; any other dense
//!   state goes through the VM's allocating push or its pop leaf.
//!
//! # Invariants
//! - Growing storage and the VM's push are deferred code: the body holds
//!   only the append into spare capacity.
//! - The receiver is proved an Array that reads the site's builtin from its
//!   pinned `%Array.prototype%` before any effect; a failed proof, or for an
//!   append an indexed accessor anywhere in the realm, leaves through the
//!   eager deopt.
//! - The inline paths cover an Array whose dense prefix is its whole length:
//!   an append to tagged storage, which grows full or absent storage into a
//!   nursery slab while no marking runs, and the removal of a present tagged
//!   last element. Any other dense state completes in the VM's allocating
//!   push or its pop leaf, neither of which runs JavaScript; a state those
//!   refuse leaves through the eager deopt before any effect. Neither node
//!   therefore writes more than element storage and lengths.
//! - Grown storage is young, so its copied words owe no barrier; the Array
//!   takes the barrier for the slab it now holds.
//! - The appended value takes the element write barrier before the lengths
//!   publish it; the removed slot is cleared to `undefined`, which needs none.
//! - The Array's dense length, its slab's length and its `length` move
//!   together, as the VM's own append and truncation do.
//! - Storage shared with a literal boilerplate reports no capacity, so an
//!   append reserves at least `length + 1` words before copying the full
//!   prefix into private storage and clearing the shared mark; a
//!   removal from it completes in the builtin, which copies it out.
//!
//! # See also
//! - `crate::graph::arm64::array_method` for the same operations on AArch64.
//! - `otter_vm::jit::JitArrayStorage` for the words read and written.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::jit::{JitArrayStorage, JitGuardedReceiver, JitMethodHolder};
use otter_vm::value::tag;

use super::Codegen;
use crate::allocation::LabRegisters;
use crate::x86_64::allocation::emit_grow_tagged_slab;
use crate::{
    Unsupported,
    graph::{
        call::IntrinsicMethod,
        ir::{DeoptReason, Kind, NodeId},
    },
};

impl Codegen<'_> {
    pub(super) fn emit_array_method(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        let (byte_pc, push) = match self.graph.node(node).kind {
            Kind::ArrayPush(byte_pc) => (byte_pc, true),
            Kind::ArrayPop(byte_pc) => (byte_pc, false),
            _ => return Ok(false),
        };
        let assigned = self.allocation.node(node).clone();
        let receiver = Self::gp(assigned.inputs[0]);
        let destination = Self::gp(assigned.result.expect("an array method result"));
        let [length, base, temp] = [
            assigned.gp_temps[0],
            assigned.gp_temps[1],
            assigned.gp_temps[2],
        ];
        let storage = JitArrayStorage::current();
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        self.emit_array_method_guard(node, byte_pc, receiver, temp, exit)?;
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        if push {
            let value = Self::gp(assigned.inputs[1]);
            let grow = self.ops.new_dynamic_label();
            let appended = self.ops.new_dynamic_label();
            dynasm!(self.ops ; .arch x64
                ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET as i32]
                ; cmp BYTE [r10], 0 ; jne =>exit);
            // Absent storage reads the empty kind, zero capacity and an empty
            // prefix, so its first append grows it.
            self.emit_whole_prefix(storage, receiver, length, true, runtime);
            dynasm!(self.ops ; .arch x64
                ; mov r11d, [Rq(receiver) + storage.capacity_byte as i32]
                ; cmp Rd(length), r11d ; jae =>grow
                ; mov Rq(base), [Rq(receiver) + storage.base_byte as i32]
                ; mov [Rq(base) + Rq(length) * 8], Rq(value));
            self.emit_element_write_barrier(node, base, length, value, &[receiver, length, base]);
            dynasm!(self.ops ; .arch x64
                ; lea r10d, [Rq(length) + 1]
                ; mov [Rq(receiver) + storage.length_byte as i32], r10d
                ; mov [Rq(base) + storage.slab_length_from_base], r10d
                ; mov [Rq(receiver) + storage.logical_length_byte as i32], r10d
                ; =>appended
                ; lea r10d, [Rq(length) + 1]);
            crate::x86_64::values::emit_box_int32(&mut self.ops, 10, destination, 11);
            let regs = LabRegisters {
                buffer: assigned.gp_temps[3],
                candidate: temp,
                end: assigned.gp_temps[4],
                scratch: assigned.gp_temps[5],
                size: base,
            };
            // Growing storage and the VM's push are the cold side of an
            // append.
            self.defer(move |codegen| {
                dynasm!(codegen.ops ; .arch x64
                    ; =>grow
                    // The copied words would owe marking barriers.
                    ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
                    ; mov r10, [r10 + crate::entry::VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
                    ; cmp BYTE [r10], 0 ; jne =>runtime
                    // Double the capacity, and at least four words.
                    ; lea Rd(base), [r11 + r11] ; mov r10d, 4 ; cmp Rd(base), r10d ; cmovb Rd(base), r10d
                    // Shared storage reports capacity zero but retains its prefix.
                    ; lea r10d, [Rq(length) + 1] ; cmp Rd(base), r10d ; cmovb Rd(base), r10d);
                emit_grow_tagged_slab(
                    &mut codegen.ops,
                    15,
                    storage,
                    [receiver, length],
                    regs,
                    runtime,
                );
                // The new storage is young and no marking runs, so its words
                // owe no barrier; the Array owes one for the slab it now holds.
                dynasm!(codegen.ops ; .arch x64
                    ; mov [Rq(base) + Rq(length) * 8], Rq(value)
                    ; lea r10d, [Rq(length) + 1]
                    ; mov [Rq(base) + storage.slab_length_from_base], r10d
                    ; mov [Rq(receiver) + storage.slab_byte as i32], Rd(temp)
                    ; mov [Rq(receiver) + storage.base_byte as i32], Rq(base)
                    ; mov r11d, [Rq(base) + storage.numeric.capacity_byte]
                    ; mov [Rq(receiver) + storage.capacity_byte as i32], r11d
                    ; mov BYTE [Rq(receiver) + storage.numeric.kind_byte as i32], storage.tagged_kind as i8
                    ; mov BYTE [Rq(receiver) + storage.own_guard_byte as i32], 0
                    ; mov [Rq(receiver) + storage.length_byte as i32], r10d
                    ; mov [Rq(receiver) + storage.logical_length_byte as i32], r10d);
                codegen.emit_cell_edge_barrier(node, receiver, temp, &[length]);
                dynasm!(codegen.ops ; .arch x64 ; jmp =>appended ; =>runtime);
                let grown = codegen.ops.new_dynamic_label();
                codegen.emit_array_push_grow(node, [receiver, value, value], exit, grown)?;
                dynasm!(codegen.ops ; .arch x64
                    ; =>grown
                    ; mov Rq(destination), r10
                    ; jmp =>done);
                Ok(())
            });
        } else {
            let empty = self.ops.new_dynamic_label();
            self.emit_whole_prefix(storage, receiver, length, false, runtime);
            dynasm!(self.ops ; .arch x64
                ; cmp BYTE [Rq(receiver) + storage.own_guard_byte as i32], 0 ; jne =>runtime
                ; test Rd(length), Rd(length) ; jz =>empty
                ; sub Rd(length), 1
                ; mov Rq(base), [Rq(receiver) + storage.base_byte as i32]
                ; mov Rq(temp), [Rq(base) + Rq(length) * 8]);
            // A hole would read the index through the prototype chain.
            self.load_immediate(10, tag::VALUE_HOLE);
            dynasm!(self.ops ; .arch x64 ; cmp Rq(temp), r10 ; je =>runtime);
            self.load_immediate(10, tag::VALUE_UNDEFINED);
            dynasm!(self.ops ; .arch x64
                ; mov [Rq(base) + Rq(length) * 8], r10
                ; mov [Rq(receiver) + storage.length_byte as i32], Rd(length)
                ; mov [Rq(base) + storage.slab_length_from_base], Rd(length)
                ; mov [Rq(receiver) + storage.logical_length_byte as i32], Rd(length)
                ; mov Rq(destination), Rq(temp)
                ; jmp =>done
                ; =>empty);
            self.load_immediate(destination, tag::VALUE_UNDEFINED);
            dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>runtime);
            self.emit_array_pop_leaf(node, receiver, exit);
            dynasm!(self.ops ; .arch x64 ; mov Rq(destination), r10);
        }
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(true)
    }

    /// Remove the last element of the Array in `receiver` through the VM's
    /// pop leaf, which never allocates: its value is in `r10` after, or,
    /// when the leaf refused before any effect, control reaches `refused`
    /// with every register as it was.
    fn emit_array_pop_leaf(&mut self, node: NodeId, receiver: u8, refused: DynamicLabel) {
        let leaf = otter_vm::runtime_stubs::ARRAY_POP_LEAF;
        let live = self.allocation.node(node).live_registers.clone();
        dynasm!(self.ops ; .arch x64 ; sub rsp, 16 ; mov [rsp], Rq(receiver));
        self.sp_delta += 16;
        let saved = self.emit_save_registers(&live) as i32;
        dynasm!(self.ops ; .arch x64
            ; mov rsi, [rsp + saved]
            ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
        );
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            leaf.entry_addr() as u64,
            leaf.descriptor,
        );
        crate::x86_64::call_abi::emit_runtime_call(&mut self.ops, leaf.descriptor);
        dynasm!(self.ops ; .arch x64 ; mov r10, rax ; mov r11, rdx);
        self.emit_restore_registers(&live, saved as u32);
        dynasm!(self.ops ; .arch x64
            ; mov Rq(receiver), [rsp]
            ; add rsp, 16
            ; test r11, r11 ; jnz =>refused
        );
        self.sp_delta -= 16;
    }

    /// Prove `receiver` an Array that reads the builtin of the guarded
    /// method call `byte_pc` from its pinned prototype, else branch to
    /// `exit`. Clobbers `temp`, `r10` and `r11`.
    fn emit_array_method_guard(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        receiver: u8,
        temp: u8,
        exit: DynamicLabel,
    ) -> Result<(), Unsupported> {
        let call = self
            .view_of(node)
            .guarded_method_calls
            .get(&byte_pc)
            .ok_or(Unsupported::OperandShape("x86 graph array method site"))?;
        let (JitGuardedReceiver::Exotic(target), JitMethodHolder::Shape(holder_shape)) =
            (call.receiver, call.holder)
        else {
            return Err(Unsupported::OperandShape("x86 graph array method proof"));
        };
        let method = IntrinsicMethod {
            receiver: target,
            holder: crate::graph::call::IntrinsicHolder::Shape(holder_shape),
            field: call.method_field,
            native_ref: call.builtin_native_ref,
        };
        // A subscribed chain proof is a dependency the generation tested
        // before this node; only the receiver remains to prove.
        if call.holder_validity.is_some() {
            self.emit_intrinsic_receiver(target, receiver, exit);
        } else {
            let site = (byte_pc, call.entry_stub_id);
            self.emit_intrinsic_method_identity(method, site, receiver, temp, exit);
        }
        Ok(())
    }

    /// Load the Array `receiver`'s dense length into `length` and branch to
    /// `other` unless its storage is tagged, or absent when `absent` admits
    /// it, and that dense prefix is its whole `length`.
    fn emit_whole_prefix(
        &mut self,
        storage: JitArrayStorage,
        receiver: u8,
        length: u8,
        absent: bool,
        other: DynamicLabel,
    ) {
        let tagged = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; movzx r10d, BYTE [Rq(receiver) + storage.numeric.kind_byte as i32]
            ; cmp r10d, i32::from(storage.tagged_kind));
        if absent {
            dynasm!(self.ops ; .arch x64 ; je =>tagged ; test r10d, r10d ; jnz =>other ; =>tagged);
        } else {
            dynasm!(self.ops ; .arch x64 ; jne =>other);
        }
        dynasm!(self.ops ; .arch x64
            ; mov Rd(length), [Rq(receiver) + storage.length_byte as i32]
            ; cmp [Rq(receiver) + storage.logical_length_byte as i32], Rd(length)
            ; jne =>other);
    }
}

//! Inline `Array.prototype.push` and `pop` at a guarded method call.
//!
//! # Contents
//! - [`Codegen::emit_array_push`] appends to tagged storage, growing it when
//!   full.
//! - [`Codegen::emit_array_pop`] removes a present tagged last element.
//!
//! # Invariants
//! - The receiver is proved an Array that reads the site's builtin from its
//!   pinned `%Array.prototype%` before any effect; a failed proof, or for an
//!   append an indexed accessor anywhere in the realm, leaves through the
//!   eager deopt.
//! - The inline paths cover an Array whose dense prefix is its whole length:
//!   an append to tagged storage, which grows full or absent storage into a
//!   nursery slab while no marking runs, and the removal of a present tagged
//!   last element. Every other state the proof admits completes in the
//!   builtin through the committed object-protocol entry.
//! - Grown storage is young, so its copied words owe no barrier; the Array
//!   takes the barrier for the slab it now holds.
//! - The appended value takes the element write barrier before the lengths
//!   publish it; the removed slot is cleared to `undefined`, which needs none.
//! - The Array's dense length, its slab's length and its `length` move
//!   together, as the VM's own append and truncation do.
//! - Storage shared with a literal boilerplate reports no capacity, so an
//!   append grows it into private storage and clears the shared mark; a
//!   removal from it completes in the builtin, which copies it out.
//!
//! # See also
//! - `otter_vm::jit::JitArrayStorage` for the words read and written.
//! - [`super::Codegen::emit_store_keyed_cached`] for the generic element store.

use super::{
    ARRAY_INDEX_PROTECTOR, Codegen, CommittedArgument, DeoptReason, DynamicLabel, IntrinsicMethod,
    NUMBER_TAG, NodeId, THREAD_OFFSET, VALUE_HOLE, VALUE_UNDEFINED,
    VM_THREAD_MARKING_FLAG_CELL_OFFSET, abi,
};
use crate::Unsupported;
use crate::allocation::LabRegisters;
use crate::arm64::allocation::emit_grow_tagged_slab;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::ObjectProtocolValueOp;
use otter_vm::jit::{JitArrayStorage, JitGuardedReceiver, JitMethodHolder};

impl Codegen<'_> {
    /// `X(destination) = X(receiver).push(X(value))` at the guarded method
    /// call `byte_pc`; `length`, `base` and `temp` are temporaries, and the
    /// last three carve grown storage with them.
    pub(super) fn emit_array_push(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        [receiver, value]: [u8; 2],
        [length, base, temp, buffer, end, scratch]: [u8; 6],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let storage = JitArrayStorage::current();
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        self.emit_array_method_guard(node, byte_pc, receiver, temp, exit)?;
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, ARRAY_INDEX_PROTECTOR]
            ; ldrb w16, [x16]
            ; cbnz w16, =>exit
        );
        let runtime = self.ops.new_dynamic_label();
        let grow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        // Absent storage reads the empty kind, zero capacity and an empty
        // prefix, so its first append grows it.
        self.emit_whole_prefix(storage, receiver, length, true, runtime);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr w16, [X(receiver), storage.capacity_byte]
            ; cmp W(length), w16
            ; b.hs =>grow
            ; ldr X(base), [X(receiver), storage.base_byte]
            ; str X(value), [X(base), X(length), lsl #3]
        );
        self.emit_element_write_barrier(node, base, length, value, &[receiver, length, base]);
        dynasm!(self.ops
            ; .arch aarch64
            ; add w16, WSP(length), #1
            ; str w16, [X(receiver), storage.length_byte]
            ; stur w16, [X(base), storage.slab_length_from_base]
            ; str w16, [X(receiver), storage.logical_length_byte]
            ; b =>done
            ; =>grow
            // The copied words would owe marking barriers.
            ; ldr x17, [x20, THREAD_OFFSET]
            ; ldr x17, [x17, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb w17, [x17]
            ; cbnz w17, =>runtime
            // Double the capacity, and at least four words.
            ; lsl W(base), w16, #1
            ; mov w17, #4
            ; cmp W(base), w17
            ; csel W(base), W(base), w17, hs
        );
        let regs = LabRegisters {
            buffer,
            candidate: temp,
            end,
            scratch,
            size: base,
        };
        emit_grow_tagged_slab(
            &mut self.ops,
            20,
            storage,
            [receiver, length],
            regs,
            runtime,
        );
        // The new storage is young and no marking runs, so its words owe no
        // barrier; the Array owes one for the slab it now holds.
        dynasm!(self.ops
            ; .arch aarch64
            ; str X(value), [X(base), X(length), lsl #3]
            ; add w16, WSP(length), #1
            ; stur w16, [X(base), storage.slab_length_from_base]
            ; str W(temp), [X(receiver), storage.slab_byte]
            ; str X(base), [X(receiver), storage.base_byte]
            ; ldur w17, [X(base), storage.numeric.capacity_byte]
            ; str w17, [X(receiver), storage.capacity_byte]
            ; mov w17, u32::from(storage.tagged_kind)
            ; strb w17, [X(receiver), storage.numeric.kind_byte]
            ; strb wzr, [X(receiver), storage.own_guard_byte]
            ; str w16, [X(receiver), storage.length_byte]
            ; str w16, [X(receiver), storage.logical_length_byte]
        );
        self.emit_write_barrier_keeping(node, receiver, temp, &[length]);
        dynasm!(self.ops
            ; .arch aarch64
            ; add w16, WSP(length), #1
            ; b =>done
            ; =>runtime
        );
        self.emit_committed_call(
            node,
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Value(value),
                CommittedArgument::Scalar(ObjectProtocolValueOp::ArrayPush.encode_abi()),
            ],
            Some(destination),
        )?;
        let committed = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>committed
            ; =>done
            ; mov W(destination), w16
            ; orr XSP(destination), X(destination), NUMBER_TAG
            ; =>committed
        );
        Ok(())
    }

    /// `X(destination) = X(receiver).pop()` at the guarded method call
    /// `byte_pc`; `length`, `base` and `temp` are temporaries.
    pub(super) fn emit_array_pop(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        receiver: u8,
        [length, base, temp]: [u8; 3],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let storage = JitArrayStorage::current();
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        self.emit_array_method_guard(node, byte_pc, receiver, temp, exit)?;
        let runtime = self.ops.new_dynamic_label();
        let empty = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.emit_whole_prefix(storage, receiver, length, false, runtime);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldrb w16, [X(receiver), storage.own_guard_byte]
            ; cbnz w16, =>runtime
            ; cbz W(length), =>empty
            ; sub WSP(length), WSP(length), #1
            ; ldr X(base), [X(receiver), storage.base_byte]
            ; ldr X(temp), [X(base), X(length), lsl #3]
        );
        // A hole would read the index through the prototype chain.
        self.load_immediate(16, VALUE_HOLE);
        dynasm!(self.ops ; .arch aarch64 ; cmp X(temp), x16 ; b.eq =>runtime);
        self.load_immediate(16, VALUE_UNDEFINED);
        dynasm!(self.ops
            ; .arch aarch64
            ; str x16, [X(base), X(length), lsl #3]
            ; str W(length), [X(receiver), storage.length_byte]
            ; stur W(length), [X(base), storage.slab_length_from_base]
            ; str W(length), [X(receiver), storage.logical_length_byte]
            ; mov X(destination), X(temp)
            ; b =>done
            ; =>empty
        );
        self.load_immediate(destination, VALUE_UNDEFINED);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>runtime);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Scalar(VALUE_UNDEFINED),
                CommittedArgument::Scalar(ObjectProtocolValueOp::ArrayPop.encode_abi()),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// Prove `X(receiver)` an Array that reads the builtin of the guarded
    /// method call `byte_pc` from its pinned prototype, else branch to
    /// `exit`. Clobbers `X(temp)`, `x16` and `x17`.
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
            .ok_or(Unsupported::OperandShape("graph array method site"))?;
        let (JitGuardedReceiver::Exotic(target), JitMethodHolder::Shape(holder_shape)) =
            (call.receiver, call.holder)
        else {
            return Err(Unsupported::OperandShape("graph array method proof"));
        };
        let method = IntrinsicMethod {
            receiver: target,
            holder_shape,
            field: call.method_field,
            native_ref: call.builtin_native_ref,
        };
        let site = (byte_pc, call.entry_stub_id);
        self.emit_intrinsic_method_identity(method, site, receiver, temp, exit);
        Ok(())
    }

    /// Load the Array `X(receiver)`'s dense length into `W(length)` and
    /// branch to `other` unless its storage is tagged, or absent when
    /// `absent` admits it, and that dense prefix is its whole `length`.
    fn emit_whole_prefix(
        &mut self,
        storage: JitArrayStorage,
        receiver: u8,
        length: u8,
        absent: bool,
        other: DynamicLabel,
    ) {
        let tagged = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; ldrb w16, [X(receiver), storage.numeric.kind_byte]
            ; cmp w16, u32::from(storage.tagged_kind)
        );
        if absent {
            dynasm!(self.ops ; .arch aarch64 ; b.eq =>tagged ; cbnz w16, =>other ; =>tagged);
        } else {
            dynasm!(self.ops ; .arch aarch64 ; b.ne =>other);
        }
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr W(length), [X(receiver), storage.length_byte]
            ; ldr w16, [X(receiver), storage.logical_length_byte]
            ; cmp w16, W(length)
            ; b.ne =>other
        );
    }
}

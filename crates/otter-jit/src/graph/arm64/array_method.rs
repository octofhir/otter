//! Inline `Array.prototype.push` and `pop` at a guarded method call.
//!
//! # Contents
//! - [`Codegen::emit_array_push`] appends into spare tagged capacity.
//! - [`Codegen::emit_array_pop`] removes a present tagged last element.
//!
//! # Invariants
//! - The receiver is proved an Array that reads the site's builtin from its
//!   pinned `%Array.prototype%` before any effect; a failed proof, or for an
//!   append an indexed accessor anywhere in the realm, leaves through the
//!   eager deopt.
//! - The inline paths cover an Array whose tagged dense prefix is its whole
//!   length: an append within capacity and the removal of a present last
//!   element. Every other state the proof admits completes in the builtin
//!   through the committed object-protocol entry, which grows storage.
//! - The appended value takes the element write barrier before the lengths
//!   publish it; the removed slot is cleared to `undefined`, which needs none.
//! - The Array's dense length, its slab's length and its `length` move
//!   together, as the VM's own append and truncation do.
//!
//! # See also
//! - `otter_vm::jit::JitArrayStorage` for the words read and written.
//! - [`super::Codegen::emit_store_keyed_cached`] for the generic element store.

use super::{
    ARRAY_INDEX_PROTECTOR, Codegen, CommittedArgument, DeoptReason, DynamicLabel, IntrinsicMethod,
    NUMBER_TAG, NodeId, THREAD_OFFSET, VALUE_HOLE, VALUE_UNDEFINED, abi,
};
use crate::Unsupported;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::ObjectProtocolValueOp;
use otter_vm::jit::{JitArrayStorage, JitGuardedReceiver, JitMethodHolder};

impl Codegen<'_> {
    /// `X(destination) = X(receiver).push(X(value))` at the guarded method
    /// call `byte_pc`; `length`, `base` and `temp` are temporaries.
    pub(super) fn emit_array_push(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        [receiver, value]: [u8; 2],
        [length, base, temp]: [u8; 3],
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
        let done = self.ops.new_dynamic_label();
        self.emit_whole_tagged_prefix(storage, receiver, length, runtime);
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr w16, [X(receiver), storage.capacity_byte]
            ; cmp W(length), w16
            ; b.hs =>runtime
            ; ldr X(base), [X(receiver), storage.base_byte]
            ; str X(value), [X(base), X(length), lsl #3]
        );
        self.emit_element_write_barrier(node, base, length, value, &[receiver, length, base]);
        dynasm!(self.ops
            ; .arch aarch64
            ; add w16, WSP(length), #1
            ; str w16, [X(receiver), storage.length_byte]
            ; stur w16, [X(base), storage.slab_length_from_base]
            ; str x16, [X(receiver), storage.logical_length_byte]
            ; mov W(destination), w16
            ; orr XSP(destination), X(destination), NUMBER_TAG
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
        dynasm!(self.ops ; .arch aarch64 ; =>done);
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
        self.emit_whole_tagged_prefix(storage, receiver, length, runtime);
        dynasm!(self.ops
            ; .arch aarch64
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
            ; str X(length), [X(receiver), storage.logical_length_byte]
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
    /// branch to `other` unless its storage is tagged and that dense prefix
    /// is its whole `length`.
    fn emit_whole_tagged_prefix(
        &mut self,
        storage: JitArrayStorage,
        receiver: u8,
        length: u8,
        other: DynamicLabel,
    ) {
        dynasm!(self.ops
            ; .arch aarch64
            ; ldrb w16, [X(receiver), storage.numeric.kind_byte]
            ; cmp w16, u32::from(storage.tagged_kind)
            ; b.ne =>other
            ; ldr W(length), [X(receiver), storage.length_byte]
            ; ldr x16, [X(receiver), storage.logical_length_byte]
            ; cmp x16, X(length)
            ; b.ne =>other
        );
    }
}

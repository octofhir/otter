//! x86-64 generic element stores for a site the optimizer does not
//! specialize.
//!
//! # Contents
//! - [`Codegen::emit_keyed_store`] stores into an Array's dense storage of
//!   any kind inline, appends at its length inline into tagged or packed
//!   numeric storage with room and through the VM's push leaf otherwise, stores a Number into a
//!   typed view of any non-BigInt kind inline, a name key through the code
//!   object's keyed table routine, and every other store in the runtime.
//!
//! # Invariants
//! - Every guard precedes the one store; a failed guard has written nothing
//!   and takes the next path, never a deopt.
//! - A hole is written only while the realm's array-index accessor protector
//!   holds and the array has no exotic sidecar.
//! - A store into numeric storage fills its hole as the VM does: the bit is
//!   cleared and counted, and the last one makes the storage packed. A
//!   tagged cell value takes the element write barrier.
//! - An append is an ordinary own addition under the same permit as a
//!   hole store. It completes without allocating inline only into tagged or packed
//!   numeric storage whose dense prefix is the whole length and below its
//!   capacity, a tagged value taking the element write barrier and a numeric
//!   one stored as its double over a clear hole bit, before the Array's dense
//!   length, its slab's length and its `length` move together; the push leaf completes
//!   any other append within the storage's capacity and its own dense-range
//!   guards, and the allocating push grows a full storage at a safepoint.
//! - A typed view is stored into inline only through its cached element
//!   base while the detach protector holds, at an int32 index below its
//!   length, and with a value that is already a Number: the conversion to
//!   the element runs no user code, and the store is the whole effect.
//! - The keyed table routine neither allocates nor collects; the runtime
//!   operation grows storage and records the site's feedback.
//!
//! # See also
//! - `crate::graph::arm64::keyed_store` for the same operation on AArch64.
//! - `otter_vm::jit::JitArrayStorage` for the storage this reads.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::binary::TypedArrayKind;
use otter_vm::jit::{JitArrayStorage, JitHoleBitmap, JitTypedArrayStorage};
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
        let (length, base, index) = (
            assigned.gp_temps[0],
            assigned.gp_temps[1],
            assigned.gp_temps[2],
        );
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
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(receiver), r10 ; jnz =>other
            ; cmp BYTE [Rq(receiver)], storage.type_tag as i8 ; jne =>not_array
            ; cmp BYTE [Rq(receiver) + storage.own_guard_byte as i32], 0 ; jne =>runtime
        );
        crate::x86_64::index::emit_number_index(&mut self.ops, key, index, 15, runtime);
        dynasm!(self.ops ; .arch x64
            ; mov Rd(length), [Rq(receiver) + storage.length_byte as i32]
            ; cmp Rd(length), Rd(index) ; ja =>in_bounds ; jne =>runtime
        );
        // A key at the length appends: inline into tagged storage whose
        // dense prefix is its whole length while it has room, else through
        // the push leaf.
        self.emit_hole_store_permit(storage, receiver, runtime);
        let leaf = self.ops.new_dynamic_label();
        let append_tagged = self.ops.new_dynamic_label();
        let appended = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; cmp Rd(length), DWORD [Rq(receiver) + storage.logical_length_byte as i32]
            ; jne =>leaf
            ; cmp Rd(length), DWORD [Rq(receiver) + storage.capacity_byte as i32]
            ; jae =>leaf
            ; mov Rq(base), [Rq(receiver) + storage.base_byte as i32]
            ; cmp BYTE [Rq(receiver) + numeric.kind_byte as i32], storage.tagged_kind as i8
            ; je =>append_tagged
            ; cmp BYTE [Rq(receiver) + numeric.kind_byte as i32], numeric.packed_kind as i8
            ; jne =>leaf
        );
        // Packed numeric storage takes a Number as its double; its new slot
        // must read as present, as the VM's own write leaves it.
        emit_tagged_number_to_float(&mut self.ops, value, 15, leaf);
        self.emit_hole_bit(numeric, base, index);
        dynasm!(self.ops ; .arch x64
            ; jc =>leaf
            ; mov r10d, Rd(index) ; movsd [Rq(base) + r10 * 8], xmm15
            ; jmp =>appended
            ; =>append_tagged
            ; mov r10d, Rd(index) ; mov [Rq(base) + r10 * 8], Rq(value)
        );
        self.emit_element_write_barrier(node, base, index, value, &[receiver, length, base]);
        dynasm!(self.ops ; .arch x64
            ; =>appended
            ; lea r10d, [Rq(length) + 1]
            ; mov DWORD [Rq(receiver) + storage.length_byte as i32], r10d
            ; mov DWORD [Rq(base) + storage.slab_length_from_base], r10d
            ; mov DWORD [Rq(receiver) + storage.logical_length_byte as i32], r10d
            ; jmp =>done
            ; =>leaf
        );
        let grow = self.ops.new_dynamic_label();
        self.emit_array_push_leaf(node, [receiver, key, value], grow, done);
        dynasm!(self.ops ; .arch x64 ; =>grow);
        self.emit_array_push_grow(node, [receiver, key, value], runtime, done)?;
        dynasm!(self.ops ; .arch x64
            ; =>in_bounds
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
        self.emit_hole_bit(numeric, base, index);
        dynasm!(self.ops ; .arch x64 ; jnc =>store_double);
        self.emit_hole_store_permit(storage, receiver, runtime);
        // The length is dead past the bounds check.
        self.emit_fill_hole(numeric, receiver, base, index, length);
        dynasm!(self.ops ; .arch x64
            ; =>store_double
            ; mov r10d, Rd(index) ; movsd [Rq(base) + r10 * 8], xmm15
            ; jmp =>done
            ; =>tagged
        );
        self.load_immediate(11, tag::VALUE_HOLE);
        dynasm!(self.ops ; .arch x64
            ; mov r10d, Rd(index) ; cmp [Rq(base) + r10 * 8], r11 ; jne =>store_tagged
        );
        self.emit_hole_store_permit(storage, receiver, runtime);
        dynasm!(self.ops ; .arch x64
            ; =>store_tagged
            ; mov r10d, Rd(index) ; mov [Rq(base) + r10 * 8], Rq(value)
        );
        self.emit_element_write_barrier(node, base, index, value, &[]);
        let typed_tag = JitTypedArrayStorage::current().type_tag;
        dynasm!(self.ops ; .arch x64
            ; jmp =>done
            ; =>not_array
            ; cmp BYTE [Rq(receiver)], typed_tag as i8 ; jne =>other
        );
        self.emit_typed_view_store([receiver, key, value], [length, base, index], runtime, done);
        dynasm!(self.ops ; .arch x64 ; =>other);
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

    /// Store the Number in `Rq(value)` at int32 `Rq(key)` of the typed view
    /// in `Rq(receiver)`, converted to its element kind: to `stored` when it
    /// stored, else to `refused` with nothing written. `kind` and `base` are
    /// temporaries; r10, r11 and xmm15 are clobbered.
    fn emit_typed_view_store(
        &mut self,
        [receiver, key, value]: [u8; 3],
        [kind, base, index]: [u8; 3],
        refused: DynamicLabel,
        stored: DynamicLabel,
    ) {
        let view = JitTypedArrayStorage::current();
        let code = |kind: TypedArrayKind| kind as i32;
        let double = self.ops.new_dynamic_label();
        let widen = self.ops.new_dynamic_label();
        let clamped = self.ops.new_dynamic_label();
        let integer = self.ops.new_dynamic_label();
        let float = self.ops.new_dynamic_label();
        crate::x86_64::index::emit_number_index(&mut self.ops, key, index, 15, refused);
        dynasm!(self.ops ; .arch x64
            // The cached base stands for the buffer's liveness and extent
            // only while no buffer was ever detached.
            ; mov Rq(base), [Rq(receiver) + view.data_byte as i32]
            ; test Rq(base), Rq(base) ; jz =>refused
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET as i32]
            ; test r10, r10 ; jz =>refused
            ; cmp BYTE [r10], 0 ; jne =>refused
            ; mov r10d, Rd(index)
            ; cmp QWORD [Rq(receiver) + view.length_byte as i32], r10 ; jbe =>refused
            ; mov Rd(kind), [Rq(receiver) + view.kind_byte as i32]
            ; mov r11, Rq(value) ; sar r11, 49 ; cmp r11, -1 ; jne =>double
            // An int32: an integer element keeps its low bits.
            ; mov r11d, Rd(value)
            ; cmp Rd(kind), code(TypedArrayKind::Uint32) ; ja =>widen
            ; cmp Rd(kind), code(TypedArrayKind::Uint8Clamped) ; jne =>integer
            ; test r11d, r11d ; js >negative
            ; cmp r11d, 255 ; jle =>clamped
            ; mov r11d, 255 ; jmp =>clamped
            ; negative: ; xor r11d, r11d ; jmp =>clamped
            ; =>widen ; cvtsi2sd xmm15, Rd(value) ; jmp =>float
            ; =>double
        );
        self.load_immediate(11, tag::NUMBER_TAG);
        dynasm!(self.ops ; .arch x64 ; test Rq(value), r11 ; jz =>refused);
        self.load_immediate(11, tag::DOUBLE_ENCODE_OFFSET);
        dynasm!(self.ops ; .arch x64
            ; neg r11 ; add r11, Rq(value) ; movq xmm15, r11
            ; cmp Rd(kind), code(TypedArrayKind::Uint32) ; ja =>float
            ; cmp Rd(kind), code(TypedArrayKind::Uint8Clamped) ; je =>refused
            // ToInt32 keeps the low bits of the truncated value; NaN, the
            // infinities and magnitudes past 2^63 read the one sentinel.
            ; cvttsd2si r11, xmm15 ; cmp r11, 1 ; jo =>refused
            // r11d holds the int32 whose low bits the element keeps.
            ; =>integer
            ; cmp Rd(kind), code(TypedArrayKind::Uint8) ; ja >half
            ; =>clamped
            ; mov BYTE [Rq(base) + r10], r11b ; jmp =>stored
            ; half:
            ; cmp Rd(kind), code(TypedArrayKind::Uint16) ; ja >word
            ; mov WORD [Rq(base) + r10 * 2], r11w ; jmp =>stored
            ; word:
            ; mov [Rq(base) + r10 * 4], r11d ; jmp =>stored
            // xmm15 holds the Number; BigInt and half-precision views refuse.
            ; =>float
            ; cmp Rd(kind), code(TypedArrayKind::Float32) ; jne >float64
            ; cvtsd2ss xmm15, xmm15 ; movss [Rq(base) + r10 * 4], xmm15 ; jmp =>stored
            ; float64:
            ; cmp Rd(kind), code(TypedArrayKind::Float64) ; jne =>refused
            ; movsd [Rq(base) + r10 * 8], xmm15 ; jmp =>stored
        );
    }

    /// A name key's store through the code object's keyed table routine:
    /// to `stored` when it stored, else to `missed` with every register as
    /// it was. Only a string or symbol key is offered.
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
            ; mov [rsp], Rq(receiver) ; mov [rsp + 8], Rq(key) ; mov [rsp + 16], Rq(value)
        );
        self.sp_delta += PARKED_BYTES;
        let saved = self.emit_save_registers(&live) as i32;
        dynasm!(self.ops ; .arch x64
            ; mov Rq(KEYED_STORE_RECEIVER), [rsp + saved]
            ; mov Rq(KEYED_STORE_KEY), [rsp + saved + 8]
            ; mov Rq(KEYED_STORE_VALUE), [rsp + saved + 16]
        );
        crate::template::x86_64::shared_property::emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.routines,
            crate::routines::Routine::KeyedStore,
        );
        dynasm!(self.ops ; .arch x64 ; mov r11, rdx);
        self.emit_restore_registers(&live, saved as u32);
        dynasm!(self.ops ; .arch x64
            ; mov Rq(receiver), [rsp] ; mov Rq(key), [rsp + 8] ; mov Rq(value), [rsp + 16]
            ; add rsp, PARKED_BYTES as i32
            ; test r11, r11 ; jz =>stored ; jmp =>missed
        );
        self.sp_delta -= PARKED_BYTES;
    }

    /// Append `Rq(value)` to the Array in `Rq(receiver)` through the VM's
    /// push leaf: to `stored` when it appended, else to `missed` with every
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
        dynasm!(self.ops ; .arch x64
            ; sub rsp, PARKED_BYTES as i32
            ; mov [rsp], Rq(receiver) ; mov [rsp + 8], Rq(key) ; mov [rsp + 16], Rq(value)
        );
        self.sp_delta += PARKED_BYTES;
        let saved = self.emit_save_registers(&live) as i32;
        dynasm!(self.ops ; .arch x64
            ; mov rsi, [rsp + saved]
            ; mov rdx, [rsp + saved + 16]
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
        dynasm!(self.ops ; .arch x64 ; mov r11, rdx);
        self.emit_restore_registers(&live, saved as u32);
        dynasm!(self.ops ; .arch x64
            ; mov Rq(receiver), [rsp] ; mov Rq(key), [rsp + 8] ; mov Rq(value), [rsp + 16]
            ; add rsp, PARKED_BYTES as i32
            ; test r11, r11 ; jz =>stored ; jmp =>missed
        );
        self.sp_delta -= PARKED_BYTES;
    }

    /// The append the inline paths or the push leaf refused: the allocating
    /// push grows the dense storage and appends `Rq(value)` to the Array in
    /// `Rq(receiver)`, leaving the tagged new length in `r10` at `stored`. A
    /// refusal precedes every effect and reaches `missed` with all three
    /// operands as they were; running out of memory may follow a collection,
    /// so it resumes the operation in the lower tier, which reads its
    /// operands from the frame.
    pub(super) fn emit_array_push_grow(
        &mut self,
        node: NodeId,
        [receiver, key, value]: [u8; 3],
        missed: DynamicLabel,
        stored: DynamicLabel,
    ) -> Result<(), Unsupported> {
        use crate::entry::{
            ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
            ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
            THREAD_OFFSET,
        };
        let stub = otter_vm::runtime_stubs::ARRAY_PUSH_ALLOC;
        let entry = stub
            .entry_addr()
            .ok_or(Unsupported::OperandShape("keyed append growth entry"))?;
        let oom = self.typed_exit(
            node,
            abi::ExitReason::AllocationMiss,
            abi::ExitAction::Resume,
        );
        self.save_live_homes(node);
        let safepoint = self.stamp_node_safepoint(node)?;
        self.stamp_pc(node);
        dynasm!(self.ops ; .arch x64
            ; sub rsp, PARKED_BYTES as i32
            ; mov [rsp], Rq(receiver) ; mov [rsp + 8], Rq(key) ; mov [rsp + 16], Rq(value)
        );
        self.sp_delta += PARKED_BYTES;
        let bytes = ALLOC_CTX_STACK_SIZE + 16;
        dynasm!(self.ops ; .arch x64 ; sub rsp, bytes as i32);
        self.sp_delta += bytes;
        dynasm!(self.ops ; .arch x64
            ; mov r10, [r15 + THREAD_OFFSET as i32] ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r10
            ; mov r10, [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32] ; mov [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], r10
            ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], safepoint as i32
            ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], self.slots.tagged as i16
            ; mov rdx, [rsp + bytes as i32]
            ; mov rcx, [rsp + bytes as i32 + 16]
            ; mov rdi, rsp ; mov esi, safepoint as i32);
        self.load_immediate(8, tag::VALUE_UNDEFINED);
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            entry as u64,
            stub.descriptor,
        );
        crate::x86_64::call_abi::emit_runtime_call(&mut self.ops, stub.descriptor);
        dynasm!(self.ops ; .arch x64
            ; mov [rsp + ALLOC_CTX_STACK_SIZE as i32], rax
            ; mov [rsp + ALLOC_CTX_STACK_SIZE as i32 + 8], rdx);
        self.restore_live_homes(node);
        let not_refused = self.ops.new_dynamic_label();
        let success = self.ops.new_dynamic_label();
        let fatal = self.fatal;
        dynasm!(self.ops ; .arch x64
            ; mov r10, [rsp + ALLOC_CTX_STACK_SIZE as i32]
            ; mov r11, [rsp + ALLOC_CTX_STACK_SIZE as i32 + 8]
            ; add rsp, bytes as i32
            ; test r11, r11 ; jz =>success
            ; cmp r11, abi::NativeResultStatus::SideExit as i32 ; jne =>not_refused
            // Refused before any effect: the parked operands are current.
            ; mov Rq(receiver), [rsp] ; mov Rq(key), [rsp + 8] ; mov Rq(value), [rsp + 16]
            ; add rsp, PARKED_BYTES as i32
            ; jmp =>missed
            ; =>not_refused
            ; add rsp, PARKED_BYTES as i32
            ; cmp r11, abi::NativeResultStatus::OutOfMemory as i32 ; je =>oom
            ; jmp =>fatal
            ; =>success
            ; add rsp, PARKED_BYTES as i32
            ; jmp =>stored
        );
        self.sp_delta -= bytes + PARKED_BYTES;
        Ok(())
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

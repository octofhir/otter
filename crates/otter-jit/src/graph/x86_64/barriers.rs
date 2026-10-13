//! x86-64 Graph generational and marking edge barriers.
//!
//! # Contents
//! - Object/value and array-element barrier fast paths.
//! - Published child-shape barriers for named and shared-cache transitions.
//! - Leaving a shape: its stable byte, or a VM call when code watches it.
//! - Deferred NoAlloc calls with complete live-register preservation.
//!
//! # Invariants
//! - A store commits before its barrier; these entries never collect or replay.
//! - Operands are staged before any platform argument register is overwritten.
//! - NoAlloc save packets are temporary scalar storage, never collector roots.
//! - Shape-child temporaries carry no SSA value after the barrier.
//!
//! # See also
//! - `otter_vm::runtime_stubs` owns barrier signatures and NoAlloc effects.
//! - `crate::x86_64::call_abi` owns the target platform's C boundary.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{native_abi as abi, value::tag};

use super::Codegen;
use crate::graph::ir::{NodeId, Repr};
use crate::graph::regalloc::Location;

impl Codegen<'_> {
    pub(super) fn emit_write_barrier(&mut self, node: NodeId, parent: u8, value: u8) {
        let done = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64 ; test Rq(value), r10 ; jnz =>done ; test Rq(value), Rq(value) ; jz =>done);
        self.emit_cell_edge_barrier(node, parent, value, &[]);
        dynasm!(self.ops ; .arch x64 ; =>done);
    }

    /// The barrier for the cell `child` just stored into `parent`, whose slow
    /// path also preserves the registers in `keep`.
    pub(super) fn emit_cell_edge_barrier(
        &mut self,
        node: NodeId,
        parent: u8,
        child: u8,
        keep: &[u8],
    ) {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let layout = self.view.gc_barrier;
        let settled = layout.young_flag | layout.remembered_flag;
        dynasm!(self.ops ; .arch x64
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
            ; cmp BYTE [r10], 0 ; jne =>slow
            ; test BYTE [Rq(parent) + layout.header_flags_byte as i32], settled as i8 ; jnz =>done
            ; test BYTE [Rq(child) + layout.header_flags_byte as i32], layout.young_flag as i8 ; jnz =>slow
            ; =>done);
        let live = self.slow_path_registers(node, keep);
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>slow);
            let saved = codegen.emit_save_registers(&live);
            dynasm!(codegen.ops ; .arch x64
                ; sub rsp, 16 ; mov [rsp], Rq(parent) ; mov [rsp + 8], Rq(child)
                ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
                ; mov rsi, [rsp] ; mov rdx, [rsp + 8]);
            codegen.sp_delta += 16;
            crate::x86_64::values::emit_load_runtime_stub(
                &mut codegen.ops,
                &mut codegen.relocations,
                otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
                abi::STUB_WRITE_BARRIER,
            );
            crate::x86_64::call_abi::emit_runtime_call(&mut codegen.ops, abi::STUB_WRITE_BARRIER);
            dynasm!(codegen.ops ; .arch x64 ; add rsp, 16);
            codegen.sp_delta -= 16;
            codegen.emit_restore_registers(&live, saved);
            dynasm!(codegen.ops ; .arch x64 ; jmp =>done);
        }));
    }

    /// The receiver is leaving its current shape: clear the shape's stable
    /// byte, or, when generations subscribe to the shape, have the VM clear
    /// it and fire them. `receiver` and `value` survive the cold call.
    pub(super) fn emit_leave_receiver_shape(&mut self, node: NodeId, receiver: u8, value: u8) {
        let watched = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let view = self.view;
        self.load_immediate(11, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64
            ; and r11, Rq(receiver)
            ; mov r10d, [Rq(receiver) + view.object_shape_byte as i32]
            ; add r10, r11
            ; test BYTE [r10 + view.shape_watched_byte as i32], otter_vm::code_dependencies::ShapeFact::Stable as i8 ; jnz =>watched
            ; mov BYTE [r10 + view.shape_stable_byte as i32], 0
            ; =>done);
        let live = self.slow_path_registers(node, &[receiver, value]);
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>watched);
            let saved = codegen.emit_save_registers(&live);
            dynasm!(codegen.ops ; .arch x64
                ; mov rsi, r10
                ; xor edx, edx
                ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]);
            crate::x86_64::values::emit_load_runtime_stub(
                &mut codegen.ops,
                &mut codegen.relocations,
                otter_vm::runtime_stubs::SHAPE_LEFT.entry_addr() as u64,
                abi::STUB_SHAPE_LEFT,
            );
            crate::x86_64::call_abi::emit_runtime_call(&mut codegen.ops, abi::STUB_SHAPE_LEFT);
            codegen.emit_restore_registers(&live, saved);
            dynasm!(codegen.ops ; .arch x64 ; jmp =>done);
        }));
    }

    /// A store just wrote slot `slot` of `context`: when the context's watch
    /// covers the slot, fire its subscribers through the VM. `context`
    /// survives the cold call.
    pub(super) fn emit_note_context_write(&mut self, node: NodeId, context: u8, slot: u16) {
        let watched = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let watch = self.view.context_layout.watch_byte as i32;
        let bit = slot as i8;
        dynasm!(self.ops ; .arch x64
            ; mov r10, [Rq(context) + watch]
            ; test r10, r10
            ; jz =>done
            ; mov r11, [r10]
            ; bt r11, bit
            ; jc =>watched
            ; =>done);
        let live = self.slow_path_registers(node, &[context]);
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>watched);
            let saved = codegen.emit_save_registers(&live);
            dynasm!(codegen.ops ; .arch x64
                ; mov rsi, r10
                ; mov edx, i32::from(slot)
                ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]);
            crate::x86_64::values::emit_load_runtime_stub(
                &mut codegen.ops,
                &mut codegen.relocations,
                otter_vm::runtime_stubs::CONTEXT_SLOT_WRITTEN.entry_addr() as u64,
                abi::STUB_CONTEXT_SLOT_WRITTEN,
            );
            crate::x86_64::call_abi::emit_runtime_call(
                &mut codegen.ops,
                abi::STUB_CONTEXT_SLOT_WRITTEN,
            );
            codegen.emit_restore_registers(&live, saved);
            dynasm!(codegen.ops ; .arch x64 ; jmp =>done);
        }));
    }

    pub(super) fn emit_shape_child_barrier(
        &mut self,
        node: NodeId,
        receiver: u8,
        shape: u32,
        child: u8,
    ) {
        self.load_immediate(child, u64::from(shape));
        self.emit_dynamic_shape_child_barrier(node, receiver, child, child);
    }

    pub(super) fn emit_dynamic_shape_child_barrier(
        &mut self,
        node: NodeId,
        receiver: u8,
        shape: u8,
        child: u8,
    ) {
        self.load_immediate(10, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64 ; and r10, Rq(receiver)
            ; mov Rd(child), Rd(shape) ; add Rq(child), r10);
        self.emit_cell_edge_barrier(node, receiver, child, &[]);
    }

    /// The barrier after storing `value` at `index` of the tagged slab whose
    /// element base is `base`: a non-cell or an old cell needs nothing; a
    /// young cell widens the slab's dirty interval, which is the whole
    /// barrier for a young or already remembered slab. A cell while marking,
    /// or a young cell in an old slab not yet remembered, reaches the slab
    /// barrier, which also preserves the registers in `keep`.
    pub(super) fn emit_element_write_barrier(
        &mut self,
        node: NodeId,
        base: u8,
        index: u8,
        value: u8,
        keep: &[u8],
    ) {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let layout = self.view.gc_barrier;
        let settled = layout.young_flag | layout.remembered_flag;
        let storage = otter_vm::jit::JitArrayStorage::current();
        let slab_flags = storage.slab_header_from_base + layout.header_flags_byte as i32;
        let (dirty_start, dirty_end) = (
            storage.slab_dirty_start_from_base,
            storage.slab_dirty_end_from_base,
        );
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(value), r10 ; jnz =>done ; test Rq(value), Rq(value) ; jz =>done
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
            ; cmp BYTE [r10], 0 ; jne =>slow
            ; test BYTE [Rq(value) + layout.header_flags_byte as i32], layout.young_flag as i8 ; jz =>done
            // The slot joins the interval the slab's next scavenge re-traces;
            // an empty interval is `[u32::MAX, 0)`.
            ; mov r10d, [Rq(base) + dirty_start] ; cmp r10d, Rd(index)
            ; cmova r10d, Rd(index) ; mov [Rq(base) + dirty_start], r10d
            ; lea r11d, [Rq(index) + 1]
            ; mov r10d, [Rq(base) + dirty_end] ; cmp r10d, r11d
            ; cmovb r10d, r11d ; mov [Rq(base) + dirty_end], r10d
            ; test BYTE [Rq(base) + slab_flags], settled as i8 ; jz =>slow
            ; =>done);
        let live = self.slow_path_registers(node, keep);
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>slow);
            let saved = codegen.emit_save_registers(&live);
            dynasm!(codegen.ops ; .arch x64
                ; sub rsp, 32
                ; mov [rsp], Rq(base) ; mov [rsp + 8], Rq(index) ; mov [rsp + 16], Rq(value)
                ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
                ; mov rsi, [rsp] ; mov edx, [rsp + 8] ; mov rcx, [rsp + 16]);
            codegen.sp_delta += 32;
            crate::x86_64::values::emit_load_runtime_stub(
                &mut codegen.ops,
                &mut codegen.relocations,
                otter_vm::runtime_stubs::ELEMENT_WRITE_BARRIER_MUTATING.entry_addr() as u64,
                abi::STUB_ELEMENT_WRITE_BARRIER,
            );
            crate::x86_64::call_abi::emit_runtime_call(
                &mut codegen.ops,
                abi::STUB_ELEMENT_WRITE_BARRIER,
            );
            dynasm!(codegen.ops ; .arch x64 ; add rsp, 32);
            codegen.sp_delta -= 32;
            codegen.emit_restore_registers(&live, saved);
            dynasm!(codegen.ops ; .arch x64 ; jmp =>done);
        }));
    }

    /// The registers a non-collecting slow path of `node` saves: the values
    /// live across it and the operands in `keep` the node still reads.
    fn slow_path_registers(
        &self,
        node: NodeId,
        keep: &[u8],
    ) -> smallvec::SmallVec<[(Location, Repr); 8]> {
        let mut live = self.allocation.node(node).live_registers.clone();
        for &register in keep {
            if !live
                .iter()
                .any(|&(location, _)| location == Location::Gp(register))
            {
                live.push((Location::Gp(register), Repr::Word));
            }
        }
        live
    }
}

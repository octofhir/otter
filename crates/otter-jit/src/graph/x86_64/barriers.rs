//! x86-64 Graph generational and marking edge barriers.
//!
//! # Contents
//! - Object/value and array-element barrier fast paths.
//! - Published child-shape barriers for named and shared-cache transitions.
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
use crate::graph::ir::NodeId;

impl Codegen<'_> {
    pub(super) fn emit_write_barrier(&mut self, node: NodeId, parent: u8, value: u8) {
        let done = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64 ; test Rq(value), r10 ; jnz =>done ; test Rq(value), Rq(value) ; jz =>done);
        self.emit_cell_edge_barrier(node, parent, value);
        dynasm!(self.ops ; .arch x64 ; =>done);
    }

    fn emit_cell_edge_barrier(&mut self, node: NodeId, parent: u8, child: u8) {
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
        let live = self.allocation.node(node).live_registers.clone();
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
        self.emit_cell_edge_barrier(node, receiver, child);
    }

    pub(super) fn emit_element_write_barrier(
        &mut self,
        node: NodeId,
        base: u8,
        index: u8,
        value: u8,
    ) {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let layout = self.view.gc_barrier;
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(value), r10 ; jnz =>done ; test Rq(value), Rq(value) ; jz =>done
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
            ; cmp BYTE [r10], 0 ; jne =>slow
            ; test BYTE [Rq(value) + layout.header_flags_byte as i32], layout.young_flag as i8 ; jnz =>slow
            ; =>done);
        let live = self.allocation.node(node).live_registers.clone();
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
}

//! x86-64 pure native leaf emission from canonical Graph homes.
//!
//! # Contents
//! - The shared System V/Microsoft heap/value/value C call and relocation.
//! - Full-word passive Probe status checks and exact eager-call recovery.
//!
//! # Invariants
//! The call allocation contract has homed every live and recovery-only value
//! before this emitter runs. The platform helper alone owns Microsoft shadow
//! space/hidden pair return. No frame/window interop, safepoint, source stamp or
//! JavaScript callback runs on a hit. Success alone commits rax; canonical-zero
//! miss leaves before effects, after the platform helper restored RSP.
//!
//! # See also
//! - `super::super::native_leaf` owns declaration admission.
//! - `crate::x86_64::call_abi` owns actual C argument and aggregate placement.

use super::*;

impl Codegen<'_> {
    pub(super) fn emit_native_leaf(
        &mut self,
        node: NodeId,
        id: abi::RuntimeStubId,
    ) -> Result<(), Unsupported> {
        let stub = super::super::native_leaf::entry(id)?;
        let inputs = self.loc(node).inputs.clone();
        if inputs.len() != 2
            || inputs
                .iter()
                .any(|input| !matches!(input, Location::TaggedSlot(_) | Location::Constant(_)))
            || self.loc(node).result != Some(Location::Gp(0))
        {
            return Err(Unsupported::OperandShape(
                "Graph native leaf canonical operands",
            ));
        }
        self.emit_move(inputs[0], Location::Gp(6));
        self.emit_move(inputs[1], Location::Gp(2));
        dynasm!(self.ops ; .arch x64
            ; mov rdi,[r15+crate::entry::THREAD_OFFSET as i32]
            ; mov rdi,[rdi+crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
        );
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            stub.entry_addr() as u64,
            stub.descriptor,
        );
        crate::x86_64::call_abi::emit_runtime_call(&mut self.ops, stub.descriptor);
        let done = self.ops.new_dynamic_label();
        let miss = self.eager_exit(node, DeoptReason::WrongType);
        let fatal = self.fatal;
        // Decode the passive subset of the VM-owned Probe domain after the
        // sole platform boundary has normalized the pair and restored RSP.
        dynasm!(self.ops ; .arch x64
            ; test rdx,rdx
            ; jz =>done
            ; cmp rdx,abi::NativeResultStatus::SideExit as i32
            ; jne =>fatal
            ; test rax,rax
            ; jnz =>fatal
            ; jmp =>miss
            ; =>done
        );
        Ok(())
    }
}

//! AArch64 pure native leaf emission from canonical Graph homes.
//!
//! # Contents
//! - The existing heap/value/value C call and VM descriptor relocation.
//! - Full-word passive Probe status checks and exact eager-call recovery.
//!
//! # Invariants
//! The call allocation contract has homed every live and recovery-only value
//! before this emitter runs. No register save span, frame/window interop,
//! safepoint, collection, source stamp or JavaScript callback runs on a hit.
//! Success alone commits x0; a canonical-zero miss leaves before effects.
//!
//! # See also
//! - `super::super::native_leaf` owns declaration admission.
//! - `super::super::regalloc` owns call-clobber preservation and eager homes.

use super::*;

impl Codegen<'_> {
    pub(super) fn emit_native_leaf(
        &mut self,
        node: NodeId,
        id: abi::RuntimeStubId,
    ) -> Result<(), Unsupported> {
        let stub = super::super::native_leaf::entry(id)?;
        let inputs = self.allocation.node(node).inputs.clone();
        if inputs.len() != 2
            || inputs
                .iter()
                .any(|input| !matches!(input, Location::TaggedSlot(_) | Location::Constant(_)))
            || self.allocation.node(node).result != Some(Location::Gp(0))
        {
            return Err(Unsupported::OperandShape(
                "Graph native leaf canonical operands",
            ));
        }
        self.emit_move(inputs[0], Location::Gp(1));
        self.emit_move(inputs[1], Location::Gp(2));
        dynasm!(self.ops ; .arch aarch64
            ; ldr x0,[x20,THREAD_OFFSET]
            ; ldr x0,[x0,VM_THREAD_GC_HEAP_OFFSET]
        );
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            stub.entry_addr() as u64,
            RelocationTarget::runtime_stub(stub.descriptor),
        );
        dynasm!(self.ops ; .arch aarch64 ; blr x16);
        let done = self.ops.new_dynamic_label();
        let miss = self.eager_exit(node, DeoptReason::WrongType);
        let miss = self.cond_target(miss);
        let fatal = self.cond_target(self.fatal);
        // The admitted passive subset of the sole Probe domain returns only
        // Success or canonical-zero SideExit. Malformed status/payload is fatal.
        dynasm!(self.ops ; .arch aarch64
            ; cbz x1,=>done
            ; cmp x1,#abi::NativeResultStatus::SideExit as u32
            ; b.ne =>fatal
            ; cbnz x0,=>fatal
            ; b =>miss
            ; =>done
        );
        Ok(())
    }
}

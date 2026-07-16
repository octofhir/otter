//! Typed control, exception, and cold-deopt operations.
//!
//! Materialized-only operations will live here so identity branching stays in
//! the VM boundary rather than leaking interpreter stack indices to the JIT.

use crate::VmError;

use super::RuntimeCall;

impl RuntimeCall<'_> {
    /// Poll interrupts and runtime budget at one compiled back-edge.
    pub fn backedge_poll(&mut self) -> Result<(), VmError> {
        // SAFETY: the branded call owns exclusive mutator access for this
        // short operation and retains only a raw descriptor afterwards.
        unsafe { &mut *self.vm.as_ptr() }.jit_backedge_poll()
    }
}

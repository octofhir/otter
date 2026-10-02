//! Prepare a published generated callee for interpreter continuation.
//!
//! # Contents
//! - Exact side-exit and generation validation.
//! - In-place frame and static-catch preparation without executing JavaScript.
//!
//! # Invariants
//! The callee retains its windows throughout this operation. A side exit
//! belongs to the generation that took it, whatever entered that generation:
//! the call trampoline links interpreter, host and generated callers alike, so
//! the caller is neither inspected nor charged. All metadata reads finish
//! before exclusive VM access. This preparation never dispatches the callee;
//! assembly owns the following continuation.
//!
//! # See also
//! - `crate::interp::jit_calls::generated` for the exit's cost policy.
//! - `crate::native_abi::deopt_call_entry` for the generated continuation owner.

use super::RuntimeCall;
use crate::{
    VmError,
    native_abi::{NO_SAFEPOINT, SideExit},
};

impl RuntimeCall<'_> {
    /// Prepare the published callee that took `exit` for continuation in
    /// the interpreter at the exit's PC.
    pub(crate) fn prepare_deopt_call(&mut self, exit: SideExit) -> Result<(), VmError> {
        // SAFETY: RuntimeCall retains the published callee; this immutable
        // view performs no allocation or JavaScript reentry.
        let callee = unsafe { self.frame.as_ref() };
        if exit.logical_pc() != callee.header.pc {
            return Err(VmError::InvalidOperand);
        }
        // SAFETY: the immutable borrow above ends here. RuntimeCall owns
        // exclusive logical access to this VM and its published frame.
        let vm = unsafe { self.vm.as_mut() };
        vm.note_entered_generation_deopt(&self.context, unsafe { self.frame.as_ref() }, exit)?;
        // The generated callee's prologue and its root homes have returned.
        // The canonical window now owns every value needed by continuation.
        let frame = unsafe { self.frame.as_mut() };
        frame.call_site = NO_SAFEPOINT;
        vm.prepare_deoptimized_frame(&self.context, frame)
    }
}

//! Source identity for boxed calls from physical and inlined activations.
//!
//! # Contents
//! Resolve a call's function and logical PC from the active safepoint recipe.
//!
//! # Invariants
//! The physical frame retains its enclosing call PC. Virtual descendants name
//! their own bytecode operation without materializing or replaying a caller.
//! Registry borrows end before any JavaScript or allocation can occur.
//!
//! # See also
//! `value_ops` validates the source opcode and completes the boxed call.

use super::RuntimeCall;
use crate::{
    VmError,
    native_abi::{NO_SAFEPOINT, NativeFrameKind},
};

impl RuntimeCall<'_> {
    pub(super) fn value_call_source(&self) -> Result<(u32, u32), VmError> {
        // SAFETY: RuntimeCall retains this published frame and its VM. Only
        // immutable metadata is read; no allocation or reentry occurs.
        let frame = unsafe { self.frame.as_ref() };
        if frame.call_site == NO_SAFEPOINT || frame.header.kind != NativeFrameKind::Optimizing {
            return Ok((self.function_id(), self.pc()));
        }
        let vm = unsafe { self.vm.as_ref() };
        let record = vm
            .jit_code_registry
            .safepoint_record(u64::from(frame.code_object_id), frame.call_site)
            .ok_or(VmError::InvalidOperand)?;
        if !record.inline_frames_virtual || record.inline_frames.is_empty() {
            return Ok((self.function_id(), self.pc()));
        }
        if record.call_pc != frame.header.pc {
            return Err(VmError::InvalidOperand);
        }
        let source = record.inline_frames.last().ok_or(VmError::InvalidOperand)?;
        let owner = self
            .context
            .for_function(source.function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let function = owner
            .exec_function(source.function_id)
            .ok_or(VmError::InvalidOperand)?;
        let pc = (0..function.code.len())
            .find(|&pc| function.instruction_byte_pc(pc) == Some(source.byte_pc))
            .and_then(|pc| u32::try_from(pc).ok())
            .ok_or(VmError::InvalidOperand)?;
        Ok((source.function_id, pc))
    }
}

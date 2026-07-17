//! Frameless compiled-callee completion and cold bailout adoption.
//!
//! # Contents
//! - Hot returned/aborted completion by compact owner id.
//! - Cold adoption of one native activation above the caller's activation
//!   floor after a bailout has actually fired.
//!
//! # Invariants
//! - Every successful prepare removes exactly its youngest
//!   [`crate::native_call_owners::NativeCallOwner`] and releases exactly one
//!   synchronous re-entry guard.
//! - Normal return and abort never construct or inspect a [`ActivationStack`]. The
//!   compiled caller writes the returned value through its live ActiveFrame.
//! - Bailout moves the owner's register window into one [`Frame`] without
//!   copying registers. Owned upvalues move with it; a borrowed closure spine
//!   is copied exactly once here, after the bailout has fired.
//! - The copied [`NativeFrame`] descriptor must still match the owner that
//!   backed it. Descriptor validation happens before interpreter dispatch.
//!
//! # See also
//! - [`super::frame`] for the paired owner-publication transaction.
//! - [`crate::NativeFrame`] for the scalar activation snapshot supplied by
//!   generated code while the native activation is still live.

use crate::*;

/// Validate that a copied native descriptor still names exactly `owner`.
fn validate_owner_frame(
    owner_id: u32,
    owner: &crate::native_call_owners::NativeCallOwner,
    frame: &NativeFrame,
) -> Result<(), VmError> {
    let upvalues = owner.upvalues.source();
    let expected_upvalue_base = upvalues.base_ptr_or_null() as u64;
    if frame.native_owner_id() != Some(owner_id)
        || frame.header.kind != owner.tier
        || frame.header.function_id != owner.function_id
        || usize::from(frame.header.register_count) != owner.registers.len()
        || frame.register_base != owner.registers.as_mut_ptr() as u64
        || frame.upvalue_base != expected_upvalue_base
        || frame.upvalue_count != upvalues.len_u32()
    {
        return Err(VmError::InvalidOperand);
    }
    Ok(())
}

/// Move one validated native owner into the sole cold interpreter frame.
fn materialize_owner_frame(
    owner: crate::native_call_owners::NativeCallOwner,
    mut native: NativeFrame,
) -> Frame {
    native.header.kind = NativeFrameKind::Interpreter;
    native.header.flags = NativeFrameFlags::empty();
    Frame {
        header: native.header,
        registers: owner.registers,
        upvalues: owner.upvalues.into_materialized(),
        self_value: native.self_value(),
        this_value: native.this_value(),
        return_register: None,
        cold: None,
    }
}

impl Interpreter {
    /// Record a direct call only after native entry actually ran.
    fn note_direct_call_entry(&mut self, tier: NativeFrameKind) {
        self.jit_runtime_stats.direct_calls = self.jit_runtime_stats.direct_calls.saturating_add(1);
        if tier == NativeFrameKind::Optimizing {
            self.jit_runtime_stats.optimized_entries =
                self.jit_runtime_stats.optimized_entries.saturating_add(1);
        }
    }

    /// Release a direct compiled call that returned normally.
    ///
    /// The returned `Value` is intentionally absent: generated code still has
    /// the caller ActiveFrame live and performs the destination write itself.
    pub fn jit_finish_direct_call_returned(&mut self, owner_id: u32) -> Result<(), VmError> {
        let owner = self.native_call_owners.pop(owner_id)?;
        self.note_direct_call_entry(owner.tier);
        if owner.tier == NativeFrameKind::Baseline {
            self.note_jit_entry_success(owner.function_id);
        }
        self.free_reg_window(owner.registers.stack_base());
        self.leave_sync_reentry();
        Ok(())
    }

    /// Adopt a bailed compiled callee above the caller's activation floor.
    ///
    /// `native.header.pc` is the exact logical resume PC published by the
    /// compiled side exit. The descriptor must be copied while its native
    /// frame is live; no pointer to that native stack record is retained here.
    pub fn jit_finish_direct_call_bailed(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        owner_id: u32,
        native: NativeFrame,
    ) -> Result<Value, VmError> {
        let owner = self.native_call_owners.pop(owner_id)?;
        if let Err(err) = validate_owner_frame(owner_id, &owner, &native) {
            self.free_reg_window(owner.registers.stack_base());
            self.leave_sync_reentry();
            return Err(err);
        }

        let function_id = owner.function_id;
        let tier = owner.tier;
        // Materialize borrowed storage before tiering bookkeeping. The native
        // activation has just been unpublished by the trampoline, so this
        // short non-GC step closes the borrowed-source lifetime immediately.
        let frame = materialize_owner_frame(owner, native);
        self.note_direct_call_entry(tier);
        if tier == NativeFrameKind::Baseline {
            self.note_jit_entry_bail(function_id);
        } else {
            self.jit_runtime_stats.optimized_deopts =
                self.jit_runtime_stats.optimized_deopts.saturating_add(1);
        }
        let floor = stack.floor();
        stack.push(frame);
        let result = self.dispatch_loop_above_rooted(context, stack, floor);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    /// Abort a direct entry after throw or before native entry was accepted.
    pub fn jit_abort_direct_call(&mut self, owner_id: u32, entered: bool) -> Result<(), VmError> {
        let owner = self.native_call_owners.pop(owner_id)?;
        if entered {
            self.note_direct_call_entry(owner.tier);
        }
        self.free_reg_window(owner.registers.stack_base());
        self.leave_sync_reentry();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        native_call_owners::{NativeCallOwner, NativeCallUpvalues},
        upvalue_source::UpvalueSource,
    };

    fn publish_test_owner(vm: &mut Interpreter, function_id: u32) -> (u32, RegisterWindow) {
        vm.enter_sync_reentry().unwrap();
        let registers = vm.alloc_reg_window(3).unwrap();
        let owner_id = vm
            .native_call_owners
            .push(NativeCallOwner {
                function_id,
                tier: NativeFrameKind::Baseline,
                registers,
                upvalues: NativeCallUpvalues::borrowed(UpvalueSource::empty()),
            })
            .unwrap();
        (owner_id, registers)
    }

    #[test]
    fn hot_return_releases_owner_without_materializing_an_activation() {
        let mut vm = Interpreter::new();
        let pooled_before = vm.reentry_stack_cache.len();
        let (owner_id, _) = publish_test_owner(&mut vm, 17);

        vm.jit_finish_direct_call_returned(owner_id).unwrap();

        assert_eq!(vm.native_call_owners.len(), 0);
        assert_eq!(vm.register_stack.checkpoint(), 0);
        assert_eq!(vm.reentry_stack_cache.len(), pooled_before);
    }

    #[test]
    fn materialization_adopts_owner_window_only_after_bail() {
        let mut vm = Interpreter::new();
        let (owner_id, registers) = publish_test_owner(&mut vm, 29);
        let owner = vm.native_call_owners.pop(owner_id).unwrap();
        let mut header = VmFrameHeader::interpreter(29, 3);
        header.kind = NativeFrameKind::Baseline;
        let mut native = NativeFrame::new(
            header,
            registers.as_mut_ptr() as u64,
            Value::function(29),
            Value::number(NumberValue::from_i32(7)),
        );
        native.header.pc = 41;
        native.set_native_owner(owner_id + 1);
        assert!(matches!(
            validate_owner_frame(owner_id, &owner, &native),
            Err(VmError::InvalidOperand)
        ));
        native.set_native_owner(owner_id);

        validate_owner_frame(owner_id, &owner, &native).unwrap();
        let mut frame = materialize_owner_frame(owner, native);

        assert_eq!(frame.registers.as_mut_ptr(), registers.as_mut_ptr());
        assert_eq!(frame.pc, 41);
        assert_eq!(frame.self_value, Value::function(29));
        assert_eq!(frame.this_value, Value::number(NumberValue::from_i32(7)));
        vm.reclaim_registers(&mut frame);
        vm.leave_sync_reentry();
    }
}

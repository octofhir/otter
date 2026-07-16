//! Frameless compiled-callee completion and cold bailout adoption.
//!
//! # Contents
//! - Hot returned/aborted completion by compact owner id.
//! - Cold adoption of one native activation into an independent interpreter
//!   stack after a bailout has actually fired.
//!
//! # Invariants
//! - Every successful prepare removes exactly its youngest
//!   [`crate::native_call_owners::NativeCallOwner`] and releases exactly one
//!   synchronous re-entry guard.
//! - Normal return and abort never construct or inspect a [`HoltStack`]. The
//!   compiled caller writes the returned value through its live ActiveFrame.
//! - Bailout moves the owner's register window and upvalue spine into one
//!   [`Frame`] without copying registers and never materializes the parent.
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
    let expected_upvalue_base = if owner.upvalues.is_empty() {
        0
    } else {
        owner.upvalues.as_ptr() as u64
    };
    if frame.native_owner_id() != Some(owner_id)
        || !matches!(
            frame.header.kind,
            NativeFrameKind::Baseline | NativeFrameKind::Optimizing
        )
        || frame.header.function_id != owner.function_id
        || usize::from(frame.header.register_count) != owner.registers.len()
        || frame.register_base != owner.registers.as_mut_ptr() as u64
        || frame.upvalue_base != expected_upvalue_base
        || usize::try_from(frame.upvalue_count).ok() != Some(owner.upvalues.len())
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
        upvalues: owner.upvalues,
        self_value: native.self_value(),
        this_value: native.this_value(),
        return_register: None,
        cold: None,
    }
}

impl Interpreter {
    /// Release a direct compiled call that returned normally.
    ///
    /// The returned `Value` is intentionally absent: generated code still has
    /// the caller ActiveFrame live and performs the destination write itself.
    pub fn jit_finish_direct_call_returned(&mut self, owner_id: u32) -> Result<(), VmError> {
        let owner = self.native_call_owners.pop(owner_id)?;
        self.note_jit_entry_success(owner.function_id);
        self.free_reg_window(owner.registers.stack_base());
        self.leave_sync_reentry();
        Ok(())
    }

    /// Adopt a bailed compiled callee into one independent interpreter stack.
    ///
    /// `native.header.pc` is the exact logical resume PC published by the
    /// compiled side exit. The descriptor must be copied while its native
    /// frame is live; no pointer to that native stack record is retained here.
    pub fn jit_finish_direct_call_bailed(
        &mut self,
        context: &ExecutionContext,
        owner_id: u32,
        native: NativeFrame,
    ) -> Result<Value, VmError> {
        let owner = self.native_call_owners.pop(owner_id)?;
        if let Err(err) = validate_owner_frame(owner_id, &owner, &native) {
            self.free_reg_window(owner.registers.stack_base());
            self.leave_sync_reentry();
            return Err(err);
        }

        self.note_jit_entry_bail(owner.function_id);
        let frame = materialize_owner_frame(owner, native);
        let mut stack = self.draw_stack();
        debug_assert!(stack.is_empty(), "pooled HoltStack must be drained");
        stack.push(frame);
        let result = self.dispatch_loop(context, &mut stack);
        self.return_stack(stack);
        self.leave_sync_reentry();
        result
    }

    /// Abort a prepared or rejected direct entry without materialization.
    pub fn jit_abort_direct_call(&mut self, owner_id: u32) -> Result<(), VmError> {
        let owner = self.native_call_owners.pop(owner_id)?;
        self.free_reg_window(owner.registers.stack_base());
        self.leave_sync_reentry();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_call_owners::NativeCallOwner;

    fn publish_test_owner(vm: &mut Interpreter, function_id: u32) -> (u32, RegisterWindow) {
        vm.enter_sync_reentry().unwrap();
        let registers = vm.alloc_reg_window(3).unwrap();
        let owner_id = vm
            .native_call_owners
            .push(NativeCallOwner {
                function_id,
                registers,
                upvalues: Vec::new().into_boxed_slice(),
            })
            .unwrap();
        (owner_id, registers)
    }

    #[test]
    fn hot_return_releases_owner_without_holt_stack() {
        let mut vm = Interpreter::new();
        let pooled_before = vm.holt_pool.len();
        let (owner_id, _) = publish_test_owner(&mut vm, 17);

        vm.jit_finish_direct_call_returned(owner_id).unwrap();

        assert_eq!(vm.native_call_owners.len(), 0);
        assert_eq!(vm.register_stack.checkpoint(), 0);
        assert_eq!(vm.holt_pool.len(), pooled_before);
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

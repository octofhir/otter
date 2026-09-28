//! Representation-neutral active-frame opcode kernels.
//!
//! Hot binding operations consume [`ActiveFrameMut`] and therefore run over
//! either a materialized [`Frame`] or the canonical [`crate::native_abi::NativeFrame`]
//! without copying registers or reconstructing a [`ActivationStack`]. Kernels never
//! advance the PC: interpreter dispatch, baseline code, and optimizing code
//! each own their continuation coordinate.
//!
//! # Contents
//! - Representation-neutral `this` load.
//! - Explicit materialized-frame operations for rest arguments and
//!   structured-exception cold state.
//!
//! # Invariants
//! - Inputs are decoded from the executable instruction format before reaching
//!   these helpers.
//! - Hot kernels do not inspect [`ActivationStack`] or [`crate::cold_frame::ColdFrame`].
//! - `this` is the activation's own binding: an arrow's copy of its creating
//!   activation's `this`, or a derived constructor's frame-held binding.
//!   A derived-constructor `this` captured by an arrow or visible to a direct
//!   eval lives in a context slot and is read by the context kernels instead.
//! - Callers advance or replace the PC only after a kernel commits.
//!
//! # See also
//! - [`crate::active_frame`]
//! - [`crate::context_ops`]
//! - [`crate::executable`]

use crate::activation_stack::ActivationStack;
use smallvec::SmallVec;

use crate::{ActiveFrameMut, Frame, Interpreter, TryHandler, Value, VmError};

impl Interpreter {
    /// Load the current `this` binding into `dst` for either frame storage.
    ///
    /// Derived constructors publish a hole until `super()` binds `this`; a
    /// hole is the canonical TDZ failure.
    pub(crate) fn frame_load_this(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.this_value();
        if value.is_hole() {
            return Err(self.err_this_uninit(crate::context_ops::DERIVED_THIS_UNINITIALIZED.into()));
        }
        frame.write(dst, value)
    }

    /// Materialize a legacy cold-sidecar rest-argument buffer.
    ///
    /// This helper remains ActivationStack-specific because allocation must trace the
    /// complete materialized stack and because the buffer is owned by
    /// [`crate::cold_frame::ColdFrame`]. It still leaves PC ownership to the
    /// caller.
    pub(crate) fn materialized_collect_rest(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        // Drain rather than clone: the rest array is built once per call and
        // CollectRest is the single consumer.
        let elements: SmallVec<[Value; 4]> = self
            .frame_cold_mut(&mut stack[top_idx])
            .map(|c| std::mem::take(&mut c.rest_args))
            .unwrap_or_default();
        let array = self.alloc_stack_rooted_array_from_values(&*stack, elements, &[], &[])?;
        ActiveFrameMut::materialized(&mut stack[top_idx]).write(dst, Value::array(array))
    }

    /// Install a verified exception region in a materialized cold sidecar.
    pub(crate) fn materialized_enter_try_region(
        &mut self,
        frame: &mut Frame,
        region: crate::executable::code_block_cfg::CodeBlockExceptionRegion,
    ) -> Result<(), VmError> {
        debug_assert_eq!(region.enter_pc, frame.pc);
        self.materialized_enter_try_handler(
            frame,
            TryHandler {
                catch_pc: region.catch_pc,
                finally_pc: region.finally_pc,
                exc_register: region.exception_register,
            },
        )
    }

    /// Install a decoded handler in a materialized cold sidecar.
    pub(crate) fn materialized_enter_try_handler(
        &mut self,
        frame: &mut Frame,
        handler: TryHandler,
    ) -> Result<(), VmError> {
        self.frame_ensure_cold(frame).handlers.push(handler);
        Ok(())
    }

    /// Drop abandoned finally completions from materialized cold state.
    pub(crate) fn materialized_pop_parked_finally(
        &mut self,
        frame: &mut Frame,
        count: usize,
    ) -> Result<(), VmError> {
        if let Some(cold) = self.frame_cold_mut(frame) {
            for _ in 0..count {
                cold.parked_finally.pop();
            }
        }
        Ok(())
    }

    /// Leave the innermost handler in a materialized cold sidecar.
    pub(crate) fn materialized_leave_try(&mut self, frame: &mut Frame) -> Result<(), VmError> {
        let popped = self.frame_cold_mut(frame).and_then(|c| c.handlers.pop());
        let Some(handler) = popped else {
            return Err(VmError::InvalidOperand);
        };
        // §14.15.3 — leaving a try (or catch) body whose handler owns
        // a `finally` falls through into the finally block; park a
        // Normal completion so `Op::EndFinally` knows this entry was
        // not an unwind.
        if handler.finally_pc.is_some() {
            let cold = self.frame_ensure_cold(frame);
            let depth = cold.handlers.len() as u32;
            cold.parked_finally
                .push((crate::cold_frame::ParkedFinally::Normal, depth));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RegisterWindow,
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader},
    };

    fn header(register_count: usize) -> VmFrameHeader {
        VmFrameHeader {
            function_id: 7,
            pc: 3,
            register_count: register_count as u16,
            kind: NativeFrameKind::Interpreter,
            flags: NativeFrameFlags::empty(),
        }
    }

    fn materialized_frame(slots: &mut [Value], self_value: Value, this_value: Value) -> Frame {
        Frame {
            header: header(slots.len()),
            registers: RegisterWindow::attached(slots.as_mut_ptr(), slots.len(), 0),
            self_value,
            this_value,
            return_register: None,
            cold: None,
        }
    }

    #[test]
    fn binding_kernels_match_materialized_and_native_frames() {
        let interpreter = Interpreter::new();
        let self_value = Value::function(31);
        let this_value = Value::number_i32(17);

        let mut materialized_slots = [Value::undefined(); 2];
        let mut materialized = materialized_frame(&mut materialized_slots, self_value, this_value);
        {
            let mut active = ActiveFrameMut::materialized(&mut materialized);
            interpreter
                .frame_load_this(&mut active, 0)
                .expect("materialized this");
            let self_value = active.self_value();
            active.write(1, self_value).expect("materialized SELF");
        }

        let mut native_slots = [Value::undefined(); 2];
        let mut native = NativeFrame::new(
            header(native_slots.len()),
            native_slots.as_mut_ptr() as u64,
            self_value,
            this_value,
        );
        {
            // SAFETY: the native frame and its initialized register window
            // remain exclusively live and unmoved for this scoped view.
            let mut active = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }
                .expect("valid native frame");
            interpreter
                .frame_load_this(&mut active, 0)
                .expect("native this");
            let self_value = active.self_value();
            active.write(1, self_value).expect("native SELF");
        }

        assert_eq!(materialized_slots, native_slots);
        assert_eq!(materialized_slots, [this_value, self_value]);
        assert_eq!(materialized.header.pc, 3);
        assert_eq!(native.header.pc, 3);
    }

    #[test]
    fn derived_this_hole_is_the_named_reference_error() {
        let interpreter = Interpreter::new();
        let mut slots = [Value::undefined()];
        let mut frame = materialized_frame(&mut slots, Value::function(7), Value::hole());
        let mut active = ActiveFrameMut::materialized(&mut frame);
        let error = interpreter
            .frame_load_this(&mut active, 0)
            .expect_err("derived this before super()");
        assert!(matches!(error, VmError::ThisUninitialized));
        assert_eq!(active.read(0).unwrap(), Value::undefined());
    }
}

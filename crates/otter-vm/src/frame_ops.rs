//! Opcode kernels over the published JavaScript frame.
//!
//! # Contents
//! - Current `this` binding and rest-array materialization.
//! - Cold exception-handler and finally state.
//!
//! # Invariants
//! Kernels receive decoded inputs and keep register access scoped. Rest values
//! come from the immutable actual-argument window after the register capacity.
//! A derived frame holds a hole until `super()` binds its own `this`; captured
//! lexical bindings live in contexts. Callers own PC advancement after effects.
//!
//! # See also
//! - [`crate::active_frame`] for register and actual-window access.
//! - [`crate::context_ops`] for captured bindings.
//! - [`crate::cold_frame`] for exception state.

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

    /// Allocate a rest array from the published actual-argument suffix.
    /// The runtime turn roots the complete physical chain across allocation.
    pub(crate) fn collect_rest(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        parameter_count: u16,
        dst: u16,
    ) -> Result<(), VmError> {
        let frame = crate::ActiveFrameRef::from_frame(&stack[top_idx]);
        let count = frame.incoming_argument_count();
        let elements: SmallVec<[Value; 4]> = (usize::from(parameter_count)..count)
            .map(|index| frame.incoming_argument(index))
            .collect::<Result<_, _>>()?;
        let array = self.alloc_stack_rooted_array_from_values(&*stack, elements, &[], &[])?;
        ActiveFrameMut::from_frame(&mut stack[top_idx]).write(dst, Value::array(array))
    }

    /// Install a verified exception region in the frame's cold record.
    pub(crate) fn frame_enter_try_region(
        &mut self,
        frame: &mut Frame,
        region: crate::executable::code_block_cfg::CodeBlockExceptionRegion,
    ) -> Result<(), VmError> {
        debug_assert_eq!(region.enter_pc, frame.pc);
        self.frame_enter_try_handler(
            frame,
            TryHandler {
                catch_pc: region.catch_pc,
                finally_pc: region.finally_pc,
                exc_register: region.exception_register,
            },
        )
    }

    /// Install a decoded handler in the frame's cold record.
    pub(crate) fn frame_enter_try_handler(
        &mut self,
        frame: &mut Frame,
        handler: TryHandler,
    ) -> Result<(), VmError> {
        self.frame_ensure_cold(frame).handlers.push(handler);
        Ok(())
    }

    /// Drop abandoned finally completions from cold state.
    pub(crate) fn frame_pop_parked_finally(
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

    /// Leave the innermost handler in the frame's cold record.
    pub(crate) fn frame_leave_try(&mut self, frame: &mut Frame) -> Result<(), VmError> {
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
    use crate::native_abi::{Frame, NativeFrameFlags, NativeFrameKind, VmFrameHeader};

    fn header(register_count: usize) -> VmFrameHeader {
        VmFrameHeader {
            function_id: 7,
            pc: 3,
            register_count: register_count as u16,
            kind: NativeFrameKind::Interpreter,
            flags: NativeFrameFlags::empty(),
        }
    }

    fn frame_fixture(slots: &mut [Value], self_value: Value, this_value: Value) -> Frame {
        Frame::new(
            header(slots.len()),
            slots.as_mut_ptr() as u64,
            self_value,
            this_value,
        )
    }

    #[test]
    fn binding_kernels_preserve_the_pc_in_every_tier() {
        let interpreter = Interpreter::new();
        let self_value = Value::function(31);
        let this_value = Value::number_i32(17);
        for kind in [
            NativeFrameKind::Interpreter,
            NativeFrameKind::Baseline,
            NativeFrameKind::Optimizing,
        ] {
            let mut slots = [Value::UNDEFINED; 2];
            let mut header = header(slots.len());
            header.kind = kind;
            let mut frame = Frame::new(header, slots.as_mut_ptr() as u64, self_value, this_value);
            let mut active = ActiveFrameMut::from_frame(&mut frame);
            interpreter
                .frame_load_this(&mut active, 0)
                .expect("this binding");
            active.write(1, active.self_value()).expect("SELF binding");
            assert_eq!(slots, [this_value, self_value]);
            assert_eq!(frame.header.pc, 3);
        }
    }

    #[test]
    fn derived_this_hole_is_the_named_reference_error() {
        let interpreter = Interpreter::new();
        let mut slots = [Value::undefined()];
        let mut frame = frame_fixture(&mut slots, Value::function(7), Value::hole());
        let mut active = ActiveFrameMut::from_frame(&mut frame);
        let error = interpreter
            .frame_load_this(&mut active, 0)
            .expect_err("derived this before super()");
        assert!(matches!(error, VmError::ThisUninitialized));
        assert_eq!(active.read(0).unwrap(), Value::undefined());
    }
}

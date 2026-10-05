//! Opcode kernels over the published JavaScript frame.
//!
//! # Contents
//! - Current `this` binding and rest-array materialization.
//! - Abandoning protocol ladders whose staged call threw.
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

use crate::activation_stack::ActivationStack;
use smallvec::SmallVec;

use crate::{ActiveFrameMut, Interpreter, Value, VmError};

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

    /// A call a protocol ladder staged completed with a throw: the ladder's
    /// instruction fails, so its parked state is dropped. An abandoned
    /// `IteratorNext` leaves its iterator done (§7.4.8 IteratorStep).
    pub(crate) fn abandon_pending_ladders(&mut self, frame: &mut crate::Frame) {
        let Some(cold) = self.frame_cold_mut(frame) else {
            return;
        };
        let pending_next = cold.pending_iterator_next.take();
        cold.pending_to_primitive = None;
        cold.pending_get_iterator = None;
        cold.pending_bind_function = None;
        if let Some(pending) = pending_next {
            self.iterator_mark_done(pending.iterator);
        }
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
        let interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
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
        let interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
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

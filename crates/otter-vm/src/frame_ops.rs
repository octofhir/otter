//! Representation-neutral active-frame opcode kernels.
//!
//! Hot binding operations consume [`ActiveFrameMut`] and therefore run over
//! either a materialized [`Frame`] or the canonical [`crate::NativeFrame`]
//! without copying registers or reconstructing a [`ActivationStack`]. Kernels never
//! advance the PC: interpreter dispatch, baseline code, and optimizing code
//! each own their continuation coordinate.
//!
//! # Contents
//! - Representation-neutral SELF, `this`, and `new.target` loads.
//! - Representation-neutral upvalue load/store operations.
//! - Explicit materialized-frame operations for fresh upvalue spines, rest
//!   arguments, and structured-exception cold state.
//!
//! # Invariants
//! - Inputs are decoded from the executable instruction format before reaching
//!   these helpers.
//! - Hot kernels do not inspect [`ActivationStack`] or [`crate::cold_frame::ColdFrame`].
//! - Every captured-value write flows through [`store_upvalue`] and its GC write
//!   barrier.
//! - Callers advance or replace the PC only after a kernel commits.
//!
//! # See also
//! - [`crate::active_frame`]
//! - [`crate::executable`]

use crate::activation_stack::ActivationStack;
use smallvec::SmallVec;

use crate::{
    ActiveFrameMut, Frame, Interpreter, TryHandler, Value, VmError, read_upvalue, store_upvalue,
};

impl Interpreter {
    /// Load the current `this` binding into `dst` for either frame storage.
    ///
    /// Derived constructors publish a hole until `super()` binds `this`; the
    /// materialized dispatch path resolves lexical-arrow inheritance before
    /// entering this kernel. A remaining hole is the canonical TDZ failure.
    pub(crate) fn frame_load_this(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.this_value();
        if value.is_hole() {
            return Err(self.err_this_uninit(
                "must call super constructor in derived class before accessing 'this' or returning from derived constructor"
                    .into(),
            ));
        }
        frame.write(dst, value)
    }

    /// Load the exact running function object (SELF) into `dst`.
    pub(crate) fn frame_load_self(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.self_value();
        frame.write(dst, value)
    }

    /// Load the immutable `new.target` binding into `dst`.
    pub(crate) fn frame_load_new_target(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.new_target_value();
        frame.write(dst, value)
    }

    /// Load one captured binding, rejecting an uninitialized TDZ cell.
    pub(crate) fn frame_load_upvalue(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        let index = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)?;
        let cell = frame.upvalue(index)?;
        let value = read_upvalue(&self.gc_heap, cell);
        if value.is_hole() {
            return Err(VmError::TemporalDeadZone { local_index: index });
        }
        frame.write(dst, value)
    }

    /// Store one captured binding through the GC write barrier.
    pub(crate) fn frame_store_upvalue(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        let index = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)?;
        let value = frame.read(src)?;
        let cell = frame.upvalue(index)?;
        store_upvalue(&mut self.gc_heap, cell, value);
        Ok(())
    }

    /// Assignment to a captured lexical binding.
    ///
    /// A hole marks a binding whose declaration initializer has not run yet;
    /// assignment before initialization is a `ReferenceError`.
    pub(crate) fn frame_store_upvalue_checked(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        let index = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)?;
        let value = frame.read(src)?;
        let cell = frame.upvalue(index)?;
        if read_upvalue(&self.gc_heap, cell).is_hole() {
            return Err(VmError::TemporalDeadZone { local_index: index });
        }
        store_upvalue(&mut self.gc_heap, cell, value);
        Ok(())
    }

    /// Replace one activation-local upvalue with a fresh TDZ cell.
    ///
    /// Both interpreter and native activations own a mutable handle window;
    /// closure identity requires the fresh cell allocation, but no frame or
    /// upvalue-spine copy is performed.
    pub(crate) fn frame_fresh_upvalue(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        idx: i32,
    ) -> Result<(), VmError> {
        let index = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)?;
        let fresh = crate::alloc_upvalue(&mut self.gc_heap, Value::hole())?;
        frame.replace_upvalue(index, fresh)
    }

    /// Resolve a materialized frame's lexical `this` through the nearest
    /// derived-constructor sidecar. Native activations carry their resolved
    /// binding directly and never call this materialized-only operation.
    pub(crate) fn materialized_this_binding(
        &self,
        stack: &ActivationStack,
        frame_index: usize,
    ) -> Result<Value, VmError> {
        let mut value = stack
            .get(frame_index)
            .ok_or(VmError::InvalidOperand)?
            .this_value;
        if value.is_hole() {
            for index in (0..=frame_index).rev() {
                let frame = &stack[index];
                if self
                    .frame_cold(frame)
                    .is_some_and(|cold| cold.is_derived_constructor)
                {
                    value = frame.this_value;
                    break;
                }
            }
        }
        Ok(value)
    }

    /// Compiled-runtime `LoadUpvalue` over the canonical activation.
    ///
    /// The compiled tier owns PC progress, so this operation performs only the
    /// captured-binding read, TDZ check, and destination commit. It does not
    /// require or materialize a [`ActivationStack`] frame.
    ///
    /// # Errors
    /// Propagates `ReferenceError` for a TDZ-hole cell and `InvalidOperand`.
    pub fn jit_runtime_load_upvalue(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        self.frame_load_upvalue(frame, dst, idx)
    }

    /// Compiled-runtime `StoreUpvalue` over the canonical activation.
    ///
    /// Captured-cell mutation still flows through [`store_upvalue`], which
    /// owns the GC write barrier. No interpreter-frame conversion participates.
    ///
    /// # Errors
    /// Propagates `InvalidOperand` for a negative or out-of-range index.
    pub fn jit_runtime_store_upvalue(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        self.frame_store_upvalue(frame, src, idx)
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
        RegisterWindow, UpvalueCell, alloc_upvalue,
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader},
        read_upvalue,
    };

    fn header(register_count: usize) -> VmFrameHeader {
        VmFrameHeader {
            function_id: 7,
            code_block_id: 11,
            pc: 3,
            register_count: register_count as u16,
            kind: NativeFrameKind::Interpreter,
            flags: NativeFrameFlags::empty(),
        }
    }

    fn materialized_frame(
        slots: &mut [Value],
        upvalues: Vec<UpvalueCell>,
        self_value: Value,
        this_value: Value,
    ) -> Frame {
        Frame {
            header: header(slots.len()),
            registers: RegisterWindow::attached(slots.as_mut_ptr(), slots.len(), 0),
            upvalues: upvalues.into_boxed_slice(),
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
        let new_target = Value::function(43);

        let mut materialized_slots = [Value::undefined(); 3];
        let mut materialized =
            materialized_frame(&mut materialized_slots, Vec::new(), self_value, this_value);
        {
            let mut active =
                ActiveFrameMut::materialized_with_new_target(&mut materialized, new_target);
            interpreter
                .frame_load_this(&mut active, 0)
                .expect("materialized this");
            interpreter
                .frame_load_self(&mut active, 1)
                .expect("materialized SELF");
            interpreter
                .frame_load_new_target(&mut active, 2)
                .expect("materialized new.target");
        }

        let mut native_slots = [Value::undefined(); 3];
        let mut native = NativeFrame::new(
            header(native_slots.len()),
            native_slots.as_mut_ptr() as u64,
            self_value,
            this_value,
        );
        native.set_new_target(new_target);
        {
            // SAFETY: the native frame and its initialized register window
            // remain exclusively live and unmoved for this scoped view.
            let mut active = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }
                .expect("valid native frame");
            interpreter
                .frame_load_this(&mut active, 0)
                .expect("native this");
            interpreter
                .frame_load_self(&mut active, 1)
                .expect("native SELF");
            interpreter
                .frame_load_new_target(&mut active, 2)
                .expect("native new.target");
        }

        assert_eq!(materialized_slots, native_slots);
        assert_eq!(materialized_slots, [this_value, self_value, new_target]);
        assert_eq!(materialized.header.pc, 3);
        assert_eq!(native.header.pc, 3);
    }

    #[test]
    fn upvalue_kernels_match_materialized_and_native_frames() {
        let mut interpreter = Interpreter::new();
        let initial = Value::number_i32(11);
        let cell = alloc_upvalue(interpreter.gc_heap_mut(), initial).expect("upvalue cell");
        let initial_slots = [
            Value::number_i32(23),
            Value::undefined(),
            Value::number_i32(37),
        ];

        let mut materialized_slots = initial_slots;
        let mut materialized = materialized_frame(
            &mut materialized_slots,
            vec![cell],
            Value::function(7),
            Value::undefined(),
        );
        {
            let mut active = ActiveFrameMut::materialized(&mut materialized);
            interpreter
                .frame_load_upvalue(&mut active, 1, 0)
                .expect("materialized load");
            interpreter
                .frame_store_upvalue(&mut active, 0, 0)
                .expect("materialized store");
            interpreter
                .frame_store_upvalue_checked(&mut active, 2, 0)
                .expect("materialized checked store");
        }
        let materialized_result = read_upvalue(interpreter.gc_heap(), cell);

        store_upvalue(interpreter.gc_heap_mut(), cell, initial);
        let mut native_slots = initial_slots;
        let native_upvalues = [cell];
        let mut native = NativeFrame::new(
            header(native_slots.len()),
            native_slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        native.set_upvalue_window(
            native_upvalues.as_ptr() as u64,
            native_upvalues.len() as u32,
        );
        {
            // SAFETY: the native frame and both initialized windows remain
            // exclusively live and unmoved for this scoped view.
            let mut active = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }
                .expect("valid native frame");
            interpreter
                .frame_load_upvalue(&mut active, 1, 0)
                .expect("native load");
            interpreter
                .frame_store_upvalue(&mut active, 0, 0)
                .expect("native store");
            interpreter
                .frame_store_upvalue_checked(&mut active, 2, 0)
                .expect("native checked store");
        }

        assert_eq!(materialized_slots, native_slots);
        assert_eq!(materialized_slots[1], initial);
        assert_eq!(materialized_result, Value::number_i32(37));
        assert_eq!(
            read_upvalue(interpreter.gc_heap(), cell),
            materialized_result
        );
        assert_eq!(materialized.header.pc, 3);
        assert_eq!(native.header.pc, 3);
    }

    #[test]
    fn upvalue_tdz_errors_match_materialized_and_native_frames() {
        let mut interpreter = Interpreter::new();
        let cell = alloc_upvalue(interpreter.gc_heap_mut(), Value::hole()).expect("TDZ cell");

        let mut materialized_slots = [Value::number_i32(9), Value::undefined()];
        let mut materialized = materialized_frame(
            &mut materialized_slots,
            vec![cell],
            Value::function(7),
            Value::undefined(),
        );
        let (materialized_load, materialized_store) = {
            let mut active = ActiveFrameMut::materialized(&mut materialized);
            let load = interpreter.frame_load_upvalue(&mut active, 1, 0);
            let store = interpreter.frame_store_upvalue_checked(&mut active, 0, 0);
            (load, store)
        };

        let mut native_slots = [Value::number_i32(9), Value::undefined()];
        let native_upvalues = [cell];
        let mut native = NativeFrame::new(
            header(native_slots.len()),
            native_slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        native.set_upvalue_window(
            native_upvalues.as_ptr() as u64,
            native_upvalues.len() as u32,
        );
        let (native_load, native_store) = {
            // SAFETY: the native frame and both initialized windows remain
            // exclusively live and unmoved for this scoped view.
            let mut active = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }
                .expect("valid native frame");
            let load = interpreter.frame_load_upvalue(&mut active, 1, 0);
            let store = interpreter.frame_store_upvalue_checked(&mut active, 0, 0);
            (load, store)
        };

        for result in [
            materialized_load,
            materialized_store,
            native_load,
            native_store,
        ] {
            assert!(matches!(
                result,
                Err(VmError::TemporalDeadZone { local_index: 0 })
            ));
        }
        assert_eq!(materialized_slots, native_slots);
        assert!(read_upvalue(interpreter.gc_heap(), cell).is_hole());
        assert_eq!(materialized.header.pc, 3);
        assert_eq!(native.header.pc, 3);
    }
}

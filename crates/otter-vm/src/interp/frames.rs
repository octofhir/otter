//! Cold activation state, completion and native frame publication.
//!
//! # Contents
//! - Cold-record ownership and suspension inputs.
//! - Return, throw and finally handling bounded by an activation floor.
//! - Publication, tracing and safepoint access for the native caller chain.
//!
//! # Invariants
//! The trampoline owns active frames and initialized tagged windows. Logical
//! completion keeps the physical frame published until assembly releases its
//! extent. Cold records transfer once on suspension and release once on
//! completion. A collecting compiled call publishes its safepoint before entry.
//!
//! # See also
//! - [`crate::activation_stack`] for the non-owning activation view.
//! - [`crate::native_abi::call_trampoline`] for active storage.
//! - [`crate::frame_state::ParkedFrameState`] for suspended storage.

#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Borrow the cold record attached to `frame`, if any.
    #[inline]
    #[must_use]
    pub(crate) fn frame_cold(&self, frame: &Frame) -> Option<&cold_frame::ColdFrame> {
        frame.cold.map(|idx| self.cold_frames.get(idx))
    }

    /// Mutable borrow of the cold record attached to `frame`, if any.
    #[inline]
    #[must_use]
    pub(crate) fn frame_cold_mut(
        &mut self,
        frame: &mut Frame,
    ) -> Option<&mut cold_frame::ColdFrame> {
        frame.cold.map(|idx| self.cold_frames.get_mut(idx))
    }

    /// Whether this frame owns the result promise of a regular async call.
    #[inline]
    #[must_use]
    pub(crate) fn frame_has_async_state(&self, frame: &Frame) -> bool {
        self.frame_cold(frame)
            .is_some_and(|cold| cold.async_state.is_some())
    }

    /// Attach regular async-call ownership to a frame. Async frames are cold by
    /// definition; ordinary synchronous frames never allocate a side record.
    #[inline]
    pub(crate) fn frame_set_async_state(
        &mut self,
        frame: &mut Frame,
        state: crate::frame_state::AsyncFrameState,
    ) {
        self.frame_ensure_cold(frame).async_state = Some(state);
    }

    /// Remove and return a regular async call's result-promise ownership.
    #[inline]
    pub(crate) fn frame_take_async_state(
        &mut self,
        frame: &mut Frame,
    ) -> Option<crate::frame_state::AsyncFrameState> {
        self.frame_cold_mut(frame)?.async_state.take()
    }

    /// Generator object owning this active body, if this is a generator frame.
    #[inline]
    #[must_use]
    pub(crate) fn frame_generator_owner(
        &self,
        frame: &Frame,
    ) -> Option<crate::generator::JsGenerator> {
        self.frame_cold(frame).and_then(|cold| cold.generator_owner)
    }

    /// Async and generator frames use suspension-specific dispatch and are not
    /// eligible for ordinary synchronous JIT entry.
    #[inline]
    #[must_use]
    pub(crate) fn frame_has_suspension_owner(&self, frame: &Frame) -> bool {
        self.frame_cold(frame)
            .is_some_and(|cold| cold.async_state.is_some() || cold.generator_owner.is_some())
    }

    /// Start the suspension owner of a freshly entered async or generator
    /// activation, on its already published frame: an async function's
    /// result promise, or a generator object that `GeneratorStart` parks the
    /// activation into. Resumed and VM-prepared activations already carry
    /// their owner and are left unchanged.
    pub(crate) fn begin_suspendable_activation(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::ExecutionContext,
    ) -> Result<(), VmError> {
        let Some(frame) = stack.last() else {
            return Ok(());
        };
        if frame.header.pc != 0
            || frame.header.kind != crate::native_abi::NativeFrameKind::Interpreter
            || self.frame_has_suspension_owner(frame)
        {
            return Ok(());
        }
        let function_id = frame.header.function_id;
        let owner = context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let function = owner
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        if function.is_generator {
            let generator = crate::generator::JsGenerator::new_for_running_activation(
                &mut self.gc_heap,
                function.is_async_generator,
            )?;
            let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
            self.frame_ensure_cold(frame).generator_owner = Some(generator);
        } else if function.is_async {
            let result_promise =
                crate::promise_dispatch::PromiseBuilder::with_context(owner.clone())
                    .pending_stack_rooted(self, stack, &[], &[])?;
            let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
            self.frame_set_async_state(
                frame,
                crate::frame_state::AsyncFrameState { result_promise },
            );
        }
        Ok(())
    }

    /// Read `fn.prototype` for a generator whose prologue just parked, through
    /// the invoked closure instance, and return the generator.
    pub(crate) fn resolve_started_generator(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::ExecutionContext,
        generator: crate::generator::JsGenerator,
        callee: Value,
        function_id: u32,
    ) -> Result<Value, VmError> {
        let generator_anchor = self.push_iteration_anchor(Value::generator(generator)) - 1;
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let result = (|| -> Result<Value, VmError> {
            let owner = context
                .for_function(function_id)
                .map_err(|_| VmError::InvalidOperand)?;
            let closure = self
                .iteration_anchor(callee_anchor)
                .as_closure(&self.gc_heap);
            let proto =
                self.function_property_get(stack, &owner, closure, function_id, "prototype")?;
            let generator = self
                .iteration_anchor(generator_anchor)
                .as_generator()
                .ok_or(VmError::InvalidOperand)?;
            generator.set_prototype_override(
                &mut self.gc_heap,
                proto.as_object().is_some().then_some(proto),
            );
            Ok(self.iteration_anchor(generator_anchor))
        })();
        self.pop_iteration_anchors_to(generator_anchor);
        result
    }

    /// Copy a cold-detached activation while its native extent remains published.
    pub(crate) fn park_active_frame(
        &mut self,
        frame: &Frame,
    ) -> crate::frame_state::ParkedFrameState {
        assert!(
            frame.cold.is_none(),
            "detach cold ownership before suspension"
        );
        crate::frame_state::ParkedFrameState::copy_from_active(frame)
    }

    /// Restore owned suspension inputs without allocating an active frame.
    pub(crate) fn resume_parked_frame(
        &mut self,
        parked: crate::frame_state::ParkedFrameState,
    ) -> Result<crate::PreparedCall, VmError> {
        Ok(parked.into_prepared())
    }

    pub(crate) fn prepared_set_async_state(
        &mut self,
        call: &mut crate::PreparedCall,
        state: crate::frame_state::AsyncFrameState,
    ) {
        let index = *call.cold.get_or_insert_with(|| self.cold_frames.acquire());
        self.cold_frames.get_mut(index).async_state = Some(state);
    }

    pub(crate) fn prepared_attach_cold(
        &mut self,
        call: &mut crate::PreparedCall,
        cold: cold_frame::ColdFrame,
    ) {
        assert!(call.cold.is_none());
        call.cold = Some(self.cold_frames.attach(cold));
    }

    /// Innermost published native frame, or null when none is published.
    #[inline]
    #[must_use]
    pub fn jit_innermost_native_frame(&self) -> *mut crate::native_abi::Frame {
        let bits = match self.jit_frame_cell {
            // SAFETY: the live compiled entry keeps its frame cell valid until
            // `jit_leave_native_frames` restores the enclosing cell.
            Some(cell) => unsafe { *cell.as_ptr() },
            None => self.jit_detached_frame,
        };
        bits as *mut crate::native_abi::Frame
    }

    fn set_jit_innermost_native_frame(&mut self, frame: *mut crate::native_abi::Frame) {
        match self.jit_frame_cell {
            // SAFETY: see `jit_innermost_native_frame`.
            Some(cell) => unsafe { *cell.as_ptr() = frame as u64 },
            None => self.jit_detached_frame = frame as u64,
        }
    }

    /// Whether any native frame is published. Executable retirement waits
    /// until none is, because published frames may return into old code.
    #[inline]
    #[must_use]
    pub(crate) fn jit_has_native_frames(&self) -> bool {
        !self.jit_innermost_native_frame().is_null()
    }

    /// Published native frames from the innermost outward.
    pub(crate) fn jit_native_frames(
        &self,
    ) -> impl Iterator<Item = *mut crate::native_abi::Frame> + '_ {
        let mut frame = self.jit_innermost_native_frame();
        std::iter::from_fn(move || {
            if frame.is_null() {
                return None;
            }
            let current = frame;
            // SAFETY: every published record stays live and linked to its
            // caller until it is unpublished, innermost first.
            frame = unsafe { (*current).caller_frame() };
            Some(current)
        })
    }

    /// Link `frame` as the innermost published native frame.
    ///
    /// # Safety
    /// `frame` and its explicit register window must remain live,
    /// initialized, stable, and exclusively owned by the active mutator until
    /// the matching [`Self::jit_pop_native_frame`].
    pub unsafe fn jit_push_native_frame(
        &mut self,
        frame: &mut crate::native_abi::Frame,
    ) -> Result<(), VmError> {
        // SAFETY: forwarded from this function's publication contract. The
        // checked view centralizes null/alignment/window validation before the
        // frame becomes visible to GC.
        unsafe { crate::ActiveFrameMut::from_ptr(frame) }.map_err(|_| VmError::InvalidOperand)?;
        self.link_native_frame(frame)
    }

    fn link_native_frame(&mut self, frame: &mut crate::native_abi::Frame) -> Result<(), VmError> {
        let caller = self.jit_innermost_native_frame();
        // SAFETY: the innermost published record is live.
        let caller_depth = unsafe { caller.as_ref() }.map_or(0, |caller| caller.depth);
        let depth = caller_depth.saturating_add(1);
        if depth > self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        frame.caller = caller as u64;
        frame.depth = depth;
        self.set_jit_innermost_native_frame(std::ptr::from_mut(frame));
        Ok(())
    }

    /// Unpublish the innermost native frame.
    #[inline]
    pub fn jit_pop_native_frame(&mut self) {
        let frame = self.jit_innermost_native_frame();
        debug_assert!(!frame.is_null());
        // SAFETY: the innermost published record is live until this unlink.
        let caller = unsafe { (*frame).caller_frame() };
        self.set_jit_innermost_native_frame(caller);
    }

    /// Begin a compiled entry whose innermost-frame cell is `cell`.
    ///
    /// The cell holds the entry's own frame, which links to the enclosing
    /// innermost frame; generated linkage then keeps the cell current. Returns
    /// the enclosing entry's cell for [`Self::jit_leave_native_frames`].
    ///
    /// # Safety
    /// `cell` must hold a valid unpublished entry frame and stay live, with
    /// that frame, until the matching leave.
    pub unsafe fn jit_enter_native_frames(
        &mut self,
        cell: std::ptr::NonNull<u64>,
    ) -> Result<Option<std::ptr::NonNull<u64>>, VmError> {
        // SAFETY: forwarded from the entry contract.
        let frame = unsafe { &mut *(*cell.as_ptr() as *mut crate::native_abi::Frame) };
        // SAFETY: as in `jit_push_native_frame`.
        unsafe { crate::ActiveFrameMut::from_ptr(frame) }.map_err(|_| VmError::InvalidOperand)?;
        let caller = self.jit_innermost_native_frame();
        // SAFETY: the innermost published record is live.
        frame.depth = unsafe { caller.as_ref() }.map_or(0, |caller| caller.depth);
        frame.caller = caller as u64;
        Ok(self.jit_frame_cell.replace(cell))
    }

    /// End the compiled entry begun by [`Self::jit_enter_native_frames`].
    pub fn jit_leave_native_frames(&mut self, enclosing: Option<std::ptr::NonNull<u64>>) {
        self.jit_frame_cell = enclosing;
    }

    /// Generated-frame depth bound for a compiled entry starting while
    /// `interpreter_frames` interpreter frames are live: a generated call may
    /// link its frame only while its caller's depth is below this value, so
    /// interpreter plus generated frames never exceed the JS depth budget.
    #[must_use]
    pub fn jit_generated_depth_limit(&self, interpreter_frames: usize) -> u32 {
        self.max_stack_depth
            .saturating_sub(u32::try_from(interpreter_frames).unwrap_or(u32::MAX))
    }

    /// Trace tagged windows, actuals, frame fields and optimized root homes
    /// through the one published activation chain.
    pub(crate) fn trace_native_jit_activations(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for native in self.jit_native_frames() {
            // SAFETY: published frames and their windows stay live until
            // unpublished.
            let frame = unsafe { crate::ActiveFrameRef::from_ptr(native) }
                .expect("published native frame must remain valid");
            frame.trace_stack_register_slots(visitor);
            frame.trace_non_register_slots(visitor);
            // SAFETY: as above; only scalar fields are read here.
            let record = unsafe { &*native };
            if record.call_site == native_abi::NO_SAFEPOINT {
                continue;
            }
            let safepoint = self
                .jit_code_registry
                .safepoint_record(u64::from(record.code_object_id), record.call_site)
                .expect("an optimized frame's call site names a live safepoint");
            debug_assert_ne!(record.machine_roots, 0);
            for location in &safepoint.tagged_locations {
                debug_assert_eq!(location.kind, native_abi::TaggedLocationKind::SpillSlot);
                // SAFETY: the frame's prologue published its root-home base,
                // and the call named by `call_site` saved every listed home
                // before it could collect; the homes live until the frame
                // returns.
                let home = unsafe {
                    (record.machine_roots as *mut crate::Value).add(usize::from(location.index))
                };
                unsafe { (&mut *home).trace_value_slot_mut(visitor) };
            }
        }
    }

    /// Opaque heap pointer for native leaf runtime stubs.
    ///
    /// Compiled code may pass this to `LeafNoAlloc` ABI entries only. Those
    /// entries must not allocate, trigger GC, or retain the pointer.
    pub fn jit_gc_heap_ptr(&self) -> *const std::ffi::c_void {
        std::ptr::addr_of!(self.gc_heap).cast::<std::ffi::c_void>()
    }

    /// The safepoint-free nursery window generated code allocates from.
    pub fn jit_allocation_window(&mut self) -> otter_gc::MachineAllocationWindow {
        self.gc_heap.machine_allocation_window()
    }

    /// Stable address of the aggregate JIT counter record for generated code.
    pub fn jit_runtime_stats_mut_ptr(&mut self) -> *mut crate::JitRuntimeStats {
        std::ptr::addr_of_mut!(self.jit_runtime_stats)
    }

    /// Collector cycle counts used to classify a rooted receiver fallback.
    pub fn jit_gc_cycle_counts(&self) -> (u64, u64) {
        self.gc_heap.gc_cycle_counts()
    }

    /// Address of the collector's incremental-marking flag byte, read by the
    /// inline write barrier compiled code emits for a pointer store.
    #[must_use]
    pub fn jit_marking_flag_ptr(&self) -> *const u8 {
        self.gc_heap.marking_flag_addr()
    }

    /// Acquire (or lazily create) this frame's cold side record and
    /// then return a mutable borrow.
    #[inline]
    pub(crate) fn frame_ensure_cold(&mut self, frame: &mut Frame) -> &mut cold_frame::ColdFrame {
        let idx = match frame.cold {
            Some(idx) => idx,
            None => {
                let idx = self.cold_frames.acquire();
                frame.cold = Some(idx);
                idx
            }
        };
        self.cold_frames.get_mut(idx)
    }

    /// Release `frame`'s cold record back to the pool if it holds one.
    /// Called when a frame is popped off the dispatcher stack.
    #[inline]
    pub(crate) fn frame_release_cold(&mut self, frame: &mut Frame) {
        if let Some(idx) = frame.cold.take() {
            self.cold_frames.release(idx);
        }
    }

    /// Detach `frame`'s cold record out of the pool, returning it as
    /// an owned [`Box`] so the caller can store it alongside the
    /// parked frame (async await, generator yield). Returns `None`
    /// when the frame had no cold state.
    #[inline]
    pub(crate) fn frame_detach_cold(
        &mut self,
        frame: &mut Frame,
    ) -> Option<Box<cold_frame::ColdFrame>> {
        let idx = frame.cold.take()?;
        Some(Box::new(self.cold_frames.detach(idx)))
    }

    /// Borrow the per-interpreter cold-frame pool.
    #[inline]
    #[must_use]
    pub(crate) fn cold_frames(&self) -> &cold_frame::ColdFramePool {
        &self.cold_frames
    }

    /// Borrow the per-realm typed intrinsic slots.
    #[inline]
    #[must_use]
    pub(crate) fn realm_intrinsics(&self) -> &realm_intrinsics::RealmIntrinsics {
        &self.realm_intrinsics
    }
}

impl Interpreter {
    /// Release an unentered request after an entry failure. Assembly owns
    /// physical extents and has already returned to the caller's floor.
    pub(crate) fn release_frames_above(
        &mut self,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
    ) {
        debug_assert!(stack.len() <= floor.depth());
        if let Some(call) = stack.take_pending() {
            self.release_prepared_inputs(call);
        }
    }

    pub(crate) fn release_prepared_inputs(&mut self, call: crate::PreparedCall) {
        let mut current = Some(call);
        while let Some(mut call) = current {
            if let Some(index) = call.cold.take() {
                self.cold_frames.release(index);
            }
            current = call.child.take().map(|child| *child);
        }
    }

    /// Pop the top frame and route its completion value.
    ///
    /// # Algorithm
    /// 1. If the popped frame was entered via `Op::New`, apply the
    ///    `OrdinaryConstruct` step-11 substitution: a non-object
    ///    return reuses the freshly allocated `this`.
    /// 2. If the popped frame is an **async** frame, settle its
    ///    `result_promise` as fulfilled with the resolved value
    ///    and drain the resulting reaction jobs into the
    ///    microtask queue. The caller's destination register was
    ///    populated with the promise at call entry, so we do not
    ///    write to it again. When the stack is now empty (an
    ///    async-resume mini-stack just finished) return
    ///    `Ok(Some(Undefined))` so the surrounding driver loop
    ///    exits cleanly; otherwise return `Ok(None)` to continue
    ///    in the caller frame.
    /// 3. For non-async frames, write the resolved value into the
    ///    caller's `return_register`. Top-of-stack `<main>` falls
    ///    through with `return_register = None` and surfaces the
    ///    completion as `Some(value)`.
    ///
    /// # Errors
    /// - [`VmError::InvalidOperand`] when the stack is empty or
    ///   the caller's return register is out of bounds.
    ///
    /// Pop one frame without crossing the caller-owned activation `floor`.
    ///
    /// Nested VM execution shares the same physical stack as its caller.  A
    /// terminal async completion therefore means "back at the region floor",
    /// not necessarily an empty stack.  A frame carrying a return register is
    /// malformed when its caller would sit below the floor.
    ///
    /// `derived_this` is the `DerivedThis` slot value a `ReturnDerived`
    /// read; `None` uses the frame's own `this`. A failure after the frame
    /// is gone reaches the caller as the call's throw completion.
    pub(crate) fn pop_frame_above(
        &mut self,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        value: Value,
        derived_this: Option<Value>,
    ) -> Result<Option<Value>, VmError> {
        if stack.is_at_floor(floor) {
            return Err(VmError::InvalidOperand);
        }
        let popped = stack.pop().ok_or_else(|| VmError::InvalidOperand)?;
        match self.complete_popped_frame(popped, value, derived_this) {
            Ok(PoppedCompletion::Value(value)) => Ok(Some(value)),
            // An async activation's completion is its promise, which only a
            // caller at the region floor still needs.
            Ok(PoppedCompletion::Promise(promise)) => {
                Ok(stack.is_at_floor(floor).then_some(promise))
            }
            Err(error) => Err(error),
        }
    }

    /// The return-value semantics of a frame [`Self::pop_frame_above`] just
    /// removed.
    fn complete_popped_frame(
        &mut self,
        popped: &mut Frame,
        value: Value,
        derived_this: Option<Value>,
    ) -> Result<PoppedCompletion, VmError> {
        // An ordinary synchronous frame owns no cold record, so the whole
        // construct/derived/async completion vocabulary resolves from one
        // pool probe rather than from a probe per question.
        let construct_target = popped.is_construct().then_some(popped.this_value);
        let is_derived_ctor = popped.is_derived_constructor();
        let mut async_state = None;
        if let Some(idx) = popped.cold.take() {
            let cold = self.cold_frames.get_mut(idx);
            async_state = cold.async_state.take();
            // Release the cold slot now so the pool can reuse it; every
            // remaining cold-record read already happened.
            self.cold_frames.release(idx);
        }
        // A derived constructor's `this`: the frame-held binding, or the
        // `DerivedThis` context slot `ReturnDerived` read.
        let derived_this = derived_this.unwrap_or(popped.this_value);
        // The frame is terminal — return its spilled register window to the
        // pool. Nothing below reads `popped.registers`.
        let resolved = if is_derived_ctor {
            // §10.2.2 derived-constructor return semantics. An object
            // return overrides the bound `this`; `undefined` yields
            // the `super(...)`-bound `this` (ReferenceError if
            // `super` never ran); any other primitive is a TypeError.
            if value.is_object_type() {
                value
            } else if value.is_undefined() {
                if derived_this.is_hole() {
                    return Err(
                        self.err_this_uninit(crate::context_ops::DERIVED_THIS_UNINITIALIZED.into())
                    );
                }
                derived_this
            } else {
                return Err(self.err_type(
                    ("derived constructors may only return an object or undefined".to_string())
                        .into(),
                ));
            }
        } else {
            match construct_target {
                Some(_) if value.is_object_type() => value,
                Some(target) => target,
                None => value,
            }
        };
        if let Some(state) = async_state {
            // The activation's completion is its result promise, rooted across
            // settlement.
            let anchor = self.push_iteration_anchor(Value::promise(state.result_promise)) - 1;
            let settled = crate::promise_dispatch::resolve_promise_from_interpreter(
                self,
                state.result_promise,
                resolved,
                None,
            );
            let promise = self.iteration_anchor(anchor);
            self.pop_iteration_anchors_to(anchor);
            settled?;
            return Ok(PoppedCompletion::Promise(promise));
        }
        Ok(PoppedCompletion::Value(resolved))
    }
}

/// What completing a popped frame produced.
enum PoppedCompletion {
    /// The value its caller receives.
    Value(Value),
    /// The settled result promise of an async activation.
    Promise(Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_values_are_owned_and_resume_as_entry_inputs() {
        let mut interp = Interpreter::new();
        let function = otter_bytecode::Function {
            locals: 3,
            ..Default::default()
        };
        let mut frame = interp.test_frame_for_function(&function).unwrap();
        frame.registers.copy_from_slice(&[
            Value::number_i32(7),
            Value::number_i32(11),
            Value::number_i32(13),
        ]);
        let parked = interp.park_active_frame(&frame);
        frame.registers.fill(Value::UNDEFINED);
        assert_eq!(parked.debug_register(1), Some(Value::number_i32(11)));
        let resumed = interp.resume_parked_frame(parked).unwrap();
        assert_eq!(resumed.initial_registers[2], Value::number_i32(13));
        assert_eq!(resumed.packet().initial_register_count, 3);
    }

    #[test]
    fn completion_preserves_the_published_caller() {
        let mut interp = Interpreter::new();
        let function = otter_bytecode::Function {
            locals: 2,
            ..Default::default()
        };
        let parent = interp.test_frame_for_function(&function).unwrap();
        let child = interp.test_frame_for_function(&function).unwrap();
        let mut stack = crate::test_support::FrameChainFixture::new();
        stack.push(parent);
        let floor = stack.floor();
        stack.push(child);
        let result = interp
            .pop_frame_above(&mut stack, floor, Value::number_i32(42), None)
            .unwrap();
        assert_eq!(result, Some(Value::number_i32(42)));
        assert_eq!(stack.len(), floor.depth());
        assert_eq!(stack[0].registers.len(), 2);
    }
}

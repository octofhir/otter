//! Call and construct opcode helpers.
//!
//! Stack-modifying call bytecodes decode variadic executable operands, prepare
//! frames, and may immediately invoke native/proxy/constructor paths. Keeping
//! that machinery here lets `lib.rs` stay closer to a dispatch map.
//!
//! # Contents
//! - Ordinary call entry and shared callable invocation.
//! - Constructor call entry and family-owned receiver/prototype setup.
//! - Spread and explicit-`this` call forms.
//! - Same-stack synchronous re-entry and reusable lean callback frames.
//! - Dispatch-local owner resolution for cross-chunk callees.
//!
//! # Invariants
//! - Call-site helpers advance the caller PC before pushing or synchronously
//!   invoking another frame.
//! - `invoke` remains the shared call path for bytecode, closures, native
//!   callables, bound functions, class constructors, and proxies.
//! - Constructor dispatch preserves `new.target` and receiver substitution
//!   invariants used by `pop_frame`. Base dispatch roots the prototype lookup
//!   it owns, records whether the receiver used generic `%Object.prototype%`
//!   fallback, and passes that rooted receiver plus provenance through the
//!   native boundary. Constructor arguments remain in their canonical root
//!   slots until that observable lookup and receiver allocation finish.
//! - Derived bytecode constructors enter with no receiver and preserve the
//!   caller's stable argument window; their direct `super(...)` dispatch owns
//!   the single prototype lookup and receiver allocation.
//! - The canonical construction packet/frame owns the allocated receiver
//!   and exact family ticket until terminal completion. Constructor layouts
//!   retain no receivers and require no pre-GC observation flush.
//! - Forwarded-call operands are reloaded after arguments materialization;
//!   the committed method lookup is never replayed.
//! - Generated receiver allocation uses the same VM-planned shape/capacity
//!   contract as the ordinary allocator; a nursery-window miss returns here
//!   while all constructor inputs remain rooted and no effect has started.
//! - Raw native invocation retains its `NativeError` until the published Host
//!   owner projects it once inside the native's creation realm.
//! - Nested call/construct dispatch appends above an `ActivationFloor` on the
//!   current rooted stack; native boundary slots are collector-rewritten in
//!   their original storage.
//! - Cross-chunk call resolution caches owned contexts only for one dispatch
//!   and invalidates them against the code-space publication epoch.
//! - A freshly-started generator remains in a moving GC root through observable
//!   `prototype` lookup and publication into the caller.
//! - Every bytecode frame is built with its exact SELF; building a frame
//!   allocates no GC memory, so call-site locals stay current until the frame
//!   is published. The callee reaches its outer bindings through SELF's
//!   context and creates its own contexts in its prologue.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

mod constructor;

use std::cell::UnsafeCell;

use crate::activation_stack::ActivationStack;
use crate::runtime_activation::CommittedValueError;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    CodeBlock, ExecutionContext, Interpreter, NativeCallInfo, NativeCtx, NativeFunction, Value,
    VmError, VmGetOutcome, VmPropertyKey,
    argument_window::{ArgumentOperands, BytecodeArgumentWindow},
    executable::OperandView,
    operand_decode::register_operand,
    read_register,
    runtime_cx::NativeCallRoots,
};

/// Mutable root state for synchronous JS re-entry before a callee frame owns
/// the values. Bound/proxy unwrapping replaces these fields in place; the
/// registered provider therefore rewrites the exact cells the dispatch loop
/// reads after any moving collection, rather than merely keeping duplicate
/// handle-arena entries alive.
struct JsCallRootSlot(UnsafeCell<Value>);

impl JsCallRootSlot {
    fn new(value: Value) -> Self {
        Self(UnsafeCell::new(value))
    }

    #[inline]
    fn get(&self) -> Value {
        // SAFETY: VM re-entry and GC run on one mutator thread. This short read
        // never spans a VM call or safepoint.
        unsafe { *self.0.get() }
    }

    #[inline]
    fn set(&self, value: Value) {
        // SAFETY: same single-mutator contract as `get`; no reference into the
        // slot escapes this non-allocating store.
        unsafe { *self.0.get() = value };
    }

    fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: the collector is the only writer while this callback runs;
        // all ordinary state operations are short and cannot trigger GC.
        unsafe { (&mut *self.0.get()).trace_value_slot_mut(visitor) };
    }
}

pub(crate) struct SyncJsCallRoots {
    current: JsCallRootSlot,
    receiver: JsCallRootSlot,
    new_target: JsCallRootSlot,
    proxy_target: JsCallRootSlot,
    args: UnsafeCell<SmallVec<[Value; 8]>>,
    scratch_0: JsCallRootSlot,
    scratch_1: JsCallRootSlot,
    construct_layout: UnsafeCell<crate::constructor_layout::ConstructorLayout>,
}

impl SyncJsCallRoots {
    pub(crate) fn call(current: Value, receiver: Value, args: SmallVec<[Value; 8]>) -> Self {
        Self {
            current: JsCallRootSlot::new(current),
            receiver: JsCallRootSlot::new(receiver),
            new_target: JsCallRootSlot::new(Value::undefined()),
            proxy_target: JsCallRootSlot::new(Value::undefined()),
            args: UnsafeCell::new(args),
            scratch_0: JsCallRootSlot::new(Value::undefined()),
            scratch_1: JsCallRootSlot::new(Value::undefined()),
            construct_layout: UnsafeCell::new(crate::constructor_layout::ConstructorLayout::null()),
        }
    }

    fn construct(current: Value, new_target: Value, args: SmallVec<[Value; 8]>) -> Self {
        Self {
            current: JsCallRootSlot::new(current),
            receiver: JsCallRootSlot::new(Value::undefined()),
            new_target: JsCallRootSlot::new(new_target),
            proxy_target: JsCallRootSlot::new(Value::undefined()),
            args: UnsafeCell::new(args),
            scratch_0: JsCallRootSlot::new(Value::undefined()),
            scratch_1: JsCallRootSlot::new(Value::undefined()),
            construct_layout: UnsafeCell::new(crate::constructor_layout::ConstructorLayout::null()),
        }
    }

    #[inline]
    pub(crate) fn target(&self) -> Value {
        self.current.get()
    }

    pub(crate) fn receiver_value(&self) -> Value {
        self.receiver.get()
    }

    pub(crate) fn set_receiver(&self, value: Value) {
        self.receiver.set(value);
    }

    pub(crate) fn scratch(&self, index: usize) -> Value {
        match index {
            0 => self.scratch_0.get(),
            1 => self.scratch_1.get(),
            _ => unreachable!("synchronous call roots expose two scratch slots"),
        }
    }

    pub(crate) fn set_scratch(&self, index: usize, value: Value) {
        match index {
            0 => self.scratch_0.set(value),
            1 => self.scratch_1.set(value),
            _ => unreachable!("synchronous call roots expose two scratch slots"),
        }
    }

    pub(crate) fn set_construct_layout(
        &self,
        layout: crate::constructor_layout::ConstructorLayout,
    ) {
        // SAFETY: a short single-mutator nonallocating assignment.
        unsafe {
            *self.construct_layout.get() = layout;
        }
    }
    pub(crate) fn construct_layout(&self) -> crate::constructor_layout::ConstructorLayout {
        // SAFETY: no reference escapes and this scalar read cannot collect.
        unsafe { *self.construct_layout.get() }
    }

    pub(crate) fn args_len(&self) -> usize {
        // SAFETY: this short read cannot allocate or overlap root tracing.
        unsafe { (&*self.args.get()).len() }
    }

    pub(crate) fn replace_args(&self, args: SmallVec<[Value; 8]>) {
        // SAFETY: short single-mutator write with no VM allocation.
        unsafe { *self.args.get() = args };
    }

    pub(crate) fn take_args(&self) -> SmallVec<[Value; 8]> {
        // SAFETY: moving the SmallVec itself cannot run GC. The caller must
        // transfer it into traced frame storage or install a slice provider
        // before the next possible collection.
        unsafe { std::mem::take(&mut *self.args.get()) }
    }
}

impl otter_gc::ExtraRootSource for SyncJsCallRoots {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.current.trace(visitor);
        // SAFETY: the registered provider owns this initialized canonical slot
        // during collection. A layout is a strong old-space handle.
        if !unsafe { (*self.construct_layout.get()).is_null() } {
            visitor(self.construct_layout.get().cast::<RawGc>());
        }
        self.receiver.trace(visitor);
        self.new_target.trace(visitor);
        self.proxy_target.trace(visitor);
        self.scratch_0.trace(visitor);
        self.scratch_1.trace(visitor);
        // SAFETY: root tracing is the only operation active on this state while
        // GC runs; ordinary reads/writes never hold a borrow across safepoints.
        let args = self.args.get();
        let (ptr, len) = unsafe { ((*args).as_mut_ptr(), (*args).len()) };
        for index in 0..len {
            unsafe { (&mut *ptr.add(index)).trace_value_slot_mut(visitor) };
        }
    }
}

/// Run a native body for a published host frame. The frame traces its
/// callee and its actuals, which `args` reads in place; only the call info's
/// copy of the receiver is rooted here.
pub(crate) fn invoke_frame_native_call(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: crate::runtime_cx::NativeContext<'_>,
    call: crate::native_function::NativeCallTarget,
    this_value: Value,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let call_info = NativeCallInfo::call(this_value);
    let roots = NativeCallRoots::new(&call_info, &[], &[]);
    // Pushed (not installed) so any outer scope's value/slice roots
    // stay visible to scavenges triggered inside this native.
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    debug_assert!(interp.gc_heap.has_frame_root_providers());
    let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(interp, stack);
    let mut ctx = NativeCtx::with_native_context(turn, &call_info, context);
    call.invoke(&mut ctx, args)
}

impl Interpreter {
    /// Attribute one generated derived-constructor completion boundary.
    pub fn record_jit_derived_construct_result_transition(&mut self) {
        self.jit_runtime_stats.derived_construct_result_transitions = self
            .jit_runtime_stats
            .derived_construct_result_transitions
            .saturating_add(1);
    }

    /// Attribute one generated derived-`this` binding boundary.
    pub fn record_jit_derived_this_bind_transition(&mut self) {
        self.jit_runtime_stats.derived_this_bind_transitions = self
            .jit_runtime_stats
            .derived_this_bind_transitions
            .saturating_add(1);
    }

    /// Attribute one exact-class superclass resolution boundary.
    pub fn record_jit_class_super_resolution_transition(&mut self) {
        self.jit_runtime_stats.class_super_resolution_transitions = self
            .jit_runtime_stats
            .class_super_resolution_transitions
            .saturating_add(1);
    }

    /// Apply the derived-constructor return rules to one generated callee.
    pub fn jit_derived_construct_result(
        &mut self,
        result: Value,
        bound_this: Value,
    ) -> Result<Value, VmError> {
        if result.is_object_type() {
            Ok(result)
        } else if result.is_undefined() {
            if bound_this.is_hole() {
                Err(self.err_this_uninit(
                    "must call super constructor in derived class before accessing 'this' or returning from derived constructor"
                        .to_string()
                        .into(),
                ))
            } else {
                Ok(bound_this)
            }
        } else {
            Err(self.err_type(
                "derived constructors may only return an object or undefined"
                    .to_string()
                    .into(),
            ))
        }
    }

    /// Read the live superclass identity from an exact class-constructor
    /// wrapper. Non-class inputs return the hole sentinel so generated code can
    /// side-exit before any construct effect.
    pub fn jit_class_super_constructor(&self, value: Value) -> Value {
        value
            .as_class_constructor()
            .map_or_else(Value::hole, |class| class.ctor_proto(&self.gc_heap))
    }

    /// Copy a compiler-collected spread argument array into an unpublished
    /// native callee frame without allocation or observable JavaScript work.
    ///
    /// # Safety
    ///
    /// `frame` must name an initialized, exclusively owned [`Frame`]
    /// whose register window remains live for this call. It is deliberately
    /// not published until the generated caller completes this copy.
    pub unsafe fn jit_copy_spread_arguments(
        &self,
        arguments: Value,
        frame: *mut crate::native_abi::Frame,
        parameter_count: u16,
    ) -> bool {
        let Some(array) = arguments.as_array() else {
            return false;
        };
        // SAFETY: upheld by the generated-linkage caller; the view validates
        // the raw frame and window descriptors before exposing scalar writes.
        let Ok(mut frame) = (unsafe { crate::ActiveFrameMut::from_ptr(frame) }) else {
            return false;
        };
        if usize::from(parameter_count) > frame.register_count() {
            return false;
        }
        crate::array::with_elements(array, &self.gc_heap, |elements| {
            for (index, value) in elements
                .iter()
                .copied()
                .take(usize::from(parameter_count))
                .enumerate()
            {
                if frame.write(index as u16, value).is_err() {
                    return false;
                }
            }
            true
        })
    }

    pub(crate) fn bind_bytecode_call_arguments(
        &mut self,
        function: &CodeBlock,
        frame: &mut crate::PreparedCall,
        args: SmallVec<[Value; 8]>,
    ) -> Result<(), VmError> {
        if function.param_count > function.register_count {
            return Err(VmError::InvalidOperand);
        }
        frame.arguments = args;
        Ok(())
    }

    pub(crate) fn invoke_native_construct_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        native: NativeFunction,
        this_value: &Value,
        new_target: &Value,
        used_object_prototype_fallback: bool,
        args: &[Value],
    ) -> Result<Value, crate::NativeError> {
        let call = native.call_target(&self.gc_heap);
        let call_info = NativeCallInfo::construct_with_receiver(
            *this_value,
            Some(*new_target),
            used_object_prototype_fallback,
        );
        // Same root coverage as the call path (`invoke_native_call_with_roots`):
        // trace the interpreter's full root set (crucially the scope-handle
        // arena, so a native constructor's `Local` handles stay live) and
        // pin `this`, `new.target`, and the argument slice across every
        // scavenge the constructor triggers. Without this a native `new X(…)`
        // ran fully unrooted — e.g. `new Set([...])` stranded its iterable.
        let slice_roots = [args];
        let roots = NativeCallRoots::new(&call_info, &[], &slice_roots);
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(self, stack);
        let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
        let raw = call.invoke(&mut ctx, args);
        let rooted_this = *ctx.this_value();
        let (interp, _) = ctx.cx.into_parts();
        let result = raw?;
        // The constructor ran under its own realm. A body-slot exotic it
        // built carries no `[[Prototype]]`, so stamp the realm's
        // intrinsic before the value escapes into another one.
        interp.register_exotic_realm_proto(&result);
        Ok(if result.is_object_type() {
            result
        } else {
            rooted_this
        })
    }

    /// Bytecode function a staged request's callee names, for call feedback.
    /// A class wrapper names its constructor; every other kind names none.
    pub(crate) fn staged_bytecode_target(&self, stack: &mut ActivationStack) -> Option<u32> {
        let callee = stack.staged_request_mut()?.callee;
        let callee = callee
            .as_class_constructor()
            .map_or(callee, |class| class.ctor(&self.gc_heap));
        callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        })
    }

    /// Handle `Op::Call`: stage the callee with an `undefined` receiver for
    /// the trampoline, which classifies it and owns its activation.
    #[cfg(test)]
    pub(crate) fn do_call<'a>(
        &mut self,
        stack: &mut ActivationStack,
        _context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        self.do_call_inner(stack, ArgumentOperands::decoded(operands.into()))
    }

    pub(crate) fn do_call_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_call_inner(stack, ArgumentOperands::execution(function, instruction))
    }

    fn do_call_inner(
        &mut self,
        stack: &mut ActivationStack,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        // The call header (`dst`, callee, argc) reads from one operand-word
        // slice; a call with two or more arguments spills past the inline words,
        // so the header cannot use the fixed-arity accessors.
        let (dst, callee_reg, argc) = match operands.words() {
            Some([dst, callee, argc, ..]) => (*dst as u16, *callee as u16, *argc),
            Some(_) => return Err(VmError::InvalidOperand),
            None => (
                operands.register(0)?,
                operands.register(1)?,
                operands.const_index(2)?,
            ),
        };
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let args =
            BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc as usize)
                .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        stack.stage_call(callee, Value::undefined(), None, args, Some(dst));
        Ok(())
    }

    /// §15.10.3 PrepareForTailCall — `Op::TailCall`. The staged request
    /// replaces this activation at the trampoline, so a strict-mode tail call
    /// uses O(1) native stack. The compiler emits it only outside
    /// `try`/`finally`; a frame whose completion needs post-processing
    /// (constructors, suspension owners, live handlers) stages an ordinary
    /// call instead.
    pub(crate) fn do_tail_call_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let top_idx = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        let frame = &stack[top_idx];
        let tail_safe = !self.frame_has_suspension_owner(frame) && !frame.is_construct();
        let return_destination = frame.return_destination;
        // SAFETY: the published record is anchored at its live machine frame.
        let return_anchor = (frame.caller, unsafe { frame.return_pc_into_caller() });
        // A proxy's own steps throw in the realm of the code calling it: a
        // transfer that would leave a caller of another realm current keeps
        // this activation instead.
        let realm_kept = self.calling_realm(Some(frame))
            == self.calling_realm(top_idx.checked_sub(1).map(|below| &stack[below]));
        self.do_call_inner(stack, ArgumentOperands::execution(function, instruction))?;
        if tail_safe
            && let Some(request) = stack.staged_request_mut()
            && (realm_kept || request.callee.as_proxy().is_none())
        {
            request.return_destination = return_destination;
            request.caller = return_anchor.0;
            request.caller_return_pc = return_anchor.1;
            request.header.flags = crate::native_abi::NativeFrameFlags::from_bits(
                request.header.flags.bits() | crate::native_abi::NativeFrameFlags::TAIL_CALL,
            );
            self.complete_interpreted_retraining_activation(&mut stack[top_idx]);
            self.frame_release_cold(&mut stack[top_idx]);
        }
        Ok(())
    }

    /// Stage `callee(...args)` with an explicit receiver; the completion is
    /// delivered to the caller's `dst` when its activation resumes.
    ///
    /// An instruction that has advanced the caller's PC continues after
    /// itself when the call completes; one that has not runs again and
    /// finds its parked state (see `dispatch_loop_inner`).
    pub(crate) fn invoke(
        &mut self,
        stack: &mut ActivationStack,
        _context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        stack.stage_call(*callee, this_value, None, args, Some(dst));
        Ok(())
    }

    /// Stage `[[Construct]]` with an explicit `new.target`.
    pub(crate) fn stage_construct(
        &mut self,
        stack: &mut ActivationStack,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) {
        stack.stage_call(
            callee,
            Value::undefined(),
            Some(new_target),
            args,
            Some(dst),
        );
    }

    /// Handle `Op::New`.
    #[cfg(test)]
    pub(crate) fn do_construct<'a>(
        &mut self,
        stack: &mut ActivationStack,
        _context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        self.do_construct_inner(stack, ArgumentOperands::decoded(operands.into()))
    }

    pub(crate) fn do_construct_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_construct_inner(stack, ArgumentOperands::execution(function, instruction))
    }

    fn do_construct_inner(
        &mut self,
        stack: &mut ActivationStack,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)? as usize;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let args = BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc)
            .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        self.stage_construct(stack, callee, callee, args, dst);
        Ok(())
    }

    /// The `new.target` a `super(...)` call forwards: the derived frame's
    /// own, or the parent constructor itself outside a construct.
    fn super_new_target(frame: &crate::Frame, callee: Value) -> Value {
        let target = frame.new_target();
        if target.is_undefined() {
            callee
        } else {
            target
        }
    }

    /// Handle fixed-arity `Op::SuperConstruct`.
    pub(crate) fn do_super_construct_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let operands = ArgumentOperands::execution(function, instruction);
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)? as usize;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let new_target = Self::super_new_target(&stack[top_idx], callee);
        let args = BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc)
            .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        let origin = std::ptr::from_mut(&mut stack[top_idx]) as u64;
        self.stage_construct(stack, callee, new_target, args, dst);
        stack
            .staged_request_mut()
            .ok_or(VmError::InvalidOperand)?
            .super_origin = origin;
        Ok(())
    }

    fn spread_call_arguments(&self, value: Value) -> Result<SmallVec<[Value; 8]>, VmError> {
        let array = value.as_array().ok_or(VmError::TypeMismatch)?;
        Ok(crate::array::with_elements(
            array,
            &self.gc_heap,
            |elements| elements.iter().copied().collect(),
        ))
    }

    pub(crate) fn do_construct_spread(
        &mut self,
        stack: &mut ActivationStack,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let args = self.spread_call_arguments(*read_register(&stack[top_idx], args_reg)?)?;
        stack[top_idx].advance_pc()?;
        self.stage_construct(stack, callee, callee, args, dst);
        Ok(())
    }

    pub(crate) fn do_super_construct_spread(
        &mut self,
        stack: &mut ActivationStack,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let new_target = Self::super_new_target(&stack[top_idx], callee);
        let args = self.spread_call_arguments(*read_register(&stack[top_idx], args_reg)?)?;
        stack[top_idx].advance_pc()?;
        let origin = std::ptr::from_mut(&mut stack[top_idx]) as u64;
        self.stage_construct(stack, callee, new_target, args, dst);
        stack
            .staged_request_mut()
            .ok_or(VmError::InvalidOperand)?
            .super_origin = origin;
        Ok(())
    }

    /// Handle `Op::CallSpread`: the receiver register holds the explicit
    /// `this` value and the arguments array holds every actual.
    pub(crate) fn do_call_spread(
        &mut self,
        stack: &mut ActivationStack,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        let args = self.spread_call_arguments(*read_register(&stack[top_idx], args_reg)?)?;
        stack[top_idx].advance_pc()?;
        stack.stage_call(callee, this_value, None, args, Some(dst));
        Ok(())
    }

    /// Handle `Op::CallWithThis`: `Op::Call` with an explicit receiver
    /// register.
    pub(crate) fn do_call_with_this_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let operands = ArgumentOperands::execution(function, instruction);
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let this_reg = operands.register(2)?;
        let argc = operands.const_index(3)? as usize;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        let args = BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 4, argc)
            .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        stack.stage_call(callee, this_value, None, args, Some(dst));
        Ok(())
    }

    /// Synchronously invoke `callee(args)` with the given `this` and return
    /// its completion, from a host callback with no enclosing turn.
    pub fn run_callable_sync(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let mut activations = ActivationStack::new();
        self.with_runtime_turn(&mut activations, |turn| {
            let (interp, stack) = turn.into_parts();
            interp.run_callable_sync_rooted(stack, Some(context), callee, this_value, args)
        })
    }

    /// Synchronously invoke a callable above the current activation floor.
    ///
    /// The request enters the same classifying trampoline as a bytecode call;
    /// this host callback retains only its own Rust frame while it runs.
    pub(crate) fn run_callable_sync_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        let source = self.callable_context(context, *callee)?;
        self.enter_sync_reentry()?;
        let floor = stack.floor();
        stack.stage_call(*callee, this_value, None, args, None);
        let result = self.execute_prepared_call(source.as_ref(), stack);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    /// Synchronously construct above the current rooted activation floor.
    pub(crate) fn run_construct_sync_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        super_origin: u64,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        self.enter_sync_reentry()?;
        let floor = stack.floor();
        stack.stage_call(*target, Value::undefined(), Some(new_target), args, None);
        let Some(request) = stack.staged_request_mut() else {
            self.leave_sync_reentry();
            return Err(VmError::InvalidOperand);
        };
        request.super_origin = super_origin;
        let result = self.execute_prepared_call(Some(context), stack);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    pub(crate) fn construct_prototype_for_callee(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, CommittedValueError> {
        let function_id = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            let owner = callee.as_closure(&self.gc_heap);
            return match self.function_property_get_with_receiver(
                stack,
                context,
                owner,
                function_id,
                Some(*callee),
                "prototype",
            )? {
                proto if proto.is_object_type() => Ok(Some(proto)),
                _ => Ok(None),
            };
        }
        if let Some(c) = callee.as_class_constructor() {
            return Ok(Some(Value::object(c.prototype(&self.gc_heap))));
        }
        if callee.as_proxy().is_some() {
            return self.construct_prototype_via_get(stack, context, callee);
        }
        if let Some(obj) = callee.as_object() {
            return Ok(match crate::object::get(obj, &self.gc_heap, "prototype") {
                Some(proto) if proto.is_object_type() => Some(proto),
                _ => None,
            });
        }
        if callee.is_bound_function() {
            return self.construct_prototype_via_get(stack, context, callee);
        }
        if let Some(native) = callee.as_native_function() {
            return native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(|_| VmError::InvalidOperand)
                .map(|desc| {
                    desc.and_then(|d| match d.kind {
                        crate::object::DescriptorKind::Data { value } if value.is_object_type() => {
                            Some(value)
                        }
                        _ => None,
                    })
                })
                .map_err(|error| CommittedValueError::JavaScript(error.into()));
        }
        Ok(None)
    }

    fn construct_prototype_via_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, CommittedValueError> {
        let callee_anchor = self.push_iteration_anchor(*callee) - 1;
        let anchor_base = callee_anchor;
        let result = (|| -> Result<Option<Value>, CommittedValueError> {
            let key = VmPropertyKey::String("prototype");
            let callee = self.iteration_anchor(callee_anchor);
            let proto =
                match self.ordinary_get_value(stack, Some(context), callee, callee, &key, 0)? {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let callee = self.iteration_anchor(callee_anchor);
                        self.run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            callee,
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?
                    }
                };
            // The getter may have collected or revoked a Proxy. Reload the
            // callee from the anchor before consulting its post-Get state.
            let revoked_proxy = self
                .iteration_anchor(callee_anchor)
                .as_proxy()
                .is_some_and(|proxy| proxy.is_revoked(&self.gc_heap));
            if !proto.is_object_type() && revoked_proxy {
                return Err(CommittedValueError::JavaScript(self.err_type(
                    ("Cannot get prototype from a revoked proxy".to_string()).into(),
                )));
            }
            Ok(proto.is_object_type().then_some(proto))
        })();
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    /// Native constructors that must NOT receive a pre-allocated
    /// receiver whose prototype is read from `new.target` before the
    /// constructor body runs. `Promise` builds its own object, and the
    /// dynamic-function constructors (`Function` and friends) parse
    /// their source and only then run `GetPrototypeFromConstructor`
    /// (§20.2.1.1.1) — an eager `new.target.prototype` read here would
    /// be observable before the SyntaxError a bad body must throw. The
    /// buffer, view and typed-array constructors validate their arguments
    /// before `OrdinaryCreateFromConstructor` (§25.1.4.1, §25.3.2.1,
    /// §23.2.5.1) and allocate their own exotic result.
    pub(crate) fn native_receiverless_constructor(&self, callee: &Value) -> Option<NativeFunction> {
        let native = if let Some(native) = callee.as_native_function() {
            native
        } else {
            let obj = callee.as_object()?;
            crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())?
        };
        [
            "Promise",
            "Function",
            "GeneratorFunction",
            "AsyncFunction",
            "AsyncGeneratorFunction",
            // §22.2.4.1 RegExp and §26.1.* WeakRef /
            // FinalizationRegistry allocate their own exotic result and
            // resolve `new.target.prototype` themselves. Pre-allocating
            // an ordinary receiver here would read that property a
            // second time, and the getter is observable.
            "RegExp",
            "WeakRef",
            "FinalizationRegistry",
            "ArrayBuffer",
            "SharedArrayBuffer",
            "DataView",
            "Int8Array",
            "Uint8Array",
            "Uint8ClampedArray",
            "Int16Array",
            "Uint16Array",
            "Int32Array",
            "Uint32Array",
            "Float16Array",
            "Float32Array",
            "Float64Array",
            "BigInt64Array",
            "BigUint64Array",
        ]
        .iter()
        .any(|expected| native.name(&self.gc_heap).eq_str(expected, &self.gc_heap))
        .then_some(native)
    }

    /// Handle `Op::CallForwardArguments`: `callee.apply(this_arg, arguments)`
    /// for a body whose every use of `arguments` is such a forward.
    ///
    /// `method` already holds the observable `GetV(callee, "apply")`. When it
    /// is %Function.prototype.apply%, the activation's incoming arguments are
    /// forwarded to `callee` directly — the arguments object is never built.
    /// Any other method value receives the activation's arguments object,
    /// materialized once per frame, exactly as a materialized `arguments`
    /// binding would have supplied it.
    pub(crate) fn do_call_forward_arguments_exec(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), CommittedValueError> {
        let dst = function
            .register(instruction, 0)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let method_reg = function
            .register(instruction, 1)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let callee_reg = function
            .register(instruction, 2)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let this_reg = function
            .register(instruction, 3)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let top_idx = stack.len() - 1;
        let method = *read_register(&stack[top_idx], method_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let callee = *read_register(&stack[top_idx], callee_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        if crate::method_ops::is_function_prototype_intrinsic_value(
            method,
            &self.gc_heap,
            crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply,
        ) {
            if !self.is_callable_runtime(&callee) {
                return Err(CommittedValueError::JavaScript(VmError::NotCallable));
            }
            let existing = stack[top_idx].arguments_object().map(Value::object);
            let forwarded = if let Some(arguments) = existing {
                self.create_list_from_array_like(stack, context, arguments)?
            } else {
                let view = crate::ActiveFrameRef::from_frame(&stack[top_idx]);
                let count = view.incoming_argument_count();
                let mut forwarded: SmallVec<[Value; 8]> = (0..count)
                    .map(|index| view.incoming_argument(index))
                    .collect::<Result<_, _>>()
                    .map_err(CommittedValueError::Fatal)?;
                self.refresh_mapped_argument_values(
                    function,
                    &crate::ActiveFrameRef::from_frame(&stack[top_idx]),
                    &mut forwarded,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                forwarded
            };
            let callee = *read_register(&stack[top_idx], callee_reg)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let this_value = *read_register(&stack[top_idx], this_reg)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            stack[top_idx]
                .advance_pc()
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            return self
                .invoke(stack, context, &callee, this_value, forwarded, dst)
                .map_err(|error| CommittedValueError::JavaScript(error.into()));
        }
        let arguments_object = self
            .materialize_frame_arguments_object(context, stack, top_idx)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        // Materialization can move a getter-produced method and both operands.
        // Their register slots retain the committed lookup without replaying it.
        let method = *read_register(&stack[top_idx], method_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let callee = *read_register(&stack[top_idx], callee_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let this_value = *read_register(&stack[top_idx], this_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        stack[top_idx]
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let args: SmallVec<[Value; 8]> = [this_value, arguments_object].into_iter().collect();
        self.invoke(stack, context, &method, callee, args, dst)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))
    }
}

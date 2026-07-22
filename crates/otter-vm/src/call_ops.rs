//! Call and construct opcode helpers.
//!
//! Stack-modifying call bytecodes decode variadic executable operands, prepare
//! frames, and may immediately invoke native/proxy/constructor paths. Keeping
//! that machinery here lets `lib.rs` stay closer to a dispatch map.
//!
//! # Contents
//! - Ordinary call entry and shared callable invocation.
//! - Constructor call entry and receiver/prototype setup.
//! - Spread and explicit-`this` call forms.
//! - Same-stack synchronous re-entry and reusable lean callback frames.
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
//! - Nested call/construct dispatch appends above an `ActivationFloor` on the
//!   current rooted stack; native boundary slots are collector-rewritten in
//!   their original storage.
//! - Lean callback state owns reusable frame storage, never a detached stack;
//!   caller argument slots remain traced through allocating frame setup.
//! - A freshly-started generator remains in a moving GC root through observable
//!   `prototype` lookup and publication into the caller.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use std::cell::UnsafeCell;

use crate::activation_stack::ActivationStack;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    AsyncFrameState, CodeBlock, ExecutionContext, Frame, Interpreter, JsObject, NativeCallInfo,
    NativeCtx, NativeFunction, Value, VmError, VmGetOutcome, VmPropertyKey, abstract_ops,
    argument_window::{ArgumentOperands, BytecodeArgumentWindow},
    executable::OperandView,
    frame_state::UpvalueSpine,
    is_constructor_runtime, native_to_vm_error_with_stack,
    operand_decode::register_operand,
    promise_dispatch, read_register,
    runtime_cx::NativeCallRoots,
    write_register,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_construct_argument_layout_does_not_acquire_cold_record() {
        let function = CodeBlock::jit_test_stub(0, 2, 1, &[]);
        let mut interp = Interpreter::new();
        let receiver = interp
            .alloc_host_object_with_roots(&[], &[])
            .expect("construct receiver");
        let window = interp.alloc_reg_window(1).expect("register window");
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            &function,
            None,
            Frame::empty_upvalues(),
            Value::object(receiver),
            window,
        );
        let live_before = interp.cold_frames().live_len();

        let error = interp
            .bind_construct_arguments_and_publish_cold(
                &function,
                &mut frame,
                smallvec::smallvec![Value::number_i32(1), Value::number_i32(2)],
                false,
                None,
                Some(receiver),
                Value::function(0),
            )
            .expect_err("invalid register layout must fail");

        assert!(matches!(error, VmError::InvalidOperand));
        assert!(
            frame.cold.is_none(),
            "failed unpublished frame must not own a cold record"
        );
        assert_eq!(
            interp.cold_frames().live_len(),
            live_before,
            "invalid construct metadata must not leak a cold-frame slot"
        );
        interp.reclaim_registers(&mut frame);
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

    pub(crate) fn args_len(&self) -> usize {
        // SAFETY: this short read cannot allocate or overlap root tracing.
        unsafe { (&*self.args.get()).len() }
    }

    pub(crate) fn replace_args(&self, args: SmallVec<[Value; 8]>) {
        // SAFETY: short single-mutator write with no VM allocation.
        unsafe { *self.args.get() = args };
    }

    fn prepend_args(&self, prefix: &[Value]) {
        if prefix.is_empty() {
            return;
        }
        // SAFETY: this is one Rust-only mutation on the mutator thread.
        // SmallVec may use Rust's allocator, but no JavaScript allocation or
        // collection can overlap the temporary slice rearrangement.
        unsafe {
            let args = &mut *self.args.get();
            let old_len = args.len();
            args.resize(old_len + prefix.len(), Value::undefined());
            args.copy_within(0..old_len, prefix.len());
            args[..prefix.len()].copy_from_slice(prefix);
        }
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

fn invoke_native_call_with_roots(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    call: crate::native_function::NativeCallTarget,
    realm_global: Option<JsObject>,
    this_value: Value,
    value_roots: &[&Value],
    args: &[Value],
) -> Result<Value, VmError> {
    let call_info = NativeCallInfo::call(this_value);
    let slice_roots = [args];
    let roots = NativeCallRoots::new(&call_info, value_roots, &slice_roots);
    // Pushed (not installed) so any outer scope's value/slice roots
    // stay visible to scavenges triggered inside this native.
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    debug_assert!(interp.gc_heap.has_frame_root_providers());
    if let Some(global) = realm_global {
        interp.with_host_realm_global(global, |interp| {
            let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(interp, stack);
            let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
            let raw = call.invoke(&mut ctx, args);
            raw.map_err(|e| native_to_vm_error_with_stack(interp, stack, e))
        })
    } else {
        let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(interp, stack);
        let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
        let raw = call.invoke(&mut ctx, args);
        raw.map_err(|e| native_to_vm_error_with_stack(interp, stack, e))
    }
}

struct PreparedBytecodeFrame {
    frame: Frame,
    is_generator: bool,
    is_async_generator: bool,
    /// Callee function id — needed to resolve the generator
    /// `[[Prototype]]` AFTER the prologue runs (§27.5.1 step 3:
    /// FunctionDeclarationInstantiation precedes
    /// OrdinaryCreateFromConstructor, so parameter side effects on
    /// `fn.prototype` are observable).
    generator_function_id: u32,
    /// Invoked closure instance — generator `[[Prototype]]` resolution
    /// must read `fn.prototype` through the per-closure bag, not the
    /// template bag, so identity matches later user reads.
    callee_closure: Option<crate::closure::JsClosure>,
}

/// `(function_id, upvalues, this, new_target, derived_this_cell,
/// eval_env, closure)` resolved from a callable value for a bytecode call.
pub(crate) type BytecodeCallTargetParts = (
    u32,
    crate::frame_state::UpvalueSpine,
    Value,
    Option<Value>,
    Option<crate::UpvalueCell>,
    Option<crate::eval_env::EvalEnvHandle>,
    Option<crate::closure::JsClosure>,
);

#[derive(Clone)]
pub(crate) struct LeanCallbackRoot {
    callback: Value,
    function_id: u32,
    bound_this: Option<Value>,
    bound_new_target: Option<Value>,
    bound_derived_this: Option<crate::UpvalueCell>,
    eval_env: Option<crate::eval_env::EvalEnvHandle>,
}

impl LeanCallbackRoot {
    fn from_callback(callback: Value, heap: &otter_gc::GcHeap) -> Option<Self> {
        if let Some(function_id) = callback.as_function() {
            return Some(Self {
                callback,
                function_id,
                bound_this: None,
                bound_new_target: None,
                bound_derived_this: None,
                eval_env: None,
            });
        }
        let closure = callback.as_closure(heap)?;
        let function_id = closure.function_id();
        let state = closure.call_state(heap);
        Some(Self {
            callback,
            function_id,
            bound_this: state.bound_this,
            bound_new_target: state.bound_new_target,
            bound_derived_this: state.bound_derived_this,
            eval_env: state.eval_env,
        })
    }

    /// Re-read the allocation-neutral captured spine from the currently traced
    /// callback value. The callback root keeps the closure allocation alive and
    /// is relocation-refreshed between builtin elements.
    fn upvalue_source(
        &self,
        heap: &otter_gc::GcHeap,
    ) -> Result<crate::upvalue_source::UpvalueSource, VmError> {
        if self.callback.as_function().is_some() {
            return Ok(crate::upvalue_source::UpvalueSource::empty());
        }
        let closure = self
            .callback
            .as_closure(heap)
            .ok_or(VmError::InvalidOperand)?;
        if closure.function_id() != self.function_id {
            return Err(VmError::InvalidOperand);
        }
        Ok(closure.call_state(heap).upvalues)
    }

    pub(crate) fn trace_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.callback.trace_value_slots(visitor);
        if let Some(value) = &self.bound_this {
            value.trace_value_slots(visitor);
        }
        if let Some(value) = &self.bound_new_target {
            value.trace_value_slots(visitor);
        }
        if let Some(cell) = &self.bound_derived_this {
            let p = cell as *const crate::UpvalueCell as *mut RawGc;
            visitor(p);
        }
        if let Some(env) = &self.eval_env {
            let p = env as *const crate::eval_env::EvalEnvHandle as *mut RawGc;
            visitor(p);
        }
    }
}

pub(crate) struct LeanCallbackState {
    root_index: usize,
    function_id: u32,
    /// Callee register-window length, read once from the executable function.
    register_count: usize,
    /// Number of formal parameters to bind on the lean path.
    param_count: usize,
    /// Strict and arrow callbacks use the incoming receiver directly.
    this_passthrough: bool,
    /// Whether the callback has captured upvalues that must be refreshed from
    /// the traced root before each off-stack recycled-frame entry.
    has_parent_upvalues: bool,
    /// True when the callback carries no bound `new.target`, derived-`this`
    /// cell, or captured eval environment — i.e. the per-element frame needs no
    /// pooled cold record. The hot Array/Map/Set/TypedArray callbacks (plain
    /// functions and arrows) all qualify, so they take the prepared-frame fast
    /// path; anything else falls to the general per-element build.
    fast_reuse: bool,
    /// Installed baseline body for the callback, resolved once on the first
    /// element that tiers up and reused for the rest of the loop. The lean
    /// state lives only for a single builtin invocation, so no mid-loop
    /// recompilation can stale this handle.
    compiled: Option<std::sync::Arc<dyn crate::jit::JitFunctionCode>>,
    /// Recycled callee frame for the prepared fast path: its register window,
    /// upvalue spine box, and frame shell are reused across elements instead of
    /// being drawn, built, and dropped per element. `None` until the first
    /// fast-path call builds it, and whenever a bail consumes it mid-loop.
    reuse_frame: Option<Frame>,
}

impl Interpreter {
    pub(crate) fn lean_callback_parent_upvalue(
        &self,
        state: &LeanCallbackState,
        idx: usize,
    ) -> Option<crate::UpvalueCell> {
        self.lean_callback_roots
            .get(state.root_index)?
            .upvalue_source(&self.gc_heap)
            .ok()?
            .read(idx)
    }

    /// §9.1 — install the frame's direct-eval variable environment:
    /// a `contains_direct_eval` function gets a FRESH record chained
    /// to the closure's captured one (so probe closures created
    /// before the eval observe later bindings); other closures just
    /// re-expose the captured record for the dynamic walkers.
    pub(crate) fn stash_frame_eval_env(
        &mut self,
        function: &crate::executable::CodeBlock,
        frame: &mut Frame,
        inherited: Option<crate::eval_env::EvalEnvHandle>,
    ) -> Result<(), VmError> {
        if function.contains_direct_eval {
            let mut frame_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                frame.trace_frame_slots(visitor);
            };
            let env = crate::eval_env::alloc_eval_env_with_roots(
                &mut self.gc_heap,
                inherited,
                &mut frame_roots,
            )
            .map_err(crate::oom_to_vm)?;
            self.frame_ensure_cold(frame).eval_env = Some(env);
        } else if inherited.is_some() {
            self.frame_ensure_cold(frame).eval_env = inherited;
        }
        Ok(())
    }

    pub(crate) fn bind_bytecode_call_arguments(
        &mut self,
        function: &CodeBlock,
        frame: &mut Frame,
        args: SmallVec<[Value; 8]>,
    ) -> Result<(), VmError> {
        let bind_count = (function.param_count as usize).min(args.len());
        let total_args = args.len();
        let incoming: Option<SmallVec<[Value; 4]>> = if function.needs_arguments {
            Some(args.iter().cloned().collect())
        } else {
            None
        };
        let mut iter = args.into_iter();
        for i in 0..bind_count {
            let value = iter.next().expect("bind_count <= len");
            let slot = frame.registers.get_mut(i).ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        let rest: Option<SmallVec<[Value; 4]>> =
            if function.has_rest && total_args > function.param_count as usize {
                Some(iter.collect())
            } else {
                None
            };
        if incoming.is_some() || rest.is_some() {
            let cold = self.frame_ensure_cold(frame);
            if let Some(v) = incoming {
                cold.incoming_args = v;
            }
            if let Some(v) = rest {
                cold.rest_args = v;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_construct_arguments_and_publish_cold(
        &mut self,
        function: &CodeBlock,
        frame: &mut Frame,
        args: SmallVec<[Value; 8]>,
        is_derived: bool,
        derived_this_cell: Option<crate::UpvalueCell>,
        receiver: Option<JsObject>,
        new_target: Value,
    ) -> Result<(), VmError> {
        // Bind before acquiring constructor cold state. Invalid bytecode can
        // reject an oversized parameter layout here; if cold storage were
        // attached first, dropping this unpublished frame would leak the pool
        // record and keep its receiver/new.target roots live indefinitely.
        self.bind_bytecode_call_arguments(function, frame, args)?;
        let cold = self.frame_ensure_cold(frame);
        if is_derived {
            cold.is_derived_constructor = true;
            cold.derived_this_cell = derived_this_cell;
        } else {
            cold.construct_target = receiver;
        }
        cold.new_target = Some(new_target);
        Ok(())
    }

    fn bind_lean_bytecode_call_arguments(
        function: &CodeBlock,
        frame: &mut Frame,
        args: &[Value],
    ) -> Result<(), VmError> {
        debug_assert!(
            !function.needs_arguments && !function.has_rest,
            "lean callback path must not need argument materialization"
        );
        let bind_count = (function.param_count as usize).min(args.len());
        for (i, value) in args.iter().copied().take(bind_count).enumerate() {
            let slot = frame.registers.get_mut(i).ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        Ok(())
    }

    fn reset_and_bind_lean_bytecode_call_arguments(
        param_count: usize,
        frame: &mut Frame,
        args: &[Value],
    ) -> Result<(), VmError> {
        let bind_count = param_count.min(args.len());
        if bind_count > frame.registers.len() {
            return Err(VmError::InvalidOperand);
        }
        for slot in frame.registers.iter_mut().skip(bind_count) {
            *slot = Value::undefined();
        }
        for (i, value) in args.iter().copied().take(bind_count).enumerate() {
            frame.registers[i] = value;
        }
        Ok(())
    }

    pub(crate) fn bytecode_call_target_parts(
        current: Value,
        effective_this: Value,
        heap: &otter_gc::GcHeap,
    ) -> Result<BytecodeCallTargetParts, VmError> {
        if let Some(function_id) = current.as_function() {
            return Ok((
                function_id,
                Frame::empty_upvalues(),
                effective_this,
                None,
                None,
                None,
                None,
            ));
        }
        if let Some(handle) = current
            .as_raw_gc()
            .and_then(|raw| raw.checked_cast::<crate::closure::JsClosureBody>())
        {
            let (function_id, upvalues, bound_this, bound_new_target, bound_derived_this, eval_env) =
                heap.read_payload(handle, |body| {
                    let ups: crate::frame_state::UpvalueSpine =
                        body.upvalues.clone().into_boxed_slice();
                    (
                        body.call_header.function_id,
                        ups,
                        body.bound_this_option(),
                        body.bound_new_target_option(),
                        body.bound_derived_this_option(),
                        body.eval_env_option(),
                    )
                });
            let c = crate::closure::JsClosure::from_parts(handle, function_id);
            let this_value = bound_this.unwrap_or(effective_this);
            return Ok((
                function_id,
                upvalues,
                this_value,
                bound_new_target,
                bound_derived_this,
                eval_env,
                Some(c),
            ));
        }
        Err(VmError::NotCallable)
    }

    fn bytecode_construct_target_parts(
        current: Value,
        heap: &otter_gc::GcHeap,
    ) -> Result<(u32, crate::frame_state::UpvalueSpine), VmError> {
        if let Some(function_id) = current.as_function() {
            return Ok((function_id, Frame::empty_upvalues()));
        }
        if let Some(c) = current.as_closure(heap) {
            let function_id = c.function_id();
            let upvalues = c.upvalues_snapshot(heap).into_boxed_slice();
            return Ok((function_id, upvalues));
        }
        Err(VmError::NotCallable)
    }

    fn build_construct_bytecode_frame(
        &mut self,
        context: &ExecutionContext,
        mut current: Value,
        mut receiver: Option<JsObject>,
        mut new_target: Value,
        mut args: SmallVec<[Value; 8]>,
        return_register: Option<u16>,
    ) -> Result<Frame, VmError> {
        let (function_id, parent_upvalues) =
            Self::bytecode_construct_target_parts(current, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // §10.2.5 — only ordinary functions get a [[Construct]] slot;
        // async functions, generators, async generators, and
        // MethodDefinition bodies are not constructors.
        if function.is_async || function.is_generator || function.is_method {
            return Err(self.err_type(("function is not a constructor".to_string()).into()));
        }
        // Everything read below lives in plain Rust locals while this frame
        // is not yet on any traced stack: a collection triggered by the cell
        // allocations must rewrite them in place or they go stale.
        let mut build_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            current.trace_value_slot_mut(visitor);
            new_target.trace_value_slot_mut(visitor);
            if let Some(receiver) = &mut receiver {
                visitor(receiver as *mut JsObject as *mut RawGc);
            }
            for value in &mut args {
                value.trace_value_slot_mut(visitor);
            }
        };
        let upvalues = Frame::build_upvalues_for_exec_with_roots(
            &mut self.gc_heap,
            function,
            parent_upvalues,
            &mut build_roots,
        )?;
        // §10.2.2 — a derived constructor enters with `this` in the
        // TDZ; `super(...)` binds it via `Op::BindThisValue`. A base
        // constructor receives the freshly-allocated receiver as
        // `this` immediately.
        let is_derived = function.is_derived_constructor;
        debug_assert!(
            !is_derived || receiver.is_none(),
            "derived construction must not materialize an outer receiver"
        );
        let this_value = if is_derived {
            Value::hole()
        } else {
            Value::object(receiver.ok_or(VmError::InvalidOperand)?)
        };
        let window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_value,
            window,
        );
        frame.self_value = current;
        let derived_this_cell = if is_derived {
            let mut frame_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                frame.trace_frame_slots(visitor);
                current.trace_value_slot_mut(visitor);
                new_target.trace_value_slot_mut(visitor);
                if let Some(receiver) = &mut receiver {
                    visitor(receiver as *mut JsObject as *mut RawGc);
                }
                for value in &mut args {
                    value.trace_value_slot_mut(visitor);
                }
            };
            Some(crate::alloc_upvalue_with_roots(
                &mut self.gc_heap,
                Value::hole(),
                &mut frame_roots,
            )?)
        } else {
            None
        };
        self.bind_construct_arguments_and_publish_cold(
            function,
            &mut frame,
            args,
            is_derived,
            derived_this_cell,
            receiver,
            new_target,
        )?;
        window_rollback.commit();
        Ok(frame)
    }

    fn build_construct_bytecode_frame_from_window(
        &mut self,
        context: &ExecutionContext,
        mut current: Value,
        mut receiver: Option<JsObject>,
        mut new_target: Value,
        args: &BytecodeArgumentWindow<'_, '_>,
        return_register: Option<u16>,
    ) -> Result<Frame, VmError> {
        let (function_id, parent_upvalues) =
            Self::bytecode_construct_target_parts(current, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // §10.2.5 — async / generator functions and methods are not
        // constructors.
        if function.is_async || function.is_generator || function.is_method {
            return Err(self.err_type(("function is not a constructor".to_string()).into()));
        }
        // See `build_construct_bytecode_frame`: the frame is not yet on a
        // traced stack, so every local must ride through the allocations.
        let mut build_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            current.trace_value_slot_mut(visitor);
            new_target.trace_value_slot_mut(visitor);
            if let Some(receiver) = &mut receiver {
                visitor(receiver as *mut JsObject as *mut RawGc);
            }
        };
        let upvalues = Frame::build_upvalues_for_exec_with_roots(
            &mut self.gc_heap,
            function,
            parent_upvalues,
            &mut build_roots,
        )?;
        let is_derived = function.is_derived_constructor;
        debug_assert!(
            !is_derived || receiver.is_none(),
            "derived construction must preserve the caller register window without a receiver"
        );
        let this_value = if is_derived {
            Value::hole()
        } else {
            Value::object(receiver.ok_or(VmError::InvalidOperand)?)
        };
        let window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_value,
            window,
        );
        frame.self_value = current;
        let mut extras = args.bind_into(function, &mut frame)?;
        let derived_this_cell = if is_derived {
            let mut frame_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                frame.trace_frame_slots(visitor);
                current.trace_value_slot_mut(visitor);
                new_target.trace_value_slot_mut(visitor);
                if let Some(receiver) = &mut receiver {
                    visitor(receiver as *mut JsObject as *mut RawGc);
                }
                for value in &mut extras.rest_args {
                    value.trace_value_slot_mut(visitor);
                }
                for value in &mut extras.incoming_args {
                    value.trace_value_slot_mut(visitor);
                }
            };
            Some(crate::alloc_upvalue_with_roots(
                &mut self.gc_heap,
                Value::hole(),
                &mut frame_roots,
            )?)
        } else {
            None
        };
        {
            let cold = self.frame_ensure_cold(&mut frame);
            if is_derived {
                cold.is_derived_constructor = true;
                cold.derived_this_cell = derived_this_cell;
            } else {
                cold.construct_target = receiver;
            }
            cold.new_target = Some(new_target);
            if !extras.is_empty() {
                cold.rest_args = extras.rest_args;
                cold.incoming_args = extras.incoming_args;
            }
        }
        window_rollback.commit();
        Ok(frame)
    }

    fn invoke_native_construct_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        native: NativeFunction,
        this_value: &Value,
        new_target: &Value,
        used_object_prototype_fallback: bool,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let call = native.call_target(&self.gc_heap);
        let call_info = NativeCallInfo::construct_with_receiver(
            *this_value,
            Some(*new_target),
            used_object_prototype_fallback,
        );
        self.record_runtime_native_call();
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
        let (interp, stack) = ctx.cx.into_parts();
        let result = raw.map_err(|e| native_to_vm_error_with_stack(interp, stack, e))?;
        Ok(if result.is_object_type() {
            result
        } else {
            rooted_this
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push_bytecode_call_frame(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee_closure: Option<crate::closure::JsClosure>,
        function_id: u32,
        parent_upvalues: UpvalueSpine,
        this_for_callee: Value,
        new_target_for_callee: Option<Value>,
        derived_this_cell: Option<crate::UpvalueCell>,
        callee_eval_env: Option<crate::eval_env::EvalEnvHandle>,
        effective_args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.record_runtime_bytecode_call();
        if self.logical_call_depth(stack) >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // Async-call entry path (spec §27.7.5.1): synthesise a
        // fresh pending result promise, write it into the caller's
        // `dst` register *now* so the call expression's value is
        // visible synchronously, and park the new frame with
        // `return_register = None` so its eventual completion
        // settles the promise instead of writing back.
        let (return_register, async_state) = if function.is_async && !function.is_generator {
            let result_promise = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(
                    self,
                    stack,
                    &[&this_for_callee],
                    &[effective_args.as_slice()],
                )?;
            let promise_value = Value::promise(result_promise);
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, promise_value)?;
            (None, Some(AsyncFrameState { result_promise }))
        } else {
            (Some(dst), None)
        };
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call_stack_rooted(
            function,
            stack,
            this_for_callee,
            &[effective_args.as_slice()],
        )?;
        let window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_for_callee,
            window,
        );
        new_frame.self_value = callee_closure
            .map(Value::closure)
            .unwrap_or_else(|| Value::function(function_id));
        if let Some(async_state) = async_state {
            self.frame_set_async_state(&mut new_frame, async_state);
        }
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut new_frame, callee_eval_env)?;
        self.bind_bytecode_call_arguments(function, &mut new_frame, effective_args)?;
        // §27.5 Generator-call entry: instead of pushing the frame
        // onto the dispatch stack, hand the caller a paused
        // [`Value::Generator`] handle that owns the prepared frame.
        // The body only runs when `.next()` resumes it.
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let generator_function_id = function.id;
            let cold = self.frame_detach_cold(&mut new_frame);
            let new_frame = self.park_active_frame(new_frame);
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                new_frame,
                cold,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            // Backlink the generator into the frame so `Op::Yield`
            // can find its owner once execution starts.
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let generator_anchor = self.push_iteration_anchor(Value::generator(gen_handle)) - 1;
            let result = (|| -> Result<(), VmError> {
                let gen_handle = self
                    .iteration_anchor(generator_anchor)
                    .as_generator()
                    .ok_or(VmError::InvalidOperand)?;
                let (frame, cold) = gen_handle
                    .take_frame(&mut self.gc_heap)
                    .ok_or(VmError::InvalidOperand)?;
                let mut frame = self.resume_parked_frame(*frame)?;
                if let Some(cold) = cold {
                    self.frame_attach_cold(&mut frame, cold);
                }
                let prologue_floor = stack.floor();
                stack.push(frame);
                let prologue = self.dispatch_loop_above_rooted(context, stack, prologue_floor);
                self.release_frames_above(stack, prologue_floor);
                prologue?;
                self.resolve_generator_prototype(
                    stack,
                    context,
                    callee_closure,
                    generator_function_id,
                    generator_anchor,
                )?;
                let generator = self.iteration_anchor(generator_anchor);
                let top_idx = stack.len() - 1;
                write_register(&mut stack[top_idx], dst, generator)
            })();
            self.pop_iteration_anchors_to(generator_anchor);
            return result;
        }
        stack.push(new_frame);
        window_rollback.commit();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_bytecode_call_frame_from_window(
        &mut self,
        stack: &ActivationStack,
        function: &CodeBlock,
        parent_upvalues: UpvalueSpine,
        this_for_callee: Value,
        new_target_for_callee: Option<Value>,
        derived_this_cell: Option<crate::UpvalueCell>,
        callee_eval_env: Option<crate::eval_env::EvalEnvHandle>,
        args: &BytecodeArgumentWindow<'_, '_>,
        return_register: Option<u16>,
        async_state: Option<AsyncFrameState>,
    ) -> Result<PreparedBytecodeFrame, VmError> {
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee =
            self.this_for_bytecode_call_stack_rooted(function, stack, this_for_callee, &[])?;
        let window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_for_callee,
            window,
        );
        if let Some(async_state) = async_state {
            self.frame_set_async_state(&mut frame, async_state);
        }
        let extras = args.bind_into(function, &mut frame)?;
        if !extras.is_empty() {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.rest_args = extras.rest_args;
            cold.incoming_args = extras.incoming_args;
        }
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut frame, callee_eval_env)?;
        let prepared = PreparedBytecodeFrame {
            frame,
            is_generator: function.is_generator,
            is_async_generator: function.is_async_generator,
            generator_function_id: function.id,
            callee_closure: None,
        };
        window_rollback.commit();
        Ok(prepared)
    }

    /// §27.5.1 step 3 / §9.1.14 — resolve a fresh generator's
    /// [[Prototype]] from `fn.prototype` AFTER the prologue ran; a
    /// non-object answer falls back (override `None`) to the realm's
    /// shared `%GeneratorPrototype%` / `%AsyncGeneratorPrototype%`.
    fn resolve_generator_prototype(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        generator_anchor: usize,
    ) -> Result<(), VmError> {
        // `owner` is the invoked closure instance: `fn.prototype`
        // materializes per closure, so resolving through the template
        // bag would hand the generator a parallel prototype object that
        // fails `Object.getPrototypeOf(gen) === fn.prototype`.
        let proto = self.function_property_get(stack, context, owner, function_id, "prototype")?;
        let gen_handle = self
            .iteration_anchor(generator_anchor)
            .as_generator()
            .ok_or(VmError::InvalidOperand)?;
        gen_handle.set_prototype_override(
            &mut self.gc_heap,
            proto.as_object().is_some().then_some(proto),
        );
        Ok(())
    }

    fn push_prepared_bytecode_call_frame(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst: u16,
        prepared: PreparedBytecodeFrame,
    ) -> Result<(), VmError> {
        let PreparedBytecodeFrame {
            mut frame,
            is_generator,
            is_async_generator,
            generator_function_id,
            callee_closure,
        } = prepared;
        if is_generator {
            frame.return_register = None;
            let cold = self.frame_detach_cold(&mut frame);
            let frame = self.park_active_frame(frame);
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                frame,
                cold,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, is_async_generator);
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let generator_anchor = self.push_iteration_anchor(Value::generator(gen_handle)) - 1;
            let result = (|| -> Result<(), VmError> {
                let gen_handle = self
                    .iteration_anchor(generator_anchor)
                    .as_generator()
                    .ok_or(VmError::InvalidOperand)?;
                let (frame, cold) = gen_handle
                    .take_frame(&mut self.gc_heap)
                    .ok_or(VmError::InvalidOperand)?;
                let mut frame = self.resume_parked_frame(*frame)?;
                if let Some(cold) = cold {
                    self.frame_attach_cold(&mut frame, cold);
                }
                let prologue_floor = stack.floor();
                stack.push(frame);
                let prologue = self.dispatch_loop_above_rooted(context, stack, prologue_floor);
                self.release_frames_above(stack, prologue_floor);
                prologue?;
                self.resolve_generator_prototype(
                    stack,
                    context,
                    callee_closure,
                    generator_function_id,
                    generator_anchor,
                )?;
                let generator = self.iteration_anchor(generator_anchor);
                let top_idx = stack.len() - 1;
                write_register(&mut stack[top_idx], dst, generator)
            })();
            self.pop_iteration_anchors_to(generator_anchor);
            return result;
        }
        stack.push(frame);
        Ok(())
    }

    fn try_push_bytecode_call_frame_from_window(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        operands: ArgumentOperands<'_>,
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let current = *callee;
        let effective_this = this_value;
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
            callee_closure,
        ) = match Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap) {
            Ok(parts) => parts,
            Err(_) if current.as_class_constructor().is_some() => {
                // §10.3.1 — a class constructor's [[Call]] always throws;
                // only [[Construct]] may enter it.
                return Err(self.err_type(
                    ("Class constructor cannot be invoked without 'new'".to_string()).into(),
                ));
            }
            Err(_) => return Ok(false),
        };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_runtime_bytecode_call();
        if self.logical_call_depth(stack) >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let top_idx = stack.len() - 1;
        let (return_register, async_state) = if function.is_async && !function.is_generator {
            let result_promise = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, stack, &[&this_for_callee], &[])?;
            let promise_value = Value::promise(result_promise);
            write_register(&mut stack[top_idx], dst, promise_value)?;
            (None, Some(AsyncFrameState { result_promise }))
        } else {
            (Some(dst), None)
        };
        let mut prepared = {
            let caller = &stack[top_idx];
            let args =
                BytecodeArgumentWindow::from_operands(caller, operands, first_arg_operand, argc);
            self.prepare_bytecode_call_frame_from_window(
                stack,
                function,
                parent_upvalues,
                this_for_callee,
                new_target_for_callee,
                derived_this_cell,
                callee_eval_env,
                &args,
                return_register,
                async_state,
            )?
        };
        // SELF is canonical hot frame state for both interpreter and native
        // activations. It records the exact invoked closure even when this
        // particular body never executes a named-SELF opcode.
        prepared.frame.self_value = current;
        prepared.callee_closure = callee_closure;
        self.push_prepared_bytecode_call_frame(stack, context, dst, prepared)?;
        Ok(true)
    }

    fn try_invoke_native_call_from_window(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        operands: ArgumentOperands<'_>,
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let top_idx = stack.len() - 1;
        let args = {
            let caller = &stack[top_idx];
            let window =
                BytecodeArgumentWindow::from_operands(caller, operands, first_arg_operand, argc);
            window.to_smallvec8()?
        };

        if let Some(obj) = callee.as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(_) = call {
                return Ok(false);
            }
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let result = invoke_native_call_with_roots(
                self,
                stack,
                context,
                call,
                realm_global,
                this_value,
                &[callee],
                args.as_slice(),
            )?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(true);
        }

        if let Some(native) = callee.as_native_function() {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(_) = call {
                return Ok(false);
            }
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let result = invoke_native_call_with_roots(
                self,
                stack,
                context,
                call,
                realm_global,
                this_value,
                &[callee],
                args.as_slice(),
            )?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots and `this` bound
    /// to `Value::undefined()` (foundation strict default).
    #[cfg(test)]
    pub(crate) fn do_call<'a>(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        self.do_call_inner(stack, context, ArgumentOperands::decoded(operands.into()))
    }

    pub(crate) fn do_call_exec(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_call_inner(
            stack,
            context,
            ArgumentOperands::execution(function, instruction),
        )
    }

    fn do_call_inner(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)?;

        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        stack[top_idx].advance_pc()?;
        if self.try_push_bytecode_call_frame_from_window(
            stack,
            context,
            &callee,
            Value::undefined(),
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        if self.try_invoke_native_call_from_window(
            stack,
            context,
            &callee,
            Value::undefined(),
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args =
            BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc as usize)
                .to_smallvec8()?;
        self.invoke(stack, context, &callee, Value::undefined(), args, dst)
    }

    /// §15.10.3 PrepareForTailCall — `Op::TailCall`. Discards the
    /// current frame and runs the callee in its place so a strict-mode
    /// tail call uses O(1) native stack.
    ///
    /// Operand layout matches [`Op::Call`] (`dst, callee, argc,
    /// args...`); `this` defaults to `undefined`. The compiler only
    /// emits this opcode for a call in a tail position that no
    /// `try`/`finally` encloses, so the discarded frame never has live
    /// handlers. Frames whose completion needs post-processing
    /// (constructors, async result frames, or any frame that still
    /// owns a handler/cold record) fall back to ordinary
    /// [`Self::do_call_inner`], preserving behaviour at a small depth cost.
    pub(crate) fn do_tail_call_exec(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_tail_call_inner(
            stack,
            context,
            ArgumentOperands::execution(function, instruction),
        )
    }

    fn do_tail_call_inner(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)? as usize;
        let top_idx = stack.len() - 1;

        // Snapshot everything that lives in the doomed frame, and decide
        // whether the frame may be discarded in place.
        let (callee, args, ret_reg) = {
            let frame = &stack[top_idx];
            let tco_safe = frame.return_register.is_some()
                && !self.frame_has_async_state(frame)
                && match self.frame_cold(frame) {
                    None => true,
                    Some(cold) => {
                        cold.handlers.is_empty()
                            && cold.construct_target.is_none()
                            && !cold.is_derived_constructor
                    }
                };
            if !tco_safe {
                // Restore ordinary call semantics: `do_call` advances the
                // caller pc and pushes a fresh callee frame above this one.
                return self.do_call_inner(stack, context, operands);
            }
            let callee = *read_register(frame, callee_reg)?;
            let args =
                BytecodeArgumentWindow::from_operands(frame, operands, 3, argc).to_smallvec8()?;
            // `return_register` indexes the caller frame (one below this
            // one); after the pop it becomes the new top frame, so the
            // callee writes its result exactly where this frame would have.
            (callee, args, frame.return_register.unwrap())
        };

        // Discard the current frame with the same cleanup `pop_frame`
        // performs (release the cold record, return the spilled register
        // window to the pool) but write no completion value — the tail
        // callee produces it. The tail-called function correctly does not
        // appear in the discarded frame's place in any later stack trace.
        let mut popped = stack.pop().ok_or(VmError::InvalidOperand)?;
        self.frame_release_cold(&mut popped);
        self.reclaim_registers(&mut popped);

        self.invoke(stack, context, &callee, Value::undefined(), args, ret_reg)
    }

    /// Invoke `callee` with the explicit receiver `this_value` and
    /// the given argument list. Centralizes the BoundFunction
    /// unwrapping, closure `bound_this` override, and frame push so
    /// every call opcode (`Op::Call`, `Op::CallWithThis`,
    /// `Op::CallMethodValue`) shares one path.
    ///
    /// `dst` is the **caller's** register that should receive the
    /// completion value when the callee returns. `caller_pc` must
    /// already be advanced before this call so the post-pop
    /// dispatch resumes after the originating instruction.
    pub(crate) fn invoke(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        // Walk through any number of `bind` layers, accumulating
        // their bound arguments and overriding `this_value` with
        // the innermost `bound_this`. The loop bound matches the
        // JS-call stack-depth limit so a pathological self-bound
        // chain still surfaces as `StackOverflow` rather than
        // unbounded recursion.
        let mut current = *callee;
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = current.as_bound_function() {
                hops += 1;
                let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                effective_this = bound_this;
                effective_args = combined;
                current = target;
                continue;
            }
            if current.as_class_constructor().is_some() {
                // §10.2.1.1 / §10.3.1 — a class constructor's [[Call]]
                // always throws, including when reached through a
                // bound-function wrapper or Reflect/Function call
                // forwarding. Only [[Construct]] may enter it.
                return Err(self.err_type(
                    ("Class constructor cannot be invoked without 'new'".to_string()).into(),
                ));
            }
            break;
        }
        if current.is_function() || current.is_closure() {
            let (
                function_id,
                parent_upvalues,
                this_for_callee,
                new_target_for_callee,
                derived_this_cell,
                callee_eval_env,
                callee_closure,
            ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
            return self.push_bytecode_call_frame(
                stack,
                context,
                callee_closure,
                function_id,
                parent_upvalues,
                this_for_callee,
                new_target_for_callee,
                derived_this_cell,
                callee_eval_env,
                effective_args,
                dst,
            );
        }
        // Native callables short-circuit the frame push: invoke
        // the closure inline, write the result into the caller's
        // dst, and advance pc on the caller frame. No stack frame
        // is created — the closure cannot itself push frames.
        if let Some(obj) = current.as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let result = invoke_native_call_with_roots(
                self,
                stack,
                context,
                call,
                realm_global,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        if let Some(native) = current.as_native_function() {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                let result = self.run_vm_intrinsic_sync_rooted(
                    stack,
                    context,
                    intrinsic,
                    effective_this,
                    effective_args,
                )?;
                let top_idx = stack.len() - 1;
                write_register(&mut stack[top_idx], dst, result)?;
                return Ok(());
            }
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let result = invoke_native_call_with_roots(
                self,
                stack,
                context,
                call,
                realm_global,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // §28.2.4.13 Proxy.[[Call]] — delegate to the `apply`
        // trap when present; otherwise call through to the
        // target as a function. Reuse the same-stack rooted dispatcher rather
        // than materialising an argv array before GetMethod: it captures the
        // target in a dedicated root slot before the observable trap lookup,
        // then allocates the arguments array only when a callable trap exists.
        if current.as_proxy().is_some() {
            let result = self.run_callable_sync_rooted(
                stack,
                context,
                &current,
                effective_this,
                effective_args,
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
            callee_closure,
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
        self.push_bytecode_call_frame(
            stack,
            context,
            callee_closure,
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
            effective_args,
            dst,
        )
    }

    /// Handle `Op::New`: allocate a fresh receiver, set its
    /// `[[Prototype]]` to `callee.prototype` (when present), and
    /// invoke the callee with `this = receiver`. The caller's `dst`
    /// register receives either the constructor's returned object
    /// or the freshly allocated receiver — `pop_frame` performs
    /// that swap so the unwind path is uniform across call shapes.
    #[cfg(test)]
    pub(crate) fn do_construct<'a>(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        self.do_construct_inner(stack, context, ArgumentOperands::decoded(operands.into()))
    }

    pub(crate) fn do_construct_exec(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_construct_inner(
            stack,
            context,
            ArgumentOperands::execution(function, instruction),
        )
    }

    fn do_construct_inner(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        stack[top_idx].advance_pc()?;
        if self.try_dispatch_construct_from_window(
            stack,
            context,
            callee,
            callee_reg,
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args =
            BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc as usize)
                .to_smallvec8()?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    fn try_dispatch_construct_from_window(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        callee_reg: u16,
        operands: ArgumentOperands<'_>,
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let mut current = callee;
        let mut effective_new_target = current;
        let is_direct_class_construct = current.as_class_constructor().is_some();
        if let Some(class) = current.as_class_constructor() {
            current = class.ctor(&self.gc_heap);
        }
        if !current.is_function() && !current.is_closure() {
            return Ok(false);
        }

        self.record_runtime_construct_call();
        let function_id = current
            .as_function()
            .or_else(|| current.as_closure(&self.gc_heap).map(|c| c.function_id()));
        if function_id
            .and_then(|id| context.exec_function(id))
            .is_some_and(|function| function.is_derived_constructor)
        {
            // A derived constructor has no receiver until `super(...)`.
            // Preserve the caller's stable argument window and push the frame
            // directly: no prototype lookup, receiver allocation, or Vec copy
            // belongs at this outer boundary.
            let top_idx = stack.len() - 1;
            let frame = {
                let caller = &stack[top_idx];
                let args = BytecodeArgumentWindow::from_operands(
                    caller,
                    operands,
                    first_arg_operand,
                    argc,
                );
                self.build_construct_bytecode_frame_from_window(
                    context,
                    current,
                    None,
                    effective_new_target,
                    &args,
                    Some(dst),
                )?
            };
            stack.push(frame);
            return Ok(true);
        }

        let proto = self.construct_prototype_for_callee(stack, context, &effective_new_target)?;
        // An observable getter may have scavenged the caller's callee. Its
        // register is the canonical traced slot for this fast path; refresh
        // both values before any later use instead of trusting pre-Get locals.
        let rooted_callee = *read_register(&stack[stack.len() - 1], callee_reg)?;
        effective_new_target = rooted_callee;
        current = if let Some(class) = rooted_callee.as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            rooted_callee
        };
        // OrdinaryCreateFromConstructor — a missing or non-object
        // `prototype` falls back to %Object.prototype% (§10.1.13).
        let proto = match proto {
            Some(proto) => proto,
            None => self.constructor_prototype_value("Object")?,
        };
        // Root every local read after this allocation: a collection here
        // moves young targets, and `current` / `effective_new_target` feed the
        // frame build and the cold-frame `new_target` below — an unrooted copy
        // would wire the constructor chain to a vacated cell. `proto` is parked
        // on the traced iteration-anchor stack rather than passed as an ad-hoc
        // value root: the collector rewrites a value root through a shared
        // reference, which a stack local's register copy can outlive, so the
        // relocated prototype is read back from the anchor slot instead.
        let proto_anchor = self.push_iteration_anchor(proto) - 1;
        let receiver = self.alloc_stack_rooted_object_with_extra_roots(
            stack,
            &[&current, &effective_new_target],
        )?;
        let proto = self.iteration_anchor(proto_anchor);
        self.pop_iteration_anchors_to(proto_anchor);
        crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        // Receiver allocation can scavenge as well. Reload the dispatch values
        // from the caller register before simple-init matching or frame setup.
        let rooted_callee = *read_register(&stack[stack.len() - 1], callee_reg)?;
        effective_new_target = rooted_callee;
        current = if let Some(class) = rooted_callee.as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            rooted_callee
        };
        if is_direct_class_construct
            && let Some(function_id) = current
                .as_function()
                .or_else(|| current.as_closure(&self.gc_heap).map(|c| c.function_id()))
            && let Some(function) = context.exec_function(function_id)
        {
            let top_idx = stack.len() - 1;
            let args_window = BytecodeArgumentWindow::from_operands(
                &stack[top_idx],
                operands,
                first_arg_operand,
                argc,
            );
            let args = args_window.to_smallvec8()?;
            let init = self.simple_constructor_init(context, function_id, function);
            if self.try_finish_simple_constructor_init(
                stack,
                function_id,
                init,
                receiver,
                proto,
                args.as_slice(),
                dst,
            )? {
                return Ok(true);
            }
        }
        let top_idx = stack.len() - 1;
        let frame = {
            let caller = &stack[top_idx];
            let args =
                BytecodeArgumentWindow::from_operands(caller, operands, first_arg_operand, argc);
            self.build_construct_bytecode_frame_from_window(
                context,
                current,
                Some(receiver),
                effective_new_target,
                &args,
                Some(dst),
            )?
        };
        stack.push(frame);
        Ok(true)
    }

    fn simple_constructor_init(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        function: &CodeBlock,
    ) -> Option<crate::constructor_fast_path::SimpleConstructorInit> {
        if let Some(cached) = self.simple_constructor_init_cache.get(&function_id) {
            return cached.clone();
        }
        let init = crate::constructor_fast_path::match_simple_constructor_init(context, function);
        self.simple_constructor_init_cache
            .insert(function_id, init.clone());
        init
    }

    fn try_finish_simple_constructor_init(
        &mut self,
        stack: &mut ActivationStack,
        function_id: u32,
        init: Option<crate::constructor_fast_path::SimpleConstructorInit>,
        receiver: JsObject,
        proto: Value,
        args: &[Value],
        dst: u16,
    ) -> Result<bool, VmError> {
        let Some(init) = init else {
            return Ok(false);
        };
        let Some(proto_obj) = proto.as_object() else {
            return Ok(false);
        };
        if init.fields.iter().any(|field| {
            !matches!(
                crate::object::lookup(proto_obj, &self.gc_heap, &field.name),
                crate::object::PropertyLookup::Absent
            )
        }) {
            return Ok(false);
        }

        let values = init
            .fields
            .iter()
            .map(|field| field.source.resolve(args))
            .collect::<Vec<_>>();

        let shape = self.simple_constructor_shape(
            function_id,
            stack,
            &receiver,
            proto,
            values.as_slice(),
            &init,
        )?;

        crate::object::set_fresh_object_shape(receiver, &mut self.gc_heap, shape);
        crate::object::initialize_shaped_data_slots(receiver, &mut self.gc_heap, values.as_slice());

        let top_idx = stack.len() - 1;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::object(receiver))?;
        Ok(true)
    }

    fn simple_constructor_shape(
        &mut self,
        function_id: u32,
        stack: &ActivationStack,
        receiver: &crate::object::JsObject,
        proto: Value,
        values: &[Value],
        init: &crate::constructor_fast_path::SimpleConstructorInit,
    ) -> Result<crate::object::ShapeHandle, VmError> {
        if let Some(shape) = self.simple_constructor_shape_cache.get(&function_id) {
            return Ok(*shape);
        }

        let mut shape = self.shape_root();
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            // The receiver is not yet published to any register; the caller
            // installs the shape into it right after this call, so it must
            // survive (and follow) any collection the shape allocs trigger.
            visitor(receiver as *const crate::object::JsObject as *mut RawGc);
            proto.trace_value_slots(visitor);
            for value in values {
                value.trace_value_slots(visitor);
            }
        };
        for field in &init.fields {
            if let Some(child) = self.shape_runtime.child_if_cached(
                &self.gc_heap,
                shape,
                &field.name,
                crate::object::PropertyFlags::data_default(),
                false,
            ) {
                shape = child;
                continue;
            }
            shape = self
                .shape_runtime
                .child_with_roots(
                    &mut self.gc_heap,
                    shape,
                    &field.name,
                    crate::object::PropertyFlags::data_default(),
                    false,
                    &mut external_visit,
                )
                .map_err(VmError::from)?;
        }
        self.simple_constructor_shape_cache
            .insert(function_id, shape);
        Ok(shape)
    }

    pub(crate) fn do_construct_spread(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let args_value = *read_register(&stack[top_idx], args_reg)?;
        let Some(arr) = args_value.as_array() else {
            return Err(VmError::TypeMismatch);
        };
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(arr, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].advance_pc()?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    pub(crate) fn do_super_construct_spread(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let new_target = self
            .frame_cold(&stack[top_idx])
            .and_then(|c| c.new_target)
            .unwrap_or(callee);
        let args_value = *read_register(&stack[top_idx], args_reg)?;
        let Some(arr) = args_value.as_array() else {
            return Err(VmError::TypeMismatch);
        };
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(arr, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].advance_pc()?;
        self.dispatch_construct_with_new_target(stack, context, callee, new_target, args, dst)
    }

    pub(crate) fn dispatch_construct(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.dispatch_construct_with_new_target(stack, context, callee, callee, args, dst)
    }

    fn dispatch_construct_with_new_target(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.record_runtime_construct_call();
        let roots = SyncJsCallRoots::construct(callee, new_target, args);
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = roots.target().as_bound_function() {
                hops += 1;
                let current = roots.target();
                let effective_new_target = roots.new_target.get();
                let (target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
                roots.prepend_args(bound_args.as_slice());
                if abstract_ops::same_value(&current, &effective_new_target, &self.gc_heap) {
                    roots.new_target.set(target);
                }
                roots.current.set(target);
                continue;
            }

            // §28.2.4.14 Proxy.[[Construct]]. Keep the proxy, handler,
            // new.target, and arguments in the registered slots through the
            // observable trap lookup. A missing trap continues this same
            // direct dispatch loop instead of synchronously executing a
            // bytecode target.
            if roots.target().as_proxy().is_some() {
                hops += 1;
                let proxy = roots
                    .target()
                    .as_proxy()
                    .expect("checked direct construct proxy");
                if proxy.is_revoked(&self.gc_heap) {
                    return Err(self.err_type(
                        ("Cannot perform 'construct' on a revoked proxy".to_string()).into(),
                    ));
                }
                // Proxy.[[Construct]] captures [[ProxyTarget]] before the
                // observable GetMethod(handler, "construct"). The getter may
                // revoke the proxy; both trap dispatch and the missing-trap
                // fallback must still use this rooted pre-Get target.
                roots.proxy_target.set(proxy.target(&self.gc_heap));
                roots.scratch_0.set(proxy.handler(&self.gc_heap));
                let trap_key = VmPropertyKey::String("construct");
                let trap_value = {
                    let handler = roots.scratch_0.get();
                    match self.ordinary_get_value(stack, context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            let handler = roots.scratch_0.get();
                            self.run_callable_sync_rooted(
                                stack,
                                context,
                                &getter,
                                handler,
                                SmallVec::new(),
                            )?
                        }
                    }
                };
                roots.scratch_1.set(trap_value);
                if self.is_callable_runtime(&roots.scratch_1.get()) {
                    // Move only at the root-aware array allocation boundary.
                    // Its slice visitor rewrites this exact owned buffer; park
                    // it back in the canonical slot before the observable
                    // trap call.
                    let effective_args = roots.take_args();
                    let current = roots.target();
                    let effective_new_target = roots.new_target.get();
                    let handler = roots.scratch_0.get();
                    let trap = roots.scratch_1.get();
                    let argv_array = self.alloc_stack_rooted_array_from_values(
                        stack,
                        effective_args.iter().copied(),
                        &[&current, &effective_new_target, &handler, &trap],
                        effective_args.as_slice(),
                    )?;
                    roots.replace_args(effective_args);
                    // Reload every value used after the allocation from its
                    // collector-rewritten slot.
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        roots.proxy_target.get(),
                        Value::array(argv_array),
                        roots.new_target.get(),
                    ];
                    let result = self.run_callable_sync_rooted(
                        stack,
                        context,
                        &roots.scratch_1.get(),
                        roots.scratch_0.get(),
                        trap_args,
                    )?;
                    if !result.is_object_type() {
                        return Err(self.err_type(
                            ("Proxy construct trap returned non-object".to_string()).into(),
                        ));
                    }
                    let top_idx = stack.len() - 1;
                    write_register(&mut stack[top_idx], dst, result)?;
                    return Ok(());
                }
                if roots.scratch_1.get().is_nullish() {
                    let target = roots.proxy_target.get();
                    roots.current.set(target);
                    roots.proxy_target.set(Value::undefined());
                    roots.scratch_0.set(Value::undefined());
                    roots.scratch_1.set(Value::undefined());
                    continue;
                }
                return Err(
                    self.err_type(("Proxy construct trap is not callable".to_string()).into())
                );
            }
            break;
        }

        // A derived bytecode constructor creates no receiver until its
        // `super(...)` call. In particular, do not observe
        // `new.target.prototype` or allocate a throwaway object at this outer
        // boundary; the direct super-construct path below will perform the
        // one OrdinaryCreateFromConstructor operation with the same rooted
        // new.target.
        let bytecode_callee = if let Some(class) = roots.target().as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            roots.target()
        };
        if let Ok((function_id, _)) =
            Self::bytecode_construct_target_parts(bytecode_callee, &self.gc_heap)
            && context
                .exec_function(function_id)
                .is_some_and(|function| function.is_derived_constructor)
        {
            let new_target = roots.new_target.get();
            let args = roots.take_args();
            let frame = self.build_construct_bytecode_frame(
                context,
                bytecode_callee,
                None,
                new_target,
                args,
                Some(dst),
            )?;
            stack.push(frame);
            return Ok(());
        }

        if let Some(native) = self.native_promise_constructor(&roots.target()) {
            let new_target = roots.new_target.get();
            let args = roots.take_args();
            let constructed = self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &Value::undefined(),
                &new_target,
                false,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        // Allocate receiver and link its prototype before pushing
        // the new frame. The constructor might mutate the receiver
        // immediately, so the prototype link must already be in
        // place.
        let new_target = roots.new_target.get();
        let proto = self.construct_prototype_for_callee(stack, context, &new_target)?;
        let used_object_prototype_fallback = proto.is_none();
        // OrdinaryCreateFromConstructor — a missing or non-object
        // `prototype` falls back to %Object.prototype% (§10.1.13).
        let proto = match proto {
            Some(proto) => proto,
            None => self.constructor_prototype_value("Object")?,
        };
        roots.scratch_0.set(proto);
        let receiver = self.alloc_stack_rooted_object_with_extra_roots(stack, &[])?;
        roots.receiver.set(Value::object(receiver));
        let proto = roots.scratch_0.get();
        crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        // Built-in constructor objects (`Number`, `Boolean`, …)
        // surface as a `Value::Object` with an internal native
        // constructor slot. Promote to the native-function construct
        // path so the JS-visible callee can also carry own
        // properties (statics + `prototype`) without leaking the
        // implementation slot through reflection.
        if let Some(obj) = roots.target().as_object()
            && let Some(native) = crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())
        {
            let this_value = roots.receiver.get();
            let new_target = roots.new_target.get();
            let args = roots.take_args();
            let constructed = self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &this_value,
                &new_target,
                used_object_prototype_fallback,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        // `Value::NativeFunction` carries `[[Construct]]` whenever
        // the runtime needs the callable to behave as a constructor
        // (e.g. `new Number(x)`). The native callback inspects
        // `NativeCtx::is_construct_call()` to differentiate the
        // call shape.
        if let Some(native) = roots.target().as_native_function() {
            let this_value = roots.receiver.get();
            let new_target = roots.new_target.get();
            let args = roots.take_args();
            let constructed = self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &this_value,
                &new_target,
                used_object_prototype_fallback,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        if let Some(class) = roots.target().as_class_constructor()
            && let Some(native) = class.ctor(&self.gc_heap).as_native_function()
        {
            let this_value = roots.receiver.get();
            let new_target = roots.new_target.get();
            let args = roots.take_args();
            let constructed = self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &this_value,
                &new_target,
                used_object_prototype_fallback,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        let bytecode_callee = if let Some(class) = roots.target().as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            roots.target()
        };
        if bytecode_callee.is_function() || bytecode_callee.is_closure() {
            let receiver = roots
                .receiver
                .get()
                .as_object()
                .ok_or(VmError::TypeMismatch)?;
            let new_target = roots.new_target.get();
            let args = roots.take_args();
            let frame = self.build_construct_bytecode_frame(
                context,
                bytecode_callee,
                Some(receiver),
                new_target,
                args,
                Some(dst),
            )?;
            stack.push(frame);
            return Ok(());
        }
        let callee = roots.target();
        let this_value = roots.receiver.get();
        let args = roots.take_args();
        self.invoke(stack, context, &callee, this_value, args, dst)?;
        // The pushed frame is now on top; mark it so `pop_frame`
        // can substitute the receiver for any non-object return.
        if let Some(top) = stack.last_mut() {
            let cold = self.frame_ensure_cold(top);
            cold.construct_target = roots.receiver.get().as_object();
            cold.new_target = Some(roots.new_target.get());
        }
        Ok(())
    }

    pub(crate) fn construct_prototype_for_callee(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
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
                });
        }
        Ok(None)
    }

    fn construct_prototype_via_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let callee_anchor = self.push_iteration_anchor(*callee) - 1;
        let anchor_base = callee_anchor;
        let result = (|| -> Result<Option<Value>, VmError> {
            let key = VmPropertyKey::String("prototype");
            let callee = self.iteration_anchor(callee_anchor);
            let proto = match self.ordinary_get_value(stack, context, callee, callee, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    let callee = self.iteration_anchor(callee_anchor);
                    self.run_callable_sync_rooted(stack, context, &getter, callee, SmallVec::new())?
                }
            };
            // The getter may have collected or revoked a Proxy. Reload the
            // callee from the anchor before consulting its post-Get state.
            let revoked_proxy = self
                .iteration_anchor(callee_anchor)
                .as_proxy()
                .is_some_and(|proxy| proxy.is_revoked(&self.gc_heap));
            if !proto.is_object_type() && revoked_proxy {
                return Err(
                    self.err_type(("Cannot get prototype from a revoked proxy".to_string()).into())
                );
            }
            Ok(proto.is_object_type().then_some(proto))
        })();
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    fn native_promise_constructor(&self, callee: &Value) -> Option<NativeFunction> {
        let native = if let Some(native) = callee.as_native_function() {
            native
        } else if let Some(obj) = callee.as_object() {
            crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())?
        } else {
            return None;
        };
        (native.name(&self.gc_heap) == "Promise").then_some(native)
    }

    /// Handle `Op::CallSpread`: read the args array, fan it out
    /// into the standard call path. The receiver register holds
    /// the explicit `this` value (foundation lowers free spread
    /// calls with `this = undefined`).
    pub(crate) fn do_call_spread(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        let args_array = read_register(&stack[top_idx], args_reg)?
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(args_array, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].advance_pc()?;
        self.invoke(stack, context, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallWithThis`: same as `do_call` but the call
    /// site supplies an explicit `this` register. Used by
    /// `Function.prototype.call` lowering and the array-literal
    /// path of `Function.prototype.apply`.
    pub(crate) fn do_call_with_this_exec(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_call_with_this_inner(
            stack,
            context,
            ArgumentOperands::execution(function, instruction),
        )
    }

    fn do_call_with_this_inner(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let this_reg = operands.register(2)?;
        let argc = operands.const_index(3)?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        stack[top_idx].advance_pc()?;
        if self.try_push_bytecode_call_frame_from_window(
            stack,
            context,
            &callee,
            this_value,
            operands,
            4,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        if self.try_invoke_native_call_from_window(
            stack,
            context,
            &callee,
            this_value,
            operands,
            4,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args =
            BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 4, argc as usize)
                .to_smallvec8()?;
        self.invoke(stack, context, &callee, this_value, args, dst)
    }
    /// Synchronously invoke `callee(args)` with the given `this` and
    /// return the completion value.
    ///
    /// # Algorithm
    /// 1. NativeFunction callees run inline — the foundation native
    ///    surface is `Fn`, so calling them here is just a function
    ///    pointer hop with `&mut self` access.
    /// 2. BoundFunction layers are unwrapped iteratively, prepending
    ///    bound args and replacing `this_value` with `bound_this`.
    /// 3. Bytecode / closure callees push a frame whose
    ///    `return_register` is `None`, which makes
    ///    [`Self::dispatch_loop`] return the completion value when
    ///    the frame pops.
    ///
    /// Used by collection `forEach` and other host-driven iteration
    /// helpers.
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
            interp.run_callable_sync_rooted(stack, context, callee, this_value, args)
        })
    }

    /// Synchronously invoke a callable above the current activation floor.
    ///
    /// The exact stack is already published by the enclosing [`RuntimeTurn`].
    /// Nested execution appends to it and releases back to the captured floor;
    /// it never installs another frame provider or draws a detached stack.
    pub(crate) fn run_callable_sync_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        self.enter_sync_reentry()?;
        let floor = stack.floor();
        let roots = SyncJsCallRoots::call(*callee, this_value, args);
        let roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let result = self.run_callable_sync_inner_rooted(stack, context, &roots);
        drop(roots_guard);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    fn run_callable_sync_inner_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        roots: &SyncJsCallRoots,
    ) -> Result<Value, VmError> {
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = roots.current.get().as_bound_function() {
                hops += 1;
                let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                let effective_args = roots.take_args();
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                roots.receiver.set(bound_this);
                roots.replace_args(combined);
                roots.current.set(target);
            } else if roots.current.get().as_class_constructor().is_some() {
                // §10.3.1 — class constructors reject [[Call]].
                return Err(self.err_type(
                    ("Class constructor cannot be invoked without 'new'".to_string()).into(),
                ));
            } else if let Some(proxy) = roots.current.get().as_proxy() {
                // §10.5.12 Proxy [[Call]] — dispatch `apply` trap or
                // fall through to target.[[Call]] when the trap is
                // absent.
                if proxy.is_revoked(&self.gc_heap) {
                    return Err(self.err_type(
                        ("Cannot perform 'apply' on a proxy that has been revoked".to_string())
                            .into(),
                    ));
                }
                hops += 1;
                // §10.5.12 captures [[ProxyTarget]] before observable
                // GetMethod(handler, "apply"), just like [[Construct]] below.
                roots.proxy_target.set(proxy.target(&self.gc_heap));
                roots.scratch_0.set(proxy.handler(&self.gc_heap));
                let trap_key = VmPropertyKey::String("apply");
                let trap_value = {
                    let handler = roots.scratch_0.get();
                    match self.ordinary_get_value(stack, context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            let handler = roots.scratch_0.get();
                            self.run_callable_sync_rooted(
                                stack,
                                context,
                                &getter,
                                handler,
                                SmallVec::new(),
                            )?
                        }
                    }
                };
                roots.scratch_1.set(trap_value);
                if self.is_callable_runtime(&trap_value) {
                    let effective_args = roots.take_args();
                    let current = roots.current.get();
                    let effective_this = roots.receiver.get();
                    let handler = roots.scratch_0.get();
                    let trap_value = roots.scratch_1.get();
                    let argv_array = self.alloc_runtime_rooted_array_from_values(
                        effective_args.iter().copied(),
                        &[&current, &effective_this, &handler, &trap_value],
                        &[effective_args.as_slice()],
                    )?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        roots.proxy_target.get(),
                        roots.receiver.get(),
                        Value::array(argv_array),
                    ];
                    return self.run_callable_sync_rooted(
                        stack,
                        context,
                        &roots.scratch_1.get(),
                        roots.scratch_0.get(),
                        trap_args,
                    );
                } else if trap_value.is_undefined() || trap_value.is_null() {
                    roots.current.set(roots.proxy_target.get());
                    roots.proxy_target.set(Value::undefined());
                } else {
                    return Err(
                        self.err_type(("Proxy apply trap is not callable".to_string()).into())
                    );
                }
            } else {
                break;
            }
        }
        if let Some(obj) = roots.current.get().as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let current = roots.current.get();
            let receiver = roots.receiver.get();
            let args = roots.take_args();
            return invoke_native_call_with_roots(
                self,
                stack,
                context,
                call,
                realm_global,
                receiver,
                &[&current],
                args.as_slice(),
            );
        }
        if let Some(native) = roots.current.get().as_native_function() {
            let native = &native;
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                let receiver = roots.receiver.get();
                let args = roots.take_args();
                return self
                    .run_vm_intrinsic_sync_rooted(stack, context, intrinsic, receiver, args);
            }
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let current = roots.current.get();
            let receiver = roots.receiver.get();
            let args = roots.take_args();
            return invoke_native_call_with_roots(
                self,
                stack,
                context,
                call,
                realm_global,
                receiver,
                &[&current],
                args.as_slice(),
            );
        }
        self.run_bytecode_callable_committed_rooted(stack, context, roots)
    }

    /// Build and run the bytecode-callable frame for `current` on `inner` — the
    /// committed tail of [`Self::run_callable_sync_inner`] once the
    /// bound/proxy/native dispatch loop has resolved to a plain bytecode
    /// function/closure.
    ///
    /// Extracted (not duplicated) so a hot native callback loop can reuse one
    /// prepared callback state across elements instead of drawing/returning a
    /// pooled stack, resolving closure metadata, and walking bound/proxy/native
    /// wrappers per call. The state owns its reservation-stable stack; this
    /// method leaves that stack empty on every completion path (the callee frame
    /// is popped), so it is immediately reusable for the next call.
    /// If `callback` is a plain bytecode function/closure eligible for the lean
    /// per-element invoke path (not native/bound/proxy/generator/async/…), enter
    /// the sync-reentry guard once, draw one reservation-stable stack, and push
    /// the resolved callback metadata onto the interpreter's traced root stack;
    /// otherwise `None`. The caller invokes the callback with the returned
    /// state per element, and MUST pass the state to
    /// [`Self::release_lean_callback_stack`] on every completion path.
    /// `function_id` is shape-stable, so resolving eligibility once is valid for
    /// the whole loop.
    pub(crate) fn acquire_lean_callback_stack(
        &mut self,
        context: &ExecutionContext,
        callback: Value,
    ) -> Option<LeanCallbackState> {
        let root = LeanCallbackRoot::from_callback(callback, &self.gc_heap)?;
        let function = context.exec_function(root.function_id).filter(|f| {
            !f.is_generator
                && !f.is_async
                && !f.is_async_generator
                && !f.needs_arguments
                && !f.has_rest
                && !f.makes_function
                && !f.contains_direct_eval
                && !f.is_derived_constructor
        })?;
        let register_count = function.register_count as usize;
        let param_count = function.param_count as usize;
        let this_passthrough = function.is_strict || function.is_arrow;
        let has_parent_upvalues = !root.upvalue_source(&self.gc_heap).ok()?.is_empty();
        // A callback with no bound `new.target` / derived-`this` cell / captured
        // eval environment needs no pooled cold record, so its per-element frame
        // is a flat register window the prepared fast path can recycle in place.
        let fast_reuse = root.bound_new_target.is_none()
            && root.bound_derived_this.is_none()
            && root.eval_env.is_none();
        if self.enter_sync_reentry().is_ok() {
            let root_index = self.lean_callback_roots.len();
            let function_id = root.function_id;
            self.lean_callback_roots.push(root);
            Some(LeanCallbackState {
                root_index,
                function_id,
                register_count,
                param_count,
                this_passthrough,
                has_parent_upvalues,
                fast_reuse,
                compiled: None,
                reuse_frame: None,
            })
        } else {
            None
        }
    }

    /// Release the lean-path state and leave the sync-reentry guard entered by
    /// [`Self::acquire_lean_callback_stack`]. No-op for `None`.
    pub(crate) fn release_lean_callback_stack(&mut self, state: Option<LeanCallbackState>) {
        if let Some(mut state) = state {
            // Return the recycled frame's spilled register backing to the pool;
            // an inline window is dropped with the frame.
            if let Some(mut frame) = state.reuse_frame.take() {
                self.frame_release_cold(&mut frame);
                self.reclaim_registers(&mut frame);
            }
            let root = self
                .lean_callback_roots
                .pop()
                .expect("lean callback root stack underflow");
            debug_assert_eq!(
                root.function_id, state.function_id,
                "lean callback roots must release in LIFO order"
            );
            self.leave_sync_reentry();
        }
    }

    fn run_bytecode_callable_committed_rooted(
        &mut self,
        inner: &mut ActivationStack,
        context: &ExecutionContext,
        roots: &SyncJsCallRoots,
    ) -> Result<Value, VmError> {
        let (function_id, _, this_for_callee, _, _, _, _) = Self::bytecode_call_target_parts(
            roots.target(),
            roots.receiver_value(),
            &self.gc_heap,
        )?;
        roots.set_receiver(this_for_callee);
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // §27.7.5.1 async-call entry for the synchronous re-entry path — a
        // builtin invoking a user async function (e.g. the `Otter.serve`
        // dispatch calling the fetch handler). Mirror the opcode call path:
        // synthesise the pending result promise now and park the frame with
        // `async_state` so its completion settles that promise. Without this
        // an async callee runs as a plain frame and its first `Op::Await`
        // finds a frame with no `async_state`, which `do_await` reports as
        // `VmError::InvalidOperand`. The promise is allocated before the
        // upvalue spine and receiver coercion (which also allocate), rooting
        // the raw receiver and arguments exactly as the opcode path does.
        let is_plain_async = function.is_async && !function.is_generator;
        let async_result_promise = if is_plain_async {
            Some(
                promise_dispatch::PromiseBuilder::with_context(context.clone())
                    .pending_stack_rooted(self, inner, &[], &[])?,
            )
        } else {
            None
        };
        // The promise allocation above can relocate closure-owned cells. Read
        // the target parts again from the registered target instead of using a
        // detached pre-allocation snapshot.
        let (_, _, this_for_callee, _, _, _, _) = Self::bytecode_call_target_parts(
            roots.target(),
            roots.receiver_value(),
            &self.gc_heap,
        )?;
        roots.set_receiver(this_for_callee);
        let this_for_callee =
            self.this_for_bytecode_call_runtime_rooted(function, roots.receiver_value(), &[])?;
        roots.set_receiver(this_for_callee);
        let (_, parent_upvalues, _, _, _, _, _) = Self::bytecode_call_target_parts(
            roots.target(),
            roots.receiver_value(),
            &self.gc_heap,
        )?;
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        // Building own upvalue cells can collect as well; refresh every
        // closure-owned optional handle before installing it on the frame.
        let (_, _, _, new_target_for_callee, derived_this_cell, callee_eval_env, _) =
            Self::bytecode_call_target_parts(
                roots.target(),
                roots.receiver_value(),
                &self.gc_heap,
            )?;
        let _window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues,
            roots.receiver_value(),
            window,
        );
        new_frame.self_value = roots.target();
        if let Some(result_promise) = async_result_promise {
            self.frame_set_async_state(
                &mut new_frame,
                crate::frame_state::AsyncFrameState { result_promise },
            );
        }
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut new_frame, callee_eval_env)?;
        self.bind_bytecode_call_arguments(function, &mut new_frame, roots.take_args())?;
        // §27.5.1 GeneratorFunction call evaluation returns a
        // generator object without executing the body. `invoke`
        // handles this for opcode calls; the synchronous re-entry
        // helper must mirror it for builtins that call user
        // functions, such as `GetSetRecord(...).[[Keys]]`.
        // <https://tc39.es/ecma262/#sec-generatorfunction-objects>
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let cold = self.frame_detach_cold(&mut new_frame);
            let new_frame = self.park_active_frame(new_frame);
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                new_frame,
                cold,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            roots.set_scratch(0, Value::generator(gen_handle));
            // §27.5 — run the generator prologue (mirroring the opcode
            // `invoke` path) so the handle is primed to its
            // suspended-start state. Without it a generator created
            // through a builtin's synchronous re-entry (e.g. an
            // `@@iterator` that is a generator function, driven by
            // `Array.from` / `GetSetRecord` / the iterator helpers) is
            // never started and reports `done` on its first `next`.
            let result = (|| -> Result<Value, VmError> {
                let gen_handle = roots
                    .scratch(0)
                    .as_generator()
                    .ok_or(VmError::InvalidOperand)?;
                let (frame, cold) = gen_handle
                    .take_frame(&mut self.gc_heap)
                    .ok_or(VmError::InvalidOperand)?;
                let mut frame = self.resume_parked_frame(*frame)?;
                if let Some(cold) = cold {
                    self.frame_attach_cold(&mut frame, cold);
                }
                let prologue_floor = inner.floor();
                inner.push(frame);
                let prologue = self.dispatch_loop_above_rooted(context, inner, prologue_floor);
                self.release_frames_above(inner, prologue_floor);
                prologue?;
                // §27.5.1 step 3 — resolve [[Prototype]] after the
                // prologue (FunctionDeclarationInstantiation) ran, through
                // the invoked closure's bag so the generator's prototype is
                // the same object later `fn.prototype` reads observe.
                let proto = self.function_property_get(
                    inner,
                    context,
                    roots.target().as_closure(&self.gc_heap),
                    function_id,
                    "prototype",
                )?;
                let gen_handle = roots
                    .scratch(0)
                    .as_generator()
                    .ok_or(VmError::InvalidOperand)?;
                gen_handle.set_prototype_override(
                    &mut self.gc_heap,
                    proto.as_object().is_some().then_some(proto),
                );
                Ok(roots.scratch(0))
            })();
            roots.set_scratch(0, Value::undefined());
            return result;
        }
        // The caller owns `inner` (a reservation-stable pooled stack): the entry
        // frame may tier up and run compiled, and a compiled callee appends its
        // frame directly onto this stack, so it must never reallocate. The frame
        // is popped on every completion path, leaving `inner` empty + reusable.
        let entry_floor = inner.floor();
        inner.push(new_frame);
        if let Some(result_promise) = async_result_promise {
            // The async frame runs to its first `await` (which parks it off
            // `inner`) or to completion (which settles the promise and pops
            // the frame); either way `inner` is left empty and the dispatch
            // loop returns. This call's value is the result promise, not the
            // loop's terminal frame value. A suspending frame must never take
            // the compiled sync-entry path, whose fast tier assumes the entry
            // frame cannot suspend.
            self.dispatch_loop_above_rooted(context, inner, entry_floor)?;
            return Ok(Value::promise(result_promise));
        }
        // Tier-up the entry frame itself: a synchronously-entered callee reaches
        // `dispatch_loop` directly (no `Op::Call`), so without this hook the
        // entry level would always interpret while only its sub-calls JIT. This
        // lets a hot recursion run compiled→compiled with no interpreted levels.
        if let Some(value) = self.dispatch_jit_sync_entry(inner, context)? {
            // The compiled entry frame ran to completion and is terminal
            // (the integer subset cannot suspend or escape its frame), so
            // return its spilled register window to the pool.
            if let Some(mut done) = inner.pop() {
                self.reclaim_registers(&mut done);
            }
            return Ok(value);
        }
        self.dispatch_loop_above_rooted(context, inner, entry_floor)
    }

    pub(crate) fn run_bytecode_callable_committed_lean_args(
        &mut self,
        stack: &mut ActivationStack,
        state: &mut LeanCallbackState,
        context: &ExecutionContext,
        effective_this: Value,
        effective_args: &[Value],
    ) -> Result<Value, VmError> {
        // Prepared fast path: a callback that needs no pooled cold record
        // resolves its compiled body once and then re-enters that body with a
        // recycled frame, so per element only the receiver coercion (cached),
        // the upvalue refresh, the register reset, and the argument bind run —
        // no per-element frame allocation, register draw, tier probe, or
        // dispatch envelope.
        if state.fast_reuse {
            let already_compiled = state.compiled.is_some();
            if state.compiled.is_none() {
                state.compiled = self.resolve_jit_code_for_fid(context, state.function_id);
            }
            if let Some(code) = state.compiled.clone() {
                if already_compiled {
                    self.note_jit_function_entry(state.function_id);
                }
                return self.invoke_prepared_lean(
                    stack,
                    state,
                    context,
                    &code,
                    effective_this,
                    effective_args,
                );
            }
            // Still cold (below the tier-up threshold). The probe above already
            // advanced the counter, so interpret this element directly without
            // re-probing through the synchronous-entry path.
            return self.invoke_cold_lean(
                stack,
                state,
                context,
                effective_this,
                effective_args,
                false,
            );
        }
        // Callback carries a bound `new.target` / derived-`this` cell / captured
        // eval environment: build a fresh frame per element and tier up through
        // the synchronous-entry path as before.
        self.invoke_cold_lean(stack, state, context, effective_this, effective_args, true)
    }

    /// Re-enter the callback's already-compiled body with the recycled frame
    /// held in `state`. See [`Self::run_bytecode_callable_committed_lean_args`].
    fn invoke_prepared_lean(
        &mut self,
        stack: &mut ActivationStack,
        state: &mut LeanCallbackState,
        context: &ExecutionContext,
        code: &std::sync::Arc<dyn crate::jit::JitFunctionCode>,
        effective_this: Value,
        effective_args: &[Value],
    ) -> Result<Value, VmError> {
        let window_rollback = self.register_window_rollback();
        // Refresh the GC-live inputs from the traced root every element: the
        // recycled frame is held off-stack between calls and is not itself
        // traced, so its captured upvalues / receiver could be relocated by a
        // moving collection the builtin triggers between elements. Reading them
        // back from `lean_callback_roots` (which IS traced) keeps them current.
        let bound_this = {
            let root = self
                .lean_callback_roots
                .get(state.root_index)
                .ok_or(VmError::InvalidOperand)?;
            root.bound_this.unwrap_or(effective_this)
        };
        let this_for_callee = if state.this_passthrough {
            bound_this
        } else {
            // OrdinaryCallBindThis performs ToObject for every sloppy call.
            // Reusing a wrapper would be observably wrong (`this` identity
            // must differ between callback invocations) and would retain an
            // untraced young handle in the lean state across moving GC.
            let function = context
                .exec_function(state.function_id)
                .ok_or(VmError::InvalidOperand)?;
            self.this_for_bytecode_call_runtime_rooted(function, bound_this, &[effective_args])?
        };
        let register_count = state.register_count;
        // Receiver coercion may collect. Re-read both exact SELF and the stable
        // source from the traced root afterwards; no cloned spine is parked in
        // the root stack.
        let (self_value, parent_upvalues) = {
            let root = self
                .lean_callback_roots
                .get(state.root_index)
                .ok_or(VmError::InvalidOperand)?;
            (root.callback, root.upvalue_source(&self.gc_heap)?)
        };
        let mut frame = match state.reuse_frame.take() {
            Some(mut frame) => {
                debug_assert_eq!(frame.registers.len(), register_count);
                frame.pc = 0;
                frame.return_register = None;
                frame.self_value = self_value;
                frame.this_value = this_for_callee;
                if state.has_parent_upvalues
                    && let Err(error) = parent_upvalues.copy_into(frame.upvalues.as_mut())
                {
                    self.frame_release_cold(&mut frame);
                    self.reclaim_registers(&mut frame);
                    return Err(error);
                }
                frame
            }
            None => {
                let function = context
                    .exec_function(state.function_id)
                    .ok_or(VmError::InvalidOperand)?;
                debug_assert_eq!(function.own_upvalue_count, 0);
                let window = self.alloc_reg_window(register_count)?;
                let mut frame = Frame::with_exec_return_upvalues_and_this(
                    function,
                    None,
                    parent_upvalues.copy_owned(),
                    this_for_callee,
                    window,
                );
                frame.self_value = self_value;
                frame
            }
        };
        if let Err(error) = Self::reset_and_bind_lean_bytecode_call_arguments(
            state.param_count,
            &mut frame,
            effective_args,
        ) {
            self.reclaim_registers(&mut frame);
            return Err(error);
        }
        let entry_floor = stack.floor();
        stack.push(frame);
        let top_idx = stack.len() - 1;
        let outcome = self
            .run_optimized_frame(stack, context, top_idx)
            .unwrap_or_else(|| self.run_compiled_frame(stack, context, top_idx, code));
        match outcome {
            crate::jit::JitExecOutcome::Returned(value) => {
                // The body ran to completion and left its frame on the stack;
                // recycle that frame (window + shell) for the next element.
                if stack.len_above(entry_floor) != 1 {
                    self.release_frames_above(stack, entry_floor);
                    return Err(VmError::InvalidOperand);
                }
                state.reuse_frame = stack.pop();
                window_rollback.commit();
                Ok(value)
            }
            crate::jit::JitExecOutcome::Bailed(pc) => {
                // Finish the partially-run frame in the interpreter. A bailout
                // consumes the recyclable frame; the next element rebuilds it.
                stack[top_idx].pc = pc;
                let result = self.dispatch_loop_above_rooted(context, stack, entry_floor);
                self.release_frames_above(stack, entry_floor);
                result
            }
            crate::jit::JitExecOutcome::Threw(err) => {
                self.release_frames_above(stack, entry_floor);
                Err(err)
            }
        }
    }

    /// Build a fresh callee frame for one lean-callback element. `probe` drives
    /// tier-up through the synchronous-entry path (for callbacks not eligible
    /// for the prepared fast path); when `false` the element is interpreted
    /// directly because the caller already advanced the tier-up counter.
    fn invoke_cold_lean(
        &mut self,
        stack: &mut ActivationStack,
        state: &mut LeanCallbackState,
        context: &ExecutionContext,
        effective_this: Value,
        effective_args: &[Value],
        probe: bool,
    ) -> Result<Value, VmError> {
        let (function_id, this_for_callee) = {
            let root = self
                .lean_callback_roots
                .get(state.root_index)
                .ok_or(VmError::InvalidOperand)?;
            (root.function_id, root.bound_this.unwrap_or(effective_this))
        };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        debug_assert!(
            !function.is_generator
                && !function.is_async
                && !function.is_async_generator
                && !function.needs_arguments
                && !function.has_rest
                && !function.makes_function
                && !function.contains_direct_eval
                && !function.is_derived_constructor,
            "lean callback eligibility must be checked before the loop"
        );
        let this_for_callee = self.this_for_bytecode_call_runtime_rooted(
            function,
            this_for_callee,
            &[effective_args],
        )?;
        // Coercion may collect; reload every closure-derived value and source
        // from the traced root before allocating fresh cells/materializing.
        let (
            mut self_value,
            parent_upvalues,
            new_target_for_callee,
            mut derived_this_cell,
            mut callee_eval_env,
        ) = {
            let root = self
                .lean_callback_roots
                .get(state.root_index)
                .ok_or(VmError::InvalidOperand)?;
            (
                root.callback,
                root.upvalue_source(&self.gc_heap)?,
                root.bound_new_target,
                root.bound_derived_this,
                root.eval_env,
            )
        };
        let mut rooted_this = this_for_callee;
        let mut rooted_new_target = new_target_for_callee;
        let mut build_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            self_value.trace_value_slot_mut(visitor);
            rooted_this.trace_value_slot_mut(visitor);
            if let Some(value) = &mut rooted_new_target {
                value.trace_value_slot_mut(visitor);
            }
            if let Some(cell) = &mut derived_this_cell {
                visitor(cell as *mut crate::UpvalueCell as *mut RawGc);
            }
            if let Some(env) = &mut callee_eval_env {
                visitor(env as *mut crate::eval_env::EvalEnvHandle as *mut RawGc);
            }
            // Keep the caller-owned argument window as the one authoritative
            // set of slots. Upvalue-cell allocation may move young values, so
            // the collector must rewrite the exact slice that the binder reads
            // below instead of a detached snapshot.
            for value in effective_args {
                value.trace_value_slots(visitor);
            }
        };
        let upvalues = Frame::build_upvalues_for_exec_from_source_with_roots(
            &mut self.gc_heap,
            function,
            parent_upvalues,
            &mut build_roots,
        )?;
        let this_for_callee = rooted_this;
        let new_target_for_callee = rooted_new_target;
        let _window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues,
            this_for_callee,
            window,
        );
        new_frame.self_value = self_value;
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.derived_this_cell = Some(cell);
        }
        let setup = self
            .stash_frame_eval_env(function, &mut new_frame, callee_eval_env)
            .and_then(|()| {
                // `effective_args` is the same collector-rewritten window
                // traced by `build_roots`; read it only after the allocating
                // upvalue build has completed.
                Self::bind_lean_bytecode_call_arguments(function, &mut new_frame, effective_args)
            });
        if let Err(error) = setup {
            self.frame_release_cold(&mut new_frame);
            self.reclaim_registers(&mut new_frame);
            return Err(error);
        }
        let entry_floor = stack.floor();
        stack.push(new_frame);
        if probe {
            match self.dispatch_jit_sync_entry(stack, context) {
                Ok(Some(value)) => {
                    self.release_frames_above(stack, entry_floor);
                    return Ok(value);
                }
                Ok(None) => {}
                Err(err) => {
                    self.release_frames_above(stack, entry_floor);
                    return Err(err);
                }
            }
        }
        let result = self.dispatch_loop_above_rooted(context, stack, entry_floor);
        self.release_frames_above(stack, entry_floor);
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
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        self.enter_sync_reentry()?;
        let floor = stack.floor();
        let roots = SyncJsCallRoots::construct(*target, new_target, args);
        let roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let result = self.run_construct_sync_inner_rooted(stack, context, &roots);
        drop(roots_guard);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    fn run_construct_sync_inner_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        roots: &SyncJsCallRoots,
    ) -> Result<Value, VmError> {
        self.record_runtime_construct_call();
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = roots.current.get().as_bound_function() {
                hops += 1;
                let (next_target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
                let effective_args = roots.take_args();
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                if abstract_ops::same_value(
                    &roots.current.get(),
                    &roots.new_target.get(),
                    &self.gc_heap,
                ) {
                    roots.new_target.set(next_target);
                }
                roots.current.set(next_target);
                roots.replace_args(combined);
            } else if let Some(proxy) = roots.current.get().as_proxy() {
                // §10.5.13 Proxy [[Construct]].
                if proxy.is_revoked(&self.gc_heap) {
                    return Err(self.err_type(
                        ("Cannot perform 'construct' on a proxy that has been revoked".to_string())
                            .into(),
                    ));
                }
                hops += 1;
                roots.proxy_target.set(proxy.target(&self.gc_heap));
                roots.scratch_0.set(proxy.handler(&self.gc_heap));
                let trap_key = VmPropertyKey::String("construct");
                let trap_value = {
                    let handler = roots.scratch_0.get();
                    match self.ordinary_get_value(stack, context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            let handler = roots.scratch_0.get();
                            self.run_callable_sync_rooted(
                                stack,
                                context,
                                &getter,
                                handler,
                                SmallVec::new(),
                            )?
                        }
                    }
                };
                roots.scratch_1.set(trap_value);
                if self.is_callable_runtime(&trap_value) {
                    let current = roots.current.get();
                    let target_value = roots.proxy_target.get();
                    let effective_new_target = roots.new_target.get();
                    let handler = roots.scratch_0.get();
                    let trap_value = roots.scratch_1.get();
                    let effective_args = roots.take_args();
                    let argv_array = self.alloc_runtime_rooted_array_from_values(
                        effective_args.iter().copied(),
                        &[
                            &current,
                            &target_value,
                            &effective_new_target,
                            &handler,
                            &trap_value,
                        ],
                        &[effective_args.as_slice()],
                    )?;
                    // The allocation may relocate the captured target. Reload
                    // it from its dedicated registered slot.
                    let target_value = roots.proxy_target.get();
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        target_value,
                        Value::array(argv_array),
                        roots.new_target.get(),
                    ];
                    let result = self.run_callable_sync_rooted(
                        stack,
                        context,
                        &roots.scratch_1.get(),
                        roots.scratch_0.get(),
                        trap_args,
                    )?;
                    if !result.is_object_type() {
                        return Err(self.err_type(
                            ("Proxy construct trap returned non-object".to_string()).into(),
                        ));
                    }
                    return Ok(result);
                } else if trap_value.is_undefined() || trap_value.is_null() {
                    roots.current.set(roots.proxy_target.get());
                    roots.proxy_target.set(Value::undefined());
                } else {
                    return Err(
                        self.err_type(("Proxy construct trap is not callable".to_string()).into())
                    );
                }
            } else {
                break;
            }
        }

        if let Some(native) = self.native_promise_constructor(&roots.current.get()) {
            let effective_args = roots.take_args();
            let new_target = roots.new_target.get();
            return self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &Value::undefined(),
                &new_target,
                false,
                effective_args.as_slice(),
            );
        }
        if let Some(native) = roots.current.get().as_native_function()
            && matches!(
                native.name(&self.gc_heap),
                "ArrayBuffer"
                    | "SharedArrayBuffer"
                    | "DataView"
                    | "Int8Array"
                    | "Uint8Array"
                    | "Uint8ClampedArray"
                    | "Int16Array"
                    | "Uint16Array"
                    | "Int32Array"
                    | "Uint32Array"
                    | "Float16Array"
                    | "Float32Array"
                    | "Float64Array"
                    | "BigInt64Array"
                    | "BigUint64Array"
            )
        {
            let effective_args = roots.take_args();
            let new_target = roots.new_target.get();
            return self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &Value::undefined(),
                &new_target,
                false,
                effective_args.as_slice(),
            );
        }

        let bytecode_callee = if let Some(class) = roots.current.get().as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            roots.current.get()
        };
        if let Ok((function_id, _)) =
            Self::bytecode_construct_target_parts(bytecode_callee, &self.gc_heap)
            && context
                .exec_function(function_id)
                .is_some_and(|function| function.is_derived_constructor)
        {
            let new_target = roots.new_target.get();
            let effective_args = roots.take_args();
            let new_frame = self.build_construct_bytecode_frame(
                context,
                bytecode_callee,
                None,
                new_target,
                effective_args,
                None,
            )?;
            let floor = stack.floor();
            stack.push(new_frame);
            return self.dispatch_loop_above_rooted(context, stack, floor);
        }

        let effective_new_target = roots.new_target.get();
        let mut proto =
            self.construct_prototype_for_callee(stack, context, &effective_new_target)?;
        let mut used_object_prototype_fallback = false;
        if proto.is_none() {
            if roots
                .current
                .get()
                .as_native_function()
                .is_some_and(|native| native.name(&self.gc_heap) == "Date")
            {
                proto = Some(self.constructor_prototype_value("Date")?);
            } else {
                proto = Some(self.constructor_prototype_value("Object")?);
                used_object_prototype_fallback = true;
            }
        }
        let proto = proto.expect("construct prototype fallback always resolves");
        roots.scratch_0.set(proto);
        let receiver = self.alloc_runtime_rooted_object_with_roots(&[], &[])?;
        roots.receiver.set(Value::object(receiver));
        let proto = roots.scratch_0.get();
        crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        // Keep arguments in `SyncJsCallRoots` through the observable
        // new-target prototype lookup and receiver allocation. Taking the
        // SmallVec earlier detached it from the registered root provider, so
        // a getter-triggered scavenge left AggregateError's iterable stale.
        // Every destination below either registers the slice immediately
        // (native) or transfers it into a frame builder with its own roots.
        let effective_args = roots.take_args();

        if let Some(obj) = roots.current.get().as_object()
            && let Some(native) = crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())
        {
            let this_value = roots.receiver.get();
            let new_target = roots.new_target.get();
            return self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &this_value,
                &new_target,
                used_object_prototype_fallback,
                effective_args.as_slice(),
            );
        }
        if let Some(native) = roots.current.get().as_native_function() {
            let this_value = roots.receiver.get();
            let new_target = roots.new_target.get();
            return self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &this_value,
                &new_target,
                used_object_prototype_fallback,
                effective_args.as_slice(),
            );
        }
        if let Some(class) = roots.current.get().as_class_constructor()
            && let Some(native) = class.ctor(&self.gc_heap).as_native_function()
        {
            let this_value = roots.receiver.get();
            let new_target = roots.new_target.get();
            return self.invoke_native_construct_rooted(
                stack,
                context,
                native,
                &this_value,
                &new_target,
                used_object_prototype_fallback,
                effective_args.as_slice(),
            );
        }
        if let Some(class) = roots.current.get().as_class_constructor() {
            roots.current.set(class.ctor(&self.gc_heap));
        }

        let current = roots.current.get();
        let new_target = roots.new_target.get();
        let new_frame = self.build_construct_bytecode_frame(
            context,
            current,
            Some(receiver),
            new_target,
            effective_args,
            None,
        )?;
        let floor = stack.floor();
        stack.push(new_frame);
        self.dispatch_loop_above_rooted(context, stack, floor)
    }
}

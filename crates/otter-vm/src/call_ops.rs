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
//!
//! # Invariants
//! - Call-site helpers advance the caller PC before pushing or synchronously
//!   invoking another frame.
//! - `invoke` remains the shared call path for bytecode, closures, native
//!   callables, bound functions, class constructors, and proxies.
//! - Constructor dispatch preserves `new.target` and receiver substitution
//!   invariants used by `pop_frame`.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use crate::holt_stack::HoltStack;
use otter_bytecode::Operand;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    AsyncFrameState, CodeBlock, ExecutionContext, Frame, Interpreter, JsObject, NativeCallInfo,
    NativeCtx, NativeFunction, Value, VmError, VmGetOutcome, VmPropertyKey, abstract_ops,
    argument_window::BytecodeArgumentWindow, executable::OperandView, frame_state::UpvalueSpine,
    is_constructor_runtime, native_to_vm_error, operand_decode::register_operand, promise_dispatch,
    read_register, write_register,
};

struct SyncNativeCallRoots<'a> {
    /// Inner registration over the interpreter, re-dispatched so its
    /// runtime roots are re-enumerated **live** at every trace. A
    /// snapshot `Vec<*mut RawGc>` taken at call entry would miss
    /// anything the native registers afterwards (fresh anchors, IC
    /// entries, shape transitions), and a moving scavenge inside the
    /// native would then sweep or relocate objects those late slots
    /// still reference.
    interp_roots: otter_gc::ExtraRoots,
    value_roots: SmallVec<[&'a Value; 4]>,
    slice_roots: SmallVec<[&'a [Value]; 2]>,
}

impl otter_gc::ExtraRootSource for SyncNativeCallRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.interp_roots.visit(visitor);
        for value in &self.value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in &self.slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    }
}

fn invoke_native_call_with_roots(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    call: crate::native_function::NativeCallTarget,
    realm_global: Option<JsObject>,
    this_value: Value,
    value_roots: &[&Value],
    args: &[Value],
) -> Result<Value, VmError> {
    let this_root = this_value;
    let mut roots = SyncNativeCallRoots {
        interp_roots: otter_gc::ExtraRoots::new::<Interpreter>(interp),
        value_roots: smallvec::smallvec![&this_root],
        slice_roots: smallvec::smallvec![args],
    };
    roots.value_roots.extend_from_slice(value_roots);
    // Pushed (not installed) so any outer scope's value/slice roots
    // stay visible to scavenges triggered inside this native.
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let call_info = NativeCallInfo::call(this_root);
    if let Some(global) = realm_global {
        interp.with_host_realm_global(global, |interp| {
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(interp, call_info, Some(context));
            let raw = call.invoke(&mut ctx, args);
            raw.map_err(|e| native_to_vm_error(interp, e))
        })
    } else {
        let mut ctx = NativeCtx::new_with_call_info_and_context(interp, call_info, Some(context));
        let raw = call.invoke(&mut ctx, args);
        raw.map_err(|e| native_to_vm_error(interp, e))
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
/// eval_env)` resolved from a callable value for a bytecode call.
pub(crate) type BytecodeCallTargetParts = (
    u32,
    crate::frame_state::UpvalueSpine,
    Value,
    Option<Value>,
    Option<crate::UpvalueCell>,
    Option<crate::eval_env::EvalEnvHandle>,
);

#[derive(Clone)]
pub(crate) struct LeanCallbackRoot {
    callback: Value,
    function_id: u32,
    parent_upvalues: crate::frame_state::UpvalueSpine,
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
                parent_upvalues: Frame::empty_upvalues(),
                bound_this: None,
                bound_new_target: None,
                bound_derived_this: None,
                eval_env: None,
            });
        }
        let closure = callback.as_closure(heap)?;
        let function_id = closure.function_id();
        let (parent_upvalues, bound_this, bound_new_target, bound_derived_this, eval_env) = heap
            .read_payload(closure.handle, |body| {
                (
                    body.upvalues.clone().into_boxed_slice(),
                    body.bound_this,
                    body.bound_new_target,
                    body.bound_derived_this,
                    body.eval_env,
                )
            });
        Some(Self {
            callback,
            function_id,
            parent_upvalues,
            bound_this,
            bound_new_target,
            bound_derived_this,
            eval_env,
        })
    }

    pub(crate) fn trace_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.callback.trace_value_slots(visitor);
        for slot in self.parent_upvalues.iter() {
            let p = slot as *const crate::UpvalueCell as *mut RawGc;
            visitor(p);
        }
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
    stack: HoltStack,
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
    /// Cached `(input this, coerced this)` pair. The builtin passes a constant
    /// receiver for the whole loop, so the §10.2 sloppy-`this` coercion runs
    /// once; a changed input recomputes it.
    cached_this: Option<(Value, Value)>,
}

impl Interpreter {
    pub(crate) fn lean_callback_parent_upvalue(
        &self,
        state: &LeanCallbackState,
        idx: usize,
    ) -> Option<crate::UpvalueCell> {
        self.lean_callback_roots
            .get(state.root_index)?
            .parent_upvalues
            .get(idx)
            .copied()
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
            let env = crate::eval_env::alloc_eval_env(&mut self.gc_heap, inherited)
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
            ));
        }
        if let Some(c) = current.as_closure(heap) {
            let function_id = c.function_id();
            let (upvalues, bound_this, bound_new_target, bound_derived_this, eval_env) = heap
                .read_payload(c.handle, |body| {
                    let ups: crate::frame_state::UpvalueSpine =
                        body.upvalues.clone().into_boxed_slice();
                    (
                        ups,
                        body.bound_this,
                        body.bound_new_target,
                        body.bound_derived_this,
                        body.eval_env,
                    )
                });
            let this_value = bound_this.unwrap_or(effective_this);
            return Ok((
                function_id,
                upvalues,
                this_value,
                bound_new_target,
                bound_derived_this,
                eval_env,
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
            let upvalues =
                heap.read_payload(c.handle, |body| body.upvalues.clone().into_boxed_slice());
            return Ok((function_id, upvalues));
        }
        Err(VmError::NotCallable)
    }

    fn build_construct_bytecode_frame(
        &mut self,
        context: &ExecutionContext,
        current: Value,
        receiver: JsObject,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
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
            current.trace_value_slots(visitor);
            new_target.trace_value_slots(visitor);
            visitor(&receiver as *const JsObject as *mut RawGc);
            for value in &args {
                value.trace_value_slots(visitor);
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
        let this_value = if is_derived {
            Value::hole()
        } else {
            Value::object(receiver)
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
        let callee_closure = current.as_closure(&self.gc_heap);
        let derived_this_cell = if is_derived {
            let mut frame_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                frame.trace_frame_slots(visitor);
                current.trace_value_slots(visitor);
                new_target.trace_value_slots(visitor);
                visitor(&receiver as *const JsObject as *mut RawGc);
                for value in &args {
                    value.trace_value_slots(visitor);
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
                cold.construct_target = Some(receiver);
            }
            cold.new_target = Some(new_target);
            cold.callee_closure = callee_closure;
        }
        self.bind_bytecode_call_arguments(function, &mut frame, args)?;
        window_rollback.commit();
        Ok(frame)
    }

    fn build_construct_bytecode_frame_from_window(
        &mut self,
        context: &ExecutionContext,
        current: Value,
        receiver: JsObject,
        new_target: Value,
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
            current.trace_value_slots(visitor);
            new_target.trace_value_slots(visitor);
            visitor(&receiver as *const JsObject as *mut RawGc);
        };
        let upvalues = Frame::build_upvalues_for_exec_with_roots(
            &mut self.gc_heap,
            function,
            parent_upvalues,
            &mut build_roots,
        )?;
        let is_derived = function.is_derived_constructor;
        let this_value = if is_derived {
            Value::hole()
        } else {
            Value::object(receiver)
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
        let callee_closure = current.as_closure(&self.gc_heap);
        let extras = args.bind_into(function, &mut frame)?;
        let derived_this_cell = if is_derived {
            let mut frame_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                frame.trace_frame_slots(visitor);
                current.trace_value_slots(visitor);
                new_target.trace_value_slots(visitor);
                visitor(&receiver as *const JsObject as *mut RawGc);
                for value in &extras.rest_args {
                    value.trace_value_slots(visitor);
                }
                for value in &extras.incoming_args {
                    value.trace_value_slots(visitor);
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
                cold.construct_target = Some(receiver);
            }
            cold.new_target = Some(new_target);
            cold.callee_closure = callee_closure;
            if !extras.is_empty() {
                cold.rest_args = extras.rest_args;
                cold.incoming_args = extras.incoming_args;
            }
        }
        window_rollback.commit();
        Ok(frame)
    }

    fn invoke_native_construct(
        &mut self,
        context: &ExecutionContext,
        native: NativeFunction,
        this_value: &Value,
        new_target: &Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let call = native.call_target(&self.gc_heap);
        let call_info = NativeCallInfo::construct(*this_value, Some(*new_target));
        self.record_runtime_native_call();
        // Same root coverage as the call path (`invoke_native_call_with_roots`):
        // trace the interpreter's full root set (crucially the scope-handle
        // arena, so a native constructor's `scoped_*` handles stay live) and
        // pin `this`, `new.target`, and the argument slice across every
        // scavenge the constructor triggers. Without this a native `new X(…)`
        // ran fully unrooted — e.g. `new Set([...])` stranded its iterable.
        let this_root = *this_value;
        let new_target_root = *new_target;
        let roots = SyncNativeCallRoots {
            interp_roots: otter_gc::ExtraRoots::new::<Interpreter>(self),
            value_roots: smallvec::smallvec![&this_root, &new_target_root],
            slice_roots: smallvec::smallvec![args],
        };
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let mut ctx = NativeCtx::new_with_call_info_and_context(self, call_info, Some(context));
        let raw = call.invoke(&mut ctx, args);
        let result = raw.map_err(|e| native_to_vm_error(self, e))?;
        Ok(if result.is_object_type() {
            result
        } else {
            *this_value
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push_bytecode_call_frame(
        &mut self,
        stack: &mut HoltStack,
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
        if stack.len() as u32 >= self.max_stack_depth {
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
        new_frame.async_state = async_state;
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
            let new_frame = self.park_active_frame(new_frame);
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                new_frame,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            // Backlink the generator into the frame so `Op::Yield`
            // can find its owner once execution starts.
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let (frame, cold) = gen_handle
                .take_frame(&mut self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let mut frame = self.resume_parked_frame(*frame)?;
            if let Some(cold) = cold {
                self.frame_attach_cold(&mut frame, cold);
            }
            let mut prologue_stack: HoltStack = HoltStack::new();
            prologue_stack.push(frame);
            self.dispatch_loop(context, &mut prologue_stack)?;
            self.resolve_generator_prototype_stack_rooted(
                context,
                stack,
                callee_closure,
                generator_function_id,
                &gen_handle,
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, Value::generator(gen_handle))?;
            return Ok(());
        }
        stack.push(new_frame);
        window_rollback.commit();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_bytecode_call_frame_from_window(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        function_id: u32,
        parent_upvalues: UpvalueSpine,
        this_for_callee: Value,
        new_target_for_callee: Option<Value>,
        derived_this_cell: Option<crate::UpvalueCell>,
        callee_eval_env: Option<crate::eval_env::EvalEnvHandle>,
        args: &BytecodeArgumentWindow<'_, '_>,
        return_register: Option<u16>,
        async_state: Option<AsyncFrameState>,
    ) -> Result<PreparedBytecodeFrame, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
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
        frame.async_state = async_state;
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
            generator_function_id: function_id,
            callee_closure: None,
        };
        window_rollback.commit();
        Ok(prepared)
    }

    /// §27.5.1 step 3 / §9.1.14 — resolve a fresh generator's
    /// [[Prototype]] from `fn.prototype` AFTER the prologue ran; a
    /// non-object answer falls back (override `None`) to the realm's
    /// shared `%GeneratorPrototype%` / `%AsyncGeneratorPrototype%`.
    fn resolve_generator_prototype_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        gen_handle: &crate::generator::JsGenerator,
    ) -> Result<(), VmError> {
        // `owner` is the invoked closure instance: `fn.prototype`
        // materializes per closure, so resolving through the template
        // bag would hand the generator a parallel prototype object that
        // fails `Object.getPrototypeOf(gen) === fn.prototype`.
        let proto = self.function_property_get_stack_rooted(
            context,
            stack,
            owner,
            function_id,
            "prototype",
        )?;
        gen_handle.set_prototype_override(
            &mut self.gc_heap,
            proto.as_object().is_some().then_some(proto),
        );
        Ok(())
    }

    fn push_prepared_bytecode_call_frame(
        &mut self,
        stack: &mut HoltStack,
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
            let frame = self.park_active_frame(frame);
            let gen_handle =
                crate::generator::JsGenerator::new_with_prototype(&mut self.gc_heap, frame, None)?;
            gen_handle.set_async(&mut self.gc_heap, is_async_generator);
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let (frame, cold) = gen_handle
                .take_frame(&mut self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let mut frame = self.resume_parked_frame(*frame)?;
            if let Some(cold) = cold {
                self.frame_attach_cold(&mut frame, cold);
            }
            let mut prologue_stack: HoltStack = HoltStack::new();
            prologue_stack.push(frame);
            self.dispatch_loop(context, &mut prologue_stack)?;
            self.resolve_generator_prototype_stack_rooted(
                context,
                stack,
                callee_closure,
                generator_function_id,
                &gen_handle,
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, Value::generator(gen_handle))?;
            return Ok(());
        }
        stack.push(frame);
        Ok(())
    }

    fn try_push_bytecode_call_frame_from_window(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        operands: OperandView<'_>,
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let current = *callee;
        let effective_this = this_value;
        if current.as_class_constructor().is_some() {
            // §10.3.1 — a class constructor's [[Call]] always
            // throws; only [[Construct]] may enter it.
            return Err(self.err_type(
                ("Class constructor cannot be invoked without 'new'".to_string()).into(),
            ));
        }
        if current.is_bound_function() {
            return Ok(false);
        }
        if !current.is_function() && !current.is_closure() {
            return Ok(false);
        }
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_runtime_bytecode_call();
        if stack.len() as u32 >= self.max_stack_depth {
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
            let args = BytecodeArgumentWindow::new(caller, operands, first_arg_operand, argc);
            self.prepare_bytecode_call_frame_from_window(
                context,
                stack,
                function_id,
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
        // Record the invoked closure so the named-function SELF binding and
        // `arguments.callee` resolve to the live instance, not a bare
        // interned function value.
        if (function.makes_function || function.uses_arguments_callee)
            && let Some(closure) = current.as_closure(&self.gc_heap)
        {
            self.frame_ensure_cold(&mut prepared.frame).callee_closure = Some(closure);
        }
        prepared.callee_closure = current.as_closure(&self.gc_heap);
        self.push_prepared_bytecode_call_frame(stack, context, dst, prepared)?;
        Ok(true)
    }

    fn try_invoke_native_call_from_window(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        operands: OperandView<'_>,
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let top_idx = stack.len() - 1;
        let args = {
            let caller = &stack[top_idx];
            let window = BytecodeArgumentWindow::new(caller, operands, first_arg_operand, argc);
            let Some(args) = window.contiguous_slice()? else {
                return Ok(false);
            };
            args
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
                context,
                call,
                realm_global,
                this_value,
                &[callee],
                args,
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
                context,
                call,
                realm_global,
                this_value,
                &[callee],
                args,
            )?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots and `this` bound
    /// to `Value::undefined()` (foundation strict default).
    pub(crate) fn do_call<'a>(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        let operands = operands.into();
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };

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
        let args = BytecodeArgumentWindow::new(&stack[top_idx], operands, 3, argc as usize)
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
    /// [`Self::do_call`], preserving behaviour at a small depth cost.
    pub(crate) fn do_tail_call(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;

        // Snapshot everything that lives in the doomed frame, and decide
        // whether the frame may be discarded in place.
        let (callee, args, ret_reg) = {
            let frame = &stack[top_idx];
            let tco_safe = frame.return_register.is_some()
                && frame.async_state.is_none()
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
                return self.do_call(stack, context, operands);
            }
            let callee = *read_register(frame, callee_reg)?;
            let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
            for i in 0..argc {
                let r = register_operand(operands.get(3 + i))?;
                args.push(*read_register(frame, r)?);
            }
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
        stack: &mut HoltStack,
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
            ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
            return self.push_bytecode_call_frame(
                stack,
                context,
                current.as_closure(&self.gc_heap),
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
                let result =
                    self.run_vm_intrinsic_sync(context, intrinsic, effective_this, effective_args)?;
                let top_idx = stack.len() - 1;
                write_register(&mut stack[top_idx], dst, result)?;
                return Ok(());
            }
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            let result = invoke_native_call_with_roots(
                self,
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
        // target as a function.
        if let Some(proxy) = current.as_proxy() {
            let argv_array = self.alloc_stack_rooted_array_from_values(
                stack,
                effective_args.iter().cloned(),
                &[&current, &effective_this],
                effective_args.as_slice(),
            )?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                effective_this,
                Value::array(argv_array),
            ];
            let result = match self.invoke_proxy_trap(context, &proxy, "apply", trap_args)? {
                Some(v) => v,
                None => {
                    // Fall through to the target's [[Call]] —
                    // `proxy.target(&self.gc_heap)` returns the original Value,
                    // which may be a callable directly.
                    let underlying = proxy.target(&self.gc_heap);
                    self.run_callable_sync(context, &underlying, effective_this, effective_args)?
                }
            };
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
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
        self.push_bytecode_call_frame(
            stack,
            context,
            current.as_closure(&self.gc_heap),
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
    pub(crate) fn do_construct<'a>(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        let operands = operands.into();
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
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
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args = BytecodeArgumentWindow::new(&stack[top_idx], operands, 3, argc as usize)
            .to_smallvec8()?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    fn try_dispatch_construct_from_window(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        callee: Value,
        operands: OperandView<'_>,
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let mut current = callee;
        let effective_new_target = current;
        let is_direct_class_construct = current.as_class_constructor().is_some();
        if let Some(class) = current.as_class_constructor() {
            current = class.ctor(&self.gc_heap);
        }
        if !current.is_function() && !current.is_closure() {
            return Ok(false);
        }

        self.record_runtime_construct_call();
        let proto = self.construct_prototype_for_callee_stack_rooted(
            context,
            stack,
            &effective_new_target,
        )?;
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
        if is_direct_class_construct
            && let Some(function_id) = current
                .as_function()
                .or_else(|| current.as_closure(&self.gc_heap).map(|c| c.function_id()))
            && let Some(function) = context.exec_function(function_id)
        {
            let top_idx = stack.len() - 1;
            let args_window =
                BytecodeArgumentWindow::new(&stack[top_idx], operands, first_arg_operand, argc);
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
            let args = BytecodeArgumentWindow::new(caller, operands, first_arg_operand, argc);
            self.build_construct_bytecode_frame_from_window(
                context,
                current,
                receiver,
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
        stack: &mut HoltStack,
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
        stack: &HoltStack,
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
        stack: &mut HoltStack,
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
        stack: &mut HoltStack,
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
        stack: &mut HoltStack,
        context: &ExecutionContext,
        callee: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.dispatch_construct_with_new_target(stack, context, callee, callee, args, dst)
    }

    fn dispatch_construct_with_new_target(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.record_runtime_construct_call();
        let mut callee = callee;
        let mut new_target = new_target;
        let mut args = args;
        let mut hops: u32 = 0;
        while let Some(bound) = callee.as_bound_function() {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            hops += 1;
            let (target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
            let mut combined: SmallVec<[Value; 8]> =
                SmallVec::with_capacity(bound_args.len() + args.len());
            combined.extend(bound_args);
            combined.extend(args);
            if abstract_ops::same_value(&callee, &new_target, &self.gc_heap) {
                new_target = target;
            }
            callee = target;
            args = combined;
        }
        // §28.2.4.14 Proxy.[[Construct]] — `new <proxy>(args)`
        // routes through the `construct` trap when present;
        // otherwise delegates to the target.
        if let Some(proxy) = callee.as_proxy() {
            let argv_array = self.alloc_stack_rooted_array_from_values(
                stack,
                args.iter().cloned(),
                &[&callee, &new_target],
                args.as_slice(),
            )?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                Value::array(argv_array),
                new_target,
            ];
            let result = match self.invoke_proxy_trap(context, &proxy, "construct", trap_args)? {
                Some(v) => {
                    // §10.5.13 step 9 — trap result must be an Object;
                    // primitive returns surface as TypeError.
                    if !v.is_object_type() {
                        return Err(self.err_type(
                            ("Proxy construct trap returned non-object".to_string()).into(),
                        ));
                    }
                    v
                }
                None => {
                    // Fall through to [[Construct]] on the underlying
                    // target via `run_construct_sync`, which honours
                    // bound/proxy/native paths and re-checks the
                    // constructor-return invariants.
                    self.run_construct_sync(
                        context,
                        &proxy.target(&self.gc_heap),
                        new_target,
                        args,
                    )?
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        if let Some(native) = self.native_promise_constructor(&callee) {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &Value::undefined(),
                &new_target,
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
        let proto =
            self.construct_prototype_for_callee_stack_rooted(context, stack, &new_target)?;
        // OrdinaryCreateFromConstructor — a missing or non-object
        // `prototype` falls back to %Object.prototype% (§10.1.13).
        let proto = match proto {
            Some(proto) => proto,
            None => self.constructor_prototype_value("Object")?,
        };
        // Park `proto` on the traced iteration-anchor stack across the receiver
        // allocation. A moving collection triggered by the allocation relocates
        // a young prototype (e.g. `%Date.prototype%`, not in the directly-rooted
        // realm-intrinsic set), and the ad-hoc value-root external visit does not
        // reliably rewrite this detached stack local; the anchor slot is a real
        // GC root, so reading it back yields the relocated handle before it wires
        // the receiver's `[[Prototype]]`.
        let proto_anchor = self.push_iteration_anchor(proto) - 1;
        let receiver =
            self.alloc_stack_rooted_object_with_value_roots(stack, &[&callee, &new_target], &args)?;
        let proto = self.iteration_anchor(proto_anchor);
        self.pop_iteration_anchors_to(proto_anchor);
        crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        let this_value = Value::object(receiver);
        // Built-in constructor objects (`Number`, `Boolean`, …)
        // surface as a `Value::Object` with an internal native
        // constructor slot. Promote to the native-function construct
        // path so the JS-visible callee can also carry own
        // properties (statics + `prototype`) without leaking the
        // implementation slot through reflection.
        if let Some(obj) = callee.as_object()
            && let Some(native) = crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())
        {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &this_value,
                &new_target,
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
        if let Some(native) = callee.as_native_function() {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &this_value,
                &new_target,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        if let Some(class) = callee.as_class_constructor()
            && let Some(native) = class.ctor(&self.gc_heap).as_native_function()
        {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &this_value,
                &new_target,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        let bytecode_callee = if let Some(class) = callee.as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            callee
        };
        if bytecode_callee.is_function() || bytecode_callee.is_closure() {
            let frame = self.build_construct_bytecode_frame(
                context,
                bytecode_callee,
                receiver,
                new_target,
                args,
                Some(dst),
            )?;
            stack.push(frame);
            return Ok(());
        }
        self.invoke(stack, context, &callee, this_value, args, dst)?;
        // The pushed frame is now on top; mark it so `pop_frame`
        // can substitute the receiver for any non-object return.
        if let Some(top) = stack.last_mut() {
            let cold = self.frame_ensure_cold(top);
            cold.construct_target = Some(receiver);
            cold.new_target = Some(new_target);
        }
        Ok(())
    }

    pub(crate) fn construct_prototype_for_callee_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let function_id = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            let owner = callee.as_closure(&self.gc_heap);
            return match self.function_property_get_stack_rooted_with_receiver(
                context,
                stack,
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
            return self.construct_prototype_via_get(context, callee);
        }
        if let Some(obj) = callee.as_object() {
            return Ok(match crate::object::get(obj, &self.gc_heap, "prototype") {
                Some(proto) if proto.is_object_type() => Some(proto),
                _ => None,
            });
        }
        if callee.is_bound_function() {
            return self.construct_prototype_via_get(context, callee);
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

    pub(crate) fn construct_prototype_for_callee_runtime_rooted(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<Value>, VmError> {
        let function_id = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            let owner = callee.as_closure(&self.gc_heap);
            return match self.function_property_get_runtime_rooted_with_receiver(
                context,
                owner,
                function_id,
                Some(*callee),
                "prototype",
                value_roots,
                slice_roots,
            )? {
                proto if proto.is_object_type() => Ok(Some(proto)),
                _ => Ok(None),
            };
        }
        if let Some(c) = callee.as_class_constructor() {
            return Ok(Some(Value::object(c.prototype(&self.gc_heap))));
        }
        if callee.as_proxy().is_some() {
            return self.construct_prototype_via_get(context, callee);
        }
        if let Some(obj) = callee.as_object() {
            return Ok(match crate::object::get(obj, &self.gc_heap, "prototype") {
                Some(proto) if proto.is_object_type() => Some(proto),
                _ => None,
            });
        }
        if callee.is_bound_function() {
            return self.construct_prototype_via_get(context, callee);
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
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let proxy = callee.as_proxy();
        let key = VmPropertyKey::String("prototype");
        let proto = match self.ordinary_get_value(context, *callee, *callee, &key, 0)? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *callee, SmallVec::new())?
            }
        };
        if !proto.is_object_type() && proxy.is_some_and(|proxy| proxy.is_revoked(&self.gc_heap)) {
            return Err(
                self.err_type(("Cannot get prototype from a revoked proxy".to_string()).into())
            );
        }
        Ok(proto.is_object_type().then_some(proto))
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
        stack: &mut HoltStack,
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
    pub(crate) fn do_call_with_this(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
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
        let args = BytecodeArgumentWindow::new(&stack[top_idx], operands, 4, argc as usize)
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
        self.enter_sync_reentry()?;
        // Host callers (timer fire, worker message dispatch) enter
        // here without `Interpreter::run`'s rooted scope, and the
        // inner body allocates (upvalue spines, `this` boxing) before
        // `dispatch_loop` registers frame roots — so the runtime
        // roots must be registered for the whole call.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        let result = self.run_callable_sync_inner(context, callee, this_value, args);
        drop(extra_roots_guard);
        self.leave_sync_reentry();
        result
    }

    /// Synchronous callable re-entry for callers that already hold a live
    /// [`otter_gc::ExtraRoots`] registration for this interpreter on the heap's
    /// LIFO stack — specifically the JIT call bridge ([`Interpreter::jit_runtime_call`]).
    ///
    /// Compiled code only ever runs under an enclosing `dispatch_loop`
    /// (loop-OSR / function-entry tier-up) or under [`Self::run_callable_sync`]
    /// itself, both of which push `ExtraRoots::new(self as &Interpreter)` before
    /// entering compiled code. That registration traces runtime-global roots
    /// (shape tables, `globalThis`, module envs, the microtask queue) which are
    /// frame-independent, so a second push for the nested call is a pure
    /// duplicate the heap's `same_source` walk already skips. Eliding it removes
    /// a `Vec` push/truncate per JIT→VM call without changing the traced root
    /// set. The stack-overflow guard (`enter_sync_reentry`) is retained because
    /// each level still consumes native stack.
    pub(crate) fn run_callable_sync_already_rooted(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.enter_sync_reentry()?;
        let result = self.run_callable_sync_inner(context, callee, this_value, args);
        self.leave_sync_reentry();
        result
    }

    fn run_callable_sync_inner(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
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
            } else if current.as_class_constructor().is_some() {
                // §10.3.1 — class constructors reject [[Call]].
                return Err(self.err_type(
                    ("Class constructor cannot be invoked without 'new'".to_string()).into(),
                ));
            } else if let Some(proxy) = current.as_proxy() {
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
                let handler = proxy.handler(&self.gc_heap);
                let trap_key = VmPropertyKey::String("apply");
                let trap_value =
                    match self.ordinary_get_value(context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            self.run_callable_sync(context, &getter, handler, SmallVec::new())?
                        }
                    };
                if self.is_callable_runtime(&trap_value) {
                    let argv_array = self.alloc_runtime_rooted_array_from_values(
                        effective_args.iter().cloned(),
                        &[&current, &effective_this, &handler, &trap_value],
                        &[effective_args.as_slice()],
                    )?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        proxy.target(&self.gc_heap),
                        effective_this,
                        Value::array(argv_array),
                    ];
                    return self.run_callable_sync(context, &trap_value, handler, trap_args);
                } else if trap_value.is_undefined() || trap_value.is_null() {
                    current = proxy.target(&self.gc_heap);
                } else {
                    return Err(
                        self.err_type(("Proxy apply trap is not callable".to_string()).into())
                    );
                }
            } else {
                break;
            }
        }
        if let Some(obj) = current.as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            return invoke_native_call_with_roots(
                self,
                context,
                call,
                realm_global,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            );
        }
        if let Some(native) = current.as_native_function() {
            let native = &native;
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                return self.run_vm_intrinsic_sync(
                    context,
                    intrinsic,
                    effective_this,
                    effective_args,
                );
            }
            self.record_runtime_native_call();
            let realm_global = native.realm_global(&self.gc_heap);
            return invoke_native_call_with_roots(
                self,
                context,
                call,
                realm_global,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            );
        }
        let mut inner = self.draw_stack();
        let result = self.run_bytecode_callable_committed(
            &mut inner,
            context,
            current,
            effective_this,
            effective_args,
        );
        self.return_stack(inner);
        result
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
        let has_parent_upvalues = !root.parent_upvalues.is_empty();
        // A callback with no bound `new.target` / derived-`this` cell / captured
        // eval environment needs no pooled cold record, so its per-element frame
        // is a flat register window the prepared fast path can recycle in place.
        let fast_reuse = root.bound_new_target.is_none()
            && root.bound_derived_this.is_none()
            && root.eval_env.is_none();
        if self.enter_sync_reentry().is_ok() {
            let stack = self.draw_stack();
            let root_index = self.lean_callback_roots.len();
            let function_id = root.function_id;
            self.lean_callback_roots.push(root);
            Some(LeanCallbackState {
                stack,
                root_index,
                function_id,
                register_count,
                param_count,
                this_passthrough,
                has_parent_upvalues,
                fast_reuse,
                compiled: None,
                reuse_frame: None,
                cached_this: None,
            })
        } else {
            None
        }
    }

    /// Return the lean-path stack to the pool and leave the sync-reentry guard
    /// entered by [`Self::acquire_lean_callback_stack`]. No-op for `None`.
    pub(crate) fn release_lean_callback_stack(&mut self, state: Option<LeanCallbackState>) {
        if let Some(mut state) = state {
            // Return the recycled frame's spilled register backing to the pool;
            // an inline window is dropped with the frame.
            if let Some(mut frame) = state.reuse_frame.take() {
                self.frame_release_cold(&mut frame);
                self.reclaim_registers(&mut frame);
            }
            self.return_stack(state.stack);
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

    pub(crate) fn run_bytecode_callable_committed(
        &mut self,
        inner: &mut HoltStack,
        context: &ExecutionContext,
        current: Value,
        effective_this: Value,
        effective_args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
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
                    .pending_stack_rooted(
                        self,
                        inner,
                        &[&this_for_callee],
                        &[effective_args.as_slice()],
                    )?,
            )
        } else {
            None
        };
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call_runtime_rooted(
            function,
            this_for_callee,
            &[effective_args.as_slice()],
        )?;
        let _window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues,
            this_for_callee,
            window,
        );
        if let Some(result_promise) = async_result_promise {
            new_frame.async_state = Some(crate::frame_state::AsyncFrameState { result_promise });
        }
        // A closure frame records its instance so the named-function SELF
        // binding inside the body resolves to it (per-instance `.prototype`),
        // and so `arguments.callee` exposes the invoked closure rather than a
        // bare interned function value. Leaf callees that neither create a
        // closure nor materialize an arguments object skip the record and the
        // cold-frame acquire.
        if (function.makes_function || function.uses_arguments_callee)
            && let Some(closure) = current.as_closure(&self.gc_heap)
        {
            self.frame_ensure_cold(&mut new_frame).callee_closure = Some(closure);
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
        // §27.5.1 GeneratorFunction call evaluation returns a
        // generator object without executing the body. `invoke`
        // handles this for opcode calls; the synchronous re-entry
        // helper must mirror it for builtins that call user
        // functions, such as `GetSetRecord(...).[[Keys]]`.
        // <https://tc39.es/ecma262/#sec-generatorfunction-objects>
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let new_frame = self.park_active_frame(new_frame);
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                new_frame,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            // §27.5 — run the generator prologue (mirroring the opcode
            // `invoke` path) so the handle is primed to its
            // suspended-start state. Without it a generator created
            // through a builtin's synchronous re-entry (e.g. an
            // `@@iterator` that is a generator function, driven by
            // `Array.from` / `GetSetRecord` / the iterator helpers) is
            // never started and reports `done` on its first `next`.
            let (frame, cold) = gen_handle
                .take_frame(&mut self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let mut frame = self.resume_parked_frame(*frame)?;
            if let Some(cold) = cold {
                self.frame_attach_cold(&mut frame, cold);
            }
            let mut prologue_stack: HoltStack = HoltStack::new();
            prologue_stack.push(frame);
            self.dispatch_loop(context, &mut prologue_stack)?;
            // §27.5.1 step 3 — resolve [[Prototype]] after the
            // prologue (FunctionDeclarationInstantiation) ran, through
            // the invoked closure's bag so the generator's prototype is
            // the same object later `fn.prototype` reads observe.
            let proto = self.function_property_get_runtime_rooted(
                context,
                current.as_closure(&self.gc_heap),
                function_id,
                "prototype",
                &[],
                &[],
            )?;
            gen_handle.set_prototype_override(
                &mut self.gc_heap,
                proto.as_object().is_some().then_some(proto),
            );
            return Ok(Value::generator(gen_handle));
        }
        // The caller owns `inner` (a reservation-stable pooled stack): the entry
        // frame may tier up and run compiled, and a compiled callee appends its
        // frame directly onto this stack, so it must never reallocate. The frame
        // is popped on every completion path, leaving `inner` empty + reusable.
        inner.push(new_frame);
        if let Some(result_promise) = async_result_promise {
            // The async frame runs to its first `await` (which parks it off
            // `inner`) or to completion (which settles the promise and pops
            // the frame); either way `inner` is left empty and the dispatch
            // loop returns. This call's value is the result promise, not the
            // loop's terminal frame value. A suspending frame must never take
            // the compiled sync-entry path, whose fast tier assumes the entry
            // frame cannot suspend.
            self.dispatch_loop(context, inner)?;
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
        self.dispatch_loop(context, inner)
    }

    pub(crate) fn run_bytecode_callable_committed_lean_args(
        &mut self,
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
            return self.invoke_cold_lean(state, context, effective_this, effective_args, false);
        }
        // Callback carries a bound `new.target` / derived-`this` cell / captured
        // eval environment: build a fresh frame per element and tier up through
        // the synchronous-entry path as before.
        self.invoke_cold_lean(state, context, effective_this, effective_args, true)
    }

    /// Re-enter the callback's already-compiled body with the recycled frame
    /// held in `state`. See [`Self::run_bytecode_callable_committed_lean_args`].
    fn invoke_prepared_lean(
        &mut self,
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
        let (bound_this, parent_upvalues) = {
            let root = self
                .lean_callback_roots
                .get(state.root_index)
                .ok_or(VmError::InvalidOperand)?;
            (
                root.bound_this.unwrap_or(effective_this),
                state
                    .has_parent_upvalues
                    .then(|| root.parent_upvalues.clone()),
            )
        };
        // The builtin passes a constant receiver across the whole loop, so the
        // §10.2 sloppy-`this` coercion is computed once and reused.
        let this_for_callee = if state.this_passthrough {
            bound_this
        } else {
            match state.cached_this {
                Some((input, coerced)) if input == bound_this => coerced,
                _ => {
                    let function = context
                        .exec_function(state.function_id)
                        .ok_or(VmError::InvalidOperand)?;
                    let coerced = self.this_for_bytecode_call_runtime_rooted(
                        function,
                        bound_this,
                        &[effective_args],
                    )?;
                    state.cached_this = Some((bound_this, coerced));
                    coerced
                }
            }
        };
        let register_count = state.register_count;
        let mut frame = match state.reuse_frame.take() {
            Some(mut frame) => {
                debug_assert_eq!(frame.registers.len(), register_count);
                frame.pc = 0;
                frame.return_register = None;
                frame.this_value = this_for_callee;
                if let Some(parent_upvalues) = parent_upvalues {
                    frame.upvalues = parent_upvalues;
                }
                frame
            }
            None => {
                let function = context
                    .exec_function(state.function_id)
                    .ok_or(VmError::InvalidOperand)?;
                let window = self.alloc_reg_window(register_count)?;
                let parent_upvalues = parent_upvalues.unwrap_or_else(Frame::empty_upvalues);
                Frame::with_exec_return_upvalues_and_this(
                    function,
                    None,
                    parent_upvalues,
                    this_for_callee,
                    window,
                )
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
        state.stack.push(frame);
        let top_idx = state.stack.len() - 1;
        if let Some(outcome) = self.run_optimized_frame(&mut state.stack, context, top_idx) {
            match outcome {
                crate::jit::JitExecOutcome::Returned(value) => {
                    state.reuse_frame = state.stack.pop();
                    window_rollback.commit();
                    return Ok(value);
                }
                crate::jit::JitExecOutcome::Bailed(pc) => {
                    state.stack[top_idx].pc = pc;
                    return self.dispatch_loop(context, &mut state.stack);
                }
                crate::jit::JitExecOutcome::Threw(err) => {
                    if let Some(mut frame) = state.stack.pop() {
                        self.frame_release_cold(&mut frame);
                        self.reclaim_registers(&mut frame);
                    }
                    return Err(err);
                }
            }
        }
        match self.run_compiled_frame(&mut state.stack, context, top_idx, code) {
            crate::jit::JitExecOutcome::Returned(value) => {
                // The body ran to completion and left its frame on the stack;
                // recycle that frame (window + shell) for the next element.
                state.reuse_frame = state.stack.pop();
                window_rollback.commit();
                Ok(value)
            }
            crate::jit::JitExecOutcome::Bailed(pc) => {
                // Finish the partially-run frame in the interpreter, which pops
                // it on return; the next element rebuilds a fresh recycled frame.
                state.stack[top_idx].pc = pc;
                self.dispatch_loop(context, &mut state.stack)
            }
            crate::jit::JitExecOutcome::Threw(err) => {
                if let Some(mut frame) = state.stack.pop() {
                    self.frame_release_cold(&mut frame);
                    self.reclaim_registers(&mut frame);
                }
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
        state: &mut LeanCallbackState,
        context: &ExecutionContext,
        effective_this: Value,
        effective_args: &[Value],
        probe: bool,
    ) -> Result<Value, VmError> {
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
        ) = {
            let root = self
                .lean_callback_roots
                .get(state.root_index)
                .ok_or(VmError::InvalidOperand)?;
            (
                root.function_id,
                root.parent_upvalues.clone(),
                root.bound_this.unwrap_or(effective_this),
                root.bound_new_target,
                root.bound_derived_this,
                root.eval_env,
            )
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
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call_runtime_rooted(
            function,
            this_for_callee,
            &[effective_args],
        )?;
        let _window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(function.register_count as usize)?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues,
            this_for_callee,
            window,
        );
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut new_frame, callee_eval_env)?;
        Self::bind_lean_bytecode_call_arguments(function, &mut new_frame, effective_args)?;
        state.stack.push(new_frame);
        if probe && let Some(value) = self.dispatch_jit_sync_entry(&mut state.stack, context)? {
            if let Some(mut done) = state.stack.pop() {
                self.reclaim_registers(&mut done);
            }
            return Ok(value);
        }
        self.dispatch_loop(context, &mut state.stack)
    }

    /// Synchronously perform `Construct(target, args, newTarget)`.
    ///
    /// This mirrors the `Op::New` user-function entry path but
    /// returns the completion directly for builtins such as
    /// `Reflect.construct`. Bound functions are unwrapped with the
    /// ECMA-262 `[[Construct]]` newTarget rewrite: constructing a
    /// bound function as itself exposes the bound target as
    /// `new.target` inside the target body.
    pub(crate) fn run_construct_sync(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.enter_sync_reentry()?;
        // Same rooting contract as [`Self::run_callable_sync`].
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        let result = self.run_construct_sync_inner(context, target, new_target, args);
        drop(extra_roots_guard);
        self.leave_sync_reentry();
        result
    }

    /// Synchronous `Construct` re-entry for callers that already hold a live
    /// [`otter_gc::ExtraRoots`] registration for this interpreter on the heap's
    /// LIFO stack — the JIT construct bridge ([`Interpreter::jit_runtime_construct_in_place`]).
    ///
    /// Mirrors [`Self::run_callable_sync_already_rooted`]: compiled code only
    /// runs under an enclosing `dispatch_loop` (loop-OSR / function-entry
    /// tier-up) or under [`Self::run_construct_sync`] itself, both of which
    /// push `ExtraRoots::new(self as &Interpreter)` before entering compiled
    /// code. That registration traces the frame-independent runtime-global
    /// roots, so a second push for the nested construct is a pure duplicate the
    /// heap's `same_source` walk already skips. The stack-overflow guard
    /// (`enter_sync_reentry`) is retained because each level still consumes
    /// native stack.
    pub(crate) fn run_construct_sync_already_rooted(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.enter_sync_reentry()?;
        let result = self.run_construct_sync_inner(context, target, new_target, args);
        self.leave_sync_reentry();
        result
    }

    fn run_construct_sync_inner(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.record_runtime_construct_call();
        let mut current = *target;
        let mut effective_new_target = new_target;
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
                let (next_target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                if abstract_ops::same_value(&current, &effective_new_target, &self.gc_heap) {
                    effective_new_target = next_target;
                }
                current = next_target;
                effective_args = combined;
            } else if let Some(proxy) = current.as_proxy() {
                // §10.5.13 Proxy [[Construct]].
                if proxy.is_revoked(&self.gc_heap) {
                    return Err(self.err_type(
                        ("Cannot perform 'construct' on a proxy that has been revoked".to_string())
                            .into(),
                    ));
                }
                hops += 1;
                let handler = proxy.handler(&self.gc_heap);
                let trap_key = VmPropertyKey::String("construct");
                let trap_value =
                    match self.ordinary_get_value(context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            self.run_callable_sync(context, &getter, handler, SmallVec::new())?
                        }
                    };
                if self.is_callable_runtime(&trap_value) {
                    let target_value = proxy.target(&self.gc_heap);
                    let argv_array = self.alloc_runtime_rooted_array_from_values(
                        effective_args.iter().cloned(),
                        &[
                            &current,
                            &target_value,
                            &effective_new_target,
                            &handler,
                            &trap_value,
                        ],
                        &[effective_args.as_slice()],
                    )?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        target_value,
                        Value::array(argv_array),
                        effective_new_target,
                    ];
                    let result =
                        self.run_callable_sync(context, &trap_value, handler, trap_args)?;
                    if !result.is_object_type() {
                        return Err(self.err_type(
                            ("Proxy construct trap returned non-object".to_string()).into(),
                        ));
                    }
                    return Ok(result);
                } else if trap_value.is_undefined() || trap_value.is_null() {
                    current = proxy.target(&self.gc_heap);
                } else {
                    return Err(
                        self.err_type(("Proxy construct trap is not callable".to_string()).into())
                    );
                }
            } else {
                break;
            }
        }

        if let Some(native) = self.native_promise_constructor(&current) {
            return self.invoke_native_construct(
                context,
                native,
                &Value::undefined(),
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(native) = current.as_native_function()
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
            return self.invoke_native_construct(
                context,
                native,
                &Value::undefined(),
                &effective_new_target,
                effective_args.as_slice(),
            );
        }

        let mut proto = self.construct_prototype_for_callee_runtime_rooted(
            context,
            &effective_new_target,
            &[&current, &effective_new_target],
            &[effective_args.as_slice()],
        )?;
        if proto.is_none()
            && current
                .as_native_function()
                .is_some_and(|native| native.name(&self.gc_heap) == "Date")
        {
            proto = Some(self.constructor_prototype_value("Date")?);
        }
        // The receiver allocation roots `proto` through the ad-hoc
        // external-visit path, which — unlike the frame-stack root walk the
        // interpreter's `do_construct` uses — does not reliably relocate this
        // detached local across a moving scavenge. Park it on the traced
        // iteration-anchor stack and read the relocated handle back before it
        // becomes the receiver's `[[Prototype]]`.
        let proto_anchor = proto.map(|value| self.push_iteration_anchor(value) - 1);
        let receiver = {
            let mut value_roots: SmallVec<[&Value; 4]> =
                smallvec::smallvec![&current, &effective_new_target];
            if let Some(proto_value) = proto.as_ref() {
                value_roots.push(proto_value);
            }
            self.alloc_runtime_rooted_object_with_roots(
                value_roots.as_slice(),
                &[effective_args.as_slice()],
            )?
        };
        if let Some(index) = proto_anchor {
            proto = Some(self.iteration_anchor(index));
            self.pop_iteration_anchors_to(index);
        }
        if let Some(proto) = proto {
            crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        }
        let this_value = Value::object(receiver);

        if let Some(obj) = current.as_object()
            && let Some(native) = crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())
        {
            return self.invoke_native_construct(
                context,
                native,
                &this_value,
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(native) = current.as_native_function() {
            return self.invoke_native_construct(
                context,
                native,
                &this_value,
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(class) = current.as_class_constructor()
            && let Some(native) = class.ctor(&self.gc_heap).as_native_function()
        {
            return self.invoke_native_construct(
                context,
                native,
                &this_value,
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(class) = current.as_class_constructor() {
            current = class.ctor(&self.gc_heap);
        }

        let new_frame = self.build_construct_bytecode_frame(
            context,
            current,
            receiver,
            effective_new_target,
            effective_args,
            None,
        )?;
        let mut inner: HoltStack = HoltStack::new();
        inner.push(new_frame);
        self.dispatch_loop(context, &mut inner)
    }
}

//! Top-level execution drivers: `run`, microtask drain, dispatch entry.
//!
//! # Contents
//! `run`/`run_inner`, `link_module`, GC-heap accessors and `force_gc`,
//! microtask drain (with per-task origin contexts) and capability
//! settlement, and the `dispatch_loop`/`dispatch_loop_tracked` shells.
//!
//! # Invariants
//! Drains that run outside `run`'s rooted scope must push the
//! interpreter's extra roots before touching the heap.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Borrow the per-isolate GC heap (read-only).
    #[must_use]
    pub fn gc_heap(&self) -> &otter_gc::GcHeap {
        &self.gc_heap
    }

    /// Mutable borrow of the per-isolate GC heap.
    #[must_use]
    pub fn gc_heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        &mut self.gc_heap
    }

    /// `pub(crate)` alias used by [`crate::runtime_cx::RuntimeCx`]
    /// to forward the heap borrow without rebinding through a
    /// public method. Tracks the explicit-context migration in
    /// task 76A.
    #[must_use]
    pub(crate) fn gc_heap_for_cx(&self) -> &otter_gc::GcHeap {
        &self.gc_heap
    }

    /// `pub(crate)` mutable alias — see [`Self::gc_heap_for_cx`].
    #[must_use]
    pub(crate) fn gc_heap_for_cx_mut(&mut self) -> &mut otter_gc::GcHeap {
        &mut self.gc_heap
    }

    /// Force a full GC cycle. Runtime-owned roots are supplied through the
    /// heap's [`otter_gc::ExtraRoots`] callback so explicit GC and
    /// allocation-triggered GC use the same root walk.
    ///
    /// **Debug / test only** — production embedders let the GC
    /// trigger itself.
    pub fn force_gc(&mut self) -> Result<(), otter_gc::OutOfMemory> {
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.gc_heap.mark_phase(&mut noop)?;
        crate::collections::run_ephemeron_fixpoint(&mut self.gc_heap);
        let finalization_jobs =
            crate::weak_refs::process_weak_refs_and_finalizers(&mut self.gc_heap);
        for job in finalization_jobs {
            let mut args = SmallVec::new();
            args.push(job.held_value);
            self.microtasks.enqueue(Microtask {
                callee: job.cleanup_callback,
                this_value: Value::undefined(),
                args,
                context: job.context,
                result_capability: None,
                kind: MicrotaskKind::FinalizationCallback,
            });
        }
        self.gc_heap.sweep_phase();
        Ok(())
    }

    /// Link a freshly compiled module into this interpreter's code
    /// space. Rebases the module's function ids onto the global id
    /// space so function values created by this chunk stay callable
    /// after they escape to frames executing other chunks (the
    /// `eval` / `new Function` / dynamic-import escape paths).
    pub fn link_module(&mut self, module: otter_bytecode::BytecodeModule) -> ExecutionContext {
        code_space::CodeSpace::link(&self.code_space, module)
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`RunError`] (a `VmError` plus a stack-frame
    /// snapshot) on bytecode malformation, type mismatch, OOM,
    /// interrupt, or stack overflow.
    pub fn run(&mut self, context: &ExecutionContext) -> Result<Value, RunError> {
        // Adopt the entry chunk's code space so chunks linked during
        // this run (eval / new Function bodies) land in the same
        // function-id space as the running script. No-op for contexts
        // produced by `link_module`.
        if !std::sync::Arc::ptr_eq(&self.code_space, context.space()) {
            self.code_space = std::sync::Arc::clone(context.space());
        }
        // Remember the realm's dispatch context as the universal microtask
        // fallback. It shares the code space adopted above, so it resolves
        // function ids for any closure in the realm — a later drain of a
        // context-less job (async-resume continuation, host-settled reaction)
        // never strands for want of one.
        self.realm_context = Some(context.clone());
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        self.pending_uncaught_throw = None;
        self.pending_uncaught_frames = None;
        self.ensure_property_ic_capacity(context);
        match self.run_inner(context) {
            Ok(v) => Ok(v),
            Err((error, frames)) => Err(RunError {
                error,
                frames,
                detail: self.take_error_detail(),
            }),
        }
    }

    /// Drain the microtask queue until empty (or
    /// [`microtask::MAX_DRAIN_ITERS`] is hit).
    ///
    /// Each task is executed by invoking its callee with `this`
    /// and `args` set up at enqueue time. Tasks pushed during the
    /// drain go on the **next** generation, mirroring V8 / JSC.
    ///
    /// Foundation exception policy: the **first** error wins.
    /// The remaining queue is left in place so a follow-up
    /// `drain_microtasks` after the embedder recovers picks up
    /// where this drain stopped. Once the `Promise` constructor
    /// lands (task 34), this flips to spec semantics ("rejected
    /// promise, continue draining").
    pub fn drain_microtasks(&mut self, context: &ExecutionContext) -> Result<(), RunError> {
        self.drain_microtasks_with_default(Some(context.clone()))
    }

    /// Drain queued microtasks using each task's origin context,
    /// falling back to the caller-supplied context for jobs created
    /// inside the same VM turn. Host-settlement paths pass `None`
    /// so missing task origin is reported as an engine error.
    pub fn drain_microtasks_with_default(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        // The drain runs outside `Interpreter::run`'s rooted scope
        // (the runtime layer drains after `run` returns), so register
        // the interpreter's runtime roots here. Without this, a
        // scavenge triggered by any allocation in a microtask body —
        // including async-resume parked frames and queued reaction
        // values — would miss every root enumerated by
        // [`crate::runtime_state::RuntimeState`] (shape side tables,
        // the microtask queue itself, globalThis, module envs) and
        // free or move objects still reachable through them.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        self.drain_microtasks_with_default_inner(default_context)
    }

    pub(crate) fn drain_microtasks_with_default_inner(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        self.record_runtime_microtask_drain_started();
        let mut iters: u32 = 0;
        let mut observed_microtask_budget = false;
        loop {
            let Some(batch_len) = self.microtasks.begin_drain() else {
                return Ok(());
            };
            if batch_len == 0 {
                self.microtasks.end_drain();
                return Ok(());
            }
            // Tasks stay queue-owned (`next_in_flight`) rather than
            // being moved into a driver-local batch, so the ones
            // waiting behind the executing task remain visible to
            // the GC root walk — parked async frames in the queue
            // hold raw register slots a scavenge must rewrite.
            while let Some(task) = self.microtasks.next_in_flight() {
                if iters >= microtask::MAX_DRAIN_ITERS {
                    self.microtasks.end_drain();
                    return Err(RunError {
                        error: self.err_json(
                            "MICROTASK_RUNAWAY",
                            format!(
                                "microtask drain exceeded {} iterations",
                                microtask::MAX_DRAIN_ITERS
                            ),
                        ),
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                }
                iters += 1;
                self.record_runtime_microtask_executed();
                if !observed_microtask_budget {
                    observed_microtask_budget =
                        self.observe_runtime_microtask_budget(u64::from(iters));
                    if observed_microtask_budget && self.runtime_budget.rejects_on_exceedance() {
                        self.runtime_budget_stats.record_budget_rejection();
                        self.microtasks.end_drain();
                        return Err(RunError {
                            error: self.err_budget(
                                ("runtime microtask budget exceeded".to_string()).into(),
                            ),
                            frames: Vec::new(),
                            detail: self.take_error_detail(),
                        });
                    }
                }
                // Context resolution is uniform for every drain entry point:
                // the job's own origin context, else the caller's hint, else
                // the realm fallback captured in `run`. Only a drain before any
                // top-level run (no realm context yet) can fail here, which is
                // an engine invariant violation, not a stranded async chain.
                let context = task
                    .context
                    .clone()
                    .or_else(|| default_context.clone())
                    .or_else(|| self.realm_context.clone());
                let Some(context) = context else {
                    self.microtasks.end_drain();
                    return Err(RunError {
                        error: VmError::InvalidOperand,
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                };
                if let Err(err) = self.invoke_microtask(&context, task) {
                    self.microtasks.end_drain();
                    return Err(err);
                }
            }
            self.microtasks.end_drain();
            // Loop continues: any tasks pushed during this
            // generation get picked up by the next `begin_drain`.
            if !self.microtasks.has_any_pending() {
                return Ok(());
            }
        }
    }

    /// Invoke one microtask top-level. Builds a fresh frame stack
    /// containing just the task's callee; runs `dispatch_loop`
    /// until it returns. Errors include the snapshot of frames
    /// the task accumulated when it failed.
    pub(crate) fn invoke_microtask(
        &mut self,
        context: &ExecutionContext,
        task: Microtask,
    ) -> Result<(), RunError> {
        // Reaction-mode rejection forwarding (§27.2.1.3.2) reads the
        // abrupt completion's [[Value]] from `pending_uncaught_throw`
        // after `dispatch_loop` returns. Clear any stale payload
        // carried over from a prior microtask so we cannot read a
        // foreign reaction's value into this one.
        self.pending_uncaught_throw = None;
        // Async-resume tasks bypass callee resolution entirely:
        // the parked frame replaces a fresh callee invocation,
        // so route them to `run_async_resume` directly.
        if let MicrotaskKind::AsyncResume {
            frame,
            cold,
            await_dst,
            fulfilled,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::undefined());
            return self.run_async_resume(context, frame, cold, await_dst, fulfilled, value);
        }
        if let MicrotaskKind::AsyncGenResume {
            frame,
            cold,
            await_dst,
            fulfilled,
            owner,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::undefined());
            return self
                .run_async_gen_resume(context, frame, cold, await_dst, fulfilled, value, owner);
        }
        // Resolve callee → function_id + upvalues. Mirrors the
        // unwrap loop inside `invoke`, but for a top-level call
        // (no caller frame to write back into).
        let result_capability = task.result_capability.clone();
        let mut current = task.callee;
        let mut effective_this = task.this_value;
        let mut effective_args: SmallVec<[Value; 8]> = task.args.into_iter().collect();
        // The task left the (traced) microtask queue; from here until
        // the callee's own roots take over, these locals are the only
        // owners of the callee/this/argument values. Everything below
        // allocates before frame roots exist — the upvalue spine, the
        // `this` box, bound-function unwrapping — and a moving
        // scavenge in any of those would otherwise launder the
        // argument values into foreign heap words. Register a live
        // root over the locals for the whole invocation.
        let locals_root = MicrotaskLocalsRoot {
            current: &raw const current,
            this_value: &raw const effective_this,
            args: &raw const effective_args,
        };
        let _locals_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&locals_root));
        self.invoke_microtask_rooted(
            context,
            result_capability,
            &mut current,
            &mut effective_this,
            &mut effective_args,
        )
    }

    /// Body of [`Self::invoke_microtask`] running under the
    /// locals-root registration (see there). `current` / `this` /
    /// `args` are traced live through the caller's registration, so
    /// the collector rewrites them in place across every allocation
    /// this path performs.
    fn invoke_microtask_rooted(
        &mut self,
        context: &ExecutionContext,
        result_capability: Option<crate::microtask::MicrotaskCapability>,
        current: &mut Value,
        effective_this: &mut Value,
        effective_args: &mut SmallVec<[Value; 8]>,
    ) -> Result<(), RunError> {
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(RunError {
                    error: VmError::StackOverflow {
                        limit: self.max_stack_depth,
                    },
                    frames: Vec::new(),
                    detail: self.take_error_detail(),
                });
            }
            if let Some(bound) = current.as_bound_function() {
                hops += 1;
                let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args.drain(..));
                *effective_this = bound_this;
                *effective_args = combined;
                *current = target;
            } else if let Some(cc) = current.as_class_constructor() {
                hops += 1;
                *current = cc.ctor(&self.gc_heap);
            } else {
                break;
            }
        }
        // Native callables run inline at the drain site: no frame
        // push, no return register. Errors propagate as RunError.
        if let Some(native) = current.as_native_function() {
            let native = &native;
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                return match self.run_vm_intrinsic_sync(
                    context,
                    intrinsic,
                    *effective_this,
                    std::mem::take(effective_args),
                ) {
                    Ok(value) => {
                        self.settle_microtask_capability(context, result_capability, Ok(value));
                        Ok(())
                    }
                    Err(vm_err) => {
                        if result_capability.is_some() {
                            let reason = vm_err_to_value(self, &vm_err);
                            self.settle_microtask_capability(
                                context,
                                result_capability,
                                Err(reason),
                            );
                            Ok(())
                        } else {
                            Err(RunError {
                                error: vm_err,
                                frames: Vec::new(),
                                detail: self.take_error_detail(),
                            })
                        }
                    }
                };
            }
            let call_info = NativeCallInfo::call(*effective_this);
            self.record_runtime_native_call();
            let mut ctx = NativeCtx::new_with_call_info_and_context(self, call_info, Some(context));
            return match call.invoke(&mut ctx, effective_args.as_slice()) {
                Ok(value) => {
                    self.settle_microtask_capability(context, result_capability, Ok(value));
                    Ok(())
                }
                Err(err) => {
                    let vm_err = native_to_vm_error(self, err);
                    if result_capability.is_some() {
                        // Reaction-mode: route the error into the
                        // downstream promise as a rejection rather
                        // than aborting the drain. If a sub-dispatch
                        // (e.g. `run_callable_sync` from within the
                        // native body) caught a user `throw`, the
                        // original `Value` was stashed on
                        // `pending_uncaught_throw` — prefer it over a
                        // stringified `vm_err_to_value` rendering so
                        // identity is preserved per §27.2.1.3.2 step
                        // 1.f.iii.
                        let reason = self
                            .pending_uncaught_throw
                            .take()
                            .unwrap_or_else(|| vm_err_to_value(self, &vm_err));
                        self.settle_microtask_capability(context, result_capability, Err(reason));
                        Ok(())
                    } else {
                        Err(RunError {
                            error: vm_err,
                            frames: Vec::new(),
                            detail: self.take_error_detail(),
                        })
                    }
                }
            };
        }
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            _new_target_for_callee,
            _derived_this_cell,
            _callee_env,
        ) = match Self::bytecode_call_target_parts(*current, *effective_this, &self.gc_heap) {
            Ok(parts) => parts,
            Err(error) => {
                return Err(RunError {
                    error,
                    frames: Vec::new(),
                    detail: self.take_error_detail(),
                });
            }
        };
        let function = match context.exec_function(function_id) {
            Some(f) => f,
            None => {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                    detail: self.take_error_detail(),
                });
            }
        };
        let upvalues =
            match Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues) {
                Ok(u) => u,
                Err(oom) => {
                    return Err(RunError {
                        error: VmError::from(oom),
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                }
            };
        let this_for_callee = match self.this_for_bytecode_call_runtime_rooted(
            function,
            this_for_callee,
            &[effective_args.as_slice()],
        ) {
            Ok(value) => value,
            Err(error) => {
                return Err(RunError {
                    error,
                    frames: Vec::new(),
                    detail: self.take_error_detail(),
                });
            }
        };
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None, // top-level — no return register
            upvalues,
            this_for_callee,
        );
        self.bind_bytecode_call_arguments(function, &mut new_frame, std::mem::take(effective_args))
            .map_err(|error| RunError {
                error,
                frames: Vec::new(),
                detail: self.take_error_detail(),
            })?;
        let mut stack: HoltStack = HoltStack::new();
        stack.push(new_frame);
        match self.dispatch_loop(context, &mut stack) {
            Ok(value) => {
                // Reaction job: settle the downstream promise with
                // the handler's return value (spec §27.2.5.4).
                self.settle_microtask_capability(context, result_capability, Ok(value));
                Ok(())
            }
            Err(error) => {
                if result_capability.is_some() {
                    // Reaction-mode unwind: route the abrupt
                    // completion's [[Value]] into the downstream
                    // promise as a rejection per ECMA-262
                    // §27.2.1.3.2 PromiseReactionJob step 1.f.iii.
                    // Spec requires the *original* thrown value, not
                    // a stringified `VmError::Uncaught` rendering;
                    // [`Self::unwind_throw_with_uncaught`] preserves
                    // it on `pending_uncaught_throw` for exactly this
                    // hop.
                    let reason = self
                        .pending_uncaught_throw
                        .take()
                        .unwrap_or_else(|| vm_err_to_value(self, &error));
                    self.settle_microtask_capability(context, result_capability, Err(reason));
                    Ok(())
                } else {
                    let frames = snapshot_frames(context, &stack);
                    Err(RunError {
                        error,
                        frames,
                        detail: self.take_error_detail(),
                    })
                }
            }
        }
    }

    /// Resolve / reject the downstream promise that a reaction
    /// job belongs to. No-op when `cap` is `None` (plain
    /// `queueMicrotask` callbacks).
    pub(crate) fn settle_microtask_capability(
        &mut self,
        context: &ExecutionContext,
        cap: Option<microtask::MicrotaskCapability>,
        outcome: Result<Value, Value>,
    ) {
        let Some(cap) = cap else {
            return;
        };
        let (callee, args): (Value, SmallVec<[Value; 4]>) = match outcome {
            Ok(v) => (cap.resolve, smallvec::smallvec![v]),
            Err(reason) => (cap.reject, smallvec::smallvec![reason]),
        };
        // Settling enqueues another microtask so the resolve/
        // reject native runs in a fresh job (matches spec
        // ordering — the next reaction picks it up on the next
        // generation).
        self.microtasks.enqueue(Microtask {
            callee,
            this_value: Value::undefined(),
            args,
            context: Some(context.clone()),
            result_capability: None,
            kind: microtask::MicrotaskKind::Call,
        });
    }

    /// Internal driver. Pulls the snapshot capture out of the
    /// dispatch loop so the hot path remains allocation-free; the
    /// snapshot is built only when a `VmError` actually escapes.
    pub(crate) fn run_inner(
        &mut self,
        context: &ExecutionContext,
    ) -> Result<Value, (VmError, Vec<StackFrameSnapshot>)> {
        let main = context.exec_main();
        let mut stack: HoltStack = HoltStack::new();
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, main, Frame::empty_upvalues())
                .map_err(|oom| (VmError::from(oom), Vec::new()))?;
        let entry_this = if main.is_module {
            Value::undefined()
        } else {
            Value::object(self.global_this)
        };
        let entry = Frame::with_exec_return_upvalues_and_this(main, None, upvalues, entry_this);
        let entry_is_async = main.is_async;
        stack.push(entry);
        // §16.2.1.7 ModuleDeclarationInstantiation step 5 — when the
        // entry function carries top-level await, wire up an async
        // result promise so `Op::Await` can park / resume normally.
        // The dispatch loop's exit returns the result promise's
        // resolved value once microtasks drain.
        let entry_promise = if entry_is_async {
            let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, &stack, &[], &[])
                .map_err(|oom| (VmError::from(oom), Vec::new()))?;
            stack
                .last_mut()
                .expect("entry frame was just pushed")
                .async_state = Some(AsyncFrameState {
                result_promise: result,
            });
            Some(result)
        } else {
            None
        };

        // Park the entry promise on the scratch root stack: once the
        // async entry frame settles and is popped, nothing else roots
        // the handle, so the microtask drain's allocations would leave
        // a bare local pointing at the promise body's vacated slot.
        let entry_promise_root = entry_promise.map(|p| self.json_root_push(Value::promise(p)));

        let dispatch_result = self.dispatch_loop(context, &mut stack);
        match dispatch_result {
            Ok(value) => {
                if let Some(root_idx) = entry_promise_root {
                    // Drain microtasks until the entry promise
                    // settles. The settled value (or rejection)
                    // becomes the program's completion value.
                    if let Err(err) = self.drain_microtasks_with_default(Some(context.clone())) {
                        self.json_root_pop_to(root_idx);
                        return Err((err.error, err.frames));
                    }
                    let promise = self
                        .json_root_get(root_idx)
                        .as_promise()
                        .expect("entry promise stays a promise across the drain");
                    let state = promise.state(&self.gc_heap);
                    self.json_root_pop_to(root_idx);
                    match state {
                        crate::promise::PromiseState::Fulfilled(v) => return Ok(v),
                        crate::promise::PromiseState::Rejected(reason) => {
                            return Err((
                                self.err_uncaught((self.render_thrown(&reason)).into()),
                                Vec::new(),
                            ));
                        }
                        crate::promise::PromiseState::Pending => return Ok(Value::undefined()),
                    }
                }
                Ok(value)
            }
            Err(err) => {
                if let Some(root_idx) = entry_promise_root {
                    self.json_root_pop_to(root_idx);
                }
                let frames = self
                    .pending_uncaught_frames
                    .take()
                    .unwrap_or_else(|| snapshot_frames(context, &stack));
                Err((err, frames))
            }
        }
    }

    /// Thin wrapper around [`Self::dispatch_loop_tracked`] that publishes
    /// `stack` as the interpreter's [`Self::active_frame_stack`] for the
    /// duration of the loop, restoring the parent pointer on exit. This
    /// is how inline native calls (the `Error` constructor,
    /// `Error.captureStackTrace`) reach the live JS call stack — the
    /// same role V8's isolate-owned `StackFrameIterator` plays. Nested
    /// dispatch loops (sync callbacks, generator drives) save/restore so
    /// the pointer always names the innermost executing stack.
    pub(crate) fn dispatch_loop(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
    ) -> Result<Value, VmError> {
        let previous = self.active_frame_stack;
        self.active_frame_stack = stack as *const HoltStack;
        // A nested dispatch allocates its frames' register windows above the
        // caller's flat-stack cursor; on exit clamp the cursor down to release
        // any window a non-locally-exited sub-frame left behind. Only ever
        // LOWER it: when this loop entered on a re-entry frame (its window was
        // allocated below the saved cursor by the caller and then reclaimed as
        // that frame returned here), the cursor is already below `saved` and
        // raising it back would re-leak that window on every `run_callable_sync`.
        let saved_reg_top = self.reg_top;
        let result = self.dispatch_loop_tracked(context, stack);
        self.reg_top = self.reg_top.min(saved_reg_top);
        self.active_frame_stack = previous;
        result
    }

    /// Drive the dispatch loop, converting convertible `VmError`
    /// variants (TypeMismatch, NotCallable, TemporalDeadZone,
    /// OutOfMemory, etc.)
    /// into typed `Error` instances that flow through `unwind_throw`
    /// — so user code can `try { … } catch (e) { e instanceof
    /// TypeError }` and observe the same shape it would in any
    /// spec-conforming engine. Variants that aren't user-recoverable
    /// (StackOverflow, Interrupted, Uncaught, MissingReturn,
    /// InvalidOperand) propagate as-is.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-objects>
    /// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
    pub(crate) fn dispatch_loop_tracked(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
    ) -> Result<Value, VmError> {
        self.ensure_property_ic_capacity(context);
        self.begin_runtime_budget_turn();
        let frame_roots = otter_gc::RawFrameRoots::new(
            stack as *const HoltStack,
            &self.cold_frames as *const cold_frame::ColdFramePool,
            trace_active_frame_roots,
        );
        let frame_root_provider: &dyn otter_gc::FrameRoots = &frame_roots;
        let frame_roots_guard = self
            .gc_heap
            .register_frame_roots(frame_root_provider as *const dyn otter_gc::FrameRoots);
        // Catch-all runtime-roots registration: every bytecode tick
        // can allocate, and some dispatch entries (generator
        // prologues spawned from host-driven drains, future embedder
        // entry points) reach here without an enclosing rooted scope.
        // The heap dedupes same-source stack entries, so re-pushing
        // under `run` / `run_callable_sync` costs one Vec slot.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        // Nested dispatch must not leak its last-instruction byte length
        // into the caller's PC advance: helpers like Op::Eval invoke
        // dispatch_loop on a sub-stack and then expect
        // self.current_byte_len to still describe the *outer* opcode
        // when they call frame.advance_pc(self.current_byte_len).
        let saved_byte_len = self.current_byte_len;
        let result = (|| -> Result<Value, VmError> {
            loop {
                match self.dispatch_loop_inner(context, stack) {
                    Ok(value) => break Ok(value),
                    Err(err) => {
                        if matches!(err, VmError::Uncaught)
                            && !stack.is_empty()
                            && let Some(thrown) = self.pending_uncaught_throw.take()
                        {
                            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                            let unwind = self.unwind_throw(context, stack, thrown);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            } else {
                                // No handler in THIS dispatch stack —
                                // restore the original thrown value so
                                // an outer dispatch loop (across a
                                // native boundary) can still unwind
                                // with identity intact instead of the
                                // rendered string.
                                self.pending_uncaught_throw = Some(thrown);
                            }
                            unwind?;
                            if stack.is_empty() {
                                break Ok(Value::undefined());
                            }
                            continue;
                        }
                        if let Some(thrown) =
                            self.vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
                        {
                            let uncaught = if matches!(
                                err,
                                VmError::OutOfMemory { .. } | VmError::JsonError
                            ) {
                                Some(err)
                            } else {
                                None
                            };
                            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                            let unwind =
                                self.unwind_throw_with_uncaught(context, stack, thrown, uncaught);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            }
                            unwind?;
                            if stack.is_empty() {
                                break Ok(Value::undefined());
                            }
                            continue;
                        }
                        break Err(err);
                    }
                }
            }
        })();
        drop(extra_roots_guard);
        drop(frame_roots_guard);
        self.finish_runtime_budget_turn();
        self.current_byte_len = saved_byte_len;
        result
    }
}

/// Live root over [`Interpreter::invoke_microtask`]'s
/// callee/this/argument locals: the values leave the traced microtask
/// queue before any callee-side roots exist, and every allocation on
/// the invocation path (upvalue spine, `this` boxing, bound-arg
/// concatenation) can drive a moving scavenge that would otherwise
/// launder them. Raw pointers because the locals are mutated
/// (bound-function unwrapping) while registered; the registration is
/// popped before the locals drop.
struct MicrotaskLocalsRoot {
    current: *const Value,
    this_value: *const Value,
    args: *const SmallVec<[Value; 8]>,
}

impl otter_gc::ExtraRootSource for MicrotaskLocalsRoot {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: `invoke_microtask` pops this registration before the
        // pointed-at locals go out of scope, so the reads always see
        // the live locals.
        unsafe {
            (*self.current).trace_value_slots(visitor);
            (*self.this_value).trace_value_slots(visitor);
            for value in (*self.args).iter() {
                value.trace_value_slots(visitor);
            }
        }
    }
}

//! # Otter JS interpreter
//!
//! Bytecode-level interpreter for the Otter JS VM. This module houses the
//! `Interpreter` shell (entry points: `run`, `execute`, `resume`, `call_function`)
//! and the `RuntimeState` god-object (heap, realms, intrinsics, native functions).
//!
//! ## Submodule index
//!
//! | Module                        | Purpose                                               |
//! |-------------------------------|-------------------------------------------------------|
//! | `activation`                  | Per-frame state (registers, PC, upvalues).            |
//! | `dispatch`                    | `step()` match + dispatch helpers.                    |
//! | `error`                       | `InterpreterError` enum + conversions.                |
//! | `execution_result`            | `ExecutionResult` return type.                        |
//! | `frame_runtime`               | Property inline-cache storage per frame.              |
//! | `number_conv`                 | §7.1.6/7 f64 → i32/u32 + StringToNumber.              |
//! | `runtime_state`               | `RuntimeState` struct + thematic impl submodules.     |
//! | `runtime_state::alloc`        | Heap allocation, gc_safepoint, install, closures.     |
//! | `runtime_state::call`         | call_callable / construct_callable / promises.         |
//! | `runtime_state::coercion`     | §7 abstract ops (ToString, ToNumber, ==, +, etc.).     |
//! | `runtime_state::eval`         | eval_source re-entry into the source compiler.        |
//! | `runtime_state::iterators`    | Iterator protocol + generator resume kernels.         |
//! | `runtime_state::proxy`        | ECMA-262 §10.5 Proxy traps.                           |
//! | `step_outcome`                | `StepOutcome`, `Completion`, `ToPrimitiveHint`.       |
//! | `tests`                       | `#[cfg(test)]` unit suite.                            |

mod activation;
mod dispatch;
mod error;
mod execution_result;
mod frame_runtime;
mod number_conv;
mod runtime_state;
mod step_outcome;

#[cfg(test)]
mod tests;

pub use activation::Activation;
pub use error::InterpreterError;
pub use execution_result::ExecutionResult;
use frame_runtime::FrameRuntimeState;
pub(crate) use step_outcome::ToPrimitiveHint;
use step_outcome::{Completion, StepOutcome, TailCallPayload};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bytecode::ProgramCounter;
use crate::descriptors::VmNativeCallError;
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::host::NativeFunctionRegistry;
use crate::intrinsics::WellKnownSymbol;
use crate::module::{Function, FunctionIndex, Module};
use crate::object::{HeapValueKind, ObjectHandle, ObjectHeap, PropertyInlineCache};
use crate::payload::NativePayloadRegistry;
use crate::property::PropertyNameRegistry;
use crate::value::RegisterValue;
use std::collections::BTreeMap;

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const BOOLEAN_DATA_SLOT: &str = "__otter_boolean_data__";
const ERROR_DATA_SLOT: &str = "__otter_error_data__";
const EXECUTION_INTERRUPTED_MESSAGE: &str = "execution interrupted";


/// Shared execution runtime for one interpreter/JIT run.
pub struct RuntimeState {
    /// §9.3 — Realm records owned by this runtime. The vector is grown only
    /// (entries are never removed), so [`crate::realm::RealmId`] indices
    /// remain stable for the lifetime of the runtime.
    pub(super) realms: Vec<crate::realm::Realm>,
    /// §9.4.1 \[\[Realm\]\] of the running execution context. Updated by the
    /// interpreter when crossing realm boundaries (e.g. via `$262.createRealm`
    /// or future cross-realm proxies).
    pub(super) current_realm: crate::realm::RealmId,
    pub(super) objects: ObjectHeap,
    pub(super) property_names: PropertyNameRegistry,
    pub(super) native_functions: NativeFunctionRegistry,
    pub(super) native_payloads: NativePayloadRegistry,
    pub(super) microtasks: crate::microtask::MicrotaskQueue,
    pub(super) host_callbacks: crate::host_callbacks::HostCallbackQueue,
    pub(super) timers: crate::event_loop::TimerRegistry,
    pub(super) console_backend: Box<dyn crate::console::ConsoleBackend>,
    pub(super) current_module: Option<Module>,
    pub(super) native_call_construct_stack: Vec<bool>,
    pub(super) native_callee_stack: Vec<ObjectHandle>,
    /// V8 stack trace API — shadow stack of execution-context activations.
    /// Pushed at the entry of every JS frame (closure, generator resume,
    /// async resume) and popped at exit; updated in-place each interpreter
    /// step so the topmost entry's PC always tracks `activation.pc()`.
    /// Reference: <https://v8.dev/docs/stack-trace-api>
    pub(super) frame_info_stack: Vec<crate::stack_frame::StackFrameInfo>,
    pub(super) next_symbol_id: u32,
    pub(super) symbol_descriptions: BTreeMap<u32, Option<Box<str>>>,
    pub(super) global_symbol_registry: BTreeMap<Box<str>, u32>,
    pub(super) global_symbol_registry_reverse: BTreeMap<u32, Box<str>>,
    /// §6.2.12 Monotonic counter for unique private class identifiers.
    /// Spec: <https://tc39.es/ecma262/#sec-private-names>
    pub(super) next_class_id: u64,
    /// §14.4.4 Transient: set by `YieldStar` opcode, consumed by
    /// the generator resume loop when handling `GeneratorYield`.
    pub(super) pending_delegation_iterator: Option<ObjectHandle>,
    /// Pending uncaught throw value, stashed by host integration code that
    /// converts an `InterpreterError::UncaughtThrow` into a textual native
    /// error so the **outer** runtime layer can later promote the error
    /// back to a structured `JsRuntimeDiagnostic`. This avoids losing the
    /// thrown value across the host module-loader boundary, where the
    /// natural error type is `String`.
    ///
    /// `None` once consumed; populated lazily and cleared by the next
    /// `take_pending_uncaught_throw` call.
    pub(super) pending_uncaught_throw: Option<RegisterValue>,
    /// Per-host-run interrupt signal inherited by JS callbacks entered from
    /// microtasks, timers, host callbacks, accessors, and promise handlers.
    pub(super) active_interrupt_flag: Option<Arc<AtomicBool>>,
    /// §B.3.5.2 — Nesting depth for class field initializer execution.
    /// When > 0, direct eval applies additional restrictions (ContainsArguments,
    /// Contains SuperCall). Incremented by RunClassFieldInitializer, decremented
    /// when the field init function returns.
    pub(super) field_initializer_depth: u32,
    /// Persistent FeedbackVectors per function — survives across activations.
    /// The interpreter writes to these during execution, and the JIT reads
    /// them for speculation decisions. Keyed by FunctionIndex.
    ///
    /// V8 model: FeedbackVector lives on SharedFunctionInfo, not on stack frame.
    feedback_vectors: std::collections::HashMap<crate::FunctionIndex, crate::feedback::FeedbackVector>,
}


/// Minimal interpreter shell for the new VM backend.
#[derive(Debug, Clone)]
pub struct Interpreter {
    /// Cooperative interrupt flag — when set to `true` by an external thread
    /// (e.g. a watchdog timer), the interpreter stops at the next back-edge.
    /// This mirrors V8's `TerminateExecution` / JSC's `VMTraps::fireTrap()`
    /// pattern: the flag is an `Arc<AtomicBool>` shared with the caller.
    /// Checked only on backward jumps (loop back-edges), so the cost is one
    /// `Relaxed` atomic load per loop iteration (~1-2 CPU cycles, branch
    /// predicted not-taken >99.999% of the time).
    interrupt_flag: Option<Arc<AtomicBool>>,
    /// Out-of-memory flag shared with the underlying object heap. Set by
    /// the allocator/reservation paths in [`otter_gc::typed::TypedHeap`]
    /// when the configured `max_heap_bytes` cap is crossed. Polled at the
    /// same GC safepoints as `interrupt_flag` and surfaced to the host as
    /// [`InterpreterError::OutOfMemory`].
    oom_flag: Option<Arc<AtomicBool>>,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// Creates a new interpreter instance with no interrupt mechanism.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interrupt_flag: None,
            oom_flag: None,
        }
    }

    /// Sets a cooperative interrupt flag.  The caller retains a clone of the
    /// `Arc<AtomicBool>` and can set it to `true` from any thread to request
    /// termination.  The interpreter checks the flag on every backward jump
    /// (loop back-edge) — one `Relaxed` atomic load per loop iteration.
    #[must_use]
    pub fn with_interrupt_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.interrupt_flag = Some(flag);
        self
    }

    /// Attaches the OOM signal flag owned by the runtime's object heap.
    /// When set, the interpreter raises [`InterpreterError::OutOfMemory`]
    /// at the next GC safepoint.
    #[must_use]
    pub fn with_oom_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.oom_flag = Some(flag);
        self
    }

    /// Returns a shareable interrupt flag, creating one if needed.
    pub fn interrupt_flag(&mut self) -> Arc<AtomicBool> {
        if let Some(ref flag) = self.interrupt_flag {
            Arc::clone(flag)
        } else {
            let flag = Arc::new(AtomicBool::new(false));
            self.interrupt_flag = Some(Arc::clone(&flag));
            flag
        }
    }

    /// Creates an interpreter configured with the runtime's active interrupt
    /// and OOM signals. Use this for every JS re-entry from runtime/native
    /// code instead of constructing an uninterruptible interpreter.
    #[must_use]
    pub fn for_runtime(runtime: &RuntimeState) -> Self {
        let mut interpreter = Self::new().with_oom_flag(runtime.oom_flag());
        if let Some(flag) = runtime.active_interrupt_flag() {
            interpreter = interpreter.with_interrupt_flag(flag);
        }
        interpreter
    }

    /// Checks the interrupt and OOM flags; returns an error if either is set.
    /// The OOM check is evaluated after the interrupt check so that a script
    /// receiving both signals (e.g. OOM inside a timeout-interrupted loop)
    /// still surfaces the timeout first.
    #[inline]
    fn check_interrupt(&self) -> Result<(), InterpreterError> {
        if let Some(ref flag) = self.interrupt_flag
            && flag.load(Ordering::Relaxed)
        {
            return Err(InterpreterError::Interrupted);
        }
        if let Some(ref flag) = self.oom_flag
            && flag.load(Ordering::Relaxed)
        {
            return Err(InterpreterError::OutOfMemory);
        }
        Ok(())
    }

    /// Merge frame-local feedback into the persistent FeedbackVector on RuntimeState.
    /// Called when a function returns or throws, so feedback accumulates across invocations.
    fn persist_feedback(
        runtime: &mut RuntimeState,
        function_index: crate::FunctionIndex,
        function: &crate::module::Function,
        frame_runtime: &FrameRuntimeState,
    ) {
        let persistent = runtime.get_or_create_feedback(function_index, function);
        // Merge: for each slot, take the max (monotonic lattice) from frame feedback.
        let frame_fv = frame_runtime.feedback();
        for (i, slot_data) in frame_fv.slots().iter().enumerate() {
            let id = crate::feedback::FeedbackSlotId(i as u16);
            match slot_data {
                crate::feedback::FeedbackSlotData::Arithmetic(fb) => {
                    persistent.record_arithmetic(id, *fb);
                }
                crate::feedback::FeedbackSlotData::Comparison(fb) => {
                    persistent.record_comparison(id, *fb);
                }
                crate::feedback::FeedbackSlotData::Branch(fb) => {
                    // Branch counters: add (saturating).
                    for _ in 0..fb.taken { persistent.record_branch(id, true); }
                    for _ in 0..fb.not_taken { persistent.record_branch(id, false); }
                }
                crate::feedback::FeedbackSlotData::Property(fb) => {
                    if let Some(cache) = fb.as_monomorphic() {
                        persistent.record_property(id, cache.shape_id(), cache.slot_index());
                    }
                }
                crate::feedback::FeedbackSlotData::Call(fb) => {
                    if let Some(target) = fb.as_monomorphic() {
                        persistent.record_call(id, target);
                    }
                }
            }
        }
    }

    /// Creates an entry activation for the module entry function.
    #[must_use]
    pub fn prepare_entry(module: &Module) -> Activation {
        let function = module.entry_function();
        let register_count = function.frame_layout().register_count();
        let mut activation = Activation::new(module.entry(), register_count);
        if function.frame_layout().receiver_slot().is_some() {
            activation
                .set_receiver(function, RegisterValue::undefined())
                .expect("entry receiver slot must exist when reserved");
        }
        activation
    }

    /// Executes a module from its entry function with a fresh runtime.
    pub fn execute(&self, module: &Module) -> Result<ExecutionResult, InterpreterError> {
        let mut runtime = RuntimeState::new();
        self.execute_module(module, &mut runtime)
    }

    /// Executes a module using an existing runtime state.
    /// Used by the event loop driver and embedders.
    pub fn execute_module(
        &self,
        module: &Module,
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        let mut activation = Self::prepare_entry(module);
        let function = module.entry_function();
        if function.frame_layout().receiver_slot().is_some() {
            let global = runtime.intrinsics().global_object();
            activation.set_receiver(function, RegisterValue::from_object_handle(global.0))?;
        }
        self.run_with_runtime(module, &mut activation, runtime)
    }

    /// §19.2.1.1 PerformEval for direct eval with an enclosing closure
    /// context. The eval'd code is compiled into a module and executed via
    /// a closure that inherits the caller's `class_id` (for private name
    /// resolution), `home_object` (for `super.x`), and `this` binding.
    ///
    /// This makes `this.#f` / `super.x` / `this.foo` work inside direct
    /// eval even though the eval code is compiled as a separate module.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-performeval>
    pub fn eval_source_direct(
        runtime: &mut RuntimeState,
        source: &str,
        caller_closure: Option<ObjectHandle>,
        caller_this: RegisterValue,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let source_url = "<direct-eval>";

        // §B.3.5.2 — If inside a field initializer, apply additional early
        // error rules (ContainsArguments, Contains SuperCall).
        let in_field_init = runtime.field_initializer_depth > 0;
        let module = if in_field_init {
            crate::source::compile_eval_field_init(source, source_url)
        } else {
            crate::source::compile_eval(source, source_url)
        }
        .map_err(|e| runtime.alloc_syntax_error(&format!("eval: {e}")))?;

        // §19.2.1.1 — When eval runs in the context of a function, the
        // eval'd code inherits that function's execution context. Create a
        // closure for the eval entry function and copy the caller's
        // closure state (class_id, home_object) into it. Then invoke the
        // closure via call_function with the caller's `this` as receiver.
        if let Some(caller) = caller_closure {
            let caller_class_id = runtime.objects.closure_class_id(caller).unwrap_or(0);
            let caller_home = runtime.objects.closure_home_object(caller).unwrap_or(None);
            let caller_realm = runtime
                .objects
                .function_realm(caller)
                .unwrap_or(None)
                .unwrap_or(runtime.current_realm);

            let entry_index = module.entry();
            let eval_closure = runtime.objects.alloc_closure(
                module.clone(),
                entry_index,
                Vec::new(),
                crate::object::ClosureFlags::normal(),
                caller_realm,
            );
            if caller_class_id != 0 {
                let _ = runtime
                    .objects
                    .set_closure_class_id(eval_closure, caller_class_id);
            }
            if let Some(home) = caller_home {
                let _ = runtime.objects.set_closure_home_object(eval_closure, home);
            }

            return Self::call_function(runtime, &module, eval_closure, caller_this, &[]).map_err(
                |e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("eval: {other}").into()),
                },
            );
        }

        // Fallback (no caller closure): run as standalone module.
        let interpreter = Interpreter::for_runtime(runtime);
        let result = interpreter
            .execute_module(&module, runtime)
            .map_err(|e| match e {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                other => VmNativeCallError::Internal(format!("eval: {other}").into()),
            })?;
        Ok(result.return_value())
    }

    /// Calls a JS function (host function or closure) by ObjectHandle.
    ///
    /// This is the entry point for the event loop to invoke timer callbacks,
    /// promise reaction handlers, and microtask callbacks. It handles both
    /// native host functions and compiled closures.
    pub fn call_function(
        runtime: &mut RuntimeState,
        _module: &Module,
        callable: ObjectHandle,
        this_value: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, InterpreterError> {
        let kind = runtime.objects.kind(callable)?;
        match kind {
            HeapValueKind::HostFunction => {
                match Self::invoke_host_function_handle(runtime, callable, this_value, arguments)? {
                    Completion::Return(value) => Ok(value),
                    Completion::Throw(value) => Err(InterpreterError::UncaughtThrow(value)),
                }
            }
            HeapValueKind::Closure => {
                if runtime
                    .objects
                    .closure_flags(callable)
                    .is_ok_and(|flags| flags.is_class_constructor())
                {
                    return Err(InterpreterError::TypeError(
                        "Class constructor cannot be invoked without 'new'".into(),
                    ));
                }

                let is_async = runtime
                    .objects
                    .closure_flags(callable)
                    .is_ok_and(|flags| flags.is_async());

                let module = runtime.objects.closure_module(callable)?;
                let callee_index = runtime.objects.closure_callee(callable)?;
                let callee_function = module
                    .function(callee_index)
                    .ok_or(InterpreterError::InvalidCallTarget)?;
                let register_count = callee_function.frame_layout().register_count();
                // Pass the closure handle so the activation can access upvalues.
                let mut activation = Activation::with_context(
                    callee_index,
                    register_count,
                    FrameMetadata::default(),
                    Some(callable),
                );

                // Set up receiver.
                if callee_function.frame_layout().receiver_slot().is_some() {
                    activation.set_receiver(callee_function, this_value)?;
                }

                // Copy arguments into parameter slots.
                let param_count = callee_function.frame_layout().parameter_count();
                for (i, &arg) in arguments.iter().take(param_count as usize).enumerate() {
                    let abs = callee_function
                        .frame_layout()
                        .resolve_user_visible(i as u16)
                        .ok_or(InterpreterError::RegisterOutOfBounds)?;
                    activation.set_register(abs, arg)?;
                }

                // ES2024 §10.4.4: Preserve overflow arguments for CreateArguments.
                if arguments.len() > param_count as usize {
                    activation.overflow_args = arguments[param_count as usize..].to_vec();
                }
                // Store actual argument count in metadata.
                activation.metadata =
                    FrameMetadata::new(arguments.len() as RegisterIndex, FrameFlags::default());

                if is_async {
                    // §27.7.5.1 AsyncFunctionStart — create a result promise,
                    // execute the body, and settle the promise on completion.
                    Self::execute_async_function_body(runtime, &module, &mut activation)
                } else {
                    let interpreter = Interpreter::for_runtime(runtime);
                    let result = interpreter.run_with_runtime(&module, &mut activation, runtime)?;
                    Ok(result.return_value())
                }
            }
            HeapValueKind::PromiseCapabilityFunction => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                Self::invoke_promise_capability_function(runtime, callable, value)?;
                Ok(RegisterValue::undefined())
            }
            HeapValueKind::PromiseCombinatorElement => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                Self::invoke_promise_combinator_element(runtime, callable, value)
            }
            HeapValueKind::PromiseFinallyFunction => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                Self::invoke_promise_finally_function(runtime, callable, value)
            }
            HeapValueKind::PromiseValueThunk => {
                // §27.2.5.3.1 step 8 / §27.2.5.3.2 step 8
                let (thunk_value, thunk_kind) = runtime
                    .objects
                    .promise_value_thunk_info(callable)
                    .ok_or(InterpreterError::InvalidHeapValueKind)?;
                match thunk_kind {
                    crate::promise::PromiseFinallyKind::ThenFinally => Ok(thunk_value),
                    crate::promise::PromiseFinallyKind::CatchFinally => {
                        Err(InterpreterError::UncaughtThrow(thunk_value))
                    }
                }
            }
            _ => Err(InterpreterError::TypeError(
                format!("{kind:?} is not a function").into(),
            )),
        }
    }

    /// Executes an async function body, wrapping the result in a Promise.
    ///
    /// ES2024 §27.7.5.1 AsyncFunctionStart
    /// Spec: <https://tc39.es/ecma262/#sec-async-functions-abstract-operations-async-function-start>
    ///
    /// Creates a result promise, runs the function body via `run_completion_with_runtime`,
    /// and settles the promise based on the outcome (return → resolve, throw → reject).
    fn execute_async_function_body(
        runtime: &mut RuntimeState,
        module: &Module,
        activation: &mut Activation,
    ) -> Result<RegisterValue, InterpreterError> {
        // §27.7.5.1 step 2: Let promiseCapability be ! NewPromiseCapability(%Promise%).
        let proto = runtime.intrinsics().promise_prototype();
        let promise = runtime.objects.alloc_promise_with_proto(proto);
        let resolve = runtime
            .objects
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Fulfill);
        let reject = runtime
            .objects
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Reject);
        let capability = crate::promise::PromiseCapability {
            promise,
            resolve,
            reject,
        };

        // §27.7.5.1 step 4: Execute the async function body.
        let interpreter = Interpreter::for_runtime(runtime);
        let result = interpreter.run_completion_with_runtime(module, activation, runtime);

        match result {
            Ok(Completion::Return(return_value)) => {
                // §27.7.5.1 step 4.a: Function completed normally — resolve the promise.
                Self::invoke_promise_capability_function(
                    runtime,
                    capability.resolve,
                    return_value,
                )?;
            }
            Ok(Completion::Throw(thrown)) => {
                // §27.7.5.1 step 4.c: Function threw — reject the promise.
                Self::invoke_promise_capability_function(runtime, capability.reject, thrown)?;
            }
            Err(InterpreterError::UncaughtThrow(thrown)) => {
                // Uncaught exception — reject the promise.
                Self::invoke_promise_capability_function(runtime, capability.reject, thrown)?;
            }
            Err(e) => return Err(e),
        }

        Ok(RegisterValue::from_object_handle(capability.promise.0))
    }

    /// Invokes a PromiseCapabilityFunction (resolve or reject) with a value.
    /// ES2024 §27.2.1.3.1 Promise Reject Functions / §27.2.1.3.2 Promise Resolve Functions
    fn invoke_promise_capability_function(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        let (promise_handle, kind) = runtime
            .objects
            .promise_capability_function_info(callable)
            .ok_or_else(|| {
                InterpreterError::TypeError("not a promise capability function".into())
            })?;

        let promise = runtime
            .objects
            .get_promise_mut(promise_handle)
            .ok_or_else(|| {
                InterpreterError::TypeError("promise capability target is not a promise".into())
            })?;

        // §27.2.1.3: If alreadyResolved is true, return undefined.
        // We use is_pending() — once settled, further calls are no-ops.
        if !promise.is_pending() {
            return Ok(());
        }

        let jobs = match kind {
            crate::promise::ReactionKind::Fulfill => {
                // §27.2.1.3.2 step 8: If value is the same promise, reject with TypeError.
                if let Some(h) = value.as_object_handle() {
                    if h == promise_handle.0 {
                        let err_handle = runtime
                            .alloc_type_error("A promise cannot be resolved with itself")
                            .map_err(|_| InterpreterError::InvalidHeapValueKind)?;
                        let promise = runtime.objects.get_promise_mut(promise_handle).unwrap();
                        promise.reject(RegisterValue::from_object_handle(err_handle.0))
                    } else {
                        // §27.2.1.3.2 step 9-11: If value is a thenable (another promise),
                        // we need to chain. For now, check if value is a promise and chain.
                        if runtime.objects.get_promise(ObjectHandle(h)).is_some() {
                            // Value is a promise — register then reactions to forward settlement.
                            Self::chain_promise_resolution(
                                runtime,
                                promise_handle,
                                ObjectHandle(h),
                            );
                            return Ok(());
                        }
                        let promise = runtime.objects.get_promise_mut(promise_handle).unwrap();
                        promise.fulfill(value)
                    }
                } else {
                    promise.fulfill(value)
                }
            }
            crate::promise::ReactionKind::Reject => promise.reject(value),
        };

        if let Some(jobs) = jobs {
            for job in jobs {
                runtime.microtasks_mut().enqueue_promise_job(job);
            }
        }

        Ok(())
    }

    /// Chains a thenable promise resolution: when `thenable` settles, forward to `promise`.
    /// ES2024 §27.2.1.3.2 step 12 — HostEnqueuePromiseJob(PromiseResolveThenableJob)
    fn chain_promise_resolution(
        runtime: &mut RuntimeState,
        promise: ObjectHandle,
        thenable: ObjectHandle,
    ) {
        // Get or create resolve/reject for the target promise.
        let resolve = runtime
            .objects
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Fulfill);
        let reject = runtime
            .objects
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Reject);

        let capability = crate::promise::PromiseCapability {
            promise,
            resolve,
            reject,
        };

        // Register reactions on the thenable.
        let thenable_promise = runtime
            .objects
            .get_promise_mut(thenable)
            .expect("thenable verified as promise");

        if let Some(immediate_job) = thenable_promise.then(Some(resolve), Some(reject), capability)
        {
            runtime.microtasks_mut().enqueue_promise_job(immediate_job);
        }
    }

    /// Invokes a PromiseCombinatorElement (per-element resolve/reject for all/allSettled/any).
    /// ES2024 §27.2.4.1.1, §27.2.4.2.1–2, §27.2.4.3.1
    fn invoke_promise_combinator_element(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        use crate::promise::PromiseCombinatorKind;

        // Extract all fields from the combinator element.
        let (combinator_kind, index, result_array, remaining_counter, result_cap, already_called) =
            runtime
                .objects
                .promise_combinator_element_info(callable)
                .ok_or_else(|| {
                    InterpreterError::TypeError("not a promise combinator element".into())
                })?;

        // §27.2.4.1.1 step 1: If alreadyCalled is true, return undefined.
        if already_called {
            return Ok(RegisterValue::undefined());
        }

        // Set alreadyCalled to true.
        runtime.objects.set_combinator_element_called(callable);

        match combinator_kind {
            PromiseCombinatorKind::AllResolve => {
                // §27.2.4.1.1: Store value at result_array[index].
                let _ = runtime
                    .objects
                    .set_index(result_array, index as usize, value);

                // Decrement remaining counter.
                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    // All elements resolved — fulfill the result promise with the array.
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.resolve,
                        RegisterValue::from_object_handle(result_array.0),
                    )?;
                }
            }
            PromiseCombinatorKind::AllSettledResolve => {
                // §27.2.4.2.1: Create { status: "fulfilled", value: value }.
                let obj = runtime.alloc_settled_result_object("fulfilled", "value", value);
                let _ = runtime.objects.set_index(
                    result_array,
                    index as usize,
                    RegisterValue::from_object_handle(obj.0),
                );

                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.resolve,
                        RegisterValue::from_object_handle(result_array.0),
                    )?;
                }
            }
            PromiseCombinatorKind::AllSettledReject => {
                // §27.2.4.2.2: Create { status: "rejected", reason: value }.
                let obj = runtime.alloc_settled_result_object("rejected", "reason", value);
                let _ = runtime.objects.set_index(
                    result_array,
                    index as usize,
                    RegisterValue::from_object_handle(obj.0),
                );

                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.resolve,
                        RegisterValue::from_object_handle(result_array.0),
                    )?;
                }
            }
            PromiseCombinatorKind::AnyReject => {
                // §27.2.4.3.1: Store error at result_array[index] (errors array).
                let _ = runtime
                    .objects
                    .set_index(result_array, index as usize, value);

                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    // All elements rejected — reject with AggregateError.
                    let err = runtime
                        .alloc_type_error("All promises were rejected")
                        .map_err(|_| InterpreterError::InvalidHeapValueKind)?;
                    // Attach errors array as property.
                    let errors_prop = runtime.intern_property_name("errors");
                    let _ = runtime.objects.set_property(
                        err,
                        errors_prop,
                        RegisterValue::from_object_handle(result_array.0),
                    );
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.reject,
                        RegisterValue::from_object_handle(err.0),
                    )?;
                }
            }
        }

        Ok(RegisterValue::undefined())
    }

    /// Decrements the counter in remaining_counter[0] and returns true if it reached 0.
    fn decrement_combinator_counter(
        runtime: &mut RuntimeState,
        counter_handle: ObjectHandle,
    ) -> bool {
        let Ok(elements) = runtime.objects.array_elements(counter_handle) else {
            return false;
        };
        let count = elements.first().and_then(|v| v.as_i32()).unwrap_or(0);
        let new_count = count - 1;
        let _ = runtime
            .objects
            .set_index(counter_handle, 0, RegisterValue::from_i32(new_count));
        new_count == 0
    }

    /// Invokes a PromiseFinallyFunction (ThenFinally/CatchFinally wrapper).
    /// ES2024 §27.2.5.3.1–2
    fn invoke_promise_finally_function(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        use crate::promise::PromiseFinallyKind;

        let (on_finally, _constructor, kind) = runtime
            .objects
            .promise_finally_function_info(callable)
            .ok_or_else(|| InterpreterError::TypeError("not a promise finally function".into()))?;

        // Call onFinally() with no arguments.
        let finally_result =
            runtime.call_host_function(Some(on_finally), RegisterValue::undefined(), &[]);

        match kind {
            PromiseFinallyKind::ThenFinally => {
                // §27.2.5.3.1: If onFinally() throws, propagate.
                // If it returns normally, return the original value.
                match finally_result {
                    Ok(_) => Ok(value),
                    Err(VmNativeCallError::Thrown(thrown)) => {
                        Err(InterpreterError::UncaughtThrow(thrown))
                    }
                    Err(VmNativeCallError::Internal(msg)) => Err(InterpreterError::NativeCall(msg)),
                }
            }
            PromiseFinallyKind::CatchFinally => {
                // §27.2.5.3.2: If onFinally() throws, propagate that throw.
                // If it returns normally, re-throw the original reason.
                match finally_result {
                    Ok(_) => Err(InterpreterError::UncaughtThrow(value)),
                    Err(VmNativeCallError::Thrown(thrown)) => {
                        Err(InterpreterError::UncaughtThrow(thrown))
                    }
                    Err(VmNativeCallError::Internal(msg)) => Err(InterpreterError::NativeCall(msg)),
                }
            }
        }
    }

    /// Runs an existing activation until it returns or traps.
    pub fn run(
        &self,
        module: &Module,
        activation: &mut Activation,
    ) -> Result<ExecutionResult, InterpreterError> {
        let mut runtime = RuntimeState::new();
        self.run_with_runtime(module, activation, &mut runtime)
    }

    /// Executes one function on an existing runtime from a prepared register window.
    pub fn execute_with_runtime(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        self.resume_with_runtime(module, function_index, 0, registers, runtime)
    }

    /// Resumes one function from an explicit PC and pre-materialized register window.
    pub fn resume(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        resume_pc: ProgramCounter,
        registers: &[RegisterValue],
    ) -> Result<ExecutionResult, InterpreterError> {
        let function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation =
            Activation::new(function_index, function.frame_layout().register_count());
        activation.copy_registers_from_slice(registers)?;
        activation.set_pc(resume_pc);

        let mut runtime = RuntimeState::new();
        self.run_with_runtime(module, &mut activation, &mut runtime)
    }

    /// Resumes one function on an existing runtime from an explicit PC.
    pub fn resume_with_runtime(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        resume_pc: ProgramCounter,
        registers: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        let function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation =
            Activation::new(function_index, function.frame_layout().register_count());
        activation.copy_registers_from_slice(registers)?;
        activation.set_pc(resume_pc);

        self.run_with_runtime(module, &mut activation, runtime)
    }

    /// Profiles monomorphic property caches for one function on a fresh runtime.
    pub fn profile_property_caches(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers: &[RegisterValue],
    ) -> Result<Box<[Option<PropertyInlineCache>]>, InterpreterError> {
        let function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation =
            Activation::new(function_index, function.frame_layout().register_count());
        activation.copy_registers_from_slice(registers)?;
        let mut runtime = RuntimeState::new();
        let mut frame_runtime = FrameRuntimeState::new(function);

        loop {
            activation.begin_step();
            match self.step(
                function,
                module,
                &mut activation,
                &mut runtime,
                &mut frame_runtime,
            )? {
                StepOutcome::Continue => {
                    activation.sync_written_open_upvalues(&mut runtime)?;
                    activation.refresh_open_upvalues_from_cells(&runtime)?;
                }
                StepOutcome::Return(_) => {
                    return Ok(frame_runtime.property_feedback);
                }
                StepOutcome::Throw(value) => {
                    return Err(InterpreterError::UncaughtThrow(value));
                }
                StepOutcome::Suspend { .. } => {
                    // Suspension not supported in feedback-collection mode.
                    return Err(InterpreterError::TypeError(
                        "await is not supported in this execution mode".into(),
                    ));
                }
                StepOutcome::TailCall { .. } => {
                    // TCO not supported in feedback-collection mode.
                    return Ok(frame_runtime.property_feedback);
                }
                StepOutcome::GeneratorYield { .. } => {
                    // Yield not supported in feedback-collection mode.
                    return Ok(frame_runtime.property_feedback);
                }
            }
        }
    }

    fn run_with_runtime(
        &self,
        module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        match self.run_completion_with_runtime(module, activation, runtime)? {
            Completion::Return(return_value) => Ok(ExecutionResult::new(return_value)),
            Completion::Throw(value) => Err(InterpreterError::UncaughtThrow(value)),
        }
    }

    fn run_completion_with_runtime(
        &self,
        module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
    ) -> Result<Completion, InterpreterError> {
        let previous_module = runtime.enter_module(module);

        // These are mutable because TailCallClosure can replace them in-place.
        let mut current_module = module.clone();
        let mut function = current_module
            .function(activation.function_index())
            .expect("activation function index must be valid")
            .clone();
        let mut frame_runtime = FrameRuntimeState::new(&function);

        // V8 stack trace API — push the activation onto the shadow execution
        // context stack so it can be observed by `Error.captureStackTrace`
        // and Error constructor capture. The matching pop is performed at
        // every return path below.
        //
        // §14.6 + diagnostic friendliness: tail calls push *additional*
        // shadow frames on top of this one (rather than replacing it) so
        // stack traces match Node/V8/Bun. We snapshot the stack length
        // here and pop down to that on every exit, cleaning up any tail
        // frames the loop accumulated.
        let shadow_baseline = runtime.frame_info_stack_len();
        runtime.push_frame_info(Self::build_frame_info(
            &current_module,
            &function,
            activation,
            false,
        ));

        loop {
            activation.begin_step();
            // Update the topmost shadow stack entry's PC so a snapshot taken
            // mid-step reports the correct call site.
            runtime.update_top_frame_pc(activation.pc());
            let outcome = match self.step(
                &function,
                &current_module,
                activation,
                runtime,
                &mut frame_runtime,
            ) {
                Ok(outcome) => outcome,
                Err(InterpreterError::UncaughtThrow(value)) => StepOutcome::Throw(value),
                Err(InterpreterError::TypeError(message)) => {
                    let error = runtime.alloc_type_error(&message)?;
                    // Attach the current shadow stack so the diagnostic
                    // reporter has frame info to render. `capture_error_stack`
                    // is a no-op for synthetic native frames.
                    let _ = crate::intrinsics::error_class::capture_error_stack(runtime, error, 0);
                    StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                }
                // §7.1.18 RequireObjectCoercible — accessing a property on
                // `null` / `undefined` is a TypeError per the spec, not an
                // engine-internal error. Promote the dispatch-level guard
                // into a JS-visible TypeError so user code can catch it
                // and the diagnostic reporter can underline the access
                // site (which the source-map entry on the GetProperty /
                // GetIndex opcode now identifies precisely).
                // Spec: <https://tc39.es/ecma262/#sec-requireobjectcoercible>
                Err(InterpreterError::InvalidObjectValue) => {
                    let error =
                        runtime.alloc_type_error("Cannot read properties of null or undefined")?;
                    let _ = crate::intrinsics::error_class::capture_error_stack(runtime, error, 0);
                    StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                }
                Err(error) => {
                    runtime.restore_module(previous_module);
                    runtime.truncate_frame_info_stack(shadow_baseline);
                    return Err(error);
                }
            };

            match outcome {
                StepOutcome::Continue => {
                    activation.sync_written_open_upvalues(runtime)?;
                    activation.refresh_open_upvalues_from_cells(runtime)?;
                }
                StepOutcome::Return(return_value) => {
                    // Persist accumulated feedback for JIT consumption.
                    Self::persist_feedback(
                        runtime,
                        activation.function_index(),
                        &function,
                        &frame_runtime,
                    );
                    runtime.restore_module(previous_module);
                    runtime.truncate_frame_info_stack(shadow_baseline);
                    return Ok(Completion::Return(return_value));
                }
                StepOutcome::Throw(value) => {
                    if self.transfer_exception(&function, activation, value) {
                        continue;
                    }
                    runtime.restore_module(previous_module);
                    runtime.truncate_frame_info_stack(shadow_baseline);
                    return Ok(Completion::Throw(value));
                }
                // §14.6 Tail call: replace the current frame in-place and
                // continue the same loop — no new Rust stack frame.
                StepOutcome::TailCall(payload) => {
                    let TailCallPayload {
                        module: callee_module,
                        activation: callee_activation,
                    } = *payload;
                    current_module = callee_module;
                    *activation = callee_activation;
                    function = current_module
                        .function(activation.function_index())
                        .expect("tail-call function index must be valid")
                        .clone();
                    frame_runtime = FrameRuntimeState::new(&function);
                    runtime.enter_module(&current_module);
                    // §14.6 Tail call — push the new frame ON TOP of the
                    // caller in the shadow stack instead of replacing it.
                    //
                    // The actual VM frame stack still gets the tail-call
                    // optimization (no recursive Rust frame, no register
                    // file growth), but the shadow stack — which is *only*
                    // used for stack-trace rendering — keeps the caller
                    // visible. This makes diagnostics match Node/V8/Bun,
                    // which never tail-call elide and therefore always
                    // show the caller.
                    //
                    // To bound memory in pathological deep recursive tail
                    // calls (e.g. mutually recursive functions iterating
                    // millions of times), we cap the shadow stack at
                    // `SHADOW_STACK_TAIL_CAP` and start eliding the OLDEST
                    // tail-called frame once the cap is hit. The most
                    // recent N frames always survive so the diagnostic
                    // user sees the failing call site and its closest
                    // callers.
                    const SHADOW_STACK_TAIL_CAP: usize = 1024;
                    if runtime.frame_info_stack_len() >= SHADOW_STACK_TAIL_CAP {
                        runtime.pop_frame_info();
                    }
                    runtime.push_frame_info(Self::build_frame_info(
                        &current_module,
                        &function,
                        activation,
                        false,
                    ));
                }
                StepOutcome::Suspend {
                    awaited_promise,
                    resume_register,
                } => {
                    // ES2024 §27.7.5.3 Await — suspend until the promise settles.
                    // Drain microtasks inline; if the promise settles, resume.
                    // This handles synchronously-resolvable chains (most common case).
                    self.drain_microtasks_for_await(runtime, &current_module)?;

                    // Check if the awaited promise settled during drain.
                    if let Some(promise) = runtime.objects.get_promise(awaited_promise) {
                        match &promise.state {
                            crate::promise::PromiseState::Fulfilled(value) => {
                                let value = *value;
                                activation.set_register(resume_register, value)?;
                                // Continue execution loop — the await resolved.
                                continue;
                            }
                            crate::promise::PromiseState::Rejected(reason) => {
                                let reason = *reason;
                                // The PC was advanced past the Await instruction
                                // in the Suspend path. Back it up so that
                                // transfer_exception finds the enclosing try/catch.
                                let current_pc = activation.pc();
                                if current_pc > 0 {
                                    activation.set_pc(current_pc - 1);
                                }
                                if self.transfer_exception(&function, activation, reason) {
                                    continue;
                                }
                                runtime.restore_module(previous_module);
                                runtime.pop_frame_info();
                                return Ok(Completion::Throw(reason));
                            }
                            crate::promise::PromiseState::Pending => {
                                // Promise still pending after draining microtasks.
                                // This would require full event-loop integration
                                // (timers, I/O) to resolve. For now, return undefined.
                                runtime.restore_module(previous_module);
                                runtime.pop_frame_info();
                                return Ok(Completion::Return(RegisterValue::undefined()));
                            }
                        }
                    } else {
                        // Not a promise — treat as fulfilled with the value.
                        runtime.restore_module(previous_module);
                        runtime.pop_frame_info();
                        return Ok(Completion::Return(RegisterValue::undefined()));
                    }
                }
                StepOutcome::GeneratorYield { yielded_value, .. } => {
                    // GeneratorYield inside a non-generator run loop — treat
                    // as a return (shouldn't normally happen outside resume_generator).
                    runtime.restore_module(previous_module);
                    runtime.pop_frame_info();
                    return Ok(Completion::Return(yielded_value));
                }
            }
        }
    }

    /// Builds a `StackFrameInfo` snapshot from a frame's owning module,
    /// function, and current activation. Captured at frame entry and at
    /// every step (the `pc` field is updated separately by
    /// `RuntimeState::update_top_frame_pc`).
    fn build_frame_info(
        module: &Module,
        function: &Function,
        activation: &Activation,
        is_native: bool,
    ) -> crate::stack_frame::StackFrameInfo {
        // The compiler stamps the top-level script body's function name with
        // the module URL so debugger UIs can show "where am I". For V8-style
        // stack traces we want the top-level frame to render as anonymous.
        let raw_name = function.name();
        let function_name = match (raw_name, module.name()) {
            (Some(name), Some(url)) if name == url => None,
            (name, _) => name.map(Box::from),
        };
        crate::stack_frame::StackFrameInfo {
            module: module.clone(),
            function_index: activation.function_index(),
            function_name,
            pc: activation.pc(),
            closure_handle: activation.closure_handle(),
            is_native,
            is_async: function.is_async(),
            is_construct: activation.construct_new_target().is_some(),
        }
    }

    /// Drains microtasks inline during an await suspension.
    /// This settles promise chains that resolve synchronously (without timers/IO).
    fn drain_microtasks_for_await(
        &self,
        runtime: &mut RuntimeState,
        module: &Module,
    ) -> Result<(), InterpreterError> {
        // Simple drain loop — process all promise jobs until exhausted.
        // This mirrors OtterRuntime::drain_microtasks but runs inside the interpreter.
        loop {
            self.check_interrupt()?;
            let mut did_work = false;
            while let Some(job) = runtime.microtasks_mut().pop_promise_job() {
                self.check_interrupt()?;
                let callback_is_self_settling = matches!(
                    runtime.objects.kind(job.callback),
                    Ok(HeapValueKind::PromiseCapabilityFunction
                        | HeapValueKind::PromiseCombinatorElement)
                );

                let call_result = Self::call_function(
                    runtime,
                    module,
                    job.callback,
                    job.this_value,
                    &[job.argument],
                );

                if let Some(result_promise) = job.result_promise
                    && !callback_is_self_settling
                {
                    match call_result {
                        Ok(handler_result) => {
                            let resolve = runtime.objects.alloc_promise_capability_function(
                                result_promise,
                                crate::promise::ReactionKind::Fulfill,
                            );
                            let _ = Self::call_function(
                                runtime,
                                module,
                                resolve,
                                RegisterValue::undefined(),
                                &[handler_result],
                            );
                        }
                        Err(InterpreterError::UncaughtThrow(reason)) => {
                            if let Some(promise) = runtime.objects.get_promise_mut(result_promise)
                                && let Some(jobs) = promise.reject(reason)
                            {
                                for j in jobs {
                                    runtime.microtasks_mut().enqueue_promise_job(j);
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                did_work = true;
            }
            if !did_work {
                break;
            }
        }
        Ok(())
    }

    fn transfer_exception(
        &self,
        function: &Function,
        activation: &mut Activation,
        value: RegisterValue,
    ) -> bool {
        let Some(handler) = function.exceptions().find_handler(activation.pc()) else {
            return false;
        };

        activation.set_pending_exception(value);
        activation.set_pc(handler.handler_pc());
        true
    }

}



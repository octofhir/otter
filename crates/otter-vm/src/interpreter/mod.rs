//! Interpreter entry points for the new VM.
//!
//! **Modularization in progress (Phase 2 of VM_REFACTOR_PLAN.md).** The
//! interpreter is being split from its original single-file `interpreter.rs`
//! into focused submodules. Completed extractions are listed below; the
//! remaining bulk (RuntimeState, step(), dispatch(), tests) still lives in
//! this `mod.rs` but will move into `runtime_state/`, `step.rs`, `dispatch.rs`,
//! and `tests/` as Phase 2 progresses.
//!
//! ## Submodule index (extracted so far)
//!
//! | Module              | Purpose                                               |
//! |---------------------|-------------------------------------------------------|
//! | `activation`        | Per-frame state: registers, PC, upvalues.             |
//! | `error`             | `InterpreterError` enum + `From` conversions.         |
//! | `execution_result`  | `ExecutionResult` return type.                        |
//! | `number_conv`       | §7.1.6 / §7.1.7 f64 → i32/u32 + StringToNumber.       |
//! | `step_outcome`      | `StepOutcome`, `Completion`, `TailCallPayload`, `ToPrimitiveHint`. |
//! | `frame_runtime`     | Per-frame transient state (property inline cache).   |

mod activation;
mod dispatch;
mod error;
mod execution_result;
mod frame_runtime;
mod number_conv;
mod step_outcome;

#[cfg(test)]
mod tests;

pub use activation::Activation;
pub use error::InterpreterError;
pub use execution_result::ExecutionResult;
use frame_runtime::FrameRuntimeState;
pub(crate) use number_conv::{f64_to_int32, f64_to_uint32};
use number_conv::{canonical_string_exotic_index, parse_string_to_number};
pub(crate) use step_outcome::ToPrimitiveHint;
use step_outcome::{Completion, StepOutcome, TailCallPayload};

use core::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use num_traits::Zero;

use crate::builders::{BurrowBuilder, ObjectMemberPlan};
use crate::bytecode::ProgramCounter;
use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::host::{HostFunctionId, NativeFunctionRegistry};
use crate::intrinsics::{
    VmIntrinsics, WellKnownSymbol, box_boolean_object, box_number_object, box_symbol_object,
};
use crate::module::{Function, FunctionIndex, Module};
use crate::object::{
    ClosureFlags as ObjectClosureFlags, HeapValueKind, ObjectError, ObjectHandle, ObjectHeap,
    PropertyAttributes, PropertyInlineCache, PropertyLookup, PropertyValue,
};
use crate::payload::{NativePayloadError, NativePayloadRegistry, VmTrace, VmValueTracer};
use crate::property::{PropertyNameId, PropertyNameRegistry};
use crate::value::RegisterValue;

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
    realms: Vec<crate::realm::Realm>,
    /// §9.4.1 \[\[Realm\]\] of the running execution context. Updated by the
    /// interpreter when crossing realm boundaries (e.g. via `$262.createRealm`
    /// or future cross-realm proxies).
    current_realm: crate::realm::RealmId,
    objects: ObjectHeap,
    property_names: PropertyNameRegistry,
    native_functions: NativeFunctionRegistry,
    native_payloads: NativePayloadRegistry,
    microtasks: crate::microtask::MicrotaskQueue,
    host_callbacks: crate::host_callbacks::HostCallbackQueue,
    timers: crate::event_loop::TimerRegistry,
    console_backend: Box<dyn crate::console::ConsoleBackend>,
    current_module: Option<Module>,
    native_call_construct_stack: Vec<bool>,
    native_callee_stack: Vec<ObjectHandle>,
    /// V8 stack trace API — shadow stack of execution-context activations.
    /// Pushed at the entry of every JS frame (closure, generator resume,
    /// async resume) and popped at exit; updated in-place each interpreter
    /// step so the topmost entry's PC always tracks `activation.pc()`.
    /// Reference: <https://v8.dev/docs/stack-trace-api>
    frame_info_stack: Vec<crate::stack_frame::StackFrameInfo>,
    next_symbol_id: u32,
    symbol_descriptions: BTreeMap<u32, Option<Box<str>>>,
    global_symbol_registry: BTreeMap<Box<str>, u32>,
    global_symbol_registry_reverse: BTreeMap<u32, Box<str>>,
    /// §6.2.12 Monotonic counter for unique private class identifiers.
    /// Spec: <https://tc39.es/ecma262/#sec-private-names>
    next_class_id: u64,
    /// §14.4.4 Transient: set by `YieldStar` opcode, consumed by
    /// the generator resume loop when handling `GeneratorYield`.
    pending_delegation_iterator: Option<ObjectHandle>,
    /// Pending uncaught throw value, stashed by host integration code that
    /// converts an `InterpreterError::UncaughtThrow` into a textual native
    /// error so the **outer** runtime layer can later promote the error
    /// back to a structured `JsRuntimeDiagnostic`. This avoids losing the
    /// thrown value across the host module-loader boundary, where the
    /// natural error type is `String`.
    ///
    /// `None` once consumed; populated lazily and cleared by the next
    /// `take_pending_uncaught_throw` call.
    pending_uncaught_throw: Option<RegisterValue>,
    /// Per-host-run interrupt signal inherited by JS callbacks entered from
    /// microtasks, timers, host callbacks, accessors, and promise handlers.
    active_interrupt_flag: Option<Arc<AtomicBool>>,
    /// §B.3.5.2 — Nesting depth for class field initializer execution.
    /// When > 0, direct eval applies additional restrictions (ContainsArguments,
    /// Contains SuperCall). Incremented by RunClassFieldInitializer, decremented
    /// when the field init function returns.
    field_initializer_depth: u32,
}

impl RuntimeState {
    /// Creates a fresh runtime state with an empty, uncapped object heap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_gc_config(otter_gc::heap::GcConfig::default())
    }

    /// Creates a fresh runtime state whose underlying object heap enforces
    /// the provided GC configuration. Use this to set a hard heap cap
    /// (`GcConfig::max_heap_bytes`) — the Otter analogue of Node's
    /// `--max-old-space-size`.
    #[must_use]
    pub fn with_gc_config(config: otter_gc::heap::GcConfig) -> Self {
        let mut objects = ObjectHeap::with_config(config);
        let mut intrinsics = VmIntrinsics::allocate(&mut objects);
        let mut property_names = PropertyNameRegistry::new();
        let mut native_functions = NativeFunctionRegistry::new();
        intrinsics
            .wire_prototype_chains(&mut objects)
            .expect("intrinsic prototype wiring should bootstrap cleanly");
        // §9.3 Realm Records — bootstrap intrinsics into the initial realm (id = 0).
        intrinsics
            .init_core(&mut objects, &mut property_names, &mut native_functions, 0)
            .expect("intrinsic core init should bootstrap cleanly");
        intrinsics
            .install_on_global(&mut objects, &mut property_names, &mut native_functions, 0)
            .expect("intrinsic global install should bootstrap cleanly");
        let mut symbol_descriptions = BTreeMap::new();
        for &symbol in intrinsics.well_known_symbols() {
            symbol_descriptions.insert(symbol.stable_id(), Some(symbol.description().into()));
        }

        Self {
            realms: vec![crate::realm::Realm::new(intrinsics)],
            current_realm: 0,
            objects,
            property_names,
            native_functions,
            native_payloads: NativePayloadRegistry::new(),
            microtasks: crate::microtask::MicrotaskQueue::new(),
            host_callbacks: crate::host_callbacks::HostCallbackQueue::new(),
            timers: crate::event_loop::TimerRegistry::new(),
            console_backend: Box::new(crate::console::StdioConsoleBackend),
            current_module: None,
            native_call_construct_stack: Vec::new(),
            native_callee_stack: Vec::new(),
            frame_info_stack: Vec::new(),
            next_symbol_id: WellKnownSymbol::Unscopables.stable_id() + 1,
            symbol_descriptions,
            global_symbol_registry: BTreeMap::new(),
            global_symbol_registry_reverse: BTreeMap::new(),
            next_class_id: 1,
            pending_delegation_iterator: None,
            pending_uncaught_throw: None,
            active_interrupt_flag: None,
            field_initializer_depth: 0,
        }
    }

    /// Stash an uncaught-throw value so the outer host layer can later lift
    /// it back into a structured diagnostic. Called by the module-loader
    /// glue right before it converts an interpreter error to a string for
    /// the legacy native-error API.
    pub fn stash_pending_uncaught_throw(&mut self, value: RegisterValue) {
        self.pending_uncaught_throw = Some(value);
    }

    /// Drains the pending uncaught-throw value, if any. The host runtime
    /// uses this after a hosted module-loader execution failure to promote
    /// the throw back into a `JsRuntimeDiagnostic`.
    pub fn take_pending_uncaught_throw(&mut self) -> Option<RegisterValue> {
        self.pending_uncaught_throw.take()
    }

    /// Returns the intrinsic registry owned by the runtime's current realm.
    #[must_use]
    pub fn intrinsics(&self) -> &VmIntrinsics {
        &self.realms[self.current_realm as usize].intrinsics
    }

    /// Returns the mutable intrinsic registry owned by the runtime's current realm.
    pub fn intrinsics_mut(&mut self) -> &mut VmIntrinsics {
        &mut self.realms[self.current_realm as usize].intrinsics
    }

    /// Returns the shared OOM signal flag owned by the object heap. Cloned
    /// for sharing with the interpreter (see [`Interpreter::with_oom_flag`]).
    pub fn oom_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.objects.oom_flag()
    }

    /// Snapshot of per-variant heap statistics. Intended for the test262
    /// runner's `--memory-profile` mode — not a hot-path API.
    pub fn collect_heap_stats(&self) -> crate::object::HeapTypeStats {
        self.objects.collect_type_stats()
    }

    /// Clears the OOM signal flag. Called by the host runtime at script
    /// entry so a previous heap-cap violation does not immediately abort a
    /// subsequent script.
    pub fn clear_oom_flag(&self) {
        self.objects.clear_oom_flag();
    }

    /// Publishes the host-run interrupt flag to VM re-entry paths.
    pub fn set_active_interrupt_flag(&mut self, flag: Option<Arc<AtomicBool>>) {
        self.active_interrupt_flag = flag;
    }

    /// Returns the active host-run interrupt flag, if this runtime is inside
    /// an interruptible run.
    pub fn active_interrupt_flag(&self) -> Option<Arc<AtomicBool>> {
        self.active_interrupt_flag.clone()
    }

    /// Returns true once the active host run has been interrupted, normally
    /// by the timeout watchdog.
    #[must_use]
    pub fn is_execution_interrupted(&self) -> bool {
        self.active_interrupt_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// Cooperative interrupt poll for native Rust entrypoints. Long native
    /// loops should call this periodically; the host-call boundary promotes
    /// this internal sentinel back to `InterpreterError::Interrupted`.
    pub fn check_interrupt(&self) -> Result<(), VmNativeCallError> {
        if self.is_execution_interrupted() {
            Err(VmNativeCallError::Internal(
                EXECUTION_INTERRUPTED_MESSAGE.into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Returns `Err(OutOfMemory)` if the object heap has signalled that the
    /// hard cap was crossed. Intended for native function implementations
    /// that allocate in bulk (e.g. `Array.prototype.concat`) so they fail
    /// fast with a catchable RangeError instead of continuing after a
    /// silent budget violation.
    pub fn check_oom(&mut self) -> Result<(), crate::descriptors::VmNativeCallError> {
        use std::sync::atomic::Ordering;
        if self.objects.oom_flag().load(Ordering::Relaxed) {
            Err(crate::descriptors::VmNativeCallError::Thrown(
                self.alloc_range_error_value("out of memory: heap limit exceeded"),
            ))
        } else {
            Ok(())
        }
    }

    /// Allocates a freshly-constructed `RangeError` object with the given
    /// message and returns it as a `RegisterValue`, ready to be surfaced
    /// via [`VmNativeCallError::Thrown`]. Mirrors the helper used by
    /// `invalid_array_length_error` in `intrinsics::array_class`.
    pub fn alloc_range_error_value(&mut self, message: &str) -> RegisterValue {
        let prototype = self.intrinsics().range_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let message_string = self.alloc_string(message);
        let message_prop = self.intern_property_name("message");
        self.objects_mut()
            .set_property(
                handle,
                message_prop,
                RegisterValue::from_object_handle(message_string.0),
            )
            .ok();
        RegisterValue::from_object_handle(handle.0)
    }

    /// Throws a fresh `RangeError` with the given message. Returns the
    /// `VmNativeCallError::Thrown` envelope used by native function
    /// implementations.
    pub fn throw_range_error(&mut self, message: &str) -> crate::descriptors::VmNativeCallError {
        crate::descriptors::VmNativeCallError::Thrown(self.alloc_range_error_value(message))
    }

    /// Returns the realm record currently bound as the running execution context's `[[Realm]]`.
    #[must_use]
    pub fn current_realm_id(&self) -> crate::realm::RealmId {
        self.current_realm
    }

    /// Returns the realm record at the given index.
    #[must_use]
    pub fn realm(&self, id: crate::realm::RealmId) -> &crate::realm::Realm {
        &self.realms[id as usize]
    }

    // ─── Stack frame snapshot API (V8 stack trace API) ──────────────────

    /// Pushes a new entry onto the shadow execution-context stack.
    /// Called at the entry of every JS frame run by the interpreter.
    pub(crate) fn push_frame_info(&mut self, info: crate::stack_frame::StackFrameInfo) {
        self.frame_info_stack.push(info);
    }

    /// Pops the topmost shadow execution-context stack entry.
    /// Called at every return path of the interpreter loop.
    pub(crate) fn pop_frame_info(&mut self) {
        self.frame_info_stack.pop();
    }

    /// Truncates the shadow execution-context stack down to `baseline`.
    /// Used by `run_completion_with_runtime` to clean up any extra tail-call
    /// frames pushed during this loop's lifetime — see the §14.6 comment in
    /// the runner for the rationale.
    pub(crate) fn truncate_frame_info_stack(&mut self, baseline: usize) {
        self.frame_info_stack.truncate(baseline);
    }

    /// Updates the topmost shadow stack entry's PC. Called from the
    /// interpreter loop at every step so the topmost frame's PC always
    /// reflects the active activation.
    pub(crate) fn update_top_frame_pc(&mut self, pc: crate::bytecode::ProgramCounter) {
        if let Some(top) = self.frame_info_stack.last_mut() {
            top.pc = pc;
        }
    }

    /// Captures a snapshot of the current shadow execution-context stack,
    /// skipping the topmost `skip` frames. The result is ordered top-down
    /// (caller-most last), matching V8's `Error.stack` formatting.
    ///
    /// Reference: <https://v8.dev/docs/stack-trace-api>
    #[must_use]
    pub fn capture_stack_snapshot(&self, skip: usize) -> Vec<crate::stack_frame::StackFrameInfo> {
        if skip >= self.frame_info_stack.len() {
            return Vec::new();
        }
        let take = self.frame_info_stack.len() - skip;
        self.frame_info_stack
            .iter()
            .take(take)
            .rev()
            .cloned()
            .collect()
    }

    /// Returns the depth of the shadow execution-context stack. Used by
    /// `Error.captureStackTrace(obj, constructorOpt?)` to compute how many
    /// frames to skip.
    #[must_use]
    pub fn frame_info_stack_len(&self) -> usize {
        self.frame_info_stack.len()
    }

    /// Returns the topmost shadow stack entries' callees in order
    /// (top-of-stack last). Used by `Error.captureStackTrace` to look up the
    /// frame matching the optional `constructorOpt` argument.
    #[must_use]
    pub fn frame_info_stack_snapshot(&self) -> &[crate::stack_frame::StackFrameInfo] {
        &self.frame_info_stack
    }

    /// Returns the captured stack frames attached to a JS error instance
    /// (via `Error()` / `Error.captureStackTrace`), or `None` when the
    /// object has no `__otter_error_stack_frames__` slot.
    ///
    /// Used by host integrations (`otter-runtime` diagnostics, the test262
    /// runner) to lift V8-style frames out of an uncaught throw without
    /// having to invoke `Error.prototype.stack` and reparse the formatted
    /// string.
    pub fn read_error_stack_frames(
        &mut self,
        handle: ObjectHandle,
    ) -> Option<Vec<crate::stack_frame::StackFrameInfo>> {
        let frames_prop =
            self.intern_property_name(crate::intrinsics::error_class::ERROR_STACK_FRAMES_SLOT);
        let lookup = self
            .objects()
            .get_property(handle, frames_prop)
            .ok()
            .flatten()?;
        if lookup.owner() != handle {
            return None;
        }
        let frames_handle = match lookup.value() {
            crate::object::PropertyValue::Data { value, .. } => {
                value.as_object_handle().map(ObjectHandle)?
            }
            _ => return None,
        };
        match self.objects().error_stack_frames(frames_handle) {
            Ok(Some(slice)) => Some(slice.to_vec()),
            _ => None,
        }
    }

    /// Reads `name` and `message` off a JS error instance, returning the
    /// V8/Node-style `(name, message)` pair used to format `Error.stack`.
    /// Falls back to `("Error", "")` when slots are missing or non-string,
    /// matching the spec defaults from §20.5.3.
    pub fn read_error_name_and_message(&mut self, handle: ObjectHandle) -> (String, String) {
        let name_prop = self.intern_property_name("name");
        let msg_prop = self.intern_property_name("message");
        let name_val = self
            .ordinary_get(
                handle,
                name_prop,
                RegisterValue::from_object_handle(handle.0),
            )
            .unwrap_or_else(|_| RegisterValue::undefined());
        let msg_val = self
            .ordinary_get(
                handle,
                msg_prop,
                RegisterValue::from_object_handle(handle.0),
            )
            .unwrap_or_else(|_| RegisterValue::undefined());
        let name = if name_val == RegisterValue::undefined() {
            "Error".to_string()
        } else {
            self.js_to_string_infallible(name_val).to_string()
        };
        let message = if msg_val == RegisterValue::undefined() {
            String::new()
        } else {
            self.js_to_string_infallible(msg_val).to_string()
        };
        (name, message)
    }

    /// §9.3.3 InitializeHostDefinedRealm — creates a brand-new realm with its
    /// own intrinsics, prototypes, constructors, and global object.
    ///
    /// Each new-VM realm holds an independent `VmIntrinsics` so cross-realm
    /// constructs (e.g. `Reflect.construct(Error, [], otherRealm.Function)`)
    /// can return prototypes from the *other* realm via
    /// `GetPrototypeFromConstructor`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-initializehostdefinedrealm>
    pub fn create_realm(
        &mut self,
    ) -> Result<crate::realm::RealmId, crate::intrinsics::IntrinsicsError> {
        let new_realm_id: crate::realm::RealmId = self
            .realms
            .len()
            .try_into()
            .map_err(|_| crate::intrinsics::IntrinsicsError::InvalidLifecycleStage)?;

        let mut intrinsics = VmIntrinsics::allocate(&mut self.objects);
        intrinsics.wire_prototype_chains(&mut self.objects)?;
        intrinsics.init_core(
            &mut self.objects,
            &mut self.property_names,
            &mut self.native_functions,
            new_realm_id,
        )?;
        intrinsics.install_on_global(
            &mut self.objects,
            &mut self.property_names,
            &mut self.native_functions,
            new_realm_id,
        )?;

        self.realms.push(crate::realm::Realm::new(intrinsics));
        Ok(new_realm_id)
    }

    /// §10.2.3 GetFunctionRealm — returns the realm of the given callable.
    ///
    /// For bound function exotic objects this falls through the chain of targets
    /// (their `[[Realm]]` is set to the target's realm at bind time, so a single
    /// read is sufficient). For proxy exotic objects this recurses on the target.
    /// Revoked proxies and non-callable values fall back to the current realm
    /// per the spirit of §10.2.3 step 4 — callers needing strict spec error
    /// reporting should validate beforehand.
    /// Spec: <https://tc39.es/ecma262/#sec-getfunctionrealm>
    #[must_use]
    pub fn get_function_realm(&self, callable: ObjectHandle) -> crate::realm::RealmId {
        if let Ok(Some(realm)) = self.objects.function_realm(callable) {
            return realm;
        }
        if !self.objects.is_proxy_revoked(callable)
            && let Ok((target, _handler)) = self.objects.proxy_parts(callable)
        {
            return self.get_function_realm(target);
        }
        self.current_realm
    }

    /// §10.1.14 GetPrototypeFromConstructor — looks up `constructor.prototype`
    /// and, if it is not an object, falls back to
    /// `realm.[[Intrinsics]].[[<intrinsic_default>]]` where `realm` comes from
    /// `GetFunctionRealm(constructor)`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    pub fn get_prototype_from_constructor(
        &mut self,
        constructor: ObjectHandle,
        intrinsic_default: crate::intrinsics::IntrinsicKey,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype_property = self.intern_property_name("prototype");
        // §10.1.14 step 2: Let proto be ? Get(constructor, "prototype").
        // Use proxy [[Get]] if the constructor is a proxy (§10.5.8).
        let proto_val = if self.is_proxy(constructor) {
            self.proxy_get(
                constructor,
                prototype_property,
                RegisterValue::from_object_handle(constructor.0),
            )?
        } else {
            self.ordinary_get(
                constructor,
                prototype_property,
                RegisterValue::from_object_handle(constructor.0),
            )
            .map_err(|error| match error {
                VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })?
        };
        // §10.1.14 step 3: If Type(proto) is not Object …
        if let Some(handle) = proto_val.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        // … 3a-b: realm = GetFunctionRealm(constructor); proto = realm intrinsic.
        let realm = self.get_function_realm(constructor);
        Ok(self.realms[realm as usize]
            .intrinsics
            .get(intrinsic_default))
    }

    /// Returns the current object heap.
    #[must_use]
    pub fn objects(&self) -> &ObjectHeap {
        &self.objects
    }

    /// Returns the mutable object heap.
    pub fn objects_mut(&mut self) -> &mut ObjectHeap {
        &mut self.objects
    }

    /// §6.2.12 — Allocates a new unique class identifier for private name resolution.
    /// Spec: <https://tc39.es/ecma262/#sec-private-names>
    pub fn alloc_class_id(&mut self) -> u64 {
        let id = self.next_class_id;
        self.next_class_id += 1;
        id
    }

    /// Returns the runtime-wide property-name registry.
    #[must_use]
    pub fn property_names(&self) -> &PropertyNameRegistry {
        &self.property_names
    }

    /// Returns the mutable runtime-wide property-name registry.
    pub fn property_names_mut(&mut self) -> &mut PropertyNameRegistry {
        &mut self.property_names
    }

    /// Returns `true` when the active native callback was entered via
    /// [[Construct]].
    #[must_use]
    pub fn is_current_native_construct_call(&self) -> bool {
        self.native_call_construct_stack
            .last()
            .copied()
            .unwrap_or(false)
    }

    /// ES2024 §10.1.13 OrdinaryCreateFromConstructor — subclass-friendly
    /// prototype selector for builtin constructors.
    ///
    /// Returns the `[[Prototype]]` that a fresh exotic instance should use:
    /// - When the native callback was invoked via `[[Construct]]` and the
    ///   pre-allocated `receiver` is an object (which is exactly what
    ///   `allocate_construct_receiver` hands us after reading
    ///   `newTarget.prototype` via `GetPrototypeFromConstructor`), returns
    ///   that object's `[[Prototype]]`. This is how `class X extends Array {}`
    ///   + `new X()` ends up with `Object.getPrototypeOf(instance) === X.prototype`.
    /// - Otherwise (plain call, missing receiver, unrelated this) returns
    ///   the caller-supplied `default_prototype` — usually the builtin's own
    ///   `%BuiltinPrototype%` intrinsic.
    ///
    /// Builtin constructors (Array, Map, Set, Error, RegExp, ArrayBuffer, …)
    /// should call this helper after allocating their exotic heap object and
    /// use the returned prototype via `set_prototype` so that subclassing
    /// works uniformly.
    /// Spec: <https://tc39.es/ecma262/#sec-ordinarycreatefromconstructor>
    #[must_use]
    pub fn subclass_prototype_or_default(
        &self,
        receiver: RegisterValue,
        default_prototype: ObjectHandle,
    ) -> ObjectHandle {
        if !self.is_current_native_construct_call() {
            return default_prototype;
        }
        let Some(receiver_handle) = receiver.as_object_handle().map(ObjectHandle) else {
            return default_prototype;
        };
        match self.objects.get_prototype(receiver_handle) {
            Ok(Some(proto)) => proto,
            _ => default_prototype,
        }
    }

    /// Returns the function object handle of the currently executing native callback.
    #[must_use]
    pub fn current_native_callee(&self) -> Option<ObjectHandle> {
        self.native_callee_stack.last().copied()
    }

    /// Creates a property key iterator (for..in) from an object and its prototype chain.
    pub fn alloc_property_iterator(
        &mut self,
        object: ObjectHandle,
    ) -> Result<ObjectHandle, ObjectError> {
        self.objects
            .alloc_property_iterator(object, &mut self.property_names)
    }

    /// Creates an empty property iterator (for null/undefined/primitives in for..in).
    pub fn alloc_empty_property_iterator(&mut self) -> Result<ObjectHandle, ObjectError> {
        self.objects.alloc_empty_property_iterator()
    }

    /// Interns one property name into the runtime-wide registry.
    pub fn intern_property_name(&mut self, name: &str) -> PropertyNameId {
        self.property_names.intern(name)
    }

    /// Interns one symbol-keyed property into the runtime-wide registry.
    pub fn intern_symbol_property_name(&mut self, symbol_id: u32) -> PropertyNameId {
        self.property_names.intern_symbol(symbol_id)
    }

    /// Returns own property keys using the runtime-wide property-name registry.
    pub fn own_property_keys(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, ObjectError> {
        let mut keys = self
            .objects
            .own_keys_with_registry(object, &mut self.property_names)?;
        keys.retain(|key| !self.is_hidden_internal_property(*key));

        let Some(string_handle) = self.string_exotic_value_handle(object)? else {
            return Ok(keys);
        };
        if string_handle == object {
            return Ok(keys);
        }

        let Some(string) = self.objects.string_value(string_handle)? else {
            return Ok(keys);
        };
        let length = string.len();
        let mut result = Vec::with_capacity(length.saturating_add(1).saturating_add(keys.len()));
        for index in 0..length {
            result.push(self.property_names.intern(&index.to_string()));
        }
        result.push(self.property_names.intern("length"));
        result.extend(
            keys.into_iter()
                .filter(|key| !self.is_string_exotic_public_key(*key, length)),
        );
        Ok(result)
    }

    /// Returns an own property descriptor without prototype traversal.
    pub fn own_property_descriptor(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        if self.is_hidden_internal_property(property) {
            return Ok(None);
        }
        if let Some(descriptor) = self.string_exotic_own_property(object, property)? {
            return Ok(Some(descriptor));
        }
        self.objects
            .own_property_descriptor(object, property, &self.property_names)
    }

    /// Returns enumerable own property keys in spec-visible enumeration order.
    pub fn enumerable_own_property_keys(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, VmNativeCallError> {
        let keys = self.own_property_keys(object).map_err(|error| {
            VmNativeCallError::Internal(format!("enumerable own keys failed: {error:?}").into())
        })?;
        let mut enumerable = Vec::with_capacity(keys.len());
        for key in keys {
            if self.property_names.is_symbol(key) {
                continue;
            }
            let Some(descriptor) = self.own_property_descriptor(object, key).map_err(|error| {
                VmNativeCallError::Internal(
                    format!("enumerable own descriptor failed: {error:?}").into(),
                )
            })?
            else {
                continue;
            };
            if descriptor.attributes().enumerable() {
                enumerable.push(key);
            }
        }
        Ok(enumerable)
    }

    /// Returns one own property value using the object itself as `receiver`.
    pub fn own_property_value(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.ordinary_get(
            object,
            property,
            RegisterValue::from_object_handle(object.0),
        )
    }

    /// Returns a named property lookup using the runtime-wide property registry.
    pub fn property_lookup(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyLookup>, ObjectError> {
        if self.is_hidden_internal_property(property) {
            return Ok(None);
        }
        if let Some(descriptor) = self.string_exotic_own_property(object, property)? {
            return Ok(Some(PropertyLookup::new(object, descriptor, None)));
        }
        self.objects
            .get_property_with_registry(object, property, &self.property_names)
    }

    /// Returns whether a named property exists on an object or its prototype chain.
    pub fn has_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, ObjectError> {
        Ok(self.property_lookup(object, property)?.is_some())
    }

    /// Writes a named property using the runtime-wide property-name registry.
    pub fn set_named_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<PropertyInlineCache, InterpreterError> {
        match self
            .objects
            .set_property_with_registry(object, property, value, &self.property_names)
        {
            Ok(cache) => Ok(cache),
            Err(ObjectError::InvalidArrayLength) => Err(self.invalid_array_length_error()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn get_array_index_value(
        &mut self,
        object: ObjectHandle,
        index: usize,
    ) -> Result<Option<RegisterValue>, VmNativeCallError> {
        let property = self.intern_property_name(&index.to_string());
        match self.property_lookup(object, property).map_err(|error| {
            VmNativeCallError::Internal(format!("array index lookup failed: {error:?}").into())
        })? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { value, .. } => Ok(Some(value)),
                PropertyValue::Accessor { getter, .. } => self
                    .call_callable_for_accessor(
                        getter,
                        RegisterValue::from_object_handle(object.0),
                        &[],
                    )
                    .map(Some)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    }),
            },
            None => Ok(None),
        }
    }

    pub fn iterator_next(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<crate::object::IteratorStep, InterpreterError> {
        use crate::object::{ArrayIteratorKind, ObjectError};

        // Check if this is a values-kind array iterator (fast path) or string iterator.
        // Non-values array iterators, Map/Set iterators return InvalidKind to use the
        // protocol-based slow path via .next().
        let kind_check = self.objects.array_iterator_kind(handle);
        match kind_check {
            Ok(ArrayIteratorKind::Keys | ArrayIteratorKind::Entries) => {
                return Err(InterpreterError::InvalidHeapValueKind);
            }
            Err(ObjectError::InvalidKind) => {
                // Not an ArrayIterator — check if string/other internal iterator.
                if matches!(
                    self.objects.kind(handle),
                    Ok(crate::object::HeapValueKind::MapIterator
                        | crate::object::HeapValueKind::SetIterator)
                ) {
                    return Err(InterpreterError::InvalidHeapValueKind);
                }
            }
            _ => {} // Values kind — continue with fast path
        }

        let cursor = self.objects.iterator_cursor(handle)?;
        if cursor.closed() {
            return Ok(crate::object::IteratorStep::done());
        }

        let step = if cursor.is_array() {
            match self.objects.array_length(cursor.iterable())? {
                Some(length) if cursor.next_index() < length => {
                    let value =
                        match self.get_array_index_value(cursor.iterable(), cursor.next_index()) {
                            Ok(value) => value,
                            Err(VmNativeCallError::Thrown(value)) => {
                                return Err(InterpreterError::UncaughtThrow(value));
                            }
                            Err(VmNativeCallError::Internal(message)) => {
                                return Err(InterpreterError::NativeCall(message));
                            }
                        };
                    match value {
                        Some(value) => crate::object::IteratorStep::yield_value(value),
                        None => {
                            crate::object::IteratorStep::yield_value(RegisterValue::undefined())
                        }
                    }
                }
                _ => crate::object::IteratorStep::done(),
            }
        } else {
            // §22.1.5.2.1 %StringIteratorPrototype%.next() — yield code points.
            // Surrogate pairs yield a single 2-unit string.
            let iterable = cursor.iterable();
            let idx = cursor.next_index();
            if let Ok(Some(js_str)) = self.objects.string_value(iterable).map(|o| o.cloned()) {
                let utf16 = js_str.as_utf16();
                if idx >= utf16.len() {
                    crate::object::IteratorStep::done()
                } else {
                    let (_, advance) = js_str.code_point_at(idx).unwrap_or((utf16[idx] as u32, 1));
                    let ch_units = utf16[idx..idx + advance].to_vec();
                    let ch_str = crate::js_string::JsString::from_utf16(ch_units);
                    let str_handle = self.objects.alloc_js_string(ch_str);
                    // Set prototype for the new string.
                    let proto = self.intrinsics().string_prototype();
                    self.objects.set_prototype(str_handle, Some(proto)).ok();
                    let step = crate::object::IteratorStep::yield_value(
                        RegisterValue::from_object_handle(str_handle.0),
                    );
                    // Advance extra for surrogate pairs (advance-1 beyond the +1 below).
                    if advance > 1 {
                        for _ in 1..advance {
                            self.objects.advance_iterator_cursor(handle, false)?;
                        }
                    }
                    step
                }
            } else {
                match self.objects.get_index(iterable, idx)? {
                    Some(value) => crate::object::IteratorStep::yield_value(value),
                    None => crate::object::IteratorStep::done(),
                }
            }
        };

        self.objects
            .advance_iterator_cursor(handle, step.is_done())?;
        Ok(step)
    }

    fn enter_module(&mut self, module: &Module) -> Option<Module> {
        let previous = self.current_module.clone();
        self.current_module = Some(module.clone());
        previous
    }

    fn restore_module(&mut self, previous: Option<Module>) {
        self.current_module = previous;
    }

    fn call_callable_for_accessor(
        &mut self,
        callable: Option<ObjectHandle>,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(callable) = callable else {
            return Ok(RegisterValue::undefined());
        };

        if let Ok(HeapValueKind::BoundFunction) = self.objects.kind(callable) {
            let (target, bound_this, bound_args) = self.objects.bound_function_parts(callable)?;
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return self.call_callable_for_accessor(Some(target), bound_this, &full_args);
        }

        let Some(module) = self.current_module.clone() else {
            return self
                .call_host_function(Some(callable), receiver, arguments)
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                });
        };

        Interpreter::call_function(self, &module, callable, receiver, arguments)
    }

    fn string_exotic_own_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        let Some(string_handle) = self.string_exotic_value_handle(object)? else {
            return Ok(None);
        };
        let Some(string) = self.objects.string_value(string_handle)? else {
            return Ok(None);
        };
        let Some(property_name) = self.property_names.get(property) else {
            return Ok(None);
        };

        if property_name == "length" {
            return Ok(Some(PropertyValue::data_with_attrs(
                RegisterValue::from_i32(i32::try_from(string.len()).unwrap_or(i32::MAX)),
                PropertyAttributes::from_flags(false, false, false),
            )));
        }

        let Some(index) = canonical_string_exotic_index(property_name) else {
            return Ok(None);
        };
        let Some(unit) = string.code_unit_at(index) else {
            return Ok(None);
        };

        let character = self.alloc_js_string(crate::js_string::JsString::from_utf16(vec![unit]));
        Ok(Some(PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(character.0),
            PropertyAttributes::from_flags(false, true, false),
        )))
    }

    fn string_exotic_value_handle(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        if self.objects.string_value(object)?.is_some() {
            return Ok(Some(object));
        }

        let backing = self.intern_property_name(STRING_DATA_SLOT);
        let Some(lookup) = self.objects.get_property(object, backing)? else {
            return Ok(None);
        };
        if lookup.owner() != object {
            return Ok(None);
        }
        let PropertyValue::Data { value, .. } = lookup.value() else {
            return Ok(None);
        };
        let Some(inner) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(None);
        };
        if self.objects.string_value(inner)?.is_some() {
            return Ok(Some(inner));
        }
        Ok(None)
    }

    fn is_hidden_internal_property(&self, property: PropertyNameId) -> bool {
        matches!(
            self.property_names.get(property),
            Some(STRING_DATA_SLOT | NUMBER_DATA_SLOT | BOOLEAN_DATA_SLOT | ERROR_DATA_SLOT)
        )
    }

    fn is_string_exotic_public_key(&self, property: PropertyNameId, length: usize) -> bool {
        let Some(name) = self.property_names.get(property) else {
            return false;
        };
        if name == "length" {
            return true;
        }
        canonical_string_exotic_index(name).is_some_and(|index| index < length)
    }

    /// Returns the runtime-wide native host-function registry.
    #[must_use]
    pub fn native_functions(&self) -> &NativeFunctionRegistry {
        &self.native_functions
    }

    /// Returns the mutable runtime-wide native host-function registry.
    pub fn native_functions_mut(&mut self) -> &mut NativeFunctionRegistry {
        &mut self.native_functions
    }

    /// Returns the runtime-owned native payload registry.
    #[must_use]
    pub fn native_payloads(&self) -> &NativePayloadRegistry {
        &self.native_payloads
    }

    /// Returns the mutable runtime-owned native payload registry.
    pub fn native_payloads_mut(&mut self) -> &mut NativePayloadRegistry {
        &mut self.native_payloads
    }

    /// Registers one host-callable native function in the runtime registry.
    pub fn register_native_function(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> HostFunctionId {
        self.native_functions.register(descriptor)
    }

    /// Returns the microtask queue.
    #[must_use]
    pub fn microtasks(&self) -> &crate::microtask::MicrotaskQueue {
        &self.microtasks
    }

    /// Returns the mutable microtask queue.
    pub fn microtasks_mut(&mut self) -> &mut crate::microtask::MicrotaskQueue {
        &mut self.microtasks
    }

    /// Returns the console backend.
    pub fn console(&self) -> &dyn crate::console::ConsoleBackend {
        self.console_backend.as_ref()
    }

    /// Replaces the console backend. Used by embedders to route output.
    pub fn set_console_backend(&mut self, backend: Box<dyn crate::console::ConsoleBackend>) {
        self.console_backend = backend;
    }

    /// Returns the timer registry.
    #[must_use]
    pub fn timers(&self) -> &crate::event_loop::TimerRegistry {
        &self.timers
    }

    /// Returns whether any cross-thread host completions are still pending.
    #[must_use]
    pub fn has_pending_host_callbacks(&self) -> bool {
        self.host_callbacks.has_pending()
    }

    /// Returns a sender that background host tasks can use to resume work on the VM thread.
    #[must_use]
    pub fn host_callback_sender(&self) -> crate::host_callbacks::HostCallbackSender {
        self.host_callbacks.sender()
    }

    /// Drains ready host completions without blocking.
    pub fn drain_host_callbacks(&mut self) {
        let callbacks = self.host_callbacks.drain_ready();
        for callback in callbacks {
            self.host_callbacks.complete_one();
            callback(self);
        }
    }

    /// Blocks until at least one pending host completion is ready, or timeout elapses.
    ///
    /// Returns `true` when at least one callback was invoked.
    pub fn wait_for_host_callbacks_interruptible<F>(
        &mut self,
        timeout: Option<std::time::Duration>,
        interrupted: F,
    ) -> bool
    where
        F: Fn() -> bool,
    {
        let callbacks = self
            .host_callbacks
            .wait_and_drain_interruptible(timeout, interrupted);
        if callbacks.is_empty() {
            return false;
        }
        for callback in callbacks {
            self.host_callbacks.complete_one();
            callback(self);
        }
        true
    }

    /// Returns the mutable timer registry.
    pub fn timers_mut(&mut self) -> &mut crate::event_loop::TimerRegistry {
        &mut self.timers
    }

    /// Schedules a one-shot timer (setTimeout).
    pub fn schedule_timeout(
        &mut self,
        callback: ObjectHandle,
        delay: std::time::Duration,
    ) -> crate::event_loop_host::TimerId {
        self.timers
            .set_timeout(callback, RegisterValue::undefined(), delay)
    }

    /// Schedules a repeating timer (setInterval).
    pub fn schedule_interval(
        &mut self,
        callback: ObjectHandle,
        interval: std::time::Duration,
    ) -> crate::event_loop_host::TimerId {
        self.timers
            .set_interval(callback, RegisterValue::undefined(), interval)
    }

    /// Cancels a timer.
    pub fn clear_timer(&mut self, id: crate::event_loop_host::TimerId) {
        self.timers.clear(id);
    }

    // -----------------------------------------------------------------------
    // Proxy helpers — §10.5 Proxy Object Internal Methods
    // -----------------------------------------------------------------------

    /// Returns `true` if the handle points to a Proxy exotic object.
    pub fn is_proxy(&self, handle: ObjectHandle) -> bool {
        self.objects.is_proxy(handle)
    }

    /// Allocates a JS TypeError and returns it as an `UncaughtThrow` so that
    /// `try/catch` in JS can intercept it.
    fn proxy_type_error(&mut self, message: &str) -> InterpreterError {
        match self.alloc_type_error(message) {
            Ok(error) => {
                InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(error.0))
            }
            Err(_) => InterpreterError::TypeError(message.into()),
        }
    }

    /// Returns `(target, handler)` for a live proxy, or throws TypeError if revoked.
    pub fn proxy_check_revoked(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<(ObjectHandle, ObjectHandle), InterpreterError> {
        if self.objects.is_proxy_revoked(handle) {
            return Err(self.proxy_type_error("Cannot perform operation on a revoked proxy"));
        }
        self.objects
            .proxy_parts(handle)
            .map_err(|e| InterpreterError::NativeCall(format!("proxy_parts: {e:?}").into()))
    }

    /// Looks up a trap method on the handler object.
    /// Returns `Some(callable)` if the trap exists, `None` if undefined/null.
    pub fn proxy_get_trap(
        &mut self,
        handler: ObjectHandle,
        trap_name: &str,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        let prop = self.intern_property_name(trap_name);
        let value = self.property_lookup(handler, prop)?;
        match value {
            Some(lookup) => match lookup.value() {
                crate::object::PropertyValue::Data { value, .. } => {
                    if value == RegisterValue::undefined() || value == RegisterValue::null() {
                        Ok(None)
                    } else if let Some(h) = value.as_object_handle().map(ObjectHandle) {
                        Ok(Some(h))
                    } else {
                        Err(self.proxy_type_error(&format!(
                            "proxy trap '{trap_name}' is not a function"
                        )))
                    }
                }
                crate::object::PropertyValue::Accessor { getter, .. } => {
                    // Accessor — call getter to obtain the trap function.
                    let trap_val = self.call_callable_for_accessor(
                        getter,
                        RegisterValue::from_object_handle(handler.0),
                        &[],
                    )?;
                    if trap_val == RegisterValue::undefined() || trap_val == RegisterValue::null() {
                        Ok(None)
                    } else if let Some(h) = trap_val.as_object_handle().map(ObjectHandle) {
                        Ok(Some(h))
                    } else {
                        Err(self.proxy_type_error(&format!(
                            "proxy trap '{trap_name}' is not a function"
                        )))
                    }
                }
            },
            None => Ok(None),
        }
    }

    /// Converts a PropertyNameId to a JS string value for passing to proxy traps.
    pub fn property_name_to_value(
        &mut self,
        property: crate::property::PropertyNameId,
    ) -> Result<RegisterValue, InterpreterError> {
        let name = self
            .property_names()
            .get(property)
            .ok_or_else(|| InterpreterError::NativeCall("property name not found".into()))?
            .to_string();
        let handle = self.alloc_string(name);
        Ok(RegisterValue::from_object_handle(handle.0))
    }

    // -----------------------------------------------------------------------
    // Proxy trap dispatch — §10.5 Proxy Object Internal Methods
    // -----------------------------------------------------------------------

    /// §10.5.8 [[Get]](P, Receiver)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-get-p-receiver>
    pub fn proxy_get(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
        receiver: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "get")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val, receiver],
                )
            }
            None => {
                // No trap — forward to target.[[Get]](P, Receiver)
                if self.is_proxy(target) {
                    self.proxy_get(target, property, receiver)
                } else {
                    match self.property_lookup(target, property)? {
                        Some(lookup) => match lookup.value() {
                            PropertyValue::Data { value, .. } => Ok(value),
                            PropertyValue::Accessor { getter, .. } => {
                                self.call_callable_for_accessor(getter, receiver, &[])
                            }
                        },
                        None => Ok(RegisterValue::undefined()),
                    }
                }
            }
        }
    }

    /// §10.5.9 [[Set]](P, V, Receiver)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-set-p-v-receiver>
    pub fn proxy_set(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
        receiver: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "set")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val, value, receiver],
                )?;
                Ok(result.is_truthy())
            }
            None => {
                // No trap — forward to target.[[Set]](P, V, Receiver)
                if self.is_proxy(target) {
                    self.proxy_set(target, property, value, receiver)
                } else {
                    self.set_named_property(target, property, value)?;
                    Ok(true)
                }
            }
        }
    }

    /// §10.5.10 [[Delete]](P)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-delete-p>
    pub fn proxy_delete_property(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "deleteProperty")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val],
                )?;
                Ok(result.is_truthy())
            }
            None => {
                // No trap — forward to target.[[Delete]](P)
                if self.is_proxy(target) {
                    self.proxy_delete_property(target, property)
                } else {
                    let deleted = self.delete_named_property(target, property)?;
                    Ok(deleted)
                }
            }
        }
    }

    /// §10.5.7 [[HasProperty]](P)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-hasproperty-p>
    pub fn proxy_has(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "has")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val],
                )?;
                Ok(result.is_truthy())
            }
            None => {
                // No trap — forward to target.[[HasProperty]](P)
                if self.is_proxy(target) {
                    self.proxy_has(target, property)
                } else {
                    self.has_property(target, property)
                        .map_err(InterpreterError::from)
                }
            }
        }
    }

    /// §10.5.12 [[Call]](thisArgument, argumentsList)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-call-thisargument-argumentslist>
    pub fn proxy_apply(
        &mut self,
        proxy: ObjectHandle,
        this_arg: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "apply")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let args_array = self.alloc_array_with_elements(arguments);
                let args_val = RegisterValue::from_object_handle(args_array.0);
                self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, this_arg, args_val],
                )
            }
            None => {
                // No trap — forward to target.[[Call]](thisArgument, argumentsList)
                self.call_callable_for_accessor(Some(target), this_arg, arguments)
            }
        }
    }

    /// §10.5.13 [[Construct]](argumentsList, newTarget)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-construct-argumentslist-newtarget>
    pub fn proxy_construct(
        &mut self,
        proxy: ObjectHandle,
        arguments: &[RegisterValue],
        new_target: ObjectHandle,
    ) -> Result<RegisterValue, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "construct")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let new_target_val = RegisterValue::from_object_handle(new_target.0);
                let args_array = self.alloc_array_with_elements(arguments);
                let args_val = RegisterValue::from_object_handle(args_array.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, args_val, new_target_val],
                )?;
                // §10.5.13 step 10: the result of [[Construct]] must be an object
                if result.as_object_handle().is_none() {
                    return Err(
                        self.proxy_type_error("'construct' on proxy: trap returned non-Object")
                    );
                }
                Ok(result)
            }
            None => {
                // No trap — forward to target.[[Construct]](argumentsList, newTarget)
                match self.construct_callable(target, arguments, new_target) {
                    Ok(value) => Ok(value),
                    Err(VmNativeCallError::Thrown(value)) => {
                        Err(InterpreterError::UncaughtThrow(value))
                    }
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.1 [[GetPrototypeOf]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-getprototypeof>
    // -----------------------------------------------------------------------
    pub fn proxy_get_prototype_of(
        &mut self,
        proxy: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "getPrototypeOf")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                // Step 5: If Type(handlerProto) is neither Object nor Null, throw TypeError.
                if result == RegisterValue::null() {
                    // §10.5.1 step 8: invariant — if target is non-extensible, trap must
                    // return the same value as target.[[GetPrototypeOf]]().
                    let target_extensible = self
                        .objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if !target_extensible {
                        let target_proto = self
                            .objects
                            .get_prototype(target)
                            .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                        if target_proto.is_some() {
                            return Err(self.proxy_type_error(
                                "'getPrototypeOf' on proxy: proxy target is non-extensible but the trap returned a prototype different from the target's prototype",
                            ));
                        }
                    }
                    Ok(None)
                } else if let Some(h) = result.as_object_handle().map(ObjectHandle) {
                    // §10.5.1 step 8: invariant check
                    let target_extensible = self
                        .objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if !target_extensible {
                        let target_proto = self
                            .objects
                            .get_prototype(target)
                            .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                        if target_proto != Some(h) {
                            return Err(self.proxy_type_error(
                                "'getPrototypeOf' on proxy: proxy target is non-extensible but the trap returned a prototype different from the target's prototype",
                            ));
                        }
                    }
                    Ok(Some(h))
                } else {
                    Err(self.proxy_type_error(
                        "'getPrototypeOf' on proxy: trap returned neither object nor null",
                    ))
                }
            }
            None => {
                // No trap — forward to target.[[GetPrototypeOf]]()
                if self.is_proxy(target) {
                    self.proxy_get_prototype_of(target)
                } else {
                    self.objects
                        .get_prototype(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.2 [[SetPrototypeOf]](V)
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-setprototypeof-v>
    // -----------------------------------------------------------------------
    pub fn proxy_set_prototype_of(
        &mut self,
        proxy: ObjectHandle,
        prototype: Option<ObjectHandle>,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "setPrototypeOf")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let proto_val = prototype
                    .map(|h| RegisterValue::from_object_handle(h.0))
                    .unwrap_or_else(RegisterValue::null);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, proto_val],
                )?;
                let boolean_trap_result = result.is_truthy();
                if !boolean_trap_result {
                    return Ok(false);
                }
                // §10.5.2 step 12: invariant — if target is non-extensible, V must be
                // SameValue as target.[[GetPrototypeOf]]().
                let target_extensible = self
                    .objects
                    .is_extensible(target)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                if !target_extensible {
                    let target_proto = self
                        .objects
                        .get_prototype(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if target_proto != prototype {
                        return Err(self.proxy_type_error(
                            "'setPrototypeOf' on proxy: trap returned truish but the proxy target is non-extensible and the new prototype is different from the current one",
                        ));
                    }
                }
                Ok(true)
            }
            None => {
                // No trap — forward to target.[[SetPrototypeOf]](V)
                if self.is_proxy(target) {
                    self.proxy_set_prototype_of(target, prototype)
                } else {
                    self.objects
                        .set_prototype(target, prototype)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.3 [[IsExtensible]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-isextensible>
    // -----------------------------------------------------------------------
    pub fn proxy_is_extensible(&mut self, proxy: ObjectHandle) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "isExtensible")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                let boolean_trap_result = result.is_truthy();
                // §10.5.3 step 8: invariant — must agree with target.[[IsExtensible]]()
                let target_extensible = self
                    .objects
                    .is_extensible(target)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                if boolean_trap_result != target_extensible {
                    return Err(self.proxy_type_error(
                        "'isExtensible' on proxy: trap result does not reflect extensibility of proxy target",
                    ));
                }
                Ok(boolean_trap_result)
            }
            None => {
                // No trap — forward to target.[[IsExtensible]]()
                if self.is_proxy(target) {
                    self.proxy_is_extensible(target)
                } else {
                    self.objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.4 [[PreventExtensions]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-preventextensions>
    // -----------------------------------------------------------------------
    pub fn proxy_prevent_extensions(
        &mut self,
        proxy: ObjectHandle,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "preventExtensions")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                let boolean_trap_result = result.is_truthy();
                // §10.5.4 step 8: if trap returns true, target must be non-extensible.
                if boolean_trap_result {
                    let target_extensible = self
                        .objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if target_extensible {
                        return Err(self.proxy_type_error(
                            "'preventExtensions' on proxy: trap returned truish but the proxy target is extensible",
                        ));
                    }
                }
                Ok(boolean_trap_result)
            }
            None => {
                // No trap — forward to target.[[PreventExtensions]]()
                if self.is_proxy(target) {
                    self.proxy_prevent_extensions(target)
                } else {
                    self.objects
                        .prevent_extensions(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.5 [[GetOwnProperty]](P)
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-getownproperty-p>
    // -----------------------------------------------------------------------
    pub fn proxy_get_own_property_descriptor(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "getOwnPropertyDescriptor")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val],
                )?;
                // Step 9: If Type(trapResultObj) is neither Object nor Undefined, throw TypeError.
                if result == RegisterValue::undefined() {
                    // §10.5.5 step 14: If targetDesc is not undefined and targetDesc.[[Configurable]]
                    // is false, throw TypeError.
                    let target_desc = self
                        .own_property_descriptor(target, property)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if let Some(td) = target_desc {
                        if !td.attributes().configurable() {
                            return Err(self.proxy_type_error(
                                "'getOwnPropertyDescriptor' on proxy: trap returned undefined for a non-configurable property",
                            ));
                        }
                        // §10.5.5 step 15: if target is non-extensible and property exists, cannot report as non-existent
                        let target_extensible = self
                            .objects
                            .is_extensible(target)
                            .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                        if !target_extensible {
                            return Err(self.proxy_type_error(
                                "'getOwnPropertyDescriptor' on proxy: trap returned undefined for an existing property on a non-extensible target",
                            ));
                        }
                    }
                    Ok(None)
                } else if let Some(desc_handle) = result.as_object_handle().map(ObjectHandle) {
                    // Convert the trap result to a PropertyDescriptor via ToPropertyDescriptor.
                    let desc = crate::abstract_ops::to_property_descriptor(Some(desc_handle), self)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                            VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                        })?;
                    // Convert PropertyDescriptor to PropertyValue using the descriptor's apply logic.
                    let pv = desc.to_property_value();
                    Ok(Some(pv))
                } else {
                    Err(self.proxy_type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap returned neither object nor undefined",
                    ))
                }
            }
            None => {
                // No trap — forward to target.[[GetOwnProperty]](P)
                if self.is_proxy(target) {
                    self.proxy_get_own_property_descriptor(target, property)
                } else {
                    self.own_property_descriptor(target, property)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.6 [[DefineOwnProperty]](P, Desc)
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-defineownproperty-p-desc>
    // -----------------------------------------------------------------------
    pub fn proxy_define_own_property(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
        desc_value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "defineProperty")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val, desc_value],
                )?;
                let boolean_trap_result = result.is_truthy();
                if !boolean_trap_result {
                    return Ok(false);
                }
                // §10.5.6 step 15: invariant — cannot define non-configurable property on
                // extensible target that doesn't have it, or change configurable→non-configurable.
                let target_desc = self
                    .own_property_descriptor(target, property)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                let target_extensible = self
                    .objects
                    .is_extensible(target)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                if target_desc.is_none() && !target_extensible {
                    return Err(self.proxy_type_error(
                        "'defineProperty' on proxy: trap returned truish for adding property to non-extensible target",
                    ));
                }
                Ok(true)
            }
            None => {
                // No trap — forward to target.[[DefineOwnProperty]](P, Desc)
                if self.is_proxy(target) {
                    self.proxy_define_own_property(target, property, desc_value)
                } else {
                    // Convert desc_value to PropertyDescriptor and apply.
                    let desc_handle = desc_value.as_object_handle().map(ObjectHandle);
                    let desc = crate::abstract_ops::to_property_descriptor(desc_handle, self)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                            VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                        })?;
                    let property_names = self.property_names().clone();
                    self.objects
                        .define_own_property_from_descriptor_with_registry(
                            target,
                            property,
                            desc,
                            &property_names,
                        )
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.11 [[OwnPropertyKeys]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-ownpropertykeys>
    // -----------------------------------------------------------------------
    pub fn proxy_own_keys(
        &mut self,
        proxy: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "ownKeys")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                // Step 7: CreateListFromArrayLike — the result must be an array-like
                // whose elements are Strings or Symbols.
                let Some(arr_handle) = result.as_object_handle().map(ObjectHandle) else {
                    return Err(
                        self.proxy_type_error("'ownKeys' on proxy: trap result is not an object")
                    );
                };
                let length_prop = self.intern_property_name("length");
                let length_val = self
                    .own_property_value(arr_handle, length_prop)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                let length = length_val.as_number().map(|n| n as usize).unwrap_or(0);
                let mut keys = Vec::with_capacity(length);
                for i in 0..length {
                    let index_key = self.intern_property_name(&i.to_string());
                    let elem = self
                        .own_property_value(arr_handle, index_key)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    // Each element must be a string (or symbol).
                    let key_id = self.property_name_from_value(elem).map_err(|e| match e {
                        VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                        VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                    })?;
                    keys.push(key_id);
                }
                Ok(keys)
            }
            None => {
                // No trap — forward to target.[[OwnPropertyKeys]]()
                if self.is_proxy(target) {
                    self.proxy_own_keys(target)
                } else {
                    self.own_property_keys(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    /// GC safepoint — called at loop back-edges and function call boundaries.
    /// Collects roots from intrinsics and the provided register window,
    /// then triggers collection if memory pressure warrants it.
    pub fn gc_safepoint(&mut self, registers: &[RegisterValue]) {
        let mut roots = self.intrinsics().gc_root_handles();
        // Extract ObjectHandle roots from the current register window.
        for reg in registers {
            if let Some(handle) = reg.as_object_handle() {
                roots.push(ObjectHandle(handle));
            }
        }
        self.objects.maybe_collect_garbage(&roots);
    }

    /// Allocates one ordinary object with the runtime default prototype.
    pub fn alloc_object(&mut self) -> ObjectHandle {
        let prototype = self.intrinsics().object_prototype();
        let handle = self.objects.alloc_object();
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("ordinary object prototype should exist");
        handle
    }

    /// Allocates one ordinary object with an explicit prototype.
    pub fn alloc_object_with_prototype(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let handle = self.objects.alloc_object();
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit object prototype should be valid");
        handle
    }

    /// Allocates one ordinary object that carries a Rust-owned native payload.
    pub fn alloc_native_object<T>(&mut self, payload: T) -> ObjectHandle
    where
        T: VmTrace + Any,
    {
        let prototype = self.intrinsics().object_prototype();
        self.alloc_native_object_with_prototype(Some(prototype), payload)
    }

    /// Allocates one payload-bearing object with an explicit prototype.
    pub fn alloc_native_object_with_prototype<T>(
        &mut self,
        prototype: Option<ObjectHandle>,
        payload: T,
    ) -> ObjectHandle
    where
        T: VmTrace + Any,
    {
        let payload = self.native_payloads.insert(payload);
        let handle = self.objects.alloc_native_object(payload);
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit native object prototype should be valid");
        handle
    }

    /// Allocates one dense array with the runtime default prototype.
    pub fn alloc_array(&mut self) -> ObjectHandle {
        let prototype = self.intrinsics().array_prototype();
        let handle = self.objects.alloc_array();
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("array prototype should exist");
        handle
    }

    /// Allocates an array and populates it with initial elements.
    pub fn alloc_array_with_elements(&mut self, elements: &[RegisterValue]) -> ObjectHandle {
        let handle = self.alloc_array();
        for &elem in elements {
            self.objects
                .push_element(handle, elem)
                .expect("array push should succeed");
        }
        handle
    }

    /// Extracts elements from an array handle into a Vec of RegisterValues.
    pub fn array_to_args(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        self.objects
            .array_elements(handle)
            .map_err(|e| VmNativeCallError::Internal(format!("array_to_args failed: {e:?}").into()))
    }

    pub fn list_from_array_like(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        let length_key = self.intern_property_name("length");
        let receiver = RegisterValue::from_object_handle(handle.0);
        let length_value = self.ordinary_get(handle, length_key, receiver)?;
        let length = usize::try_from(self.js_to_uint32(length_value).map_err(
            |error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            },
        )?)
        .unwrap_or(usize::MAX);

        let mut values = Vec::with_capacity(length);
        for index in 0..length {
            let property = self.intern_property_name(&index.to_string());
            let value = self.ordinary_get(handle, property, receiver)?;
            values.push(value);
        }
        Ok(values)
    }

    /// Allocates one string object with the runtime default prototype.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let prototype = self.intrinsics().string_prototype();
        let handle = self.objects.alloc_string(value);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        handle
    }

    /// Allocates a string from a WTF-16 `JsString` with the runtime default prototype.
    ///
    /// Preserves lone surrogates as-is.
    pub fn alloc_js_string(&mut self, value: crate::js_string::JsString) -> ObjectHandle {
        let prototype = self.intrinsics().string_prototype();
        let handle = self.objects.alloc_js_string(value);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        handle
    }

    /// Allocates one BigInt heap value (no prototype — BigInt is a primitive type).
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn alloc_bigint(&mut self, value: &str) -> ObjectHandle {
        self.objects.alloc_bigint(value)
    }

    /// Allocates a fully-initialized RegExp instance with the spec-mandated
    /// own `lastIndex` property.
    ///
    /// §22.2.3.1 RegExpCreate / §22.2.3.1.1 RegExpAlloc steps 4-5 require the
    /// object to expose `lastIndex` as a data property with attributes
    /// `{ [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: false }`
    /// and value 0. Defining it up front (instead of letting the first write
    /// create a writable/enumerable/configurable slot) is what lets
    /// `/./.lastIndex === 0`, `verifyProperty` checks, and `delete re.lastIndex`
    /// behave per spec.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-regexpcreate>
    pub fn alloc_regexp(
        &mut self,
        pattern: &str,
        flags: &str,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let handle = self.objects.alloc_regexp(pattern, flags, prototype);
        let last_index = self.intern_property_name("lastIndex");
        let descriptor = crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_i32(0),
            crate::object::PropertyAttributes::from_flags(true, false, false),
        );
        self.objects
            .define_own_property(handle, last_index, descriptor)
            .ok();
        handle
    }

    /// Returns the decimal string backing a BigInt handle.
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn bigint_value(&self, handle: ObjectHandle) -> Option<&str> {
        self.objects.bigint_value(handle).ok().flatten()
    }

    /// Allocates one fresh symbol primitive with a VM-wide stable identifier.
    pub fn alloc_symbol(&mut self) -> RegisterValue {
        self.alloc_symbol_with_description(None)
    }

    /// Allocates one fresh symbol primitive and records its optional description.
    pub fn alloc_symbol_with_description(
        &mut self,
        description: Option<Box<str>>,
    ) -> RegisterValue {
        let symbol_id = self.next_symbol_id;
        self.next_symbol_id = self
            .next_symbol_id
            .checked_add(1)
            .expect("symbol identifier space exhausted");
        self.symbol_descriptions.insert(symbol_id, description);
        RegisterValue::from_symbol_id(symbol_id)
    }

    /// Returns the recorded description for a symbol value, if any.
    #[must_use]
    pub fn symbol_description(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.symbol_descriptions
            .get(&symbol_id)
            .and_then(|description| description.as_deref())
    }

    /// Interns a global-registry symbol key and returns the canonical symbol value.
    pub fn intern_global_symbol(&mut self, key: Box<str>) -> RegisterValue {
        if let Some(&symbol_id) = self.global_symbol_registry.get(key.as_ref()) {
            return RegisterValue::from_symbol_id(symbol_id);
        }

        let symbol = self.alloc_symbol_with_description(Some(key.clone()));
        let symbol_id = symbol
            .as_symbol_id()
            .expect("allocated symbol should expose a symbol id");
        self.global_symbol_registry.insert(key.clone(), symbol_id);
        self.global_symbol_registry_reverse.insert(symbol_id, key);
        symbol
    }

    /// Returns the registry key for a symbol value, if it was created via `Symbol.for`.
    #[must_use]
    pub fn symbol_registry_key(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.global_symbol_registry_reverse
            .get(&symbol_id)
            .map(Box::as_ref)
    }

    /// Allocates a new symbol from a JS-visible description value.
    pub fn create_symbol_from_value(
        &mut self,
        description: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if description == RegisterValue::undefined() {
            return Ok(self.alloc_symbol_with_description(None));
        }
        let description = self.coerce_symbol_string(description)?;
        Ok(self.alloc_symbol_with_description(Some(description)))
    }

    /// Resolves `Symbol.for(key)` using the runtime-wide global symbol registry.
    pub fn symbol_for_value(
        &mut self,
        key: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let key = self.coerce_symbol_string(key)?;
        Ok(self.intern_global_symbol(key))
    }

    fn coerce_symbol_string(&mut self, value: RegisterValue) -> Result<Box<str>, InterpreterError> {
        self.js_to_string(value)
    }

    /// Allocates one host-callable function with the runtime default prototype.
    /// The function is bound to the runtime's currently-active realm.
    pub fn alloc_host_function(&mut self, function: HostFunctionId) -> ObjectHandle {
        let prototype = self.intrinsics().function_prototype();
        let realm = self.current_realm;
        let handle = self.objects.alloc_host_function(function, realm);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        handle
    }

    /// Allocates one host function from descriptor metadata and installs `.name` / `.length`.
    pub fn alloc_host_function_from_descriptor(
        &mut self,
        descriptor: NativeFunctionDescriptor,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let js_name = descriptor.js_name().to_string();
        let length = descriptor.length();
        let host_function = self.register_native_function(descriptor);
        let handle = self.alloc_host_function(host_function);
        self.install_host_function_length_name(handle, length, &js_name)?;
        Ok(handle)
    }

    /// Installs descriptor-driven members onto one existing host-owned object.
    pub fn install_burrow(
        &mut self,
        target: ObjectHandle,
        descriptors: &[NativeFunctionDescriptor],
    ) -> Result<(), VmNativeCallError> {
        let plan = BurrowBuilder::from_descriptors(descriptors)
            .map(BurrowBuilder::build)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to normalize host object surface: {error}").into(),
                )
            })?;

        for member in plan.members() {
            match member {
                ObjectMemberPlan::Method(function) => {
                    let host_function = self.register_native_function(function.clone());
                    let handle = self.alloc_host_function(host_function);
                    self.install_host_function_length_name(
                        handle,
                        function.length(),
                        function.js_name(),
                    )?;
                    let property = self.intern_property_name(function.js_name());
                    self.objects
                        .define_own_property(
                            target,
                            property,
                            PropertyValue::data_with_attrs(
                                RegisterValue::from_object_handle(handle.0),
                                PropertyAttributes::builtin_method(),
                            ),
                        )
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!(
                                    "failed to install host object method '{}': {error:?}",
                                    function.js_name()
                                )
                                .into(),
                            )
                        })?;
                }
                ObjectMemberPlan::Accessor(accessor) => {
                    let getter = accessor
                        .getter()
                        .cloned()
                        .map(|descriptor| {
                            let function = self.register_native_function(descriptor);
                            Ok(self.alloc_host_function(function))
                        })
                        .transpose()?;
                    let setter = accessor
                        .setter()
                        .cloned()
                        .map(|descriptor| {
                            let function = self.register_native_function(descriptor);
                            Ok(self.alloc_host_function(function))
                        })
                        .transpose()?;
                    let property = self.intern_property_name(accessor.js_name());
                    self.objects
                        .define_accessor(target, property, getter, setter)
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!(
                                    "failed to install host object accessor '{}': {error:?}",
                                    accessor.js_name()
                                )
                                .into(),
                            )
                        })?;
                }
            }
        }

        Ok(())
    }

    /// Registers a native function and installs it as a property on the global object.
    ///
    /// This is the primary API for embedders to inject host-provided globals
    /// (e.g., `print`, `$DONE`, `$262`) into the runtime.
    pub fn install_native_global(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> ObjectHandle {
        let host_fn = self.native_functions.register(descriptor);
        let handle = self.alloc_host_function(host_fn);
        let global = self.intrinsics().global_object();
        let prop = self.property_names.intern(
            self.native_functions
                .get(host_fn)
                .expect("just registered")
                .js_name(),
        );
        self.objects
            .set_property(global, prop, RegisterValue::from_object_handle(handle.0))
            .expect("global property installation should succeed");
        handle
    }

    /// Installs a value property on the global object.
    pub fn install_global_value(&mut self, name: &str, value: RegisterValue) {
        let global = self.intrinsics().global_object();
        let prop = self.property_names.intern(name);
        self.objects
            .set_property(global, prop, value)
            .expect("global property installation should succeed");
    }

    fn install_host_function_length_name(
        &mut self,
        handle: ObjectHandle,
        length: u16,
        name: &str,
    ) -> Result<(), VmNativeCallError> {
        let length_prop = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install function length for '{name}': {error:?}").into(),
                )
            })?;

        let name_prop = self.intern_property_name("name");
        let name_handle = self.alloc_string(name);
        self.objects
            .define_own_property(
                handle,
                name_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install function name for '{name}': {error:?}").into(),
                )
            })?;

        Ok(())
    }

    /// Allocates one bytecode closure with the runtime default function prototype.
    /// The closure is bound to the runtime's currently-active realm.
    pub fn alloc_closure(
        &mut self,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
        flags: ObjectClosureFlags,
    ) -> ObjectHandle {
        // Generator functions should have %GeneratorFunction.prototype%
        // as their [[Prototype]], not %Function.prototype%.
        let prototype = if flags.is_generator() {
            self.intrinsics().generator_function_prototype()
        } else {
            self.intrinsics().function_prototype()
        };
        let module = self
            .current_module
            .clone()
            .expect("closure allocation requires active module context");
        let realm = self.current_realm;
        let handle = self
            .objects
            .alloc_closure(module, callee, upvalues, flags, realm);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        let closure_length = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .map(|function| function.length())
            .unwrap_or(0);
        let closure_name = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .and_then(|function| function.name())
            .unwrap_or("")
            .to_string();
        let length_property = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(closure_length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure length should install");
        let name_property = self.intern_property_name("name");
        let name_handle = self.alloc_string(closure_name);
        self.objects
            .define_own_property(
                handle,
                name_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure name should install");
        // §10.2.6 MakeConstructor + §27.3.3 — Constructable closures AND
        // generator functions get a `.prototype` own property. Generators
        // are not constructable but still get `.prototype` per §27.3.3.
        if flags.is_constructable() || flags.is_generator() {
            let prototype_property = self.intern_property_name("prototype");
            let constructor_property = self.intern_property_name("constructor");
            let instance_prototype = self.alloc_object();
            self.objects
                .define_own_property(
                    handle,
                    prototype_property,
                    PropertyValue::data_with_attrs(
                        RegisterValue::from_object_handle(instance_prototype.0),
                        PropertyAttributes::function_prototype(),
                    ),
                )
                .expect("closure prototype object should install");
            // §27.3.3 — Generator function prototypes do NOT get a
            // `.constructor` back-link. Only regular constructors do.
            if !flags.is_generator() {
                self.objects
                    .define_own_property(
                        instance_prototype,
                        constructor_property,
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_object_handle(handle.0),
                            PropertyAttributes::constructor_link(),
                        ),
                    )
                    .expect("closure prototype.constructor should install");
            }
        }

        handle
    }

    /// ES2024 §7.2.1 Type — returns `true` when the value is an ECMAScript
    /// Object (not a primitive). In our VM, strings and BigInts are heap-
    /// allocated but are still primitives per the spec.
    pub fn is_ecma_object(&self, value: RegisterValue) -> bool {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return false;
        };
        !matches!(
            self.objects.kind(handle),
            Ok(HeapValueKind::String | HeapValueKind::BigInt)
        )
    }

    /// ES2024 §7.2.4 IsConstructor — checks if a value has `[[Construct]]`.
    pub fn is_constructible(&self, handle: ObjectHandle) -> bool {
        match self.objects.kind(handle) {
            Ok(HeapValueKind::HostFunction) => {
                // Host functions are constructors only if registered with Constructor slot kind.
                if let Ok(Some(host_fn_id)) = self.objects.host_function(handle) {
                    self.native_functions.get(host_fn_id).is_some_and(|desc| {
                        desc.slot_kind() == crate::descriptors::NativeSlotKind::Constructor
                    })
                } else {
                    false
                }
            }
            Ok(HeapValueKind::Closure) => self
                .objects
                .closure_flags(handle)
                .is_ok_and(|f| f.is_constructable()),
            Ok(HeapValueKind::BoundFunction) => self
                .objects
                .bound_function_parts(handle)
                .is_ok_and(|(target, _, _)| self.is_constructible(target)),
            Ok(HeapValueKind::Proxy) => {
                // A proxy is constructible if its target is constructible.
                self.objects
                    .proxy_parts(handle)
                    .is_ok_and(|(target, _)| self.is_constructible(target))
            }
            _ => false,
        }
    }

    /// Resolves one native payload from a payload-bearing object.
    pub fn native_payload<T>(&self, handle: ObjectHandle) -> Result<&T, NativePayloadError>
    where
        T: Any,
    {
        let payload = self
            .objects
            .native_payload_id(handle)?
            .ok_or(NativePayloadError::MissingPayload)?;
        self.native_payloads.get::<T>(payload)
    }

    /// Resolves one mutable native payload from a payload-bearing object.
    pub fn native_payload_mut<T>(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<&mut T, NativePayloadError>
    where
        T: Any,
    {
        let payload = self
            .objects
            .native_payload_id(handle)?
            .ok_or(NativePayloadError::MissingPayload)?;
        self.native_payloads.get_mut::<T>(payload)
    }

    /// Resolves one native payload from a JS-visible receiver value.
    pub fn native_payload_from_value<T>(
        &self,
        value: &RegisterValue,
    ) -> Result<&T, NativePayloadError>
    where
        T: Any,
    {
        let handle = value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(NativePayloadError::ExpectedObjectValue)?;
        self.native_payload::<T>(handle)
    }

    /// Resolves one mutable native payload from a JS-visible receiver value.
    pub fn native_payload_mut_from_value<T>(
        &mut self,
        value: &RegisterValue,
    ) -> Result<&mut T, NativePayloadError>
    where
        T: Any,
    {
        let handle = value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(NativePayloadError::ExpectedObjectValue)?;
        self.native_payload_mut::<T>(handle)
    }

    /// Traces GC-visible values stored inside native payload-bearing objects.
    pub fn trace_native_payload_roots(
        &self,
        tracer: &mut dyn VmValueTracer,
    ) -> Result<(), NativePayloadError> {
        let mut result = Ok(());
        self.objects
            .trace_native_payload_links(&mut |_handle, payload| {
                if result.is_ok() {
                    result = self.native_payloads.trace_payload(payload, tracer);
                }
            });
        result
    }

    /// Converts a JS-visible property key value into the runtime property-name id.
    pub fn property_name_from_value(
        &mut self,
        value: RegisterValue,
    ) -> Result<PropertyNameId, VmNativeCallError> {
        crate::abstract_ops::to_property_key(self, value)
    }

    /// Executes ordinary named-property `[[Get]]` with an explicit receiver.
    pub fn ordinary_get(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
        receiver: RegisterValue,
    ) -> Result<RegisterValue, VmNativeCallError> {
        match self.property_lookup(target, property).map_err(|error| {
            VmNativeCallError::Internal(format!("ordinary get failed: {error:?}").into())
        })? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { value, .. } => Ok(value),
                PropertyValue::Accessor { getter, .. } => self
                    .call_callable_for_accessor(getter, receiver, &[])
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    }),
            },
            None => Ok(RegisterValue::undefined()),
        }
    }

    /// Executes ordinary named-property `[[Set]]` with an explicit receiver.
    pub fn ordinary_set(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
        receiver: RegisterValue,
        value: RegisterValue,
    ) -> Result<bool, VmNativeCallError> {
        match self.property_lookup(target, property).map_err(|error| {
            VmNativeCallError::Internal(format!("ordinary set failed: {error:?}").into())
        })? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { attributes, .. } => {
                    let Some(receiver_handle) =
                        self.non_string_object_handle(receiver).map_err(|error| {
                            VmNativeCallError::Internal(
                                format!("ordinary set receiver check failed: {error:?}").into(),
                            )
                        })?
                    else {
                        return Ok(false);
                    };

                    if !attributes.writable() {
                        return Ok(false);
                    }

                    if lookup.owner() == receiver_handle {
                        if let Some(cache) = lookup.cache() {
                            let updated = self
                                .objects
                                .set_cached(receiver_handle, property, value, cache)
                                .map_err(|error| {
                                    VmNativeCallError::Internal(
                                        format!("ordinary set receiver update failed: {error:?}")
                                            .into(),
                                    )
                                })?;
                            if !updated {
                                self.objects
                                    .set_property(receiver_handle, property, value)
                                    .map_err(|error| {
                                        VmNativeCallError::Internal(
                                            format!(
                                                "ordinary set receiver fallback failed: {error:?}"
                                            )
                                            .into(),
                                        )
                                    })?;
                            }
                            return Ok(true);
                        }

                        return self.ordinary_set_on_receiver(receiver_handle, property, value);
                    }

                    self.ordinary_set_on_receiver(receiver_handle, property, value)
                }
                PropertyValue::Accessor { setter, .. } => {
                    let _ = self
                        .call_callable_for_accessor(setter, receiver, &[value])
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            InterpreterError::NativeCall(message)
                            | InterpreterError::TypeError(message) => {
                                VmNativeCallError::Internal(message)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?;
                    Ok(setter.is_some())
                }
            },
            None => {
                let Some(receiver_handle) =
                    self.non_string_object_handle(receiver).map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("ordinary set receiver create check failed: {error:?}").into(),
                        )
                    })?
                else {
                    return Ok(false);
                };
                self.ordinary_set_on_receiver(receiver_handle, property, value)
            }
        }
    }

    fn ordinary_set_on_receiver(
        &mut self,
        receiver_handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, VmNativeCallError> {
        match self
            .own_property_descriptor(receiver_handle, property)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("ordinary set receiver own-descriptor failed: {error:?}").into(),
                )
            })? {
            Some(PropertyValue::Data { attributes, .. }) => {
                if !attributes.writable() {
                    return Ok(false);
                }
                self.set_named_property(receiver_handle, property, value)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                self.receiver_data_property_matches(receiver_handle, property, value)
            }
            Some(PropertyValue::Accessor { setter, .. }) => {
                let _ = self
                    .call_callable_for_accessor(
                        setter,
                        RegisterValue::from_object_handle(receiver_handle.0),
                        &[value],
                    )
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                Ok(setter.is_some())
            }
            None => {
                if !self
                    .objects
                    .is_extensible(receiver_handle)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("ordinary set receiver extensible check failed: {error:?}")
                                .into(),
                        )
                    })?
                {
                    return Ok(false);
                }
                self.set_named_property(receiver_handle, property, value)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                self.receiver_data_property_matches(receiver_handle, property, value)
            }
        }
    }

    fn receiver_data_property_matches(
        &mut self,
        receiver_handle: ObjectHandle,
        property: PropertyNameId,
        expected: RegisterValue,
    ) -> Result<bool, VmNativeCallError> {
        let descriptor = self
            .own_property_descriptor(receiver_handle, property)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("ordinary set receiver verification failed: {error:?}").into(),
                )
            })?;
        match descriptor {
            Some(PropertyValue::Data { value, .. }) => {
                self.objects.same_value(value, expected).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("ordinary set receiver SameValue failed: {error:?}").into(),
                    )
                })
            }
            _ => Ok(false),
        }
    }

    pub fn call_host_function(
        &mut self,
        callable: Option<ObjectHandle>,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.check_interrupt()?;

        let Some(callable) = callable else {
            return Ok(RegisterValue::undefined());
        };

        // ES2024 §10.4.1.1 [[Call]] — resolve bound function chain.
        if let Ok(HeapValueKind::BoundFunction) = self.objects.kind(callable) {
            let (target, bound_this, bound_args) =
                self.objects.bound_function_parts(callable).map_err(|e| {
                    VmNativeCallError::Internal(format!("bound function resolution: {e:?}").into())
                })?;
            // Prepend bound_args to arguments.
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return self.call_host_function(Some(target), bound_this, &full_args);
        }

        // ES2024 §27.2.1.3 — Promise capability resolve/reject functions.
        if let Ok(HeapValueKind::PromiseCapabilityFunction) = self.objects.kind(callable) {
            let value = arguments
                .first()
                .copied()
                .unwrap_or(RegisterValue::undefined());
            Interpreter::invoke_promise_capability_function(self, callable, value).map_err(
                |e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                },
            )?;
            return Ok(RegisterValue::undefined());
        }

        // Promise combinator/finally/thunk dispatch.
        match self.objects.kind(callable) {
            Ok(HeapValueKind::PromiseCombinatorElement) => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                return Interpreter::invoke_promise_combinator_element(self, callable, value)
                    .map_err(|e| match e {
                        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    });
            }
            Ok(HeapValueKind::PromiseFinallyFunction) => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                return Interpreter::invoke_promise_finally_function(self, callable, value)
                    .map_err(|e| match e {
                        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    });
            }
            Ok(HeapValueKind::PromiseValueThunk) => {
                if let Some((v, k)) = self.objects.promise_value_thunk_info(callable) {
                    return match k {
                        crate::promise::PromiseFinallyKind::ThenFinally => Ok(v),
                        crate::promise::PromiseFinallyKind::CatchFinally => {
                            Err(VmNativeCallError::Thrown(v))
                        }
                    };
                }
            }
            _ => {}
        }

        // If it's a Closure (compiled JS function), dispatch through Interpreter::call_function.
        if let Ok(HeapValueKind::Closure) = self.objects.kind(callable) {
            // call_function ignores the module param for closures (gets it from the closure).
            // We need a Module reference, so extract from the closure itself.
            let module = self.objects.closure_module(callable).map_err(|e| {
                VmNativeCallError::Internal(format!("closure module lookup: {e:?}").into())
            })?;
            return Interpreter::call_function(self, &module, callable, receiver, arguments)
                .map_err(|e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                });
        }

        let host_function = self
            .objects
            .host_function(callable)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("native callable lookup failed: {error:?}").into(),
                )
            })?
            .ok_or_else(|| {
                VmNativeCallError::Internal("native callable is not a host function".into())
            })?;
        let descriptor = self
            .native_functions
            .get(host_function)
            .cloned()
            .ok_or_else(|| {
                VmNativeCallError::Internal("host function descriptor is missing".into())
            })?;

        self.native_callee_stack.push(callable);
        let result = (descriptor.callback())(&receiver, arguments, self);
        self.native_callee_stack.pop();
        self.check_interrupt()?;
        match result {
            Ok(value) => Ok(value),
            Err(VmNativeCallError::Thrown(value)) => Err(VmNativeCallError::Thrown(value)),
            Err(VmNativeCallError::Internal(message)) => Err(VmNativeCallError::Internal(message)),
        }
    }

    /// Allocates a reusable VM promise backed by the runtime's intrinsic Promise prototype.
    pub fn alloc_vm_promise(&mut self) -> crate::promise::VmPromise {
        let promise_prototype = self.intrinsics().promise_prototype();
        let promise = self
            .objects_mut()
            .alloc_promise_with_proto(promise_prototype);
        let resolve = self
            .objects_mut()
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Fulfill);
        let reject = self
            .objects_mut()
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Reject);
        if let Some(js_promise) = self.objects_mut().get_promise_mut(promise) {
            js_promise.resolve_function = Some(resolve);
            js_promise.reject_function = Some(reject);
        }
        crate::promise::VmPromise::new(crate::promise::PromiseCapability {
            promise,
            resolve,
            reject,
        })
    }

    /// Settles one reusable VM promise through its resolve capability function.
    pub fn fulfill_vm_promise(
        &mut self,
        promise: crate::promise::VmPromise,
        value: RegisterValue,
    ) -> Result<(), VmNativeCallError> {
        self.call_host_function(
            Some(promise.resolve_handle()),
            RegisterValue::undefined(),
            &[value],
        )?;
        Ok(())
    }

    /// Settles one reusable VM promise through its reject capability function.
    pub fn reject_vm_promise(
        &mut self,
        promise: crate::promise::VmPromise,
        reason: RegisterValue,
    ) -> Result<(), VmNativeCallError> {
        self.call_host_function(
            Some(promise.reject_handle()),
            RegisterValue::undefined(),
            &[reason],
        )?;
        Ok(())
    }

    /// Allocates and immediately fulfills one reusable VM promise.
    pub fn alloc_fulfilled_vm_promise(
        &mut self,
        value: RegisterValue,
    ) -> Result<crate::promise::VmPromise, VmNativeCallError> {
        let promise = self.alloc_vm_promise();
        self.fulfill_vm_promise(promise, value)?;
        Ok(promise)
    }

    /// Allocates and immediately rejects one reusable VM promise.
    pub fn alloc_rejected_vm_promise(
        &mut self,
        reason: RegisterValue,
    ) -> Result<crate::promise::VmPromise, VmNativeCallError> {
        let promise = self.alloc_vm_promise();
        self.reject_vm_promise(promise, reason)?;
        Ok(promise)
    }

    /// Allocates a promise already fulfilled with the provided value.
    pub fn alloc_resolved_promise(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        Ok(self.alloc_fulfilled_vm_promise(value)?.promise_handle())
    }

    /// Allocates a promise already rejected with the provided reason.
    pub fn alloc_rejected_promise(
        &mut self,
        reason: RegisterValue,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        Ok(self.alloc_rejected_vm_promise(reason)?.promise_handle())
    }

    /// Allocates one iterator result object `{ value, done }`.
    pub fn alloc_iter_result_object(
        &mut self,
        value: RegisterValue,
        done: bool,
    ) -> Result<RegisterValue, VmNativeCallError> {
        crate::intrinsics::create_iter_result_object(value, done, self)
    }

    pub fn call_callable(
        &mut self,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.call_callable_for_accessor(Some(callable), receiver, arguments)
            .map_err(|error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::TypeError(message) => {
                    // Convert TypeError to a catchable JS TypeError so
                    // `assert.throws(TypeError, ...)` can intercept it.
                    match self.alloc_type_error(&message) {
                        Ok(handle) => {
                            VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
                        }
                        Err(_) => VmNativeCallError::Internal(message),
                    }
                }
                InterpreterError::NativeCall(message) => VmNativeCallError::Internal(message),
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })
    }

    pub fn construct_callable(
        &mut self,
        target: ObjectHandle,
        arguments: &[RegisterValue],
        new_target: ObjectHandle,
    ) -> Result<RegisterValue, VmNativeCallError> {
        if !self.is_constructible(target) {
            let error = self
                .alloc_type_error("construct target is not constructible")
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct TypeError allocation failed: {error}").into(),
                    )
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ));
        }
        if !self.is_constructible(new_target) {
            let error = self
                .alloc_type_error("construct newTarget is not constructible")
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct TypeError allocation failed: {error}").into(),
                    )
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ));
        }
        let kind = self.objects.kind(target).map_err(|error| {
            VmNativeCallError::Internal(
                format!("construct target kind lookup failed: {error:?}").into(),
            )
        })?;
        let completion = match kind {
            HeapValueKind::BoundFunction => {
                let (bound_target, _, bound_args) =
                    self.objects.bound_function_parts(target).map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("construct bound function lookup failed: {error:?}").into(),
                        )
                    })?;
                let mut full_args = bound_args;
                full_args.extend_from_slice(arguments);
                let forwarded_new_target = if new_target == target {
                    bound_target
                } else {
                    new_target
                };
                return self.construct_callable(bound_target, &full_args, forwarded_new_target);
            }
            HeapValueKind::HostFunction => {
                let host_function = self
                    .objects
                    .host_function(target)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("construct host function lookup failed: {error:?}").into(),
                        )
                    })?
                    .ok_or_else(|| {
                        VmNativeCallError::Internal(
                            "construct target host function is missing".into(),
                        )
                    })?;
                let intrinsic_default =
                    Interpreter::host_function_default_intrinsic(self, host_function);
                let default_receiver = RegisterValue::from_object_handle(
                    Interpreter::allocate_construct_receiver(self, new_target, intrinsic_default)
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?
                        .0,
                );
                let completion = Interpreter::invoke_registered_host_function(
                    self,
                    host_function,
                    target,
                    default_receiver,
                    arguments,
                    true,
                )
                .map_err(|error| match error {
                    InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                })?;
                Interpreter::apply_construct_return_override(completion, default_receiver)
            }
            HeapValueKind::Closure => {
                let module = self.objects.closure_module(target).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct closure module lookup failed: {error:?}").into(),
                    )
                })?;
                let callee_index = self.objects.closure_callee(target).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct closure callee lookup failed: {error:?}").into(),
                    )
                })?;
                let callee_function = module.function(callee_index).ok_or_else(|| {
                    VmNativeCallError::Internal("construct closure callee is missing".into())
                })?;
                let register_count = callee_function.frame_layout().register_count();
                let is_derived_constructor = callee_function.is_derived_constructor();
                let default_receiver = if is_derived_constructor {
                    RegisterValue::undefined()
                } else {
                    RegisterValue::from_object_handle(
                        Interpreter::allocate_construct_receiver(
                            self,
                            new_target,
                            crate::intrinsics::IntrinsicKey::ObjectPrototype,
                        )
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?
                        .0,
                    )
                };
                let mut activation = Activation::with_context(
                    callee_index,
                    register_count,
                    FrameMetadata::new(
                        arguments.len() as RegisterIndex,
                        FrameFlags::new(true, true, false),
                    ),
                    Some(target),
                );
                activation.set_construct_new_target(Some(new_target));

                if callee_function.frame_layout().receiver_slot().is_some() {
                    activation
                        .set_receiver(callee_function, default_receiver)
                        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
                }

                let param_count = callee_function.frame_layout().parameter_count();
                for (index, &argument) in arguments.iter().take(param_count as usize).enumerate() {
                    let register = callee_function
                        .frame_layout()
                        .resolve_user_visible(index as u16)
                        .ok_or_else(|| {
                            VmNativeCallError::Internal(
                                "construct argument register resolution failed".into(),
                            )
                        })?;
                    activation
                        .set_register(register, argument)
                        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
                }
                if arguments.len() > param_count as usize {
                    activation.overflow_args = arguments[param_count as usize..].to_vec();
                }

                let completion = Interpreter::for_runtime(self)
                    .run_completion_with_runtime(&module, &mut activation, self)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                if is_derived_constructor {
                    match completion {
                        Completion::Return(value) if self.is_ecma_object(value) => {
                            Completion::Return(value)
                        }
                        Completion::Return(value) if value != RegisterValue::undefined() => {
                            let error = self
                                .alloc_type_error(
                                    "Derived constructors may only return object or undefined values",
                                )
                                .map_err(|error| {
                                    VmNativeCallError::Internal(format!("{error}").into())
                                })?;
                            Completion::Throw(RegisterValue::from_object_handle(error.0))
                        }
                        Completion::Return(_) => {
                            // §10.2.1.3 [[Construct]] step 11: read `this`
                            // from the receiver slot. If `super()` was called
                            // from inside an arrow (which writes to the lexical
                            // "this" upvalue instead), the receiver slot may
                            // still hold `undefined`. Fall back to scanning the
                            // first few local registers for an initialized
                            // object — the compile-time "this" binding is
                            // always the first local allocated by
                            // `declare_this_binding`.
                            let mut this_value = RegisterValue::undefined();
                            if callee_function.frame_layout().receiver_slot().is_some() {
                                let recv =
                                    activation.receiver(callee_function).map_err(|error| {
                                        VmNativeCallError::Internal(format!("{error}").into())
                                    })?;
                                if self.is_ecma_object(recv) {
                                    this_value = recv;
                                } else {
                                    let local_range = callee_function.frame_layout().local_range();
                                    if !local_range.is_empty()
                                        && let Ok(val) = activation.register(
                                            callee_function
                                                .frame_layout()
                                                .resolve_user_visible(
                                                    callee_function
                                                        .frame_layout()
                                                        .parameter_count(),
                                                )
                                                .unwrap_or(0),
                                        )
                                        && self.is_ecma_object(val)
                                    {
                                        this_value = val;
                                    }
                                }
                            }
                            if self.is_ecma_object(this_value) {
                                Completion::Return(this_value)
                            } else {
                                let error = self
                                    .alloc_reference_error(
                                        "Must call super constructor in derived class before returning from derived constructor",
                                    )
                                    .map_err(|error| {
                                        VmNativeCallError::Internal(
                                            format!(
                                                "construct ReferenceError allocation failed: {error}"
                                            )
                                            .into(),
                                        )
                                    })?;
                                Completion::Throw(RegisterValue::from_object_handle(error.0))
                            }
                        }
                        Completion::Throw(value) => Completion::Throw(value),
                    }
                } else {
                    Interpreter::apply_construct_return_override(completion, default_receiver)
                }
            }
            _ => {
                return Err(VmNativeCallError::Internal(
                    "construct target is not callable".into(),
                ));
            }
        };

        match completion {
            Completion::Return(value) => Ok(value),
            Completion::Throw(value) => Err(VmNativeCallError::Thrown(value)),
        }
    }

    fn delete_named_property(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        self.objects
            .delete_property_with_registry(target, property, &self.property_names)
            .map_err(Into::into)
    }

    fn invalid_array_length_error(&mut self) -> InterpreterError {
        let prototype = self.intrinsics().range_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let message = self.alloc_string("Invalid array length");
        let message_prop = self.intern_property_name("message");
        self.objects
            .set_property(
                handle,
                message_prop,
                RegisterValue::from_object_handle(message.0),
            )
            .ok();
        InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(handle.0))
    }

    fn own_data_property(
        &mut self,
        handle: ObjectHandle,
        slot_name: &str,
    ) -> Result<Option<RegisterValue>, InterpreterError> {
        let backing = self.intern_property_name(slot_name);
        let Some(lookup) = self.objects.get_property(handle, backing)? else {
            return Ok(None);
        };
        if lookup.owner() != handle {
            return Ok(None);
        }
        let PropertyValue::Data { value, .. } = lookup.value() else {
            return Ok(None);
        };
        Ok(Some(value))
    }

    fn string_wrapper_data(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        Ok(self
            .own_data_property(handle, STRING_DATA_SLOT)?
            .and_then(|value| value.as_object_handle().map(ObjectHandle)))
    }

    /// §7.2.15 IsLooselyEqual(x, y)
    /// <https://tc39.es/ecma262/#sec-islooselyequal>
    fn js_loose_eq(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        if self.objects.strict_eq(lhs, rhs)? {
            return Ok(true);
        }
        if (lhs == RegisterValue::undefined() && rhs == RegisterValue::null())
            || (lhs == RegisterValue::null() && rhs == RegisterValue::undefined())
        {
            return Ok(true);
        }

        // §7.2.15 step 10-11: BigInt == Number comparison.
        if lhs.is_bigint() && rhs.as_number().is_some() {
            return self.bigint_equals_number(lhs, rhs);
        }
        if lhs.as_number().is_some() && rhs.is_bigint() {
            return self.bigint_equals_number(rhs, lhs);
        }

        // §7.2.15 step 12-13: BigInt == String comparison.
        if lhs.is_bigint() && self.value_is_string(rhs)? {
            let rhs_str = self.js_to_string(rhs)?;
            if let Ok(rhs_val) = rhs_str.parse::<num_bigint::BigInt>() {
                let lhs_val = self.parse_bigint_value(lhs)?;
                return Ok(lhs_val == rhs_val);
            }
            return Ok(false);
        }
        if self.value_is_string(lhs)? && rhs.is_bigint() {
            let lhs_str = self.js_to_string(lhs)?;
            if let Ok(lhs_val) = lhs_str.parse::<num_bigint::BigInt>() {
                let rhs_val = self.parse_bigint_value(rhs)?;
                return Ok(lhs_val == rhs_val);
            }
            return Ok(false);
        }

        let coerced_lhs = self.coerce_loose_equality_primitive(lhs)?;
        let coerced_rhs = self.coerce_loose_equality_primitive(rhs)?;
        if coerced_lhs == coerced_rhs {
            return Ok(true);
        }
        if coerced_lhs != lhs || coerced_rhs != rhs {
            return self.js_loose_eq(coerced_lhs, coerced_rhs);
        }

        Ok(false)
    }

    fn non_string_object_handle(
        &self,
        value: RegisterValue,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(None);
        };
        if matches!(self.objects.kind(handle)?, HeapValueKind::String) {
            return Ok(None);
        }
        Ok(Some(handle))
    }

    fn computed_property_name(
        &mut self,
        key: RegisterValue,
    ) -> Result<PropertyNameId, InterpreterError> {
        self.property_name_from_value(key)
            .map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("property key coercion threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })
    }

    /// ES2024 §10.2.9 SetFunctionName — overrides a closure's own `name` data
    /// property based on a runtime property key. Used when installing
    /// computed-key class methods/getters/setters (and object-literal methods)
    /// so their `Function.name` matches the evaluated key.
    ///
    /// For Symbol keys the name becomes `"[desc]"` (or `""` when the symbol
    /// has no description). For string/numeric keys the name is the `ToString`
    /// of the property key. An optional `prefix` (e.g. `"get"` / `"set"`) is
    /// prepended followed by a U+0020 SPACE, matching the spec.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-setfunctionname>
    fn update_closure_function_name(
        &mut self,
        closure: ObjectHandle,
        key: RegisterValue,
        prefix: Option<&str>,
    ) -> Result<(), InterpreterError> {
        // 1. Derive the base name from the property key.
        let base_name: String = if let Some(sid) = key.as_symbol_id() {
            match self
                .symbol_descriptions
                .get(&sid)
                .and_then(|description| description.as_deref())
            {
                Some(description) => format!("[{description}]"),
                None => String::new(),
            }
        } else if let Some(handle) = key.as_object_handle().map(ObjectHandle) {
            // String heap values (WTF-16). Fall back to stringifying whatever
            // the runtime considers the property key form.
            match self.objects.string_value(handle) {
                Ok(Some(js_string)) => js_string.to_string(),
                _ => {
                    // Non-string object keys should have been coerced upstream
                    // by ToPropertyKey; treat them as empty to stay defensive.
                    String::new()
                }
            }
        } else if let Some(i) = key.as_i32() {
            i.to_string()
        } else if let Some(f) = key.as_number() {
            // Numeric keys are rare as computed class-method keys — they would
            // already be stringified by ToPropertyKey upstream. Fall back to
            // Rust's float formatting here; it matches JS Number::toString for
            // the common integer/decimal cases we care about.
            format!("{f}")
        } else {
            String::new()
        };

        // 2. Apply the optional prefix.
        let full_name = match prefix {
            Some(prefix_str) => format!("{prefix_str} {base_name}"),
            None => base_name,
        };

        // 3. Define the "name" own property. The slot installed by
        //    `alloc_closure` is configurable, so re-defining it is legal.
        let name_property = self.intern_property_name("name");
        let name_value = self.alloc_string(full_name);
        self.objects
            .define_own_property(
                closure,
                name_property,
                crate::object::PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_value.0),
                    crate::object::PropertyAttributes::function_length(),
                ),
            )
            .map_err(|_| InterpreterError::TypeError("closure name define failed".into()))?;
        Ok(())
    }

    pub(crate) fn property_base_object_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            return Err(InterpreterError::TypeError(
                "Cannot read properties of null or undefined".into(),
            ));
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        if let Some(boolean) = value.as_bool() {
            let object =
                box_boolean_object(RegisterValue::from_bool(boolean), self).map_err(|error| {
                    match error {
                        VmNativeCallError::Thrown(_) => {
                            InterpreterError::TypeError("boolean boxing threw".into())
                        }
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    }
                })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed boolean should return object handle"),
            ));
        }
        if let Some(number) = value.as_number() {
            let object =
                box_number_object(RegisterValue::from_number(number), self).map_err(|error| {
                    match error {
                        VmNativeCallError::Thrown(_) => {
                            InterpreterError::TypeError("number boxing threw".into())
                        }
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    }
                })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed number should return object handle"),
            ));
        }
        if value.is_bigint() {
            let wrapper =
                self.alloc_object_with_prototype(Some(self.intrinsics().bigint_prototype()));
            return Ok(wrapper);
        }
        if value.is_symbol() {
            let object = box_symbol_object(value, self).map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("symbol boxing threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed symbol should return object handle"),
            ));
        }
        Err(InterpreterError::InvalidObjectValue)
    }

    pub(crate) fn property_set_target_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            return Err(InterpreterError::TypeError(
                "Cannot set properties of null or undefined".into(),
            ));
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        if value.as_bool().is_some() {
            return Ok(self.intrinsics().boolean_prototype());
        }
        if value.as_number().is_some() {
            return Ok(self.intrinsics().number_prototype());
        }
        if value.is_symbol() {
            return Ok(self.intrinsics().symbol_prototype());
        }
        Err(InterpreterError::InvalidObjectValue)
    }

    fn is_primitive_property_base(&self, value: RegisterValue) -> Result<bool, ObjectError> {
        if value.as_bool().is_some() || value.as_number().is_some() || value.is_symbol() {
            return Ok(true);
        }
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        Ok(matches!(self.objects.kind(handle)?, HeapValueKind::String))
    }

    fn ordinary_to_primitive(
        &mut self,
        value: RegisterValue,
        hint: ToPrimitiveHint,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        let method_names = match hint {
            ToPrimitiveHint::String => ["toString", "valueOf"],
            ToPrimitiveHint::Number => ["valueOf", "toString"],
        };

        for method_name in method_names {
            let property = self.intern_property_name(method_name);
            let method =
                self.ordinary_get(handle, property, value)
                    .map_err(|error| match error {
                        VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    })?;
            let Some(callable) = method.as_object_handle().map(ObjectHandle) else {
                continue;
            };
            if !self.objects.is_callable(callable) {
                continue;
            }

            let result = self
                .call_callable(callable, value, &[])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;
            if self.non_string_object_handle(result)?.is_none() {
                return Ok(result);
            }
        }

        Err(InterpreterError::TypeError(
            "Cannot convert object to primitive value".into(),
        ))
    }

    pub(crate) fn js_to_primitive_with_hint(
        &mut self,
        value: RegisterValue,
        hint: ToPrimitiveHint,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        if self.objects.string_value(handle)?.is_some() {
            return Ok(value);
        }

        let to_primitive =
            self.intern_symbol_property_name(WellKnownSymbol::ToPrimitive.stable_id());
        let exotic =
            self.ordinary_get(handle, to_primitive, value)
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;

        if exotic != RegisterValue::undefined() && exotic != RegisterValue::null() {
            let Some(callable) = exotic.as_object_handle().map(ObjectHandle) else {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive is not callable".into(),
                ));
            };
            if !self.objects.is_callable(callable) {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive is not callable".into(),
                ));
            }

            let hint_value = match hint {
                ToPrimitiveHint::String => self.alloc_string("string"),
                ToPrimitiveHint::Number => self.alloc_string("number"),
            };
            let result = self
                .call_callable(
                    callable,
                    value,
                    &[RegisterValue::from_object_handle(hint_value.0)],
                )
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;
            if self.non_string_object_handle(result)?.is_some() {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive must return a primitive value".into(),
                ));
            }
            return Ok(result);
        }

        self.ordinary_to_primitive(value, hint)
    }

    fn coerce_loose_equality_primitive(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
    }

    pub(crate) fn js_to_string(
        &mut self,
        value: RegisterValue,
    ) -> Result<Box<str>, InterpreterError> {
        if value == RegisterValue::undefined() {
            return Ok("undefined".into());
        }
        if value == RegisterValue::null() {
            return Ok("null".into());
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { "true" } else { "false" }.into());
        }
        if value.is_symbol() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a Symbol value to a string".into(),
            ));
        }
        // §6.1.6.2.14 BigInt::toString(x)
        if let Some(handle) = value.as_bigint_handle() {
            let str_val = self
                .objects
                .bigint_value(ObjectHandle(handle))?
                .unwrap_or("0");
            return Ok(str_val.to_string().into_boxed_str());
        }
        if let Some(number) = value.as_number() {
            let text = if number.is_nan() {
                "NaN".to_string()
            } else if number.is_infinite() {
                if number.is_sign_positive() {
                    "Infinity".to_string()
                } else {
                    "-Infinity".to_string()
                }
            } else if number == 0.0 {
                "0".to_string()
            } else if number.fract() == 0.0 {
                format!("{number:.0}")
            } else {
                number.to_string()
            };
            return Ok(text.into_boxed_str());
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(string.to_string().into_boxed_str());
            }
            let primitive = self.js_to_primitive_with_hint(value, ToPrimitiveHint::String)?;
            if primitive != value {
                return self.js_to_string(primitive);
            }
            return Ok("[object Object]".into());
        }

        Ok(String::new().into_boxed_str())
    }

    /// Infallible ToString — returns "" on any error.
    pub fn js_to_string_infallible(&mut self, value: RegisterValue) -> Box<str> {
        self.js_to_string(value).unwrap_or_default()
    }

    /// ES spec 7.1.4 ToNumber — converts a value to its numeric representation.
    /// <https://tc39.es/ecma262/#sec-tonumber>
    pub fn js_to_number(&mut self, value: RegisterValue) -> Result<f64, InterpreterError> {
        if value == RegisterValue::undefined() {
            return Ok(f64::NAN);
        }
        if value == RegisterValue::null() {
            return Ok(0.0);
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { 1.0 } else { 0.0 });
        }
        if value.is_symbol() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a Symbol value to a number".into(),
            ));
        }
        // §7.1.4 step 1.e: BigInt → throw TypeError.
        if value.is_bigint() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a BigInt value to a number".into(),
            ));
        }
        if let Some(number) = value.as_number() {
            return Ok(number);
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(parse_string_to_number(&string.to_rust_string()));
            }
            let primitive = self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)?;
            if primitive != value {
                return self.js_to_number(primitive);
            }
            return Ok(f64::NAN);
        }
        Ok(f64::NAN)
    }

    /// ES spec 7.1.6 ToInt32 — converts a value to a signed 32-bit integer.
    pub fn js_to_int32(&mut self, value: RegisterValue) -> Result<i32, InterpreterError> {
        let n = self.js_to_number(value)?;
        Ok(f64_to_int32(n))
    }

    /// ES spec 7.1.7 ToUint32 — converts a value to an unsigned 32-bit integer.
    pub fn js_to_uint32(&mut self, value: RegisterValue) -> Result<u32, InterpreterError> {
        let n = self.js_to_number(value)?;
        Ok(f64_to_uint32(n))
    }

    /// ES spec 7.1.1 ToPrimitive with hint Number — converts an object to
    /// a primitive value.  Returns the value unchanged for non-objects.
    fn js_to_primitive_number(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
    }

    /// ES spec 7.2.13 Abstract Relational Comparison.
    /// <https://tc39.es/ecma262/#sec-abstract-relational-comparison>
    /// Returns `Some(true)` for less-than, `Some(false)` for not less-than,
    /// `None` for undefined (NaN involved).
    fn js_abstract_relational_comparison(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        left_first: bool,
    ) -> Result<Option<bool>, InterpreterError> {
        // 1-2. ToPrimitive with hint Number.
        let (px, py) = if left_first {
            let px = self.js_to_primitive_number(lhs)?;
            let py = self.js_to_primitive_number(rhs)?;
            (px, py)
        } else {
            let py = self.js_to_primitive_number(rhs)?;
            let px = self.js_to_primitive_number(lhs)?;
            (px, py)
        };

        // 3. If both are strings, compare lexicographically.
        let px_is_string = self.value_is_string(px)?;
        let py_is_string = self.value_is_string(py)?;
        if px_is_string && py_is_string {
            let sx = self.js_to_string(px)?;
            let sy = self.js_to_string(py)?;
            return Ok(Some(sx.as_ref() < sy.as_ref()));
        }

        // §7.2.13 step 3.a: If both are BigInt, use BigInt::lessThan.
        if px.is_bigint() && py.is_bigint() {
            return self.bigint_less_than(px, py);
        }

        // §7.2.13 step 3.b: Mixed BigInt/Number comparison.
        if px.is_bigint() && py.as_number().is_some() {
            return self.bigint_number_less_than(px, py);
        }
        if px.as_number().is_some() && py.is_bigint() {
            // number < bigint ≡ !(bigint < number) && !(bigint == number)
            // But spec says: reverse roles in step 3.c.
            return self.number_bigint_less_than(px, py);
        }

        // §7.2.13 step 3.d: Mixed BigInt + String comparison.
        if px.is_bigint() && py_is_string {
            let sy = self.js_to_string(py)?;
            if let Ok(ny) = sy.parse::<num_bigint::BigInt>() {
                let lhs_val = self.parse_bigint_value(px)?;
                return Ok(Some(lhs_val < ny));
            }
            return Ok(None);
        }
        if px_is_string && py.is_bigint() {
            let sx = self.js_to_string(px)?;
            if let Ok(nx) = sx.parse::<num_bigint::BigInt>() {
                let rhs_val = self.parse_bigint_value(py)?;
                return Ok(Some(nx < rhs_val));
            }
            return Ok(None);
        }

        // 4. Otherwise, coerce both to numbers.
        let nx = self.js_to_number(px)?;
        let ny = self.js_to_number(py)?;
        // NaN comparisons return undefined (None).
        if nx.is_nan() || ny.is_nan() {
            return Ok(None);
        }
        Ok(Some(nx < ny))
    }

    /// Parse the BigInt value from a register into a `num_bigint::BigInt`.
    fn parse_bigint_value(
        &self,
        value: RegisterValue,
    ) -> Result<num_bigint::BigInt, InterpreterError> {
        let handle = ObjectHandle(
            value
                .as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let str_val = self
            .objects
            .bigint_value(handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;
        str_val
            .parse()
            .map_err(|_| InterpreterError::InvalidConstant)
    }

    /// §6.1.6.2.12 BigInt::lessThan(x, y)
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-lessThan>
    fn bigint_less_than(
        &self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let lhs_val = self.parse_bigint_value(lhs)?;
        let rhs_val = self.parse_bigint_value(rhs)?;
        Ok(Some(lhs_val < rhs_val))
    }

    /// §7.2.13 step 3.b: BigInt < Number comparison.
    fn bigint_number_less_than(
        &self,
        bigint_val: RegisterValue,
        number_val: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let n = number_val.as_number().unwrap();
        if n.is_nan() || n.is_infinite() {
            return Ok(if n.is_nan() {
                None
            } else if n.is_sign_positive() {
                Some(true) // bigint < +Infinity
            } else {
                Some(false) // bigint < -Infinity
            });
        }
        let bv = self.parse_bigint_value(bigint_val)?;
        // Convert number to integer for comparison.
        let n_int = num_bigint::BigInt::from(n as i64);
        if bv < n_int {
            Ok(Some(true))
        } else if bv > n_int {
            Ok(Some(false))
        } else {
            // bv == n_int, but n may have fractional part
            Ok(Some((n_int.to_string().parse::<f64>().unwrap_or(0.0)) < n))
        }
    }

    /// §7.2.13 step 3.c: Number < BigInt comparison.
    fn number_bigint_less_than(
        &self,
        number_val: RegisterValue,
        bigint_val: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let n = number_val.as_number().unwrap();
        if n.is_nan() || n.is_infinite() {
            return Ok(if n.is_nan() {
                None
            } else if n.is_sign_positive() {
                Some(false) // +Infinity < bigint → false
            } else {
                Some(true) // -Infinity < bigint → true
            });
        }
        let bv = self.parse_bigint_value(bigint_val)?;
        let n_int = num_bigint::BigInt::from(n as i64);
        if n_int < bv {
            Ok(Some(true))
        } else if n_int > bv {
            Ok(Some(false))
        } else {
            // n_int == bv, but n may have fractional part
            Ok(Some(n < n_int.to_string().parse::<f64>().unwrap_or(0.0)))
        }
    }

    /// §7.2.15 BigInt == Number comparison.
    /// <https://tc39.es/ecma262/#sec-islooselyequal>
    fn bigint_equals_number(
        &self,
        bigint_val: RegisterValue,
        number_val: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let n = number_val.as_number().unwrap();
        if n.is_nan() || n.is_infinite() {
            return Ok(false);
        }
        // If n has a fractional part, it can never equal a BigInt.
        if n.fract() != 0.0 {
            return Ok(false);
        }
        let bv = self.parse_bigint_value(bigint_val)?;
        let n_int = num_bigint::BigInt::from(n as i64);
        Ok(bv == n_int)
    }

    /// ES spec 7.1.2 ToBoolean — runtime-aware truthiness check.
    /// <https://tc39.es/ecma262/#sec-toboolean>
    /// Unlike `RegisterValue::is_truthy()`, this correctly handles heap strings
    /// (empty string "" is falsy) and BigInt (0n is falsy).
    pub(crate) fn js_to_boolean(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        // §7.1.2 step 7: BigInt — 0n is falsy, all others truthy.
        if let Some(handle) = value.as_bigint_handle() {
            let str_val = self
                .objects
                .bigint_value(ObjectHandle(handle))?
                .unwrap_or("0");
            return Ok(str_val != "0");
        }
        // Fast path: non-object values use the NaN-box check.
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value.is_truthy());
        };
        // Heap strings: empty string is falsy, non-empty is truthy.
        if let Some(s) = self.objects.string_value(handle)? {
            return Ok(!s.is_empty());
        }
        // All other objects are truthy.
        Ok(true)
    }

    /// ES spec §7.3.21 OrdinaryHasInstance — `value instanceof constructor`.
    /// ES2024 §7.3.22 InstanceofOperator(V, target).
    fn js_instance_of(
        &mut self,
        value: RegisterValue,
        constructor: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        // 1. If target is not an Object, throw a TypeError.
        let Some(ctor_handle) = constructor.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not an object".into(),
            ));
        };

        // 2. Let instOfHandler be ? GetMethod(target, @@hasInstance).
        let has_instance_sym =
            self.intern_symbol_property_name(WellKnownSymbol::HasInstance.stable_id());
        let handler = self
            .ordinary_get(ctor_handle, has_instance_sym, constructor)
            .map_err(|error| match error {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;

        // 3. If instOfHandler is not undefined, then
        if handler != RegisterValue::undefined() && handler != RegisterValue::null() {
            let Some(handler_handle) = handler.as_object_handle().map(ObjectHandle) else {
                return Err(InterpreterError::TypeError(
                    "@@hasInstance is not callable".into(),
                ));
            };
            if !self.objects.is_callable(handler_handle) {
                return Err(InterpreterError::TypeError(
                    "@@hasInstance is not callable".into(),
                ));
            }
            // a. Return ! ToBoolean(? Call(instOfHandler, target, « V »)).
            let result = self
                .call_callable(handler_handle, constructor, &[value])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                    VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                })?;
            return self.js_to_boolean(result);
        }

        // 4. If IsCallable(target) is false, throw a TypeError.
        if !self.objects.is_callable(ctor_handle) {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not callable".into(),
            ));
        }

        // 5. Return ? OrdinaryHasInstance(target, V).
        self.ordinary_has_instance(value, ctor_handle)
    }

    /// ES2024 §7.3.21 OrdinaryHasInstance(C, O).
    fn ordinary_has_instance(
        &mut self,
        value: RegisterValue,
        constructor: ObjectHandle,
    ) -> Result<bool, InterpreterError> {
        // 1. If IsCallable(C) is false, return false.
        if !self.objects.is_callable(constructor) {
            return Ok(false);
        }

        // 2. If C has a [[BoundTargetFunction]] internal slot, unwrap.
        let mut effective_ctor = constructor;
        while matches!(
            self.objects.kind(effective_ctor),
            Ok(HeapValueKind::BoundFunction)
        ) {
            let (target, _, _) = self.objects.bound_function_parts(effective_ctor)?;
            effective_ctor = target;
        }

        // 3. If Type(O) is not Object, return false.
        let Some(obj_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };

        // 4. Let P be ? Get(C, "prototype").
        let proto_prop = self.intern_property_name("prototype");
        let proto_value = self
            .ordinary_get(
                effective_ctor,
                proto_prop,
                RegisterValue::from_object_handle(effective_ctor.0),
            )
            .map_err(|error| match error {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;

        // 5. If Type(P) is not Object, throw a TypeError.
        let Some(proto_handle) = proto_value.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Function has non-object prototype in instanceof check".into(),
            ));
        };

        // 6. Repeat: walk the prototype chain of O.
        let mut current = self.objects.get_prototype(obj_handle)?;
        let mut depth = 0;
        while let Some(p) = current {
            if p == proto_handle {
                return Ok(true);
            }
            depth += 1;
            if depth > 45 {
                break;
            }
            current = self.objects.get_prototype(p)?;
        }
        Ok(false)
    }

    /// ES2024 §13.10.1 The `in` Operator — `HasProperty(object, ToPropertyKey(key))`.
    fn js_has_property(
        &mut self,
        key: RegisterValue,
        object: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let Some(obj_handle) = self.non_string_object_handle(object)? else {
            return Err(InterpreterError::TypeError(
                "Cannot use 'in' operator to search for property in non-object".into(),
            ));
        };
        let property = self.computed_property_name(key)?;
        // §10.5.7 — Proxy [[HasProperty]] trap
        if self.is_proxy(obj_handle) {
            return self.proxy_has(obj_handle, property);
        }
        self.has_property(obj_handle, property)
            .map_err(InterpreterError::from)
    }

    /// Allocate an error object with the correct prototype chain.
    fn alloc_reference_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().reference_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Allocate a TypeError object with the correct prototype chain.
    pub fn alloc_type_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().type_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Allocates one RangeError instance with the given message.
    pub fn alloc_range_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().range_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Creates a { status: "...", [value_key]: value } object for Promise.allSettled.
    /// ES2024 §27.2.4.2.1–2
    pub fn alloc_settled_result_object(
        &mut self,
        status: &str,
        value_key: &str,
        value: RegisterValue,
    ) -> ObjectHandle {
        let obj = self.alloc_object();
        let status_prop = self.intern_property_name("status");
        let status_str = self.objects.alloc_string(status);
        let _ = self.objects.set_property(
            obj,
            status_prop,
            RegisterValue::from_object_handle(status_str.0),
        );
        let value_prop = self.intern_property_name(value_key);
        let _ = self.objects.set_property(obj, value_prop, value);
        obj
    }

    /// §19.2.1 Step 1: If x is not a String, return None.
    /// Extracts the string content if `value` is a string primitive.
    /// Does NOT coerce — returns None for non-string values.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-eval-x>
    pub fn value_as_string(&self, value: RegisterValue) -> Option<String> {
        let handle = value.as_object_handle().map(ObjectHandle)?;
        self.objects
            .string_value(handle)
            .ok()
            .flatten()
            .map(|s| s.to_string())
    }

    /// Checks whether a value is a string type (heap string or string wrapper).
    fn value_is_string(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        if self.objects.string_value(handle)?.is_some() {
            return Ok(true);
        }
        if let Some(inner) = self.string_wrapper_data(handle)?
            && self.objects.string_value(inner)?.is_some()
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// §6.1.6.2 BigInt arithmetic helper — performs a binary operation on two
    /// BigInt register values and returns the result as a new BigInt.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-add>
    fn bigint_binary_op(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        op: fn(&num_bigint::BigInt, &num_bigint::BigInt) -> num_bigint::BigInt,
    ) -> Result<RegisterValue, InterpreterError> {
        let lhs_handle = ObjectHandle(
            lhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );

        let lhs_str = self
            .objects
            .bigint_value(lhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;
        let rhs_str = self
            .objects
            .bigint_value(rhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;

        let lhs_val: num_bigint::BigInt = lhs_str
            .parse()
            .map_err(|_| InterpreterError::InvalidConstant)?;
        let rhs_val: num_bigint::BigInt = rhs_str
            .parse()
            .map_err(|_| InterpreterError::InvalidConstant)?;

        let result = op(&lhs_val, &rhs_val);
        let handle = self.alloc_bigint(&result.to_string());
        Ok(RegisterValue::from_bigint_handle(handle.0))
    }

    /// §6.1.6.2.10 BigInt::divide — truncating division, RangeError on zero divisor.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-divide>
    fn bigint_checked_div(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        self.bigint_binary_op(lhs, rhs, |a, b| {
            if b.is_zero() {
                // Caller would need to signal error; we use a sentinel approach below.
                num_bigint::BigInt::from(0)
            } else {
                a / b
            }
        })
        .and_then(|result| {
            // Re-check for division by zero via the original rhs.
            let rhs_handle = ObjectHandle(rhs.as_bigint_handle().unwrap());
            let rhs_str = self
                .objects
                .bigint_value(rhs_handle)
                .ok()
                .flatten()
                .unwrap_or("0");
            if rhs_str == "0" {
                return Err(InterpreterError::TypeError("Division by zero".into()));
            }
            Ok(result)
        })
    }

    /// §6.1.6.2.11 BigInt::remainder — RangeError on zero divisor.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-remainder>
    fn bigint_checked_rem(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        // Check for zero divisor first.
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_str = self
            .objects
            .bigint_value(rhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;
        if rhs_str == "0" {
            return Err(InterpreterError::TypeError("Division by zero".into()));
        }
        self.bigint_binary_op(lhs, rhs, |a, b| a % b)
    }

    /// §12.8.3 The Addition Operator ( + )
    /// <https://tc39.es/ecma262/#sec-addition-operator-plus>
    fn js_add(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        // §13.15.3 ApplyStringOrNumericBinaryOperator — step 1-4: ToPrimitive first.
        let lprim = self.js_to_primitive_with_hint(lhs, ToPrimitiveHint::Number)?;
        let rprim = self.js_to_primitive_with_hint(rhs, ToPrimitiveHint::Number)?;

        // §13.15.3 step 5: If either is a String, do string concatenation.
        let lhs_is_string = self.value_is_string(lprim)?;
        let rhs_is_string = self.value_is_string(rprim)?;
        if lhs_is_string || rhs_is_string {
            let mut text = self.js_to_string(lprim)?.into_string();
            text.push_str(&self.js_to_string(rprim)?);
            let value = self.alloc_string(text);
            return Ok(RegisterValue::from_object_handle(value.0));
        }

        // §6.1.6.2.7 BigInt::add — both operands BigInt.
        if lprim.is_bigint() && rprim.is_bigint() {
            return self.bigint_binary_op(lprim, rprim, |a, b| a + b);
        }
        // Mixed BigInt + non-BigInt → TypeError (§12.15.3 step 6).
        if lprim.is_bigint() || rprim.is_bigint() {
            return Err(InterpreterError::TypeError(
                "Cannot mix BigInt and other types, use explicit conversions".into(),
            ));
        }

        if let (Some(lhs_number), Some(rhs_number)) = (lprim.as_number(), rprim.as_number()) {
            return Ok(RegisterValue::from_number(lhs_number + rhs_number));
        }

        // i32 fast-path — only valid when both operands are integers.
        if lprim.as_i32().is_some() && rprim.as_i32().is_some() {
            return lprim.add_i32(rprim).map_err(InterpreterError::InvalidValue);
        }

        // General case: coerce to Number (ToNumber). Undefined → NaN,
        // null → 0, bool → 0/1.
        let lhs_num = self.js_to_number(lprim)?;
        let rhs_num = self.js_to_number(rprim)?;
        Ok(RegisterValue::from_number(lhs_num + rhs_num))
    }

    fn js_typeof(&mut self, value: RegisterValue) -> Result<RegisterValue, InterpreterError> {
        let kind = if value == RegisterValue::undefined() {
            "undefined"
        } else if value == RegisterValue::null() {
            "object"
        } else if value.as_bool().is_some() {
            "boolean"
        } else if value.is_symbol() {
            "symbol"
        } else if value.is_bigint() {
            "bigint"
        } else if value.as_number().is_some() {
            "number"
        } else if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            match self.objects.kind(handle)? {
                HeapValueKind::String => "string",
                HeapValueKind::HostFunction
                | HeapValueKind::Closure
                | HeapValueKind::BoundFunction
                | HeapValueKind::PromiseCapabilityFunction
                | HeapValueKind::PromiseCombinatorElement
                | HeapValueKind::PromiseFinallyFunction
                | HeapValueKind::PromiseValueThunk => "function",
                HeapValueKind::Object
                | HeapValueKind::Array
                | HeapValueKind::UpvalueCell
                | HeapValueKind::Iterator
                | HeapValueKind::Promise
                | HeapValueKind::Map
                | HeapValueKind::Set
                | HeapValueKind::MapIterator
                | HeapValueKind::SetIterator
                | HeapValueKind::WeakMap
                | HeapValueKind::WeakSet
                | HeapValueKind::WeakRef
                | HeapValueKind::FinalizationRegistry
                | HeapValueKind::Generator
                | HeapValueKind::AsyncGenerator
                | HeapValueKind::ArrayBuffer
                | HeapValueKind::SharedArrayBuffer
                | HeapValueKind::RegExp
                | HeapValueKind::Proxy
                | HeapValueKind::TypedArray
                | HeapValueKind::DataView
                | HeapValueKind::ErrorStackFrames => "object",
                HeapValueKind::BigInt => "bigint",
            }
        } else {
            "undefined"
        };

        let string = self.alloc_string(kind);
        Ok(RegisterValue::from_object_handle(string.0))
    }

    // ─── Generator Support (§27.5) ────────────────────────────────────

    /// Creates a `{ value, done }` iterator result object.
    /// Convenience wrapper around `create_iter_result_object`.
    pub fn create_iter_result(
        &mut self,
        value: RegisterValue,
        done: bool,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let obj = self.alloc_object();
        let value_prop = self.intern_property_name("value");
        let done_prop = self.intern_property_name("done");
        self.objects
            .set_property(obj, value_prop, value)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        self.objects
            .set_property(obj, done_prop, RegisterValue::from_bool(done))
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        Ok(obj)
    }

    /// Allocates a generator object in SuspendedStart state.
    ///
    /// Called when a generator function is invoked — instead of executing the
    /// body, we create a generator object that will lazily execute on `.next()`.
    pub fn alloc_generator(
        &mut self,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let prototype = self.intrinsics().generator_prototype();
        self.objects.alloc_generator(
            Some(prototype),
            module,
            function_index,
            closure_handle,
            arguments,
        )
    }

    /// Resumes a suspended generator. Called by the native `.next()`, `.return()`,
    /// and `.throw()` methods on `%GeneratorPrototype%`.
    pub(crate) fn resume_generator(
        &mut self,
        generator: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: crate::intrinsics::GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Interpreter::resume_generator_impl(self, generator, sent_value, resume_kind)
    }

    // ─── Async Generator Support (§27.6) ────────────────────────────────

    /// Allocates an async generator object in SuspendedStart state.
    ///
    /// Called when an `async function*` is invoked — instead of executing the
    /// body, we create an async generator object that lazily executes on `.next()`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorstart>
    pub fn alloc_async_generator(
        &mut self,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let prototype = self.intrinsics().async_generator_prototype();
        self.objects.alloc_async_generator(
            Some(prototype),
            module,
            function_index,
            closure_handle,
            arguments,
        )
    }

    /// Resumes a suspended async generator. Dequeues the front request
    /// and runs the body until next yield/await/return/throw.
    ///
    /// §27.6.3.3 AsyncGeneratorResume
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorresume>
    pub(crate) fn resume_async_generator(
        &mut self,
        generator: ObjectHandle,
    ) -> Result<(), VmNativeCallError> {
        Interpreter::resume_async_generator_impl(self, generator)
    }

    // ─── yield* delegation helpers (§14.4.4) ────────────────────────────

    /// Calls `iterator.next(value)` — tries the internal fast path first
    /// (ArrayIterator/StringIterator), then falls back to protocol-based `.next()`.
    /// Returns (done, value).
    /// Spec: <https://tc39.es/ecma262/#sec-iteratornext>
    pub(crate) fn call_iterator_next_with_value(
        &mut self,
        iterator: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(bool, RegisterValue), InterpreterError> {
        // Fast path: internal array/string iterators (ignores sent value,
        // which is correct per spec — arrays/strings don't use it).
        match self.iterator_next(iterator) {
            Ok(step) => {
                return Ok((step.is_done(), step.value()));
            }
            Err(InterpreterError::InvalidHeapValueKind) => {
                // Not an internal fast-path iterator — fall through to protocol.
            }
            Err(e) => return Err(e),
        }

        // Slow path: protocol-based iterator — look up .next() and call it.
        let next_prop = self.intern_property_name("next");
        let iter_val = RegisterValue::from_object_handle(iterator.0);
        let next_fn = self
            .ordinary_get(iterator, next_prop, iter_val)
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        let callable = next_fn
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| self.objects.is_callable(*h))
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator .next is not a function".into())
            })?;
        let result_obj = self
            .call_callable(callable, iter_val, &[value])
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        self.read_iter_result(result_obj)
    }

    /// Calls `iterator.throw(value)` if the method exists.
    /// Returns `Some((done, value))` if `.throw` exists, `None` if it doesn't.
    /// Internal array/string iterators don't have `.throw()` — returns `None`.
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation> step 7.b
    pub(crate) fn call_iterator_throw(
        &mut self,
        iterator: ObjectHandle,
        value: RegisterValue,
    ) -> Result<Option<(bool, RegisterValue)>, InterpreterError> {
        // Internal array/string iterators have no .throw() method.
        if self.is_internal_fast_path_iterator(iterator) {
            return Ok(None);
        }

        let throw_prop = self.intern_property_name("throw");
        let iter_val = RegisterValue::from_object_handle(iterator.0);
        let throw_fn = self
            .ordinary_get(iterator, throw_prop, iter_val)
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        if throw_fn == RegisterValue::undefined() || throw_fn == RegisterValue::null() {
            return Ok(None);
        }
        let callable = throw_fn
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| self.objects.is_callable(*h))
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator .throw is not a function".into())
            })?;
        let result_obj = self
            .call_callable(callable, iter_val, &[value])
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        self.read_iter_result(result_obj).map(Some)
    }

    /// Calls `iterator.return(value)` if the method exists.
    /// Returns `Some((done, value))` if `.return` exists, `None` if it doesn't.
    /// Internal array/string iterators have no `.return()` — returns `None`.
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation> step 7.c
    pub(crate) fn call_iterator_return(
        &mut self,
        iterator: ObjectHandle,
        value: RegisterValue,
    ) -> Result<Option<(bool, RegisterValue)>, InterpreterError> {
        // Internal array/string iterators have no .return() method.
        if self.is_internal_fast_path_iterator(iterator) {
            return Ok(None);
        }

        let return_prop = self.intern_property_name("return");
        let iter_val = RegisterValue::from_object_handle(iterator.0);
        let return_fn =
            self.ordinary_get(iterator, return_prop, iter_val)
                .map_err(|e| match e {
                    VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                    VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                })?;
        if return_fn == RegisterValue::undefined() || return_fn == RegisterValue::null() {
            return Ok(None);
        }
        let callable = return_fn
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| self.objects.is_callable(*h))
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator .return is not a function".into())
            })?;
        let result_obj = self
            .call_callable(callable, iter_val, &[value])
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        self.read_iter_result(result_obj).map(Some)
    }

    /// Returns `true` if the handle is an internal array (values-kind) or string iterator
    /// that uses the `iterator_next` fast path and has no protocol-level `.next()`/`.throw()`/`.return()`.
    fn is_internal_fast_path_iterator(&self, handle: ObjectHandle) -> bool {
        matches!(self.objects.kind(handle), Ok(HeapValueKind::Iterator))
    }

    /// Reads `done` and `value` from an iterator result object.
    fn read_iter_result(
        &mut self,
        result_obj: RegisterValue,
    ) -> Result<(bool, RegisterValue), InterpreterError> {
        let result_handle = result_obj
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator result must be an object".into())
            })?;
        let done_prop = self.intern_property_name("done");
        let done_val = self
            .ordinary_get(result_handle, done_prop, result_obj)
            .unwrap_or_else(|_| RegisterValue::from_bool(false));
        let done = self.js_to_boolean(done_val).unwrap_or(false);
        let value_prop = self.intern_property_name("value");
        let value = self
            .ordinary_get(result_handle, value_prop, result_obj)
            .unwrap_or_else(|_| RegisterValue::undefined());
        Ok((done, value))
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  §19.2.1 eval(x) — PerformEval
    //  Spec: <https://tc39.es/ecma262/#sec-eval-x>
    // ═══════════════════════════════════════════════════════════════════════

    /// §19.2.1.1 PerformEval ( x, strictCaller, direct )
    ///
    /// Compiles and executes `source` as a Script in the current runtime.
    /// Returns the completion value of the last expression statement.
    ///
    /// When `direct` is false (indirect eval), the code runs in the global
    /// scope and is never strict unless the eval code itself contains a
    /// "use strict" directive.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-performeval>
    pub fn eval_source(
        &mut self,
        source: &str,
        direct: bool,
        _strict_caller: bool,
    ) -> Result<RegisterValue, VmNativeCallError> {
        // §19.2.1.1 Step 2: If x is not a String, return x.
        // (Handled by the caller before reaching this method.)

        // §19.2.1.1 Step 4-10: Parse the source as a Script.
        let source_url = if direct {
            "<direct-eval>"
        } else {
            "<indirect-eval>"
        };

        // §B.3.5.2 — If inside a field initializer, apply additional early
        // error rules (ContainsArguments, Contains SuperCall).
        let in_field_init = direct && self.field_initializer_depth > 0;
        let module = if in_field_init {
            crate::source::compile_eval_field_init(source, source_url)
        } else {
            crate::source::compile_eval(source, source_url)
        }
        .map_err(|e| {
            // §19.2.1.1 Step 5: If parsing fails, throw a SyntaxError.
            self.alloc_syntax_error(&format!("eval: {e}"))
        })?;

        // §19.2.1.1 Step 16-25: Evaluate the parsed script.
        let interpreter = Interpreter::for_runtime(self);
        let result = interpreter
            .execute_module(&module, self)
            .map_err(|e| match e {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                other => VmNativeCallError::Internal(format!("eval: {other}").into()),
            })?;

        Ok(result.return_value())
    }

    /// Allocates a SyntaxError object with the given message.
    /// §20.5.5.4 NativeError
    /// Spec: <https://tc39.es/ecma262/#sec-nativeerror-message>
    pub fn alloc_syntax_error(&mut self, message: &str) -> VmNativeCallError {
        let prototype = self.intrinsics().syntax_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg = self.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects
            .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
            .ok();
        let name = self.alloc_string("SyntaxError");
        let name_prop = self.intern_property_name("name");
        self.objects
            .set_property(handle, name_prop, RegisterValue::from_object_handle(name.0))
            .ok();
        VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
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



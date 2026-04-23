//! `RuntimeState` — the shared VM heap owner. Bootstrap (new / with_gc_config),
//! intrinsic accessors, interrupt / OOM plumbing, realm management, property-name
//! registry, native function & payload registries, host integration (microtasks,
//! timers, console, host callbacks), and the accessor / ordinary-property /
//! string-exotic helpers that remain in this file.
//!
//! Thematic impl clusters are split into submodules:
//!
//! | Submodule    | Purpose                                                 |
//! |--------------|---------------------------------------------------------|
//! | `alloc`      | Heap allocation, gc_safepoint, install, closures.       |
//! | `call`       | call_callable / construct_callable / promises.           |
//! | `coercion`   | §7 abstract ops (ToPrimitive/ToString/ToNumber/==, +).   |
//! | `eval`       | eval_source re-entry into the source compiler.          |
//! | `iterators`  | Iterator protocol + generator resume kernels.           |
//! | `proxy`      | ECMA-262 §10.5 Proxy traps.                             |

mod alloc;
mod call;
mod coercion;
mod eval;
mod iterators;
mod proxy;

// Aliases so child submodules (coercion.rs etc.) can import via plain `super::*`
// instead of the unergonomic `super::super::` syntax. Children of `runtime_state`
// can see these private items via descendant visibility rules.
use super::number_conv::{
    canonical_string_exotic_index, f64_to_int32, f64_to_uint32, parse_string_to_number,
};
use super::step_outcome::Completion;
use super::{
    Activation, BOOLEAN_DATA_SLOT, ERROR_DATA_SLOT, EXECUTION_INTERRUPTED_MESSAGE, Interpreter,
    InterpreterError, NUMBER_DATA_SLOT, RuntimeState, STRING_DATA_SLOT, ToPrimitiveHint,
};

use core::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::descriptors::VmNativeCallError;
use crate::host::{HostFunctionId, NativeFunctionRegistry};
use crate::intrinsics::{VmIntrinsics, WellKnownSymbol};
use crate::module::Module;
use crate::object::{
    HeapValueKind, ObjectError, ObjectHandle, ObjectHeap, PropertyAttributes, PropertyInlineCache,
    PropertyLookup, PropertyValue,
};
use crate::payload::{NativePayloadError, NativePayloadRegistry, VmValueTracer};
use crate::property::{PropertyNameId, PropertyNameRegistry};
use crate::value::RegisterValue;

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
            feedback_vectors: std::collections::HashMap::new(),
            tier_up_budgets: std::collections::HashMap::new(),
            tier_up_blacklisted: std::collections::HashSet::new(),
            tier_up_hook: None,
        }
    }

    /// Get or create a persistent FeedbackVector for a function.
    /// The vector survives across activations and is shared with the JIT.
    pub fn get_or_create_feedback(
        &mut self,
        function_index: crate::FunctionIndex,
        function: &crate::module::Function,
    ) -> &mut crate::feedback::FeedbackVector {
        self.feedback_vectors
            .entry(function_index)
            .or_insert_with(|| crate::feedback::FeedbackVector::from_layout(function.feedback()))
    }

    /// Get the persistent FeedbackVector for a function (read-only, for JIT).
    pub fn feedback_vector(
        &self,
        function_index: crate::FunctionIndex,
    ) -> Option<&crate::feedback::FeedbackVector> {
        self.feedback_vectors.get(&function_index)
    }

    /// Mutable accessor for the persistent FeedbackVector. Used by the
    /// tier-up hook's deopt path to demote a slot that produced a
    /// bailout, so the next recompile picks a guarded variant instead
    /// of re-issuing the same trust-int32 stencil.
    pub fn feedback_vector_mut(
        &mut self,
        function_index: crate::FunctionIndex,
    ) -> Option<&mut crate::feedback::FeedbackVector> {
        self.feedback_vectors.get_mut(&function_index)
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

    /// Alias for `objects()` used by JIT.
    #[must_use]
    pub fn heap(&self) -> &ObjectHeap {
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

    /// §7.4.1 GetIterator(obj, sync) — spec-compliant iterable →
    /// iterator resolution. Looks up `@@iterator` on `iterable_val`,
    /// calls it with `iterable_val` as receiver, and returns the
    /// resulting iterator object. Throws TypeError when `@@iterator`
    /// is missing / non-callable / returns a non-object.
    ///
    /// Intended for the `GetIterator` opcode dispatch and any
    /// future `for await` plumbing. Built-in iterables (Array,
    /// String, Map, Set, TypedArray) have `@@iterator` installed by
    /// the intrinsic bootstrap, so this unified path handles both
    /// built-in and user-defined iterables uniformly.
    pub fn iterator_open(
        &mut self,
        iterable_val: RegisterValue,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let Some(iterable) = iterable_val.as_object_handle().map(ObjectHandle) else {
            return Err(self.throw_as_type_error("Value is not iterable"));
        };
        let iter_sym = self
            .intern_symbol_property_name(crate::intrinsics::WellKnownSymbol::Iterator.stable_id());
        let method_val = self.ordinary_get(iterable, iter_sym, iterable_val)?;
        let Some(method) = method_val.as_object_handle().map(ObjectHandle) else {
            return Err(self.throw_as_type_error("Value is not iterable"));
        };
        if !self.objects.is_callable(method) {
            return Err(self.throw_as_type_error("Value is not iterable"));
        }
        let iter_val = self.call_callable(method, iterable_val, &[])?;
        iter_val
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| self.throw_as_type_error("Iterator is not an object"))
    }

    /// §7.4.2 IteratorStep — drives one `iterator.next()` call and
    /// unpacks `{value, done}`. Tries the built-in fast path
    /// (`iterator_next`) first; falls back to the protocol path
    /// (call the iterator's own `next` method, coerce `done` via
    /// `ToBoolean`, read `value`) for user-defined iterators.
    ///
    /// Returns `IteratorStep::done()` when the iterator signals
    /// completion and `IteratorStep::yield_value(v)` otherwise.
    pub fn iterator_step_protocol(
        &mut self,
        iter: ObjectHandle,
    ) -> Result<crate::object::IteratorStep, VmNativeCallError> {
        match self.iterator_next(iter) {
            Ok(step) => return Ok(step),
            Err(InterpreterError::InvalidHeapValueKind) => {}
            Err(other) => return Err(interp_err_to_vm(self, other)),
        }
        let iter_val = RegisterValue::from_object_handle(iter.0);
        let next_prop = self.intern_property_name("next");
        let next_method = self.ordinary_get(iter, next_prop, iter_val)?;
        let Some(next_handle) = next_method.as_object_handle().map(ObjectHandle) else {
            return Err(self.throw_as_type_error("Iterator has no 'next' method"));
        };
        if !self.objects.is_callable(next_handle) {
            return Err(self.throw_as_type_error("Iterator's 'next' is not callable"));
        }
        let result = self.call_callable(next_handle, iter_val, &[])?;
        let Some(result_obj) = result.as_object_handle().map(ObjectHandle) else {
            return Err(self.throw_as_type_error("Iterator next() result is not an object"));
        };
        let done_prop = self.intern_property_name("done");
        let done_val = self.ordinary_get(result_obj, done_prop, result)?;
        let done = match self.js_to_boolean(done_val) {
            Ok(b) => b,
            Err(err) => return Err(interp_err_to_vm(self, err)),
        };
        if done {
            return Ok(crate::object::IteratorStep::done());
        }
        let value_prop = self.intern_property_name("value");
        let value_val = self.ordinary_get(result_obj, value_prop, result)?;
        Ok(crate::object::IteratorStep::yield_value(value_val))
    }

    /// §7.4.11 IteratorClose for both built-in and user-defined
    /// iterators. When `suppress_throw` is true, JS throws produced
    /// while fetching/calling `.return` are ignored so an existing
    /// abrupt completion remains the externally visible result.
    pub fn iterator_close_protocol(
        &mut self,
        iter: ObjectHandle,
        suppress_throw: bool,
    ) -> Result<(), VmNativeCallError> {
        self.check_interrupt()?;
        match self.objects.iterator_close(iter) {
            Ok(()) => return Ok(()),
            Err(crate::object::ObjectError::InvalidKind) => {}
            Err(error) => {
                return Err(VmNativeCallError::Internal(
                    format!("iterator close failed: {error:?}").into(),
                ));
            }
        }

        let iter_val = RegisterValue::from_object_handle(iter.0);
        let return_prop = self.intern_property_name("return");
        let return_method = match self.ordinary_get(iter, return_prop, iter_val) {
            Ok(value) => value,
            Err(VmNativeCallError::Thrown(_)) if suppress_throw => return Ok(()),
            Err(error) => return Err(error),
        };
        if return_method == RegisterValue::undefined() || return_method == RegisterValue::null() {
            return Ok(());
        }
        let Some(return_handle) = return_method.as_object_handle().map(ObjectHandle) else {
            if suppress_throw {
                return Ok(());
            }
            return Err(self.throw_as_type_error("Iterator return is not callable"));
        };
        if !self.objects.is_callable(return_handle) {
            if suppress_throw {
                return Ok(());
            }
            return Err(self.throw_as_type_error("Iterator return is not callable"));
        }

        let result = match self.call_callable(return_handle, iter_val, &[]) {
            Ok(value) => value,
            Err(VmNativeCallError::Thrown(_)) if suppress_throw => return Ok(()),
            Err(error) => return Err(error),
        };
        if result.as_object_handle().is_none() && !suppress_throw {
            return Err(self.throw_as_type_error("Iterator return result is not an object"));
        }
        Ok(())
    }

    /// Wraps a plain message string in a JS `TypeError` value and
    /// returns the `VmNativeCallError::Thrown` variant so callers
    /// can `?`-propagate without open-coding the allocator dance.
    /// Internal alloc failures collapse into
    /// `VmNativeCallError::Internal`.
    fn throw_as_type_error(&mut self, message: &str) -> VmNativeCallError {
        match self.alloc_type_error(message) {
            Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
            Err(err) => VmNativeCallError::Internal(format!("{err}").into()),
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

    pub(super) fn enter_module(&mut self, module: &Module) -> Option<Module> {
        let previous = self.current_module.clone();
        self.current_module = Some(module.clone());
        previous
    }

    pub(super) fn restore_module(&mut self, previous: Option<Module>) {
        self.current_module = previous;
    }

    pub(super) fn call_callable_for_accessor(
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
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts an `InterpreterError` raised from a nested path (e.g.
/// the built-in `iterator_next` fast path) into a
/// `VmNativeCallError`, collapsing thrown JS values into
/// `Thrown` and everything else into `Internal`. Mirrors the
/// inverse conversion performed all over `call_callable`.
fn interp_err_to_vm(runtime: &mut RuntimeState, err: InterpreterError) -> VmNativeCallError {
    match err {
        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
        InterpreterError::TypeError(message) => match runtime.alloc_type_error(&message) {
            Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
            Err(inner) => VmNativeCallError::Internal(format!("{inner}").into()),
        },
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}

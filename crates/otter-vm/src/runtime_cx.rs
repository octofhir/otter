//! Explicit runtime-turn context for VM dispatch and native bindings.
//!
//! [`RuntimeTurn<'rt>`] owns the two mutable reborrows that define one
//! synchronous VM turn: the isolate [`Interpreter`] and its current
//! [`ActivationStack`]. [`NativeCtx<'rt>`] is the safe binding view handed to
//! native functions. Keeping both borrows in one value makes live-frame
//! diagnostics and nested execution use the caller's real activation stack
//! instead of an interpreter-held raw-pointer bridge.
//!
//! Both context types are `!Send + !Sync` (enforced by static assertions in
//! [`crate::lib`]) and never cross `.await` — the lifetime parameter
//! `'rt` is what the borrow checker uses to keep the context tied
//! to a single mutator turn.
//!
//! # Contents
//!
//! - [`RuntimeTurn`] — explicit interpreter + activation-stack ownership.
//! - [`NativeCtx`] — high-level native binding API for the current turn.
//! - [`NativeScope`] — allocation-safe handle scope for native builders.
//! - `NativeCallRoots` — exact collector-rewritten native-call boundary slots.
//!
//! # Invariants
//!
//! - A runtime turn refers to exactly one interpreter and one activation stack.
//! - Native code never discovers live frames through TLS, globals, locks, or a
//!   diagnostic raw pointer; it receives the current stack through the turn.
//! - A native-call root record traces only its call-local slots; the enclosing
//!   runtime turn owns the single interpreter and activation-root traversal.
//! - Neither context crosses `.await` or escapes the mutator turn.
//! - Heap allocation and mutation stay rooted through `NativeCtx`/`NativeScope`.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-agents> (one mutator per agent).
//! - [Event loop](../../../docs/book/src/engine/event-loop.md).
//! - [GC API](../../../docs/book/src/engine/gc-api.md).

use std::marker::PhantomData;

use otter_gc::raw::RawGc;

use crate::{
    ActivationStack, ExecutionContext, Interpreter, IteratorHandle, IteratorState, Local,
    NativeError, Value, VmError, array,
    binary::array_buffer::JsArrayBuffer,
    collections,
    handles::HandleScope,
    native_function, object,
    promise::{JsPromise, JsPromiseHandle, PromiseState},
    weak_refs,
};

/// Explicit owner of one synchronous VM/runtime turn.
///
/// The interpreter and activation stack are disjoint allocations, but runtime
/// algorithms almost always need them together. Bundling their reborrows here
/// prevents native bindings and nested execution from inventing an ambient
/// route back to the currently executing frames.
///
/// # Lifetime contract
///
/// `'rt` is the lifetime of the enclosing mutator turn — the
/// dispatch loop's `&mut self` borrow. The borrow checker prevents
/// `RuntimeTurn` from crossing `.await`, escaping into a
/// `'static + Send` future, or being captured by `tokio::spawn`
/// (see compile-fail tests under
/// `crates/otter-vm/tests/compile_fail/`).
///
/// # Construction
///
/// `RuntimeTurn` is `pub(crate)`, but construction additionally requires that
/// the exact [`ActivationStack`] is inside
/// [`Interpreter::with_runtime_turn`]. Seeing an unrelated collector provider
/// is not sufficient. Native bindings receive [`NativeCtx<'rt>`] (a public
/// view) instead.
///
pub(crate) struct RuntimeTurn<'rt> {
    /// The interpreter owns the GC heap and every other isolate
    /// resource (string heap, microtask queue, intrinsic
    /// registries). One isolate has one mutator (ECMA-262 §16.6),
    /// so `&mut Interpreter` is the right shape for the context.
    pub(crate) interp: &'rt mut Interpreter,
    /// The single materialized activation stack for this turn. Nested
    /// execution opens an [`crate::ActivationFloor`] on this same stack.
    activations: &'rt mut ActivationStack,
    /// PhantomData carries the `'rt` lifetime so callers cannot
    /// store the context past the mutator turn even if `interp`
    /// is later split out.
    _marker: PhantomData<&'rt mut ()>,
}

impl<'rt> RuntimeTurn<'rt> {
    /// Reborrow a turn from the exact stack published at the lexical runtime
    /// boundary.
    ///
    /// The stack marker is private to `activation_stack.rs`; callers cannot
    /// manufacture it from `GcHeap::has_frame_root_providers()` or borrow
    /// rootedness from a different stack.
    #[must_use]
    pub(crate) fn from_rooted_parts(
        interp: &'rt mut Interpreter,
        activations: &'rt mut ActivationStack,
    ) -> Self {
        assert!(
            activations.is_runtime_rooted_by(interp),
            "RuntimeTurn requires this exact ActivationStack to be rooted by this Interpreter"
        );
        Self {
            interp,
            activations,
            _marker: PhantomData,
        }
    }

    /// Borrow the owning interpreter.
    #[must_use]
    pub(crate) fn interp(&self) -> &Interpreter {
        self.interp
    }

    /// Borrow the current materialized activation stack.
    #[must_use]
    pub(crate) fn activations(&self) -> &ActivationStack {
        self.activations
    }

    /// Consume the turn and return its original disjoint borrows.
    pub(crate) fn into_parts(self) -> (&'rt mut Interpreter, &'rt mut ActivationStack) {
        (self.interp, self.activations)
    }

    /// Reborrow the disjoint interpreter and activation fields without losing
    /// the lexical turn boundary.
    pub(crate) fn with_parts<R>(
        &mut self,
        body: impl FnOnce(&mut Interpreter, &mut ActivationStack) -> R,
    ) -> R {
        body(&mut *self.interp, &mut *self.activations)
    }

    /// Borrow the GC heap immutably.
    #[must_use]
    pub(crate) fn heap(&self) -> &otter_gc::GcHeap {
        self.interp.gc_heap_for_cx()
    }

    /// Borrow the GC heap mutably.
    #[must_use]
    pub(crate) fn heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        self.interp.gc_heap_for_cx_mut()
    }
}

/// Call-site metadata for native bindings.
///
/// The values are the collector-rewritten root slots for the active call.
/// Native code may inspect them synchronously, but must not store them or move
/// them into async work.
#[derive(Debug, Clone)]
pub struct NativeCallInfo {
    this_value: Value,
    new_target: Option<Value>,
}

impl NativeCallInfo {
    /// Ordinary function/method call metadata.
    #[must_use]
    pub fn call(this_value: Value) -> Self {
        Self {
            this_value,
            new_target: None,
        }
    }

    /// Constructor call metadata.
    #[must_use]
    pub fn construct(this_value: Value, new_target: Option<Value>) -> Self {
        Self {
            this_value,
            new_target,
        }
    }

    /// Foundation default for legacy callers.
    #[must_use]
    pub fn default_call() -> Self {
        Self::call(Value::undefined())
    }
}

impl otter_gc::ExtraRootSource for NativeCallInfo {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.this_value.trace_value_slots(visitor);
        if let Some(new_target) = &self.new_target {
            new_target.trace_value_slots(visitor);
        }
    }
}

/// Exact collector-visible state for one native invocation.
///
/// The call metadata, argument window, and additional boundary values stay in
/// their original storage for the whole invoke. Moving collections rewrite
/// those slots in place, so [`NativeCtx`] and the native `args` slice observe
/// forwarded handles without a second snapshot or adapter buffer.
pub(crate) struct NativeCallRoots<'a> {
    call_info: &'a NativeCallInfo,
    value_roots: &'a [&'a Value],
    slice_roots: &'a [&'a [Value]],
}

impl<'a> NativeCallRoots<'a> {
    #[must_use]
    pub(crate) fn new(
        call_info: &'a NativeCallInfo,
        value_roots: &'a [&'a Value],
        slice_roots: &'a [&'a [Value]],
    ) -> Self {
        Self {
            call_info,
            value_roots,
            slice_roots,
        }
    }
}

impl otter_gc::ExtraRootSource for NativeCallRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        otter_gc::ExtraRootSource::visit_extra_roots(self.call_info, visitor);
        for value in self.value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in self.slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    }
}

/// Public-to-native binding context. Handed to `holt!` / `couch!` /
/// `#[dive]` entry points so native code allocates and mutates
/// against the right isolate without reaching for thread-local
/// state.
///
/// `NativeCtx<'rt>` is `!Send + !Sync` and never crosses `.await`.
/// The lifetime `'rt` is the mutator turn — the same constraint
/// that applies to [`RuntimeTurn<'rt>`].
pub struct NativeCtx<'rt> {
    pub(crate) cx: RuntimeTurn<'rt>,
    call_info: &'rt NativeCallInfo,
    // Borrowed, not owned: the caller's execution context outlives the native
    // call, so the per-call path takes a reference instead of cloning four
    // `Arc`s + a `FrozenVec` on every native invocation. Owned copies are made
    // only on the rare re-entrant paths that stash the context past the call
    // (microtask enqueue, `interp_mut_and_context`).
    context: Option<&'rt ExecutionContext>,
}

/// Allocation-safe contributor view for one native handle scope.
///
/// `NativeScope` owns the short mutable borrow of [`NativeCtx`] and keeps the
/// collector token private. Every JavaScript value retained across a possible
/// allocation is represented by [`Local`], so module code never threads raw
/// roots, heap references, or write barriers. The wrapper is two references
/// and is fully monomorphized; opening a scope performs no JS-visible registry
/// work and allocates only when an operation itself allocates.
///
/// A scope cannot cross `.await`, and a [`Local`] cannot escape the closure
/// passed to [`NativeCtx::scope`]. Use [`NativeScope::finish`] exactly once at
/// the return boundary to turn the final local back into a VM value.
pub struct NativeScope<'scope, 'rt> {
    ctx: &'scope mut NativeCtx<'rt>,
    token: &'scope crate::handles::HandleScope,
}

impl<'rt> NativeCtx<'rt> {
    /// Build a native binding view from the explicit current runtime turn.
    ///
    /// `pub(crate)` keeps the raw activation-stack borrow out of the host ABI;
    /// VM call boundaries construct the turn and native callbacks receive only
    /// the high-level context.
    #[must_use]
    pub(crate) fn from_runtime_turn(
        cx: RuntimeTurn<'rt>,
        call_info: &'rt NativeCallInfo,
        context: Option<&'rt ExecutionContext>,
    ) -> Self {
        Self {
            cx,
            call_info,
            context,
        }
    }

    /// Convert a native error while this context's exact activation stack is
    /// still published to the collector.
    pub(crate) fn native_error_to_vm(&mut self, error: crate::NativeError) -> crate::VmError {
        self.cx.with_parts(|interp, activations| {
            crate::native_to_vm_error_with_stack(interp, activations, error)
        })
    }

    /// Run host-side native work in a fresh empty activation turn.
    ///
    /// This is the public replacement for constructing a `NativeCtx` from a
    /// bare interpreter. The higher-ranked callback prevents the context (and
    /// its local activation stack) from escaping. JavaScript re-entry opened
    /// from the callback uses the context's normal rooted APIs.
    pub fn with_host_context<R>(
        interp: &mut Interpreter,
        call_info: NativeCallInfo,
        context: Option<&ExecutionContext>,
        body: impl for<'turn> FnOnce(&mut NativeCtx<'turn>) -> R,
    ) -> R {
        let mut activations = ActivationStack::new();
        interp.with_runtime_turn(&mut activations, |turn| {
            let roots = NativeCallRoots::new(&call_info, &[], &[]);
            let _call_roots = turn
                .interp
                .gc_heap
                .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
            let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, context);
            body(&mut ctx)
        })
    }

    /// Return the execution context active for this native call, when present.
    #[must_use]
    pub fn execution_context(&self) -> Option<&ExecutionContext> {
        self.context
    }

    /// The stored context reference, decoupled from the `&self` borrow
    /// (the field is a `Copy` reference with the mutator-turn lifetime).
    /// Lets the marshalling layer hold the context across a later
    /// `interp_mut` borrow.
    #[must_use]
    pub(crate) fn context_ref(&self) -> Option<&'rt ExecutionContext> {
        self.context
    }

    /// Snapshot the current JavaScript activation stack, top frame first.
    ///
    /// The activation borrow comes from this context's [`RuntimeTurn`], so the
    /// operation is safe during inline native execution and needs no ambient
    /// pointer back into the interpreter.
    pub(crate) fn capture_active_frames(
        &self,
        context: &ExecutionContext,
    ) -> Vec<crate::StackFrameSnapshot> {
        crate::error_ops::snapshot_frames(context, self.cx.activations())
    }

    /// Capture the current JavaScript stack as Node-compatible call-site JSON.
    ///
    /// `skip` drops frames from the top and `count` caps the result. Source
    /// resolution is isolate-local, while the frame walk is driven directly by
    /// the explicit activation stack owned by this turn.
    pub fn capture_call_sites_json(
        &self,
        context: &ExecutionContext,
        skip: usize,
        count: usize,
    ) -> String {
        let mut frames = self.capture_active_frames(context);
        let skip = skip.min(frames.len());
        frames.drain(0..skip);
        if frames.len() > count {
            frames.truncate(count);
        }
        let sites: Vec<crate::CallSiteInfo> = frames
            .into_iter()
            .map(|frame| {
                let (line, column) = self
                    .cx
                    .interp()
                    .source_line_col(&frame.module, frame.span.0)
                    .unwrap_or((0, 0));
                let source_line = self
                    .cx
                    .interp()
                    .source_line_text(&frame.module, line)
                    .map(ToOwned::to_owned);
                let source_line_before = line
                    .checked_sub(1)
                    .and_then(|line| self.cx.interp().source_line_text(&frame.module, line))
                    .map(ToOwned::to_owned);
                let source_line_after = self
                    .cx
                    .interp()
                    .source_line_text(&frame.module, line.saturating_add(1))
                    .map(ToOwned::to_owned);
                let source_lines_after = (1..=8)
                    .filter_map(|offset| {
                        self.cx
                            .interp()
                            .source_line_text(&frame.module, line.saturating_add(offset))
                            .map(ToOwned::to_owned)
                    })
                    .collect::<Vec<_>>();
                crate::CallSiteInfo {
                    function_name: frame.function_name,
                    script_name: frame.module,
                    line_number: line,
                    column_number: column,
                    column,
                    source_line,
                    source_line_before,
                    source_line_after,
                    source_lines_after,
                }
            })
            .collect();
        serde_json::to_string(&sites).unwrap_or_else(|_| "[]".to_string())
    }

    /// Borrow the owning interpreter together with the current
    /// execution context. Use this when a native needs to re-enter VM
    /// code that also needs the caller context for observable coercions.
    ///
    /// `pub` so out-of-crate host bindings (the test262 agent
    /// harness in `crates/otter-test262/src/agent.rs`, future
    /// runtime extensions) can re-enter the interpreter without
    /// reimplementing the borrow split.
    pub fn interp_mut_and_context(&mut self) -> (&mut Interpreter, Option<ExecutionContext>) {
        (self.cx.interp, self.context.cloned())
    }

    /// Return the JavaScript receiver for the active native call.
    #[must_use]
    pub fn this_value(&self) -> &Value {
        &self.call_info.this_value
    }

    /// Return `new.target` for constructor calls.
    #[must_use]
    pub fn new_target(&self) -> Option<&Value> {
        self.call_info.new_target.as_ref()
    }

    /// Whether this native call is executing as a constructor.
    #[must_use]
    pub fn is_construct_call(&self) -> bool {
        self.call_info.new_target.is_some()
    }

    /// Clone the isolate's cooperative-cancellation handle.
    #[must_use]
    pub fn interrupt_handle(&self) -> crate::InterruptFlag {
        self.cx.interp.interrupt_handle()
    }

    /// Borrow the GC heap immutably.
    #[must_use]
    pub fn heap(&self) -> &otter_gc::GcHeap {
        self.cx.heap()
    }

    /// Borrow the GC heap mutably.
    #[must_use]
    pub fn heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        self.cx.heap_mut()
    }

    /// Reserve native/off-object memory with RAII release.
    pub fn reserve_external(
        &mut self,
        bytes: u64,
    ) -> Result<otter_gc::ExternalMemory, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        self.heap_mut()
            .reserve_external_with_roots(bytes, &mut external_visit)
    }

    /// Adjust the cumulative amount of memory retained outside GC cells.
    ///
    /// This is the high-level host-ABI counterpart to [`Self::reserve_external`]
    /// for APIs such as Node-API whose contract reports signed deltas rather
    /// than transferring ownership of one RAII token. Positive adjustments are
    /// booked against the heap cap and can trigger collection; negative
    /// adjustments release the same reservation. A host is expected not to
    /// release more than it previously reserved.
    pub fn adjust_external_memory(
        &mut self,
        change_in_bytes: i64,
    ) -> Result<i64, otter_gc::OutOfMemory> {
        let current = self
            .cx
            .interp
            .external_memory_adjustment
            .as_ref()
            .map_or(0, otter_gc::ExternalMemory::bytes);
        let adjusted = (i128::from(current) + i128::from(change_in_bytes))
            .clamp(0, i128::from(i64::MAX)) as u64;

        if let Some(reservation) = self.cx.interp.external_memory_adjustment.as_mut() {
            reservation.resize(adjusted)?;
            if adjusted == 0 {
                self.cx.interp.external_memory_adjustment = None;
            }
        } else if adjusted != 0 {
            let reservation = self.reserve_external(adjusted)?;
            self.cx.interp.external_memory_adjustment = Some(reservation);
        }

        Ok(adjusted as i64)
    }

    /// Allocate an ordinary object through the native root contract.
    pub fn alloc_object(&mut self) -> Result<object::JsObject, otter_gc::OutOfMemory> {
        self.alloc_object_with_roots(&[], &[])
    }

    /// Insert a generic persistent root for a host-owned resource.
    pub fn persistent_root_insert(&mut self, value: Value) -> crate::PersistentRootId {
        self.cx.interp.persistent_root_insert(value)
    }

    /// Read a generic persistent root.
    #[must_use]
    pub fn persistent_root_get(&self, id: crate::PersistentRootId) -> Option<Value> {
        self.cx.interp.persistent_root_get(id)
    }

    /// Remove a generic persistent root.
    pub fn persistent_root_remove(&mut self, id: crate::PersistentRootId) -> Option<Value> {
        self.cx.interp.persistent_root_remove(id)
    }

    /// Allocate an ordinary object while keeping additional local values alive.
    pub(crate) fn alloc_object_with_roots(
        &mut self,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<object::JsObject, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let shape_root = self.cx.interp.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        let object = object::alloc_object_with_shape_roots(
            self.heap_mut(),
            shape_root,
            &mut external_visit,
        )?;
        // OrdinaryObjectCreate(%Object.prototype%) — natives building
        // JS-visible objects (resolvedOptions, formatToParts entries, …)
        // expect `hasOwnProperty` & friends to resolve. Install the
        // prototype only after the allocation (which can scavenge and
        // relocate a still-young realm prototype); the realm-intrinsic
        // table is always traced, so a post-alloc read is current.
        if let Some(proto) = self.cx.interp.object_prototype_object_opt() {
            object::set_prototype(object, self.heap_mut(), Some(proto));
        }
        Ok(object)
    }

    /// Set an ordinary string-keyed property while keeping the native-call
    /// root set alive during any ShapeBody allocation.
    pub(crate) fn set_property(
        &mut self,
        obj: object::JsObject,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        self.set_property_with_roots(obj, key, value, &[], &[])
    }

    /// Set an ordinary string-keyed property while keeping additional native
    /// local values alive during any ShapeBody allocation.
    pub(crate) fn set_property_with_roots(
        &mut self,
        obj: object::JsObject,
        key: &str,
        value: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(), VmError> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let value_root = value;
        let mut combined_roots = Vec::with_capacity(value_roots.len() + 1);
        combined_roots.push(&value_root);
        combined_roots.extend_from_slice(value_roots);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                combined_roots.as_slice(),
                slice_roots,
            );
        };
        self.cx
            .interp
            .set_property_with_extra_roots(obj, key, value, &mut external_visit)
    }

    /// Allocate a host-data object through the native root contract.
    pub fn alloc_host_object<T: object::HostObjectData>(
        &mut self,
        data: T,
    ) -> Result<object::JsObject, otter_gc::OutOfMemory> {
        self.alloc_host_object_with_roots(data, &[], &[])
    }

    /// Resolve the prototype that `new <constructor_name>()` installs on its
    /// instances — i.e. `globalThis[constructor_name].prototype` — or `None`
    /// when that is not an object.
    ///
    /// A native host constructor that builds and returns its own instance
    /// object bypasses the engine's automatic `new.target.prototype` linkage,
    /// leaving the instance with a null prototype. Such constructors call this
    /// and re-parent the instance (`instanceof` and inherited prototype methods
    /// then work). Subclass linkage that must honor `new.target` is left to the
    /// JS subclass via `Object.setPrototypeOf`.
    pub fn class_instance_prototype(&mut self, constructor_name: &str) -> Option<Value> {
        self.cx
            .interp
            .constructor_prototype_value(constructor_name)
            .ok()
            .filter(|value| value.is_object_type())
    }

    /// Allocate a host-data object while keeping additional local values alive.
    pub(crate) fn alloc_host_object_with_roots<T: object::HostObjectData>(
        &mut self,
        data: T,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<object::JsObject, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let shape_root = self.cx.interp.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        object::alloc_host_object_with_shape_roots(
            self.heap_mut(),
            shape_root,
            data,
            &mut external_visit,
        )
    }

    /// Allocate a captured native function through the native root contract.
    ///
    /// Captures are traced automatically. Code that must keep other temporary
    /// JS values alive across this allocation should use [`Self::scope`] and
    /// park them as [`Scoped`] handles instead of threading root slices.
    pub fn native_value<F>(
        &mut self,
        name: &'static str,
        captures: smallvec::SmallVec<[Value; 4]>,
        call: F,
    ) -> Result<Value, otter_gc::OutOfMemory>
    where
        F: for<'call> Fn(&mut NativeCtx<'call>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        native_function::native_value_with_captures_and_roots(
            self.heap_mut(),
            name,
            captures,
            &mut external_visit,
            call,
        )
    }

    /// VM-internal captured native allocation with additional transient roots.
    pub(crate) fn native_value_with_captures<F>(
        &mut self,
        name: &'static str,
        captures: smallvec::SmallVec<[Value; 4]>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
        call: F,
    ) -> Result<Value, otter_gc::OutOfMemory>
    where
        F: for<'call> Fn(&mut NativeCtx<'call>, &[Value], &[Value]) -> Result<Value, NativeError>
            + 'static,
    {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let capture_roots = captures.clone();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
            for value in &capture_roots {
                value.trace_value_slots(visitor);
            }
        };
        native_function::native_value_with_captures_unchecked_with_roots(
            self.heap_mut(),
            name,
            captures,
            &mut external_visit,
            call,
        )
    }

    /// Allocate a pre-fulfilled promise through the native root contract.
    pub(crate) fn fulfilled_promise_with_roots(
        &mut self,
        value: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let value_root = value;
        let mut combined_roots = Vec::with_capacity(value_roots.len() + 1);
        combined_roots.push(&value_root);
        combined_roots.extend_from_slice(value_roots);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                combined_roots.as_slice(),
                slice_roots,
            );
        };
        JsPromiseHandle::fulfilled_with_roots(self.heap_mut(), value, &mut external_visit)
    }

    /// Allocate a `Map` body through the native root contract.
    pub fn alloc_map(&mut self) -> Result<collections::JsMap, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_map_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `Set` body through the native root contract.
    pub fn alloc_set(&mut self) -> Result<collections::JsSet, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_set_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// `new target(...args)` — construct an instance via the VM construct path.
    ///
    /// Re-enters the interpreter synchronously above this context's current
    /// activation floor; used by native code that needs to build platform
    /// objects through their real constructors (e.g. the structured-clone
    /// materializer rebuilding `Date`/`RegExp`/typed arrays).
    pub fn construct(&mut self, target: Value, args: &[Value]) -> Result<Value, NativeError> {
        self.construct_owned(target, args.iter().copied().collect())
    }

    pub(crate) fn construct_owned(
        &mut self,
        target: Value,
        args: smallvec::SmallVec<[Value; 8]>,
    ) -> Result<Value, NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "construct",
                reason: "missing execution context".to_string(),
            })?;
        self.cx.with_parts(|interp, stack| {
            interp
                .run_construct_sync_rooted(stack, &context, &target, target, args)
                .map_err(|err| native_function::vm_to_native_error(interp, err, "construct"))
        })
    }

    /// Invoke a callable synchronously with an explicit receiver.
    ///
    /// This is the high-level re-entry path for host ABIs whose contract
    /// requires an immediate callback result (notably Node-API's
    /// `napi_call_function`). Callers must keep any values they retain across
    /// this call in scoped or persistent roots.
    pub fn call(
        &mut self,
        target: Value,
        this_value: Value,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        self.call_owned(target, this_value, args.iter().copied().collect())
    }

    /// Compile a CommonJS wrapper on the current runtime turn.
    ///
    /// The synthesized module executes above an activation floor on this
    /// context's shared stack, so nested `require` never publishes a detached
    /// frame stack.
    pub fn create_commonjs_wrapper(
        &mut self,
        module_url: &str,
        body: &str,
    ) -> Result<Value, NativeError> {
        self.cx.with_parts(|interp, stack| {
            interp
                .create_commonjs_wrapper(stack, module_url, body)
                .map_err(|error| {
                    native_function::vm_to_native_error(interp, error, "CommonJS wrapper")
                })
        })
    }

    /// Evaluate a linked module graph on the current runtime turn.
    ///
    /// Host loaders open one [`NativeCtx::with_host_context`] boundary; module
    /// init, top-level await, and nested dynamic import then share its exact
    /// activation stack.
    pub fn evaluate_module(
        &mut self,
        url: &str,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        let context = self.context.cloned().ok_or(VmError::InvalidOperand)?;
        self.cx
            .with_parts(|interp, stack| interp.evaluate_module(stack, &context, url))
    }

    pub(crate) fn call_owned(
        &mut self,
        target: Value,
        this_value: Value,
        args: smallvec::SmallVec<[Value; 8]>,
    ) -> Result<Value, NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "call",
                reason: "missing execution context".to_string(),
            })?;
        self.cx.with_parts(|interp, stack| {
            interp
                .run_callable_sync_rooted(stack, &context, &target, this_value, args)
                .map_err(|err| native_function::vm_to_native_error(interp, err, "call"))
        })
    }

    /// Create a pending Promise together with its resolving functions.
    ///
    /// The three returned values are current at the return boundary; callers
    /// retaining them across any later allocation must immediately place them
    /// in scoped or persistent roots.
    pub fn promise_capability(&mut self) -> Result<(Value, Value, Value), NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "Promise",
                reason: "missing execution context".to_string(),
            })?;
        let builder = crate::promise_dispatch::PromiseBuilder::with_context(context);
        builder
            .construct_native_rooted(self, &[], &[])
            .map(|(promise, resolve, reject)| (Value::promise(promise), resolve, reject))
            .map_err(|error| NativeError::OutOfMemory {
                name: "Promise",
                requested_bytes: error.requested_bytes(),
                heap_limit_bytes: error.heap_limit_bytes(),
            })
    }

    /// Return the Node-API typed-array metadata and a pointer to its first byte.
    ///
    /// The pointer remains valid while the backing ArrayBuffer is alive and is
    /// not resized or detached. This mirrors the lifetime contract of
    /// `napi_get_typedarray_info`; callers must keep `value` rooted.
    pub fn typed_array_info(
        &mut self,
        value: Value,
    ) -> Option<(u32, usize, *mut u8, Value, usize)> {
        let typed = value.as_typed_array(self.heap())?;
        let kind = typed.kind().as_u32();
        let length = typed.length(self.heap());
        let byte_offset = typed.byte_offset(self.heap());
        let buffer = typed.buffer(self.heap());
        let data = buffer.with_bytes_mut(self.heap_mut(), |bytes| {
            if byte_offset > bytes.len() {
                std::ptr::null_mut()
            } else {
                // SAFETY: `byte_offset <= len`; `add(len)` is a valid one-past
                // pointer for a zero-length view.
                unsafe { bytes.as_mut_ptr().add(byte_offset) }
            }
        });
        Some((kind, length, data, Value::array_buffer(buffer), byte_offset))
    }

    /// Apply ECMAScript `IsArray`, including recursive Proxy handling.
    pub fn is_array(&mut self, value: Value) -> Result<bool, NativeError> {
        crate::abstract_ops::is_array(self.heap(), &value)
            .map_err(|error| native_function::vm_to_native_error(self.cx.interp, error, "IsArray"))
    }

    /// Return the logical length of an Array exotic value.
    #[must_use]
    pub fn array_length(&self, value: Value) -> Option<usize> {
        value
            .as_array()
            .map(|array| crate::array::len(array, self.heap()))
    }

    /// Test whether `value` is an instance of `constructor` using the VM's
    /// ordinary `instanceof` semantics and the active execution context.
    ///
    /// Native platform adapters use this for branded arguments such as URL
    /// objects instead of accepting arbitrary duck-typed host objects.
    pub fn is_instance_of(
        &mut self,
        value: Value,
        constructor: Value,
    ) -> Result<bool, NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "instanceof",
                reason: "missing execution context".to_string(),
            })?;
        self.cx.with_parts(|interp, stack| {
            interp
                .ordinary_has_instance(stack, &context, &constructor, &value)
                .map_err(|err| native_function::vm_to_native_error(interp, err, "instanceof"))
        })
    }

    /// ECMAScript `===` comparison for two rooted/current values.
    #[must_use]
    pub fn strict_equals(&self, left: Value, right: Value) -> bool {
        crate::abstract_ops::is_strictly_equal(&left, &right, self.heap())
    }

    /// Resolve a `globalThis.<name>` value (e.g. a constructor) for native use.
    #[must_use]
    pub fn global_value(&self, name: &str) -> Option<Value> {
        let global = *self.cx.interp.global_this();
        object::get(global, self.heap(), name)
    }

    /// Perform ordinary/exotic JavaScript `Get(receiver, key)` through the
    /// active execution context.
    pub fn get_value_property(&mut self, receiver: Value, key: &str) -> Result<Value, NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "get property",
                reason: "missing execution context".to_string(),
            })?;
        self.cx.with_parts(|interp, stack| {
            interp
                .get_property(stack, &context, receiver, key)
                .map_err(|err| native_function::vm_to_native_error(interp, err, "get property"))
        })
    }

    /// Return enumerable own string keys through the target's JavaScript
    /// internal methods, including Proxy `ownKeys` and descriptor traps.
    pub fn enumerable_own_string_keys(
        &mut self,
        target: Value,
    ) -> Result<Vec<String>, NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "enumerate properties",
                reason: "missing execution context".to_string(),
            })?;
        self.cx.with_parts(|interp, stack| {
            interp
                .enumerable_own_string_keys_for_value(stack, &context, target, 0)
                .map_err(|err| {
                    native_function::vm_to_native_error(interp, err, "enumerate properties")
                })
        })
    }

    /// Perform JavaScript `Set(receiver, key, value, true)` through the active
    /// execution context, including callable and exotic receivers.
    pub fn set_value_property(
        &mut self,
        receiver: Value,
        key: &str,
        value: Value,
    ) -> Result<(), NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "set property",
                reason: "missing execution context".to_string(),
            })?;
        let ok = self.cx.with_parts(|interp, stack| {
            interp
                .ordinary_set_data_value(
                    stack,
                    &context,
                    receiver,
                    &crate::VmPropertyKey::String(key),
                    value,
                    receiver,
                    0,
                )
                .map_err(|err| native_function::vm_to_native_error(interp, err, "set property"))
        })?;
        if ok {
            Ok(())
        } else {
            Err(NativeError::TypeError {
                name: "set property",
                reason: format!("Cannot assign to property '{key}'"),
            })
        }
    }

    /// Allocate a `WeakMap` body through the native root contract.
    pub fn alloc_weak_map(&mut self) -> Result<collections::JsWeakMap, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_weak_map_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `WeakSet` body through the native root contract.
    pub fn alloc_weak_set(&mut self) -> Result<collections::JsWeakSet, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_weak_set_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `WeakRef` body through the native root contract.
    pub(crate) fn alloc_weak_ref(
        &mut self,
        target: &Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<weak_refs::JsWeakRef, crate::VmError> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        weak_refs::alloc_weak_ref_with_roots(self.heap_mut(), target, &mut external_visit)
    }

    /// Allocate a `FinalizationRegistry` body through the native root contract.
    pub(crate) fn alloc_finalization_registry(
        &mut self,
        cleanup_callback: Value,
        cleanup_context: Option<ExecutionContext>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<weak_refs::JsFinalizationRegistry, crate::VmError> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        weak_refs::alloc_finalization_registry_with_context_and_roots(
            self.heap_mut(),
            cleanup_callback,
            cleanup_context,
            &mut external_visit,
        )
    }

    /// Insert into a `Map` through the native root contract.
    pub fn map_set(
        &mut self,
        map: &mut collections::JsMap,
        key: Value,
        value: Value,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let map_root = Value::map(*map);
        let key_root = key;
        let value_root = value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                &[&map_root, &key_root, &value_root],
                &[],
            );
        };
        collections::map_set_with_roots(map, self.heap_mut(), key, value, &mut external_visit)
    }

    /// Insert into a `Set` through the native root contract.
    pub fn set_add(
        &mut self,
        set: &mut collections::JsSet,
        value: Value,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let set_root = Value::set(*set);
        let value_root = value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                &[&set_root, &value_root],
                &[],
            );
        };
        collections::set_add_with_roots(set, self.heap_mut(), value, &mut external_visit)
    }

    /// Seal a host-owned Set snapshot against all JavaScript Set mutators.
    pub fn make_set_readonly(&mut self, value: Value) -> Result<(), NativeError> {
        let set = value.as_set().ok_or_else(|| NativeError::TypeError {
            name: "make Set readonly",
            reason: "value is not a Set".to_string(),
        })?;
        collections::set_make_readonly(set, self.heap_mut());
        Ok(())
    }

    /// Insert into a `WeakMap` through the native root contract.
    pub fn weak_map_set(
        &mut self,
        map: &mut collections::JsWeakMap,
        key: Value,
        value: Value,
    ) -> Result<(), collections::CollectionError> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let map_root = Value::weak_map(*map);
        let key_root = key;
        let value_root = value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                &[&map_root, &key_root, &value_root],
                &[],
            );
        };
        collections::weak_map_set_with_roots(map, self.heap_mut(), key, value, &mut external_visit)
    }

    /// Insert into a `WeakSet` through the native root contract.
    pub fn weak_set_add(
        &mut self,
        set: &mut collections::JsWeakSet,
        value: Value,
    ) -> Result<(), collections::CollectionError> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let set_root = Value::weak_set(*set);
        let value_root = value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                &[&set_root, &value_root],
                &[],
            );
        };
        collections::weak_set_add_with_roots(set, self.heap_mut(), value, &mut external_visit)
    }

    /// Allocate an array through the native root contract.
    pub fn array_from_elements<I>(
        &mut self,
        elements: I,
    ) -> Result<array::JsArray, otter_gc::OutOfMemory>
    where
        I: IntoIterator<Item = Value>,
    {
        self.array_from_elements_with_roots(elements, &[], &[])
    }

    /// Allocate an array while keeping additional local values alive.
    pub(crate) fn array_from_elements_with_roots<I>(
        &mut self,
        elements: I,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<array::JsArray, otter_gc::OutOfMemory>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let prototype = self.cx.interp.current_array_prototype_override();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
            if let Some(prototype) = &prototype {
                prototype.trace_value_slots(visitor);
            }
        };
        let array =
            array::from_elements_with_roots(self.heap_mut(), elements, &mut external_visit)?;
        self.cx.interp.register_array_prototype_override(array);
        Ok(array)
    }

    /// Allocate a zero-filled fixed-length `ArrayBuffer` through the native
    /// root contract.
    pub(crate) fn alloc_array_buffer_zeroed(
        &mut self,
        len: usize,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<JsArrayBuffer>, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        JsArrayBuffer::try_new_with_roots(len, self.heap_mut(), &mut external_visit)
    }

    /// Install a per-instance `[[Prototype]]` override on an array unless the
    /// override is redundant: the default realm's `%Array.prototype%` is
    /// already what an unstamped array resolves to, and stamping it would
    /// materialize the exotic sidecar that disqualifies the array from every
    /// dense fast path. A subclass prototype (or any non-default-realm proto)
    /// is always installed.
    pub fn set_array_prototype_override_checked(&mut self, array: array::JsArray, proto: Value) {
        if !self.cx.interp.active_realm_is_extra
            && self
                .cx
                .interp
                .realm_intrinsics
                .array_prototype
                .is_some_and(|p| Value::object(p).to_bits() == proto.to_bits())
        {
            return;
        }
        array::set_prototype_override(array, self.heap_mut(), Some(proto));
    }

    /// Store an array element through the native root contract.
    pub fn array_set(
        &mut self,
        array: array::JsArray,
        index: usize,
        value: Value,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let value_root = value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                &[&value_root],
                &[],
            );
        };
        array::set_with_roots(array, self.heap_mut(), index, value, &mut external_visit)
    }

    /// Push an array element through the native root contract.
    pub fn array_push(
        &mut self,
        array: array::JsArray,
        value: Value,
    ) -> Result<usize, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let value_root = value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                &[&value_root],
                &[],
            );
        };
        array::push_with_roots(array, self.heap_mut(), value, &mut external_visit)
    }

    /// Allocate iterator state through the native root contract.
    pub(crate) fn alloc_iterator_state(
        &mut self,
        state: IteratorState,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<IteratorHandle, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let prototype = self.cx.interp.iterator_prototype_override_for_state(&state);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
            if let Some(prototype) = &prototype {
                prototype.trace_value_slots(visitor);
            }
        };
        // Old-space: native callers copy the returned handle into Rust
        // locals across GC-bearing calls, so the cell must never move
        // under a young-space scavenge.
        let handle = self
            .heap_mut()
            .alloc_old_with_roots(state, &mut external_visit)?;
        self.cx
            .interp
            .register_iterator_prototype_override(handle, prototype);
        Ok(handle)
    }

    /// Borrow the owning interpreter for native functions that need
    /// isolate services outside the heap (microtasks, string tables,
    /// intrinsic registries).
    #[must_use]
    pub fn interp_mut(&mut self) -> &mut Interpreter {
        self.cx.interp
    }

    /// Reborrow the current interpreter and exact rooted activation stack for
    /// an internal high-level operation that can synchronously re-enter JS.
    pub(crate) fn with_turn_parts<R>(
        &mut self,
        body: impl FnOnce(&mut Interpreter, &mut ActivationStack) -> R,
    ) -> R {
        self.cx.with_parts(body)
    }

    pub(crate) fn collect_native_roots(&self) -> Vec<*mut RawGc> {
        self.cx.interp.collect_runtime_roots()
    }

    /// Drain the current isolate microtask queue and unwrap a native promise
    /// that settled during the drain.
    ///
    /// This is a generic host-event helper for native integrations that accept
    /// `T | Promise<T>` results. It does not block on future host work; a
    /// promise that remains pending after the current microtask drain is
    /// reported as a type error so callers can wire a real async continuation
    /// instead of parking the VM.
    pub fn resolve_native_promise_after_microtasks(
        &mut self,
        value: Value,
        name: &'static str,
    ) -> Result<Value, NativeError> {
        let Some(promise) = value.as_promise() else {
            return Ok(value);
        };
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name,
                reason: "missing execution context".to_string(),
            })?;
        self.cx
            .interp
            .drain_microtasks(&context)
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })?;
        match promise.state(self.heap()) {
            PromiseState::Fulfilled(value) => Ok(value),
            PromiseState::Rejected(reason) => Err(NativeError::Thrown {
                name,
                message: reason.display_string(self.heap()),
            }),
            PromiseState::Pending => Err(NativeError::TypeError {
                name,
                reason: "promise is still pending after microtask drain".to_string(),
            }),
        }
    }

    /// Allocate a fixed-length `ArrayBuffer` backing store.
    ///
    /// Receiver, call arguments, runtime state, and the owned input buffer are
    /// rooted/accounted automatically. Multi-allocation callers keep any
    /// additional JS temporaries in [`Self::scope`].
    pub fn array_buffer_from_bytes(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<crate::binary::JsArrayBuffer, otter_gc::OutOfMemory> {
        self.array_buffer_from_bytes_rooted(bytes, &[], &[])
    }

    /// VM-internal variant for algorithms that have not yet adopted handles.
    pub(crate) fn array_buffer_from_bytes_rooted(
        &mut self,
        bytes: Vec<u8>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::binary::JsArrayBuffer, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = *self.this_value();
        let new_target = self.new_target().cloned();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        crate::binary::JsArrayBuffer::from_bytes_with_roots(
            bytes,
            self.cx.heap_mut(),
            &mut external_visit,
        )
    }

    /// Allocate a resizable `ArrayBuffer` backing store under the same
    /// root contract as [`Self::array_buffer_from_bytes_rooted`].
    pub(crate) fn array_buffer_resizable_rooted(
        &mut self,
        len: usize,
        max_byte_length: usize,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<crate::binary::JsArrayBuffer>, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = *self.this_value();
        let new_target = self.new_target().cloned();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        crate::binary::JsArrayBuffer::new_resizable_with_roots(
            len,
            max_byte_length,
            self.cx.heap_mut(),
            &mut external_visit,
        )
    }

    /// Queue an isolate-local microtask for the current execution
    /// context.
    ///
    /// Native bindings use this for JS-visible scheduling surfaces
    /// such as `process.nextTick`. The task stays on the owning
    /// interpreter and is drained by the runtime checkpoint; no VM
    /// values cross into host/Tokio work.
    pub fn queue_microtask(
        &mut self,
        callee: Value,
        args: impl IntoIterator<Item = Value>,
    ) -> Result<(), crate::NativeError> {
        if !self.cx.interp.is_callable_runtime(&callee) {
            return Err(crate::NativeError::TypeError {
                name: "NativeCtx::queue_microtask",
                reason: "callback is not a function".to_string(),
            });
        }
        let context = self
            .context
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name: "NativeCtx::queue_microtask",
                reason: "missing execution context".to_string(),
            })?;
        self.cx.interp.microtasks_mut().enqueue(crate::Microtask {
            callee,
            this_value: Value::undefined(),
            args: args.into_iter().collect(),
            context: Some(context),
            result_capability: None,
            kind: crate::microtask::MicrotaskKind::Call,
        });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Handle-scope surface (see `crate::handles`).
    //
    // `scope` opens a collector-traced arena range and hands bindings a
    // `NativeScope`. Its high-level builders immediately park every result as
    // a `Local`, whose arena slot is rewritten in place by moving collections.

    /// Open a handle scope, run `f`, then restore its arena and root-provider
    /// depths on ordinary return or panic.
    ///
    /// This is the sound path for native code that builds a JS value out of
    /// several allocations. Every [`Local`] minted by the [`NativeScope`]
    /// passed to `f` is parked in a collector-traced arena, so a moving scavenge
    /// driven by a later allocation rewrites the slot in place instead of
    /// leaving a Rust local pointing at a vacated cell. The handle's `'s`
    /// lifetime pins it to the closure so none can escape. Consume the scope
    /// with [`NativeScope::finish`] to return exactly one completed value.
    ///
    /// ```
    /// # fn main() -> Result<(), otter_vm::NativeError> {
    /// use otter_vm::{Interpreter, NativeCallInfo, NativeCtx, Value};
    ///
    /// let mut interp = Interpreter::new();
    /// let port: u16 = 8080;
    /// let object_value = NativeCtx::with_host_context(
    ///     &mut interp,
    ///     NativeCallInfo::default_call(),
    ///     None,
    ///     |ctx| ctx.scope(|mut scope| {
    ///         let obj = scope.object()?;
    ///         let href = scope.string("http://localhost:8080/")?;
    ///         scope.set(obj, "href", href)?;
    ///         let port_value = scope.number(f64::from(port));
    ///         scope.set(obj, "port", port_value)?;
    ///         Ok::<Value, otter_vm::NativeError>(scope.finish(obj))
    ///     }),
    /// )?;
    /// # let _ = object_value;
    /// # Ok(())
    /// # }
    /// ```
    pub fn scope<R>(&mut self, f: impl for<'s> FnOnce(NativeScope<'s, 'rt>) -> R) -> R {
        let frame = crate::handles::HandleScopeFrame::enter(self.cx.interp);
        let token = frame.token();
        f(NativeScope {
            ctx: self,
            token: &token,
        })
    }
}

impl<'scope, 'rt> NativeScope<'scope, 'rt> {
    #[inline]
    fn vm_error(&self, error: VmError, operation: &'static str) -> NativeError {
        native_function::vm_to_native_error(self.ctx.cx.interp, error, operation)
    }

    #[inline]
    pub(crate) fn raw(&self, value: Local<'_>) -> Value {
        self.ctx.cx.interp.escape_scoped(value)
    }

    pub(crate) fn with_turn_parts<R>(
        &mut self,
        body: impl FnOnce(&mut Interpreter, &mut ActivationStack) -> R,
    ) -> R {
        self.ctx.cx.with_parts(body)
    }

    /// Open a nested handle range. Values stored into an outer rooted object
    /// remain live, while transient child handles are discarded as soon as the
    /// closure returns. This keeps bulk builders bounded without exposing the
    /// underlying token or context.
    pub fn scope<R>(&mut self, f: impl for<'child> FnOnce(NativeScope<'child, 'rt>) -> R) -> R {
        let frame = crate::handles::HandleScopeFrame::enter(self.ctx.cx.interp);
        let token = frame.token();
        f(NativeScope {
            ctx: &mut *self.ctx,
            token: &token,
        })
    }

    /// Root an incoming VM value in this scope.
    #[must_use]
    #[inline]
    pub fn value(&mut self, value: Value) -> Local<'scope> {
        self.ctx.cx.interp.scoped_value(self.token, value)
    }

    /// Root argument `index`; a missing argument becomes `undefined`.
    #[must_use]
    #[inline]
    pub fn argument(&mut self, args: &[Value], index: usize) -> Local<'scope> {
        self.value(args.get(index).copied().unwrap_or_else(Value::undefined))
    }

    /// Root the receiver of the active native call.
    #[must_use]
    #[inline]
    pub fn this(&mut self) -> Local<'scope> {
        self.value(self.ctx.call_info.this_value)
    }

    /// Root `new.target`, or return `None` for an ordinary call.
    #[must_use]
    #[inline]
    pub fn new_target(&mut self) -> Option<Local<'scope>> {
        self.ctx.call_info.new_target.map(|value| self.value(value))
    }

    /// Allocate an ordinary object with `%Object.prototype%`.
    pub fn object(&mut self) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_object(self.token);
        result.map_err(|error| self.vm_error(error, "NativeScope::object"))
    }

    /// Allocate a null-prototype object.
    pub fn bare_object(&mut self) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_object_bare(self.token);
        result.map_err(|error| self.vm_error(error, "NativeScope::bare_object"))
    }

    /// Allocate an ordinary object with the prototype held by `prototype`.
    pub fn object_with_prototype(
        &mut self,
        prototype: Local<'_>,
    ) -> Result<Local<'scope>, NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_object_with_proto(self.token, prototype);
        result.map_err(|error| self.vm_error(error, "NativeScope::object_with_prototype"))
    }

    /// Allocate an array of `len` holes.
    pub fn array(&mut self, len: usize) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_array(self.token, len);
        result.map_err(|error| self.vm_error(error, "NativeScope::array"))
    }

    /// Allocate an `ArrayBuffer` owning `bytes` and root it in this scope.
    pub fn array_buffer_from_bytes(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<Local<'scope>, NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_array_buffer_from_bytes(self.token, bytes);
        result.map_err(|error| self.vm_error(error, "NativeScope::array_buffer_from_bytes"))
    }

    /// Rewrap shared backing storage as a rooted `SharedArrayBuffer`.
    pub fn shared_array_buffer(
        &mut self,
        body: std::sync::Arc<crate::binary::array_buffer::SharedBody>,
    ) -> Result<Local<'scope>, NativeError> {
        let buffer =
            crate::binary::JsArrayBuffer::from_shared_arc(self.ctx.cx.interp.gc_heap_mut(), body)
                .map_err(|error| NativeError::OutOfMemory {
                name: "NativeScope::shared_array_buffer",
                requested_bytes: error.requested_bytes(),
                heap_limit_bytes: error.heap_limit_bytes(),
            })?;
        Ok(self.value(Value::array_buffer(buffer)))
    }

    /// Allocate a JavaScript string from UTF-8.
    pub fn string(&mut self, text: &str) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_string(self.token, text);
        result.map_err(|error| self.vm_error(error, "NativeScope::string"))
    }

    /// Allocate a `Set` collection.
    pub fn set_collection(&mut self) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_collection_set(self.token);
        result.map_err(|error| self.vm_error(error, "NativeScope::set_collection"))
    }

    /// Insert a rooted value into a rooted `Set` without exposing its raw GC
    /// handle to the binding. The collection backend rewrites its exact local
    /// handle if reserving external storage triggers a moving collection.
    pub fn set_add(&mut self, set: Local<'_>, value: Local<'_>) -> Result<(), NativeError> {
        let mut set = self
            .raw(set)
            .as_set()
            .ok_or_else(|| NativeError::TypeError {
                name: "NativeScope::set_add",
                reason: "value is not a Set".to_string(),
            })?;
        let value = self.raw(value);
        self.ctx
            .set_add(&mut set, value)
            .map_err(|error| self.vm_error(VmError::from(error), "NativeScope::set_add"))
    }

    /// Seal a host-owned `Set` snapshot against every JavaScript mutator.
    pub fn make_set_readonly(&mut self, set: Local<'_>) -> Result<(), NativeError> {
        let set = self.raw(set);
        self.ctx.make_set_readonly(set)
    }

    /// Allocate a Proxy over rooted `target` and `handler` values.
    pub fn proxy(
        &mut self,
        target: Local<'_>,
        handler: Local<'_>,
    ) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_proxy(self.token, target, handler);
        result.map_err(|error| self.vm_error(error, "NativeScope::proxy"))
    }

    /// Allocate an object carrying Rust-owned host data.
    pub fn host_object<T: object::HostObjectData>(
        &mut self,
        data: T,
    ) -> Result<Local<'scope>, NativeError> {
        let object = self.ctx.alloc_host_object(data)?;
        Ok(self.value(Value::object(object)))
    }

    /// Borrow host data behind a rooted object, including an ancestor view of
    /// a declared host-class instance.
    ///
    /// The callback runs while the object's GC payload is borrowed. It must
    /// only inspect Rust data: allocating a JavaScript value or re-entering the
    /// VM during the callback is forbidden. The callback deliberately receives
    /// no VM context, and this method keeps the scope borrowed for its entire
    /// duration so safe Rust cannot allocate through the same scope.
    ///
    /// ```compile_fail
    /// # use otter_vm::{Local, NativeScope};
    /// # fn allocating_during_borrow(
    /// #     scope: &mut NativeScope<'_, '_>,
    /// #     host: Local<'_>,
    /// # ) {
    /// let _ = scope.with_host_data::<String, _>(host, |_| {
    ///     scope.string("forbidden")
    /// });
    /// # }
    /// ```
    pub fn with_host_data<T: std::any::Any, R>(
        &self,
        value: Local<'_>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, NativeError> {
        crate::marshal::host_data_view_raw(self.raw(value), self.ctx.heap(), f).map_err(|reason| {
            NativeError::TypeError {
                name: "NativeScope::with_host_data",
                reason,
            }
        })
    }

    /// Mutable counterpart of [`Self::with_host_data`]. The callback owns the
    /// sole mutable payload borrow and has the same no-allocation/no-re-entry
    /// contract.
    pub fn with_host_data_mut<T: std::any::Any, R>(
        &mut self,
        value: Local<'_>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, NativeError> {
        let raw = self.raw(value);
        crate::marshal::host_data_view_raw_mut(raw, self.ctx.heap_mut(), f).map_err(|reason| {
            NativeError::TypeError {
                name: "NativeScope::with_host_data_mut",
                reason,
            }
        })
    }

    /// Borrow the live byte range of an `ArrayBuffer` or typed-array view for
    /// one non-allocating callback. A detached or internally out-of-bounds
    /// buffer source presents an empty slice; non-buffer values yield `None`.
    ///
    /// The callback runs while the buffer payload (and, for shared buffers, its
    /// backing-store lock) is borrowed. It must not allocate JavaScript values
    /// or re-enter the VM. The callback's result cannot borrow the input slice,
    /// and this method keeps the scope borrowed, so safe Rust enforces both
    /// boundaries.
    ///
    /// ```compile_fail
    /// # use otter_vm::{Local, NativeScope};
    /// # fn allocating_during_buffer_borrow(
    /// #     scope: &mut NativeScope<'_, '_>,
    /// #     buffer: Local<'_>,
    /// # ) {
    /// let _ = scope.with_buffer_source_bytes(buffer, |_| {
    ///     scope.string("forbidden")
    /// });
    /// # }
    /// ```
    pub fn with_buffer_source_bytes<R>(
        &self,
        value: Local<'_>,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        with_buffer_source_bytes(self.raw(value), self.ctx.heap(), f)
    }

    /// Copy the live byte range out of an `ArrayBuffer` or typed-array view.
    /// The owned result remains valid across later VM allocations and
    /// collections. Prefer [`Self::with_buffer_source_bytes`] when the caller
    /// can consume the bytes synchronously without copying.
    #[must_use]
    pub fn buffer_source_bytes(&self, value: Local<'_>) -> Option<Vec<u8>> {
        self.with_buffer_source_bytes(value, <[u8]>::to_vec)
    }

    /// Root a number immediate.
    #[must_use]
    pub fn number(&mut self, value: f64) -> Local<'scope> {
        self.ctx.cx.interp.scoped_number(self.token, value)
    }

    /// Root a boolean immediate.
    #[must_use]
    pub fn boolean(&mut self, value: bool) -> Local<'scope> {
        self.ctx.cx.interp.scoped_boolean(self.token, value)
    }

    /// Allocate a `BigInt` preserving all signed 128-bit input bits.
    pub fn bigint_i128(&mut self, value: i128) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_bigint_i128(self.token, value);
        result.map_err(|error| self.vm_error(error, "NativeScope::bigint_i128"))
    }

    /// Root `undefined`.
    #[must_use]
    pub fn undefined(&mut self) -> Local<'scope> {
        self.ctx.cx.interp.scoped_undefined(self.token)
    }

    /// Root `null`.
    #[must_use]
    pub fn null(&mut self) -> Local<'scope> {
        self.ctx.cx.interp.scoped_null(self.token)
    }

    /// Read an ordinary string-keyed property. Missing properties become
    /// `undefined`; the result is rooted before returning.
    pub fn get(&mut self, object: Local<'_>, key: &str) -> Result<Local<'scope>, NativeError> {
        let result = self.ctx.cx.interp.scoped_get(self.token, object, key);
        result.map_err(|error| self.vm_error(error, "NativeScope::get"))
    }

    /// Store a string-keyed property through the rooted object path.
    pub fn set(
        &mut self,
        object: Local<'_>,
        key: &str,
        value: Local<'_>,
    ) -> Result<(), NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_set(self.token, object, key, value);
        result.map_err(|error| self.vm_error(error, "NativeScope::set"))
    }

    /// Store a symbol-keyed property on a rooted ordinary object or Array
    /// exotic. The symbol and value are resolved from their handle slots at
    /// the write boundary, so earlier allocations cannot leave stale handles.
    pub fn set_symbol(
        &mut self,
        object: Local<'_>,
        key: Local<'_>,
        value: Local<'_>,
    ) -> Result<(), NativeError> {
        let symbol =
            self.raw(key)
                .as_symbol(self.ctx.heap())
                .ok_or_else(|| NativeError::TypeError {
                    name: "NativeScope::set_symbol",
                    reason: "property key is not a symbol".to_string(),
                })?;
        let result = self
            .ctx
            .cx
            .interp
            .scoped_set_symbol(self.token, object, symbol, value);
        result.map_err(|error| self.vm_error(error, "NativeScope::set_symbol"))
    }

    /// Define a data property with explicit descriptor flags.
    pub fn define(
        &mut self,
        object: Local<'_>,
        key: &str,
        value: Local<'_>,
        flags: object::PropertyFlags,
    ) -> Result<(), NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_define_data(self.token, object, key, value, flags);
        result.map_err(|error| self.vm_error(error, "NativeScope::define"))
    }

    /// Define a symbol-keyed data property with explicit descriptor flags.
    pub fn define_symbol(
        &mut self,
        object: Local<'_>,
        key: Local<'_>,
        value: Local<'_>,
        flags: object::PropertyFlags,
    ) -> Result<(), NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_define_symbol(self.token, object, key, value, flags);
        result.map_err(|error| self.vm_error(error, "NativeScope::define_symbol"))
    }

    /// Install a native callable on an object.
    pub fn set_callable(
        &mut self,
        object: Local<'_>,
        callable: Local<'_>,
    ) -> Result<(), NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_set_call_native(self.token, object, callable);
        result.map_err(|error| self.vm_error(error, "NativeScope::set_callable"))
    }

    /// Set an object's prototype; `None` installs a null prototype.
    pub fn set_prototype(
        &mut self,
        object: Local<'_>,
        prototype: Option<Local<'_>>,
    ) -> Result<(), NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_set_prototype(self.token, object, prototype);
        result.map_err(|error| self.vm_error(error, "NativeScope::set_prototype"))
    }

    /// Read an array index, rooting `undefined` for a hole or out-of-range
    /// index.
    pub fn index(&mut self, array: Local<'_>, index: usize) -> Result<Local<'scope>, NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_get_index(self.token, array, index);
        result.map_err(|error| self.vm_error(error, "NativeScope::index"))
    }

    /// Store a rooted value at an array index.
    pub fn set_index(
        &mut self,
        array: Local<'_>,
        index: usize,
        value: Local<'_>,
    ) -> Result<(), NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_set_index(self.token, array, index, value);
        result.map_err(|error| self.vm_error(error, "NativeScope::set_index"))
    }

    /// Return the logical length of a rooted array.
    pub fn array_length(&self, array: Local<'_>) -> Result<usize, NativeError> {
        self.ctx
            .cx
            .interp
            .scoped_array_length(array)
            .map_err(|error| self.vm_error(error, "NativeScope::array_length"))
    }

    /// Allocate a static native method value through the no-capture fast path.
    pub fn native_method(
        &mut self,
        name: &'static str,
        length: u8,
        call: native_function::NativeFastFn,
    ) -> Result<Local<'scope>, NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .scoped_native_static(self.token, name, length, call);
        result.map_err(|error| self.vm_error(error, "NativeScope::native_method"))
    }

    /// Allocate a native callable from an already-classified static or dynamic
    /// target. Static module surfaces should prefer [`Self::native_method`].
    pub fn native_call(
        &mut self,
        name: &'static str,
        length: u8,
        call: crate::NativeCall,
    ) -> Result<Local<'scope>, NativeError> {
        let result = self
            .ctx
            .cx
            .interp
            .native_function_from_call_host_rooted(name, length, call, &[], &[])
            .map(|value| self.value(value));
        result.map_err(|error| self.vm_error(VmError::from(error), "NativeScope::native_call"))
    }

    /// Strictly read a JavaScript string into owned Rust text.
    pub fn string_value(&self, value: Local<'_>) -> Result<String, NativeError> {
        let raw = self.raw(value);
        raw.as_string(self.ctx.heap())
            .map(|string| string.to_lossy_string(self.ctx.heap()))
            .ok_or_else(|| NativeError::TypeError {
                name: "NativeScope::string_value",
                reason: "expected a string".to_string(),
            })
    }

    /// Render a rooted value with the VM's non-coercing diagnostic display.
    #[must_use]
    pub fn display_string(&self, value: Local<'_>) -> String {
        self.raw(value).display_string(self.ctx.heap())
    }

    /// Return enumerable own string keys through JavaScript internal methods.
    pub fn enumerable_own_string_keys(
        &mut self,
        value: Local<'_>,
    ) -> Result<Vec<String>, NativeError> {
        let value = self.raw(value);
        self.ctx.enumerable_own_string_keys(value)
    }

    /// Strictly read a JavaScript number.
    pub fn number_value(&self, value: Local<'_>) -> Result<f64, NativeError> {
        self.raw(value)
            .as_f64()
            .ok_or_else(|| NativeError::TypeError {
                name: "NativeScope::number_value",
                reason: "expected a number".to_string(),
            })
    }

    /// Strictly read a JavaScript boolean.
    pub fn boolean_value(&self, value: Local<'_>) -> Result<bool, NativeError> {
        self.raw(value)
            .as_boolean()
            .ok_or_else(|| NativeError::TypeError {
                name: "NativeScope::boolean_value",
                reason: "expected a boolean".to_string(),
            })
    }

    /// Whether a local currently holds `undefined`.
    #[must_use]
    pub fn is_undefined(&self, value: Local<'_>) -> bool {
        self.raw(value).is_undefined()
    }

    /// Whether a local currently holds `null`.
    #[must_use]
    pub fn is_null(&self, value: Local<'_>) -> bool {
        self.raw(value).is_null()
    }

    /// Whether a local currently holds a JavaScript string.
    #[must_use]
    pub fn is_string(&self, value: Local<'_>) -> bool {
        self.raw(value).as_string(self.ctx.heap()).is_some()
    }

    /// Whether a local currently holds any object-shaped JavaScript value.
    #[must_use]
    pub fn is_object(&self, value: Local<'_>) -> bool {
        self.raw(value).is_object_type()
    }

    /// Whether a local is callable under the active VM's `[[Call]]` rules.
    #[must_use]
    pub fn is_callable(&self, value: Local<'_>) -> bool {
        self.ctx.cx.interp.is_callable_runtime(&self.raw(value))
    }

    /// Apply ECMAScript `IsArray`, including Proxy forwarding.
    pub fn is_array(&mut self, value: Local<'_>) -> Result<bool, NativeError> {
        let value = self.raw(value);
        self.ctx.is_array(value)
    }

    /// Test ordinary JavaScript `instanceof` semantics on rooted values.
    pub fn is_instance_of(
        &mut self,
        value: Local<'_>,
        constructor: Local<'_>,
    ) -> Result<bool, NativeError> {
        let value = self.raw(value);
        let constructor = self.raw(constructor);
        self.ctx.is_instance_of(value, constructor)
    }

    /// ECMAScript strict equality for two rooted values.
    #[must_use]
    pub fn strict_equals(&self, left: Local<'_>, right: Local<'_>) -> bool {
        self.ctx.strict_equals(self.raw(left), self.raw(right))
    }

    /// Resolve and root a property from `globalThis`.
    #[must_use]
    pub fn global(&mut self, name: &str) -> Option<Local<'scope>> {
        let value = self.ctx.global_value(name)?;
        Some(self.value(value))
    }

    /// Root the active realm's `globalThis` object itself.
    #[must_use]
    pub fn global_this(&mut self) -> Local<'scope> {
        let global = *self.ctx.cx.interp.global_this();
        self.value(Value::object(global))
    }

    /// Queue an isolate-local microtask from rooted inputs.
    pub fn queue_microtask(
        &mut self,
        callee: Local<'_>,
        args: &[Local<'_>],
    ) -> Result<(), NativeError> {
        let callee = self.raw(callee);
        let args: smallvec::SmallVec<[Value; 8]> =
            args.iter().map(|value| self.raw(*value)).collect();
        self.ctx.queue_microtask(callee, args)
    }

    /// Invoke a rooted callable synchronously and root its result.
    pub fn call(
        &mut self,
        target: Local<'_>,
        this_value: Local<'_>,
        args: &[Local<'_>],
    ) -> Result<Local<'scope>, NativeError> {
        self.call_vm(target, this_value, args)
            .map_err(|error| self.vm_error(error, "NativeScope::call"))
    }

    /// VM-internal variant of [`Self::call`] that keeps the exact abrupt
    /// completion available to algorithms which must catch it as a JavaScript
    /// value (notably the Promise constructor).
    pub(crate) fn call_vm(
        &mut self,
        target: Local<'_>,
        this_value: Local<'_>,
        args: &[Local<'_>],
    ) -> Result<Local<'scope>, VmError> {
        let context = self.ctx.context.clone().ok_or(VmError::InvalidOperand)?;
        let target = self.raw(target);
        let this_value = self.raw(this_value);
        let args: smallvec::SmallVec<[Value; 8]> =
            args.iter().map(|value| self.raw(*value)).collect();
        let result = self.ctx.cx.with_parts(|interp, stack| {
            interp.run_callable_sync_rooted(stack, &context, &target, this_value, args)
        })?;
        Ok(self.value(result))
    }

    /// Invoke a rooted constructor synchronously and root its result.
    pub fn construct(
        &mut self,
        target: Local<'_>,
        args: &[Local<'_>],
    ) -> Result<Local<'scope>, NativeError> {
        let target = self.raw(target);
        let args: smallvec::SmallVec<[Value; 8]> =
            args.iter().map(|value| self.raw(*value)).collect();
        let result = self.ctx.construct_owned(target, args)?;
        Ok(self.value(result))
    }

    /// Finish this scope with one value. Consuming the scope prevents module
    /// code from allocating again after extracting the raw return value.
    #[must_use]
    pub fn finish(self, value: Local<'scope>) -> Value {
        self.raw(value)
    }

    pub(crate) fn context(&mut self) -> &mut NativeCtx<'rt> {
        self.ctx
    }

    pub(crate) fn into_parts(self) -> (&'scope mut NativeCtx<'rt>, &'scope HandleScope) {
        (self.ctx, self.token)
    }
}

/// Borrow a buffer source while holding only a non-allocating heap read. The
/// result type is independent of the slice lifetime, so the borrowed bytes
/// cannot escape the callback.
pub(crate) fn with_buffer_source_bytes<R>(
    value: Value,
    heap: &otter_gc::GcHeap,
    f: impl FnOnce(&[u8]) -> R,
) -> Option<R> {
    if let Some(view) = value.as_typed_array(heap) {
        let offset = view.byte_offset(heap);
        let length = view.byte_length(heap);
        return Some(view.buffer(heap).with_bytes(heap, |bytes| {
            let range = offset
                .checked_add(length)
                .and_then(|end| bytes.get(offset..end))
                .unwrap_or_default();
            f(range)
        }));
    }
    value
        .as_array_buffer()
        .map(|buffer| buffer.with_bytes(heap, f))
}

/// Owned-copy convenience over [`with_buffer_source_bytes`].
pub(crate) fn copy_buffer_source_bytes(value: Value, heap: &otter_gc::GcHeap) -> Option<Vec<u8>> {
    with_buffer_source_bytes(value, heap, <[u8]>::to_vec)
}

pub(crate) fn visit_native_roots(
    visitor: &mut dyn FnMut(*mut RawGc),
    runtime_roots: &[*mut RawGc],
    this_value: &Value,
    new_target: Option<&Value>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) {
    for &slot in runtime_roots {
        visitor(slot);
    }
    this_value.trace_value_slots(visitor);
    if let Some(new_target) = new_target {
        new_target.trace_value_slots(visitor);
    }
    for value in value_roots {
        value.trace_value_slots(visitor);
    }
    for slice in slice_roots {
        for value in *slice {
            value.trace_value_slots(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeCallInfo, NativeCtx};
    use crate::{Interpreter, NativeError, Value, error_classes::ErrorKind, native_value_static};

    fn with_ctx<R>(
        interp: &mut Interpreter,
        call_info: NativeCallInfo,
        body: impl for<'turn> FnOnce(&mut NativeCtx<'turn>) -> R,
    ) -> R {
        NativeCtx::with_host_context(interp, call_info, None, body)
    }

    fn with_default_ctx<R>(
        interp: &mut Interpreter,
        body: impl for<'turn> FnOnce(&mut NativeCtx<'turn>) -> R,
    ) -> R {
        with_ctx(interp, NativeCallInfo::call(Value::undefined()), body)
    }

    #[test]
    fn native_ctx_object_allocation_uses_young_space() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        with_default_ctx(&mut interp, |ctx| {
            let _object = ctx.alloc_object().expect("native object allocation");
        });
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NativeCtx::alloc_object should allocate through root-aware young allocation"
        );
    }

    #[test]
    fn native_ctx_array_allocation_uses_young_space() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        with_ctx(
            &mut interp,
            NativeCallInfo::call(Value::number_i32(7)),
            |ctx| {
                let array = ctx
                    .array_from_elements([Value::number_i32(1)])
                    .expect("native array allocation");
                ctx.array_push(array, Value::number_i32(2))
                    .expect("native array growth");
            },
        );
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NativeCtx::array_from_elements should allocate through root-aware young allocation"
        );
    }

    #[test]
    fn native_call_info_slots_follow_moving_gc() {
        let mut interp = Interpreter::new();
        let receiver = with_default_ctx(&mut interp, |ctx| {
            Value::object(ctx.alloc_object().expect("native receiver allocation"))
        });
        let before = receiver.as_raw_gc().expect("receiver is a heap cell").0;

        let (this_value, new_target) = with_ctx(
            &mut interp,
            NativeCallInfo::construct(receiver, Some(receiver)),
            |ctx| {
                let mut moved = false;
                for _ in 0..8 {
                    let _churn = ctx.alloc_object().expect("young-space churn");
                    ctx.cx.interp.collect_minor_tracing_runtime_roots();
                    let current = ctx
                        .this_value()
                        .as_raw_gc()
                        .expect("rooted receiver remains a heap cell")
                        .0;
                    if current != before {
                        moved = true;
                        break;
                    }
                }
                assert!(
                    moved,
                    "native call receiver did not relocate under minor GC"
                );
                (*ctx.this_value(), *ctx.new_target().expect("new.target"))
            },
        );

        assert_eq!(this_value, new_target);
        assert!(this_value.as_object().is_some());
        assert_ne!(
            this_value.as_raw_gc().expect("forwarded receiver").0,
            before
        );
    }

    #[test]
    fn native_ctx_collection_allocation_uses_young_space() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        with_ctx(
            &mut interp,
            NativeCallInfo::construct(Value::undefined(), Some(Value::number_i32(1))),
            |ctx| {
                let mut map = ctx.alloc_map().expect("native map allocation");
                ctx.map_set(&mut map, Value::number_i32(1), Value::number_i32(2))
                    .expect("native map insert");
                let mut set = ctx.alloc_set().expect("native set allocation");
                ctx.set_add(&mut set, Value::number_i32(3))
                    .expect("native set insert");
                let weak_key = Value::object(ctx.alloc_object().expect("native weak key"));
                let weak_value = Value::object(ctx.alloc_object().expect("native weak value"));
                let mut weak_map = ctx.alloc_weak_map().expect("native weak map allocation");
                ctx.weak_map_set(&mut weak_map, weak_key, weak_value)
                    .expect("native weak map insert");
                let mut weak_set = ctx.alloc_weak_set().expect("native weak set allocation");
                ctx.weak_set_add(&mut weak_set, weak_key)
                    .expect("native weak set insert");
            },
        );
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NativeCtx collection helpers should allocate through root-aware young allocation"
        );
    }

    #[test]
    fn native_ctx_weak_ref_allocation_uses_young_space() {
        fn cleanup(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::undefined())
        }

        let mut interp = Interpreter::new();
        let cleanup =
            native_value_static(interp.gc_heap_mut(), "cleanup", 0, cleanup).expect("cleanup");
        let before = interp.gc_heap().stats().new_allocated_bytes;
        with_ctx(
            &mut interp,
            NativeCallInfo::construct(Value::undefined(), Some(Value::undefined())),
            |ctx| {
                let target = Value::object(ctx.alloc_object().expect("target"));
                let _weak_ref = ctx
                    .alloc_weak_ref(&target, &[], &[])
                    .expect("native weak ref allocation");
                let _registry = ctx
                    .alloc_finalization_registry(cleanup, None, &[], &[])
                    .expect("native finalization registry allocation");
            },
        );
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NativeCtx weak-ref helpers should allocate through root-aware young allocation"
        );
    }

    #[test]
    fn native_ctx_error_allocation_uses_young_space() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        with_ctx(
            &mut interp,
            NativeCallInfo::construct(Value::undefined(), Some(Value::undefined())),
            |ctx| {
                let registry = ctx.interp_mut().error_classes_clone();
                let error = registry
                    .make_instance_native_rooted(ctx, ErrorKind::TypeError, Some("boom"), &[], &[])
                    .expect("native error allocation");
                assert!(
                    crate::object::get(error, ctx.heap(), "message").is_some_and(|v| v.is_string())
                );
            },
        );
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Native error constructors should allocate through root-aware young allocation"
        );
    }

    #[test]
    fn native_ctx_aggregate_error_allocation_roots_errors_array() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        with_ctx(
            &mut interp,
            NativeCallInfo::construct(Value::undefined(), Some(Value::undefined())),
            |ctx| {
                let registry = ctx.interp_mut().error_classes_clone();
                let errors = [Value::number_i32(1)];
                let error = registry
                    .make_aggregate_instance_native_rooted(
                        ctx,
                        errors.as_slice(),
                        Some("all rejected"),
                        &[],
                        &[],
                    )
                    .expect("native aggregate error allocation");
                assert!(
                    crate::object::get(error, ctx.heap(), "errors").is_some_and(|v| v.is_array())
                );
            },
        );
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Native AggregateError should allocate the error and errors array through root-aware young allocation"
        );
    }

    #[test]
    fn native_scope_host_data_borrow_returns_owned_state_before_allocation() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct HostState {
            label: String,
            count: usize,
        }

        let mut interp = Interpreter::new();
        with_default_ctx(&mut interp, |ctx| {
            ctx.scope(|mut scope| {
                let host = scope
                    .host_object(HostState {
                        label: "otter".to_string(),
                        count: 1,
                    })
                    .expect("host object");

                let before = scope
                    .with_host_data::<HostState, _>(host, Clone::clone)
                    .expect("host data read");
                scope
                    .with_host_data_mut::<HostState, _>(host, |state| state.count += 1)
                    .expect("host data mutation");
                let after = scope
                    .with_host_data::<HostState, _>(host, Clone::clone)
                    .expect("host data read after mutation");

                // The callbacks have ended before this allocation. Their owned
                // snapshots do not retain a GC-payload borrow.
                let _allocation_after_borrow = scope.string("allocation is now safe").unwrap();
                assert_eq!(before.count, 1);
                assert_eq!(after.count, 2);
                assert_eq!(after.label, "otter");
            });
        });
    }

    #[test]
    fn native_scope_buffer_source_returns_owned_copy() {
        use crate::binary::typed_array::{JsTypedArray, TypedArrayKind};

        let mut interp = Interpreter::new();
        with_default_ctx(&mut interp, |ctx| {
            let buffer = ctx
                .array_buffer_from_bytes(vec![1, 2, 3, 4])
                .expect("array buffer");
            let view = JsTypedArray::new(ctx.heap_mut(), buffer, TypedArrayKind::Uint8, 1, 2)
                .expect("typed array view");
            ctx.scope(|mut scope| {
                let view = scope.value(Value::typed_array(view));
                let observed = scope
                    .with_buffer_source_bytes(view, |bytes| {
                        (bytes.len(), bytes.first().copied(), bytes.last().copied())
                    })
                    .expect("buffer source byte borrow");
                assert_eq!(observed, (2, Some(2), Some(3)));

                let copied = scope
                    .buffer_source_bytes(view)
                    .expect("buffer source bytes");

                // A later VM allocation cannot invalidate the returned Rust-owned
                // copy because no slice into the GC payload escaped the read.
                let _allocation_after_copy = scope.string("later allocation").unwrap();
                assert_eq!(copied, [2, 3]);

                let number = scope.number(1.0);
                assert!(scope.buffer_source_bytes(number).is_none());
            });
        });
    }

    /// Build a nested value (`{ name, count, items: [1, "two"] }`) through
    /// `NativeCtx::scope`, forcing a minor collection between every allocation
    /// so each parked handle is relocated at least once, then read every field
    /// back through the (rewritten) handles. Proves the native scoped surface
    /// keeps sibling handles current across the moves that turn a raw held
    /// offset stale.
    #[test]
    fn native_ctx_scope_builds_nested_value_across_minor_gc() {
        let mut interp = Interpreter::new();
        let ok = with_default_ctx(&mut interp, |ctx| {
            ctx.scope(|mut scope| {
                let obj = scope.object().unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();

                let name = scope.string("otter").unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();
                scope.set(obj, "name", name).unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();

                let count = scope.number(42.0);
                scope.set(obj, "count", count).unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();

                let arr = scope.array(0).unwrap();
                let e0 = scope.number(1.0);
                scope.set_index(arr, 0, e0).unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();
                let e1 = scope.string("two").unwrap();
                scope.set_index(arr, 1, e1).unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();
                scope.set(obj, "items", arr).unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();

                // Read every field back through the relocated object handle.
                let name_read = scope.get(obj, "name").unwrap();
                assert_eq!(scope.string_value(name_read).unwrap(), "otter");

                let count_read = scope.get(obj, "count").unwrap();
                assert_eq!(scope.number_value(count_read).unwrap(), 42.0);

                let items_read = scope.get(obj, "items").unwrap();
                assert_eq!(scope.array_length(items_read).unwrap(), 2);
                let first = scope.index(items_read, 0).unwrap();
                let second = scope.index(items_read, 1).unwrap();
                assert_eq!(scope.number_value(first).unwrap(), 1.0);
                assert_eq!(scope.string_value(second).unwrap(), "two");
                true
            })
        });
        assert!(ok);
    }

    /// A `%Object.prototype%`-proto'd object built inside `NativeCtx::scope`
    /// must survive — and relocate under — a minor scavenge, with its stored
    /// property still readable through the rewritten handle. The raw offset is
    /// asserted to change so the test provably exercised a move (mirrors the
    /// interpreter-level `scoped_object_survives_and_moves_under_minor_gc`).
    #[test]
    fn native_ctx_scoped_object_relocates_under_minor_gc() {
        let mut interp = Interpreter::new();
        let (moved, content) = with_default_ctx(&mut interp, |ctx| {
            ctx.scope(|mut scope| {
                let obj = scope.object().unwrap();
            let value = scope.string("payload").unwrap();
            scope.set(obj, "k", value).unwrap();
            let before = scope.raw(obj).as_raw_gc().expect("object is a heap cell").0;

            // Churn young space and scavenge until the survivor is evacuated to
            // the other semispace (its offset changes), proving the arena slot
            // was rewritten in place rather than left dangling.
            let mut after = before;
            let mut moved = false;
            for _ in 0..8 {
                let _churn = scope.object().unwrap();
                scope
                    .context()
                    .cx
                    .interp
                    .collect_minor_tracing_runtime_roots();
                after = scope
                    .raw(obj)
                    .as_raw_gc()
                    .expect("object still a heap cell after gc")
                    .0;
                if after != before {
                    moved = true;
                    break;
                }
            }
            assert!(
                moved,
                "scoped object never relocated across a minor GC (before={before}, after={after}); \
                 the move test did not exercise a relocation",
            );

            let read_back = scope.get(obj, "k").unwrap();
            let content = scope
                .string_value(read_back)
                .expect("property still a string");
                (moved, content)
            })
        });
        assert!(moved);
        assert_eq!(content, "payload");
    }
}

// `RuntimeTurn` and `NativeCtx` are `!Send + !Sync` because they hold a
// `&mut Interpreter` (which is `!Send + !Sync` by virtue of holding a
// `GcHeap`) plus the turn-local activation borrow. `lib.rs` reinforces this
// contract with static assertions.

//! Explicit runtime context for VM dispatch and native bindings.
//!
//! [`RuntimeCx<'rt>`] is the internal context handed to VM dispatch
//! and built-in helpers; it bundles the borrow set every algorithm
//! needs (`&mut RuntimeState`, `&mut GcHeap`, intrinsics) so callers
//! never reach for thread-local heap lookup. [`NativeCtx<'rt>`] is
//! the public-to-native binding view used by `holt!` / `couch!` /
//! `#[dive]` style entry points.
//!
//! Both types are `!Send + !Sync` (enforced by static assertions in
//! [`crate::lib`]) and never cross `.await` — the lifetime parameter
//! `'rt` is what the borrow checker uses to keep the context tied
//! to a single mutator turn.
//!
//! # Why explicit context?
//!
//! The GC heap used to be reachable through a thread-default escape hatch on
//! `GcHeap`. That helper could not prove which isolate owns a handle once
//! Tokio worker migration enters the picture, and it hid borrow boundaries from
//! the type system. Every read / write / write-barrier path must know which
//! isolate owns the object. The explicit-context types are the type-level
//! expression of that rule.
//!
//! # Status
//!
//! The thread-default escape hatch on `GcHeap` was removed; every caller now
//! threads `&GcHeap` / `&mut GcHeap` (or
//! `&NativeCtx<'_>` / `&mut NativeCtx<'_>` for native bindings)
//! explicitly.
//!
//! # Spec
//!
//! - <https://tc39.es/ecma262/#sec-agents> (one mutator per agent).
//! - [Event loop](../../../docs/book/src/engine/event-loop.md).
//! - [GC API](../../../docs/book/src/engine/gc-api.md).

use std::marker::PhantomData;

use otter_gc::raw::RawGc;

use crate::{
    ExecutionContext, HandleScope, Interpreter, IteratorHandle, IteratorState, NativeError, Scoped,
    Value, VmError, array,
    binary::array_buffer::JsArrayBuffer,
    collections, native_function, object,
    promise::{JsPromise, JsPromiseHandle, PromiseState},
    weak_refs,
};

/// Internal VM context. Carried explicitly through the dispatch
/// loop and built-in helper signatures so every algorithm sees the
/// `&mut GcHeap` it allocates against and the `&mut Interpreter`
/// it reads / mutates.
///
/// # Lifetime contract
///
/// `'rt` is the lifetime of the enclosing mutator turn — the
/// dispatch loop's `&mut self` borrow. The borrow checker prevents
/// `RuntimeCx` from crossing `.await`, escaping into a
/// `'static + Send` future, or being captured by `tokio::spawn`
/// (see compile-fail tests under
/// `crates/otter-vm/tests/compile_fail/`).
///
/// # Construction
///
/// `RuntimeCx` is `pub(crate)` — only the dispatch loop and a
/// small set of internal helpers may build one. Native bindings
/// receive [`NativeCtx<'rt>`] (a public view) instead.
///
pub(crate) struct RuntimeCx<'rt> {
    /// The interpreter owns the GC heap and every other isolate
    /// resource (string heap, microtask queue, intrinsic
    /// registries). One isolate has one mutator (ECMA-262 §16.6),
    /// so `&mut Interpreter` is the right shape for the context.
    pub(crate) interp: &'rt mut Interpreter,
    /// PhantomData carries the `'rt` lifetime so callers cannot
    /// store the context past the mutator turn even if `interp`
    /// is later split out.
    _marker: PhantomData<&'rt mut ()>,
}

impl<'rt> RuntimeCx<'rt> {
    /// Build a fresh context from an interpreter borrow.
    ///
    /// `pub(crate)` — only the dispatch loop / internal helpers
    /// build a [`RuntimeCx`].
    #[must_use]
    pub(crate) fn new(interp: &'rt mut Interpreter) -> Self {
        Self {
            interp,
            _marker: PhantomData,
        }
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
/// The values are snapshots for the active call only. Native code may inspect
/// them synchronously, but must not store them or move them into async work.
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

/// Public-to-native binding context. Handed to `holt!` / `couch!` /
/// `#[dive]` entry points so native code allocates and mutates
/// against the right isolate without reaching for thread-local
/// state.
///
/// `NativeCtx<'rt>` is `!Send + !Sync` and never crosses `.await`.
/// The lifetime `'rt` is the mutator turn — the same constraint
/// that applies to [`RuntimeCx<'rt>`].
pub struct NativeCtx<'rt> {
    pub(crate) cx: RuntimeCx<'rt>,
    call_info: NativeCallInfo,
    // Borrowed, not owned: the caller's execution context outlives the native
    // call, so the per-call path takes a reference instead of cloning four
    // `Arc`s + a `FrozenVec` on every native invocation. Owned copies are made
    // only on the rare re-entrant paths that stash the context past the call
    // (microtask enqueue, `interp_mut_and_context`).
    context: Option<&'rt ExecutionContext>,
}

impl<'rt> NativeCtx<'rt> {
    /// Build a native context from an interpreter borrow.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn new(interp: &'rt mut Interpreter) -> Self {
        Self::new_with_call_info(interp, NativeCallInfo::default_call())
    }

    /// Build a native context with explicit call-site metadata.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn new_with_call_info(
        interp: &'rt mut Interpreter,
        call_info: NativeCallInfo,
    ) -> Self {
        Self::new_with_call_info_and_context(interp, call_info, None)
    }

    /// Build a native context with explicit call-site metadata and
    /// execution context. Builtins that need to re-enter JS
    /// observable algorithms (for example Proxy traps) use the
    /// context to invoke callbacks with the same function table as
    /// the caller.
    #[must_use]
    pub fn new_with_call_info_and_context(
        interp: &'rt mut Interpreter,
        call_info: NativeCallInfo,
        context: Option<&'rt ExecutionContext>,
    ) -> Self {
        Self {
            cx: RuntimeCx::new(interp),
            call_info,
            context,
        }
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

    /// Allocate a GC payload through the owning isolate.
    ///
    /// This is the safe allocation path for native/builtin authors:
    /// allocation stays tied to the active [`NativeCtx`] instead of a
    /// thread-local heap lookup or a raw `GcHeap` borrow.
    pub fn alloc<T: otter_gc::Traceable>(
        &mut self,
        value: T,
    ) -> Result<otter_gc::Gc<T>, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value;
        let new_target = self.call_info.new_target;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        self.heap_mut().alloc_with_roots(value, &mut external_visit)
    }

    /// Allocate a long-lived GC payload directly in old-space.
    ///
    /// This mirrors the VM's current migration constraints for
    /// handles stored in non-moving Rust containers.
    pub fn alloc_old<T: otter_gc::Traceable>(
        &mut self,
        value: T,
    ) -> Result<otter_gc::Gc<T>, otter_gc::OutOfMemory> {
        self.heap_mut().alloc_old(value)
    }

    /// Record a GC-bearing value store into `parent`.
    pub fn record_write<T: ?Sized, V: otter_gc::GcStore + ?Sized>(
        &mut self,
        parent: otter_gc::Gc<T>,
        value: &V,
    ) {
        self.heap_mut().record_write(parent, value);
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

    /// Allocate an ordinary object through the native root contract.
    pub fn alloc_object(&mut self) -> Result<object::JsObject, otter_gc::OutOfMemory> {
        self.alloc_object_with_roots(&[], &[])
    }

    /// Park `value` on the interpreter's GC-traced scratch root stack and return
    /// its index. Host helpers that hold a JS handle across a re-entrant call
    /// (`run_callable_sync`, a nested allocation) must park it here, then read
    /// the relocated handle back via [`Self::scratch_root`] before reusing it,
    /// because a bare local is not a GC root and a moving scavenge during the
    /// call would leave it pointing at the value's vacated slot.
    pub fn push_scratch_root(&mut self, value: Value) -> usize {
        self.cx.interp.json_root_push(value)
    }

    /// Read the (possibly relocated) value parked at `idx`.
    #[must_use]
    pub fn scratch_root(&self, idx: usize) -> Value {
        self.cx.interp.json_root_get(idx)
    }

    /// Pop the scratch root stack back down to `idx` (the value
    /// [`Self::push_scratch_root`] returned).
    pub fn pop_scratch_root_to(&mut self, idx: usize) {
        self.cx.interp.json_root_pop_to(idx);
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
    pub fn alloc_object_with_roots(
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
        object::alloc_object_with_shape_roots(self.heap_mut(), shape_root, &mut external_visit)
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
    pub fn alloc_host_object_with_roots<T: object::HostObjectData>(
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
    pub fn native_value_with_captures<F>(
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
    pub fn fulfilled_promise_with_roots(
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
    /// Re-enters the interpreter synchronously (same rooting contract as
    /// [`Interpreter::run_callable_sync`]); used by native code that needs to
    /// build platform objects through their real constructors (e.g. the
    /// structured-clone materializer rebuilding `Date`/`RegExp`/typed arrays).
    pub fn construct(&mut self, target: Value, args: &[Value]) -> Result<Value, NativeError> {
        let context = self
            .context
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "construct",
                reason: "missing execution context".to_string(),
            })?;
        let argv: smallvec::SmallVec<[Value; 8]> = args.iter().copied().collect();
        self.cx
            .interp
            .run_construct_sync(&context, &target, target, argv)
            .map_err(|err| native_function::vm_to_native_error(self.cx.interp, err, "construct"))
    }

    /// Resolve a `globalThis.<name>` value (e.g. a constructor) for native use.
    #[must_use]
    pub fn global_value(&self, name: &str) -> Option<Value> {
        let global = *self.cx.interp.global_this();
        object::get(global, self.heap(), name)
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
    pub fn alloc_weak_ref(
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
    pub fn alloc_finalization_registry(
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
    pub fn array_from_elements_with_roots<I>(
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
    pub fn alloc_array_buffer_zeroed(
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
    pub fn alloc_iterator_state(
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

    /// Enter a branded GC session for root/weak operations.
    ///
    /// Persistent roots and weak handles created inside the closure
    /// carry the fresh isolate brand and can only be read/upgraded
    /// through a matching session.
    pub fn with_gc_session<R>(
        &mut self,
        f: impl for<'iso> FnOnce(otter_gc::GcSession<'iso, '_>) -> R,
    ) -> R {
        otter_gc::with_gc_session(self.heap_mut(), f)
    }

    /// Borrow the owning interpreter for native functions that need
    /// isolate services outside the heap (microtasks, string tables,
    /// intrinsic registries).
    #[must_use]
    pub fn interp_mut(&mut self) -> &mut Interpreter {
        self.cx.interp
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

    /// Allocate a fixed-length `ArrayBuffer` backing store, keeping the
    /// receiver, call arguments, and caller-supplied roots reachable
    /// across the reservation.
    pub fn array_buffer_from_bytes_rooted(
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
    pub fn array_buffer_resizable_rooted(
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
    // `scope` opens a collector-traced arena range; the `scoped_*` methods
    // allocate through the already-rooted VM paths and immediately park the
    // result, handing back a `Scoped` index handle that resolves through the
    // arena on every read. A moving scavenge rewrites the parked slot in place,
    // so a handle can never go stale — the ad-hoc `value_roots` threading these
    // methods replace is no longer the caller's problem.

    /// Open a handle scope, run `f`, then truncate the scope-handle arena back
    /// to the length it had on entry.
    ///
    /// This is the sound path for native code that builds a JS value out of
    /// several allocations. Every handle minted inside `f` (via the `scoped_*`
    /// methods) is parked in a collector-traced arena, so a moving scavenge
    /// driven by a later allocation rewrites the slot in place instead of
    /// leaving a Rust local pointing at a vacated cell. A [`Scoped`] handle
    /// borrows the `&HandleScope` token, not the context, so allocating calls
    /// (`&mut self`) interleave freely with live handles, and its `'s` lifetime
    /// pins it to the closure so none can escape.
    ///
    /// Read a handle back out with [`Self::escape`] for immediate hand-off to
    /// the VM (a function return, or a store into an already-rooted object);
    /// the raw `Value` it yields is valid only until the next allocation.
    ///
    /// ```
    /// # fn main() -> Result<(), otter_vm::NativeError> {
    /// use otter_vm::{Interpreter, NativeCallInfo, NativeCtx, Value};
    ///
    /// let mut interp = Interpreter::new();
    /// let mut ctx = NativeCtx::new_with_call_info_and_context(
    ///     &mut interp,
    ///     NativeCallInfo::default_call(),
    ///     None,
    /// );
    ///
    /// let port: u16 = 8080;
    /// let object_value = ctx.scope(|ctx, s| {
    ///     let obj = ctx.scoped_object(s)?;
    ///     let href = ctx.scoped_string(s, "http://localhost:8080/")?;
    ///     ctx.scoped_set(s, obj, "href", href)?;
    ///     let port_value = ctx.scoped_number(s, f64::from(port));
    ///     ctx.scoped_set(s, obj, "port", port_value)?;
    ///     Ok::<Value, otter_vm::NativeError>(ctx.escape(obj))
    /// })?;
    /// # let _ = object_value;
    /// # Ok(())
    /// # }
    /// ```
    pub fn scope<R>(&mut self, f: impl FnOnce(&mut NativeCtx<'_>, &HandleScope) -> R) -> R {
        let base = self.cx.interp.handle_arena_len();
        let scope = HandleScope::new(base);
        // Host-side native calls (module init, timer/worker dispatch) run without
        // the dispatch loop's extra-roots provider. Register the runtime root set
        // — which traces the handle arena — for the scope so a wide-number box
        // allocated inside a scoped write cannot strand a sibling handle. No-op
        // (and free) under dispatch, where a provider is already installed.
        let roots_depth = self.cx.interp.push_scope_runtime_roots();
        let r = f(self, &scope);
        self.cx.interp.pop_scope_runtime_roots(roots_depth);
        self.cx.interp.handle_arena_truncate(base);
        r
    }

    /// Map an interpreter-side [`VmError`] onto the native error model, the way
    /// the neighbouring native allocation helpers do.
    fn scoped_error(&self, err: VmError, name: &'static str) -> NativeError {
        native_function::vm_to_native_error(self.cx.interp, err, name)
    }

    /// Allocate a string and park it in scope `s`.
    pub fn scoped_string<'s>(
        &mut self,
        s: &'s HandleScope,
        text: &str,
    ) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_string(s, text);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_string"))
    }

    /// Allocate an ordinary object with `%Object.prototype%` (the prototype a
    /// `{}` literal resolves to) and park it in scope `s`.
    pub fn scoped_object<'s>(&mut self, s: &'s HandleScope) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_object(s);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_object"))
    }

    /// Allocate a bare (null-prototype) object and park it in scope `s`.
    pub fn scoped_object_bare<'s>(
        &mut self,
        s: &'s HandleScope,
    ) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_object_bare(s);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_object_bare"))
    }

    /// Allocate an array of `length` `len` (elements start as holes) and park
    /// it in scope `s`. Fill it with [`Self::scoped_set_index`].
    pub fn scoped_array<'s>(
        &mut self,
        s: &'s HandleScope,
        len: usize,
    ) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_array(s, len);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_array"))
    }

    /// Park an `f64` number in scope `s`. Numbers are NaN-boxed immediates, so
    /// this never allocates and never fails; parking keeps number construction
    /// reading like every other scoped creation.
    #[must_use]
    pub fn scoped_number<'s>(&mut self, s: &'s HandleScope, n: f64) -> Scoped<'s> {
        self.cx.interp.scoped_number(s, n)
    }

    /// Park a boolean immediate in scope `s`.
    #[must_use]
    pub fn scoped_boolean<'s>(&mut self, s: &'s HandleScope, b: bool) -> Scoped<'s> {
        self.cx.interp.scoped_boolean(s, b)
    }

    /// Park the `undefined` immediate in scope `s`.
    #[must_use]
    pub fn scoped_undefined<'s>(&mut self, s: &'s HandleScope) -> Scoped<'s> {
        self.cx.interp.scoped_undefined(s)
    }

    /// Park the `null` immediate in scope `s`.
    #[must_use]
    pub fn scoped_null<'s>(&mut self, s: &'s HandleScope) -> Scoped<'s> {
        self.cx.interp.scoped_null(s)
    }

    /// Root an incoming raw `Value` in scope `s` and hand back a handle to it.
    /// Use this at the top of a native body to bring receiver/argument values
    /// under scope management before the first allocation.
    #[must_use]
    pub fn scoped_value<'s>(&mut self, s: &'s HandleScope, value: Value) -> Scoped<'s> {
        self.cx.interp.scoped_value(s, value)
    }

    /// Read property `key` from the object handle `obj` and park the result in
    /// scope `s`. Both handles resolve through the arena at call time. Absent
    /// properties read back as `undefined`.
    pub fn scoped_get<'s>(
        &mut self,
        s: &'s HandleScope,
        obj: Scoped<'_>,
        key: &str,
    ) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_get(s, obj, key);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_get"))
    }

    /// Write `value` to property `key` on the object handle `obj`, resolving
    /// both handles through the arena at call time.
    pub fn scoped_set(
        &mut self,
        s: &HandleScope,
        obj: Scoped<'_>,
        key: &str,
        value: Scoped<'_>,
    ) -> Result<(), NativeError> {
        let result = self.cx.interp.scoped_set(s, obj, key, value);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_set"))
    }

    /// Define data property `key` on the object handle `obj` with explicit
    /// attribute `flags`, resolving both handles through the arena at call
    /// time.
    pub fn scoped_define_data(
        &mut self,
        s: &HandleScope,
        obj: Scoped<'_>,
        key: &str,
        value: Scoped<'_>,
        flags: object::PropertyFlags,
    ) -> Result<(), NativeError> {
        let result = self.cx.interp.scoped_define_data(s, obj, key, value, flags);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_define_data"))
    }

    /// Allocate an ordinary object whose prototype is the object held by the
    /// `proto` handle (e.g. a class's `.prototype`), and park it in scope `s`.
    /// Use this to build a native instance that must carry a specific prototype
    /// chain — the server request path builds `Request`/`Headers` instances
    /// this way. A `proto` handle not holding an object yields a null prototype.
    pub fn scoped_object_with_proto<'s>(
        &mut self,
        s: &'s HandleScope,
        proto: Scoped<'_>,
    ) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_object_with_proto(s, proto);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_object_with_proto"))
    }

    /// Define the symbol-keyed data property carried by the `key` handle on the
    /// object handle `obj`, with explicit attribute `flags`. The symbol lives in
    /// a scope handle so it survives the allocations that built `value`; all
    /// handles resolve through the arena at call time. Mirrors
    /// [`Self::scoped_define_data`] for the private-slot symbols that back the
    /// Fetch classes.
    pub fn scoped_define_symbol(
        &mut self,
        s: &HandleScope,
        obj: Scoped<'_>,
        key: Scoped<'_>,
        value: Scoped<'_>,
        flags: object::PropertyFlags,
    ) -> Result<(), NativeError> {
        let result = self
            .cx
            .interp
            .scoped_define_symbol(s, obj, key, value, flags);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_define_symbol"))
    }

    /// Store `value` at array index `index` on the array handle `arr`,
    /// resolving both handles through the arena at call time.
    pub fn scoped_set_index(
        &mut self,
        s: &HandleScope,
        arr: Scoped<'_>,
        index: usize,
        value: Scoped<'_>,
    ) -> Result<(), NativeError> {
        let result = self.cx.interp.scoped_set_index(s, arr, index, value);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_set_index"))
    }

    /// Allocate a host-data object through the native root contract and park it
    /// in scope `s`. The object is created null-prototype (as by
    /// [`Self::alloc_host_object`]); install a prototype and methods afterwards
    /// through the scope.
    pub fn scoped_host_object<'s, T: object::HostObjectData>(
        &mut self,
        s: &'s HandleScope,
        data: T,
    ) -> Result<Scoped<'s>, NativeError> {
        let object = self.alloc_host_object(data)?;
        Ok(self.cx.interp.scoped_value(s, Value::object(object)))
    }

    /// Allocate a static builtin native function value and park it in scope
    /// `s`. Mirrors the object-builder `builtin_method` path (a builtin-tagged
    /// function backed by the static fast-call `call`); define the result on a
    /// scoped object with `object::PropertyFlags` from
    /// [`crate::Attr::builtin_function`] via [`Self::scoped_define_data`].
    pub fn scoped_native_method<'s>(
        &mut self,
        s: &'s HandleScope,
        name: &'static str,
        length: u8,
        call: native_function::NativeFastFn,
    ) -> Result<Scoped<'s>, NativeError> {
        let result = self.cx.interp.scoped_native_static(s, name, length, call);
        result.map_err(|err| self.scoped_error(err, "NativeCtx::scoped_native_method"))
    }

    /// Read a handle as a Rust `String`, if it currently holds a JS string.
    /// Non-allocating on the VM heap.
    #[must_use]
    pub fn scoped_as_str(&self, v: Scoped<'_>) -> Option<String> {
        let raw = self.cx.interp.escape_scoped(v);
        raw.as_string(self.heap())
            .map(|s| s.to_lossy_string(self.heap()))
    }

    /// Read a handle as an `f64`, if it currently holds a number.
    #[must_use]
    pub fn scoped_as_f64(&self, v: Scoped<'_>) -> Option<f64> {
        self.cx.interp.escape_scoped(v).as_f64()
    }

    /// Whether the handle currently holds `undefined`.
    #[must_use]
    pub fn scoped_is_undefined(&self, v: Scoped<'_>) -> bool {
        self.cx.interp.escape_scoped(v).is_undefined()
    }

    /// Whether the handle currently holds `null`.
    #[must_use]
    pub fn scoped_is_null(&self, v: Scoped<'_>) -> bool {
        self.cx.interp.escape_scoped(v).is_null()
    }

    /// Whether the handle currently holds an ordinary object.
    #[must_use]
    pub fn scoped_is_object(&self, v: Scoped<'_>) -> bool {
        self.cx.interp.escape_scoped(v).as_object().is_some()
    }

    /// Read the current raw `Value` behind a scope handle for immediate
    /// hand-off across the scope boundary — a function return to the VM, or a
    /// store into an already-rooted object. The returned `Value` is valid only
    /// until the next allocation; never hold it across one.
    #[must_use]
    pub fn escape(&self, v: Scoped<'_>) -> Value {
        self.cx.interp.escape_scoped(v)
    }
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

    #[test]
    fn native_ctx_object_allocation_uses_young_space() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        {
            let mut ctx = NativeCtx::new(&mut interp);
            let _object = ctx.alloc_object().expect("native object allocation");
        }
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
        {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::call(Value::number_i32(7)),
            );
            let array = ctx
                .array_from_elements([Value::number_i32(1)])
                .expect("native array allocation");
            ctx.array_push(array, Value::number_i32(2))
                .expect("native array growth");
        }
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NativeCtx::array_from_elements should allocate through root-aware young allocation"
        );
    }

    #[test]
    fn native_ctx_collection_allocation_uses_young_space() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(Value::undefined(), Some(Value::number_i32(1))),
            );
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
        }
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
        {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(Value::undefined(), Some(Value::undefined())),
            );
            let target = Value::object(ctx.alloc_object().expect("target"));
            let _weak_ref = ctx
                .alloc_weak_ref(&target, &[], &[])
                .expect("native weak ref allocation");
            let _registry = ctx
                .alloc_finalization_registry(cleanup, None, &[], &[])
                .expect("native finalization registry allocation");
        }
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
        {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(Value::undefined(), Some(Value::undefined())),
            );
            let registry = ctx.interp_mut().error_classes_clone();
            let error = registry
                .make_instance_native_rooted(&mut ctx, ErrorKind::TypeError, Some("boom"), &[], &[])
                .expect("native error allocation");
            assert!(
                crate::object::get(error, ctx.heap(), "message").is_some_and(|v| v.is_string())
            );
        }
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
        {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(Value::undefined(), Some(Value::undefined())),
            );
            let registry = ctx.interp_mut().error_classes_clone();
            let errors = [Value::number_i32(1)];
            let error = registry
                .make_aggregate_instance_native_rooted(
                    &mut ctx,
                    errors.as_slice(),
                    Some("all rejected"),
                    &[],
                    &[],
                )
                .expect("native aggregate error allocation");
            assert!(crate::object::get(error, ctx.heap(), "errors").is_some_and(|v| v.is_array()));
        }
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Native AggregateError should allocate the error and errors array through root-aware young allocation"
        );
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
        let mut ctx = NativeCtx::new(&mut interp);
        let ok = ctx.scope(|ctx, s| {
            let obj = ctx.scoped_object(s).unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();

            let name = ctx.scoped_string(s, "otter").unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();
            ctx.scoped_set(s, obj, "name", name).unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();

            let count = ctx.scoped_number(s, 42.0);
            ctx.scoped_set(s, obj, "count", count).unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();

            let arr = ctx.scoped_array(s, 0).unwrap();
            let e0 = ctx.scoped_number(s, 1.0);
            ctx.scoped_set_index(s, arr, 0, e0).unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();
            let e1 = ctx.scoped_string(s, "two").unwrap();
            ctx.scoped_set_index(s, arr, 1, e1).unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();
            ctx.scoped_set(s, obj, "items", arr).unwrap();
            ctx.cx.interp.collect_minor_tracing_runtime_roots();

            // Read every field back through the relocated object handle.
            let name_read = ctx.scoped_get(s, obj, "name").unwrap();
            assert_eq!(ctx.scoped_as_str(name_read).as_deref(), Some("otter"));

            let count_read = ctx.scoped_get(s, obj, "count").unwrap();
            assert_eq!(ctx.scoped_as_f64(count_read), Some(42.0));

            let items_read = ctx.scoped_get(s, obj, "items").unwrap();
            let items_value = ctx.escape(items_read);
            let js_array = items_value
                .as_array()
                .expect("items reads back as an array");
            let (first, second, len) = crate::array::with_elements(js_array, ctx.heap(), |els| {
                (els[0], els[1], els.len())
            });
            assert_eq!(len, 2);
            assert_eq!(first.as_f64(), Some(1.0));
            assert_eq!(
                second
                    .as_string(ctx.heap())
                    .expect("element 1 is a string")
                    .to_lossy_string(ctx.heap()),
                "two"
            );
            true
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
        let mut ctx = NativeCtx::new(&mut interp);
        let (moved, content) = ctx.scope(|ctx, s| {
            let obj = ctx.scoped_object(s).unwrap();
            let value = ctx.scoped_string(s, "payload").unwrap();
            ctx.scoped_set(s, obj, "k", value).unwrap();
            let before = ctx
                .escape(obj)
                .as_raw_gc()
                .expect("object is a heap cell")
                .0;

            // Churn young space and scavenge until the survivor is evacuated to
            // the other semispace (its offset changes), proving the arena slot
            // was rewritten in place rather than left dangling.
            let mut after = before;
            let mut moved = false;
            for _ in 0..8 {
                let _churn = ctx.scoped_object(s).unwrap();
                ctx.cx.interp.collect_minor_tracing_runtime_roots();
                after = ctx
                    .escape(obj)
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

            let read_back = ctx.scoped_get(s, obj, "k").unwrap();
            let content = ctx
                .scoped_as_str(read_back)
                .expect("property still a string");
            (moved, content)
        });
        assert!(moved);
        assert_eq!(content, "payload");
    }
}

// `RuntimeCx` and `NativeCtx` are `!Send + !Sync` because they
// hold a `&mut Interpreter` (which is `!Send + !Sync` by virtue
// of holding a `GcHeap`, and reinforced by the static_assertions
// in `lib.rs`).

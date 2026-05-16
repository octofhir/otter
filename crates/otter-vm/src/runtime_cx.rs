//! Explicit runtime context for VM dispatch and native bindings.
//!
//! [`RuntimeCx<'rt>`] is the internal context handed to VM dispatch
//! and built-in helpers; it bundles the borrow set every algorithm
//! needs (`&mut RuntimeState`, `&mut GcHeap`, intrinsics) so callers
//! never reach for thread-local heap lookup. [`NativeCtx<'rt>`] is
//! the public-to-native binding view used by `#[dive]` /
//! `#[js_namespace]` style entry points.
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
    ExecutionContext, Interpreter, IteratorHandle, IteratorState, Value, array, collections,
    object, weak_refs,
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
    #[allow(dead_code)] // wired in by tasks 77-83 caller migration
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
        Self::call(Value::Undefined)
    }
}

/// Public-to-native binding context. Handed to `#[dive]` /
/// `#[js_namespace]` entry points so native code allocates and
/// mutates against the right isolate without reaching for
/// thread-local state.
///
/// `NativeCtx<'rt>` is `!Send + !Sync` and never crosses `.await`.
/// The lifetime `'rt` is the mutator turn — the same constraint
/// that applies to [`RuntimeCx<'rt>`].
///
/// # Migration
///
/// Native bindings under `crates/otter-modules` and active product
/// crates use this instead of ad-hoc runtime handles.
pub struct NativeCtx<'rt> {
    pub(crate) cx: RuntimeCx<'rt>,
    call_info: NativeCallInfo,
    context: Option<ExecutionContext>,
}

impl<'rt> NativeCtx<'rt> {
    /// Build a native context from an interpreter borrow.
    #[must_use]
    #[allow(dead_code)] // wired in by tasks 82-83 native-binding migration
    pub(crate) fn new(interp: &'rt mut Interpreter) -> Self {
        Self::new_with_call_info(interp, NativeCallInfo::default_call())
    }

    /// Build a native context with explicit call-site metadata.
    #[must_use]
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
    pub(crate) fn new_with_call_info_and_context(
        interp: &'rt mut Interpreter,
        call_info: NativeCallInfo,
        context: Option<ExecutionContext>,
    ) -> Self {
        Self {
            cx: RuntimeCx::new(interp),
            call_info,
            context,
        }
    }

    /// Execution context for the active native call.
    #[must_use]
    pub(crate) fn execution_context(&self) -> Option<&ExecutionContext> {
        self.context.as_ref()
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
        (self.cx.interp, self.context.clone())
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
    pub(crate) fn heap_mut(&mut self) -> &mut otter_gc::GcHeap {
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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

    /// Allocate an ordinary object while keeping additional local values alive.
    pub fn alloc_object_with_roots(
        &mut self,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<object::JsObject, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        object::alloc_object_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `Map` body through the native root contract.
    pub fn alloc_map(&mut self) -> Result<collections::JsMap, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_map_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `Set` body through the native root contract.
    pub fn alloc_set(&mut self) -> Result<collections::JsSet, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_set_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `WeakMap` body through the native root contract.
    pub fn alloc_weak_map(&mut self) -> Result<collections::JsWeakMap, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::alloc_weak_map_with_roots(self.heap_mut(), &mut external_visit)
    }

    /// Allocate a `WeakSet` body through the native root contract.
    pub fn alloc_weak_set(&mut self) -> Result<collections::JsWeakSet, otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_native_roots(visitor, &roots, &this_value, new_target.as_ref(), &[], &[]);
        };
        collections::set_add_with_roots(set, self.heap_mut(), value, &mut external_visit)
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        array::from_elements_with_roots(self.heap_mut(), elements, &mut external_visit)
    }

    /// Store an array element through the native root contract.
    pub fn array_set(
        &mut self,
        array: array::JsArray,
        index: usize,
        value: Value,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let roots = self.collect_native_roots();
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let value_root = value.clone();
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
        let value_root = value.clone();
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
        let this_value = self.call_info.this_value.clone();
        let new_target = self.call_info.new_target.clone();
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
        self.heap_mut().alloc_with_roots(state, &mut external_visit)
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
        let context = self.context.clone().ok_or(crate::NativeError::TypeError {
            name: "NativeCtx::queue_microtask",
            reason: "missing execution context".to_string(),
        })?;
        self.cx.interp.microtasks_mut().enqueue(crate::Microtask {
            callee,
            this_value: Value::Undefined,
            args: args.into_iter().collect(),
            context: Some(context),
            result_capability: None,
            kind: crate::microtask::MicrotaskKind::Call,
        });
        Ok(())
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
    use crate::{
        Interpreter, NativeError, NumberValue, Value, error_classes::ErrorKind, native_value_static,
    };

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
                NativeCallInfo::call(Value::Number(NumberValue::from_i32(7))),
            );
            let array = ctx
                .array_from_elements([Value::Number(NumberValue::from_i32(1))])
                .expect("native array allocation");
            ctx.array_push(array, Value::Number(NumberValue::from_i32(2)))
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
                NativeCallInfo::construct(
                    Value::Undefined,
                    Some(Value::Number(NumberValue::from_i32(1))),
                ),
            );
            let mut map = ctx.alloc_map().expect("native map allocation");
            ctx.map_set(
                &mut map,
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(2)),
            )
            .expect("native map insert");
            let mut set = ctx.alloc_set().expect("native set allocation");
            ctx.set_add(&mut set, Value::Number(NumberValue::from_i32(3)))
                .expect("native set insert");
            let _weak_map = ctx.alloc_weak_map().expect("native weak map allocation");
            let _weak_set = ctx.alloc_weak_set().expect("native weak set allocation");
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
            Ok(Value::Undefined)
        }

        let mut interp = Interpreter::new();
        let cleanup =
            native_value_static(interp.gc_heap_mut(), "cleanup", 0, cleanup).expect("cleanup");
        let before = interp.gc_heap().stats().new_allocated_bytes;
        {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(Value::Undefined, Some(Value::Undefined)),
            );
            let target = Value::Object(ctx.alloc_object().expect("target"));
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
                NativeCallInfo::construct(Value::Undefined, Some(Value::Undefined)),
            );
            let registry = ctx.interp_mut().error_classes_clone();
            let error = registry
                .make_instance_native_rooted(&mut ctx, ErrorKind::TypeError, Some("boom"), &[], &[])
                .expect("native error allocation");
            assert!(matches!(
                crate::object::get(error, ctx.heap(), "message"),
                Some(Value::String(_))
            ));
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
                NativeCallInfo::construct(Value::Undefined, Some(Value::Undefined)),
            );
            let registry = ctx.interp_mut().error_classes_clone();
            let errors = [Value::Number(NumberValue::from_i32(1))];
            let error = registry
                .make_aggregate_instance_native_rooted(
                    &mut ctx,
                    errors.as_slice(),
                    Some("all rejected"),
                    &[],
                    &[],
                )
                .expect("native aggregate error allocation");
            assert!(matches!(
                crate::object::get(error, ctx.heap(), "errors"),
                Some(Value::Array(_))
            ));
        }
        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Native AggregateError should allocate the error and errors array through root-aware young allocation"
        );
    }
}

// `RuntimeCx` and `NativeCtx` are `!Send + !Sync` because they
// hold a `&mut Interpreter` (which is `!Send + !Sync` by virtue
// of holding a `GcHeap`, and reinforced by the static_assertions
// in `lib.rs`).

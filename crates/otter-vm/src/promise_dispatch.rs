//! `Promise` constructor + statics + prototype dispatch.
//!
//! Slice 34. Connects three layers:
//!
//! - The bytecode-side opcodes ([`otter_bytecode::Op::PromiseNew`],
//!   [`otter_bytecode::Op::PromiseCall`]) and the universal
//!   [`otter_bytecode::Op::CallMethodValue`] when its receiver is
//!   a [`crate::Value::Promise`].
//! - The value-level state machine implemented by
//!   [`crate::JsPromiseHandle`] / [`crate::PurePromise`].
//! - The microtask queue: settlement reactions land on the queue as plain
//!   [`crate::Microtask`]s.
//!
//! # Contents
//! - [`statics_call`] — dispatcher for `Promise.<name>(args...)`
//!   (`resolve`, `reject`, `all`, `race`).
//! - [`prototype_call`] — dispatcher for
//!   `promise.<name>(args...)` (`then`, `catch`, `finally`).
//! - [`PromiseBuilder`] — root-aware `NewPromiseCapability`
//!   (§27.2.1.5).
//!
//! # Invariants
//! - Native `resolve` / `reject` closures capture the promise via
//!   `JsPromiseHandle::clone()` (shared body). They are
//!   idempotent — once a promise settles, subsequent resolve /
//!   reject calls are no-ops per spec §27.2.1.4 / §27.2.1.7.
//! - Settlement enqueues all pending reactions onto
//!   `Interpreter::microtasks` so the surrounding drain picks
//!   them up on the next generation.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-promise-objects>
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

use crate::error_classes::{ErrorClassRegistry, ErrorKind};
use crate::execution_context::ExecutionContext;
use crate::holt_stack::HoltStack;
use crate::native_function::{
    NativeError, native_value_with_captures_unchecked_with_roots, traced_native_value_with_length,
};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
    PromiseThenOutcome,
};
use crate::string::JsString;
use crate::{Interpreter, NativeCtx, Value};
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::{SmallVec, smallvec};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

struct PromiseSlots {
    /// `Value::array(values)` in a `Cell` so [`PromiseSlots::trace`]
    /// rewrites the FIELD in place when the collector moves the array
    /// body. A bare `JsArray` copy here goes stale across the very
    /// first combinator-loop allocation (user iterators, capability
    /// promises), and tracing a temporary `Value` copy updates the
    /// temporary, not the field — exactly the Promise.all
    /// use-after-move this replaces.
    values: Cell<Value>,
    keys: Option<Cell<Value>>,
    remaining: Cell<usize>,
}

/// Registration adapter: keeps a combinator's slot arrays AND its
/// frame locals traced live for the duration of the iteration loop.
/// The slot arrays' `Cell` fields are rewritten in place; the locals
/// (`cap`, the constructor, the iterator record, `promise_resolve`)
/// are plain `Value` stack slots that the collector rewrites through
/// the recorded pointers — every allocation the loop performs (user
/// iterator re-entry, per-element capability functions, downstream
/// promises) would otherwise leave them pointing at vacated cells,
/// which is exactly how a combinator ended up invoking a *different*
/// promise's resolve function.
struct CombinatorRoot {
    slots: Option<Arc<PromiseSlots>>,
    locals: Vec<*const Value>,
}

impl otter_gc::ExtraRootSource for CombinatorRoot {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        if let Some(slots) = &self.slots {
            slots.trace(visitor);
        }
        // SAFETY: the guard is dropped before the locals go out of
        // scope (it lives in the same frame, declared after them).
        for &local in &self.locals {
            unsafe { (*local).trace_value_slots(visitor) };
        }
    }
}

/// RAII heap registration for [`CombinatorRoot`]: unregisters on drop
/// so early error returns leave the heap's extra-roots stack balanced.
struct SlotsRootGuard {
    _registration: otter_gc::ExtraRootsGuard,
    /// Boxed so the registered source address stays stable for the
    /// registration's lifetime.
    _root: Box<CombinatorRoot>,
}

fn register_combinator_root(
    interp: &mut Interpreter,
    slots: Option<&Arc<PromiseSlots>>,
    cap: &PromiseCapability,
    locals: &[&Value],
) -> SlotsRootGuard {
    let mut pointers: Vec<*const Value> = Vec::with_capacity(locals.len() + 3);
    pointers.push(&raw const cap.promise);
    pointers.push(&raw const cap.resolve);
    pointers.push(&raw const cap.reject);
    pointers.extend(locals.iter().map(|value| *value as *const Value));
    let root = Box::new(CombinatorRoot {
        slots: slots.map(Arc::clone),
        locals: pointers,
    });
    let registration = interp
        .gc_heap_mut()
        .register_extra_roots(otter_gc::ExtraRoots::new(&*root));
    SlotsRootGuard {
        _registration: registration,
        _root: root,
    }
}

struct CapabilityExecutorState {
    resolve: RefCell<Option<Value>>,
    reject: RefCell<Option<Value>>,
}

impl CapabilityExecutorState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            resolve: RefCell::new(None),
            reject: RefCell::new(None),
        })
    }

    fn trace(&self, visitor: &mut SlotVisitor<'_>) {
        if let Some(value) = self.resolve.borrow().as_ref() {
            value.trace_value_slots(visitor);
        }
        if let Some(value) = self.reject.borrow().as_ref() {
            value.trace_value_slots(visitor);
        }
    }

    fn call(&self, args: &[Value]) -> Result<Value, NativeError> {
        if self.resolve.borrow().is_some() {
            return Err(NativeError::TypeError {
                name: "Promise",
                reason: "promise capability executor already has a resolve function".to_string(),
            });
        }
        if self.reject.borrow().is_some() {
            return Err(NativeError::TypeError {
                name: "Promise",
                reason: "promise capability executor already has a reject function".to_string(),
            });
        }
        let resolve = args.first().cloned().unwrap_or(Value::undefined());
        let reject = args.get(1).cloned().unwrap_or(Value::undefined());
        if !resolve.is_undefined() {
            *self.resolve.borrow_mut() = Some(resolve);
        }
        if !reject.is_undefined() {
            *self.reject.borrow_mut() = Some(reject);
        }
        Ok(Value::undefined())
    }
}

impl PromiseSlots {
    fn new(
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Arc<Self>, NativeError> {
        let values = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                value_roots,
                slice_roots,
            )
            .map_err(|_| oom_native("Promise combinator"))?;
        Ok(Arc::new(Self {
            values: Cell::new(Value::array(values)),
            keys: None,
            remaining: Cell::new(1),
        }))
    }

    fn new_keyed(
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Arc<Self>, NativeError> {
        let values = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                value_roots,
                slice_roots,
            )
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        let values_root = Value::array(values);
        let mut key_roots = Vec::with_capacity(value_roots.len() + 1);
        key_roots.extend_from_slice(value_roots);
        key_roots.push(&values_root);
        let keys = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                &key_roots,
                slice_roots,
            )
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        Ok(Arc::new(Self {
            values: Cell::new(Value::array(values)),
            keys: Some(Cell::new(Value::array(keys))),
            remaining: Cell::new(1),
        }))
    }

    fn trace(&self, visitor: &mut SlotVisitor<'_>) {
        // Trace the fields in place: `Cell::as_ptr` reaches the stored
        // `Value` itself, so a moving collection rewrites the handles
        // this struct will read next, not a temporary copy.
        unsafe {
            (*self.values.as_ptr()).trace_value_slots(visitor);
            if let Some(keys) = &self.keys {
                (*keys.as_ptr()).trace_value_slots(visitor);
            }
        }
    }

    fn values_array(&self) -> crate::array::JsArray {
        self.values
            .get()
            .as_array()
            .expect("PromiseSlots::values always holds an array")
    }

    fn keys_array(&self) -> Option<crate::array::JsArray> {
        self.keys.as_ref().map(|keys| {
            keys.get()
                .as_array()
                .expect("PromiseSlots::keys always holds an array")
        })
    }

    fn array_value(&self) -> Value {
        self.values.get()
    }

    fn keys_value(&self) -> Option<Value> {
        self.keys.as_ref().map(Cell::get)
    }

    fn reserve_slot(
        &self,
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<usize, NativeError> {
        let roots = interp.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        let len = crate::array::push_with_roots(
            self.values_array(),
            interp.gc_heap_mut(),
            Value::hole(),
            &mut external_visit,
        )
        .map_err(|_| oom_native("Promise combinator"))?;
        self.remaining.set(self.remaining.get().saturating_add(1));
        Ok(len - 1)
    }

    fn reserve_keyed_slot(
        &self,
        interp: &mut Interpreter,
        key: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<usize, NativeError> {
        let Some(keys) = self.keys_array() else {
            return Err(NativeError::TypeError {
                name: "Promise keyed combinator",
                reason: "missing keyed slots".to_string(),
            });
        };
        let key_root = key;
        let values_root = self.array_value();
        let keys_root = Value::array(keys);
        let mut roots = Vec::with_capacity(value_roots.len() + 3);
        roots.extend_from_slice(value_roots);
        roots.push(&key_root);
        roots.push(&values_root);
        roots.push(&keys_root);
        let runtime_roots = interp.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &runtime_roots, &roots, slice_roots);
        };
        crate::array::push_with_roots(keys, interp.gc_heap_mut(), key, &mut external_visit)
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        let len = crate::array::push_with_roots(
            self.values_array(),
            interp.gc_heap_mut(),
            Value::hole(),
            &mut external_visit,
        )
        .map_err(|_| oom_native("Promise keyed combinator"))?;
        self.remaining.set(self.remaining.get().saturating_add(1));
        Ok(len - 1)
    }

    fn fill(&self, heap: &mut otter_gc::GcHeap, index: usize, value: Value) -> bool {
        let did_fill = crate::array::with_elements_mut(self.values_array(), heap, |elements| {
            let Some(slot) = elements.get_mut(index) else {
                return false;
            };
            if !slot.is_hole() {
                return false;
            }
            *slot = value;
            true
        });
        if !did_fill {
            return false;
        }
        let count = self.remaining.get().saturating_sub(1);
        self.remaining.set(count);
        count == 0
    }

    fn finish_iteration(&self) -> bool {
        let count = self.remaining.get().saturating_sub(1);
        self.remaining.set(count);
        count == 0
    }

    fn collect_values(&self, heap: &otter_gc::GcHeap) -> Vec<Value> {
        crate::array::with_elements(self.values_array(), heap, |elements| {
            elements
                .iter()
                .map(|slot| {
                    if slot.is_hole() {
                        Value::undefined()
                    } else {
                        *slot
                    }
                })
                .collect()
        })
    }

    fn collect_keys(&self, heap: &otter_gc::GcHeap) -> Vec<Value> {
        let Some(keys) = self.keys_array() else {
            return Vec::new();
        };
        crate::array::with_elements(keys, heap, |elements| elements.to_vec())
    }
}

/// Root-aware helper for constructing ECMA-262 §27.2.1.5
/// `NewPromiseCapability` records with an explicit VM execution
/// context. Each method routes through the appropriate root walker
/// (runtime / stack / native) so heap allocations remain visible to
/// GC during the construction sequence.
#[derive(Debug, Clone, Default)]
pub struct PromiseBuilder {
    context: Option<ExecutionContext>,
}

impl PromiseBuilder {
    /// Create a builder without a captured VM context.
    #[must_use]
    pub fn new() -> Self {
        Self { context: None }
    }

    /// Create a builder whose capabilities retain `context` for
    /// later VM-dispatched settlement work.
    #[must_use]
    pub fn with_context(context: ExecutionContext) -> Self {
        Self {
            context: Some(context),
        }
    }

    /// Create a builder from an optional context.
    #[must_use]
    pub fn with_optional_context(context: Option<ExecutionContext>) -> Self {
        Self { context }
    }

    pub(crate) fn pending_runtime_rooted(
        &self,
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        JsPromiseHandle::pending_with_roots(interp.gc_heap_mut(), &mut external_visit)
    }

    pub(crate) fn pending_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &HoltStack,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        JsPromiseHandle::pending_with_roots(interp.gc_heap_mut(), &mut external_visit)
    }

    pub(crate) fn fulfilled_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &HoltStack,
        value: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        JsPromiseHandle::fulfilled_with_roots(interp.gc_heap_mut(), value, &mut external_visit)
    }

    pub(crate) fn rejected_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &HoltStack,
        reason: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        JsPromiseHandle::rejected_with_roots(interp.gc_heap_mut(), reason, &mut external_visit)
    }

    pub(crate) fn rejected_runtime_rooted(
        &self,
        interp: &mut Interpreter,
        reason: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        JsPromiseHandle::rejected_with_roots(interp.gc_heap_mut(), reason, &mut external_visit)
    }

    pub(crate) fn pending_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = ctx.collect_native_roots();
        let this_value = *ctx.this_value();
        let new_target = ctx.new_target().cloned();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            crate::runtime_cx::visit_native_roots(
                visitor,
                &roots,
                &this_value,
                new_target.as_ref(),
                value_roots,
                slice_roots,
            );
        };
        JsPromiseHandle::pending_with_roots(ctx.heap_mut(), &mut external_visit)
    }

    pub(crate) fn construct_runtime_rooted(
        &self,
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_runtime_rooted(interp, value_roots, slice_roots)?;
        let promise_value = Value::promise(promise);
        let mut resolve_roots = Vec::with_capacity(value_roots.len() + 1);
        resolve_roots.extend_from_slice(value_roots);
        resolve_roots.push(&promise_value);
        let resolve = make_resolve_native_runtime_rooted(
            interp,
            promise,
            self.context.clone(),
            &resolve_roots,
            slice_roots,
        )?;
        // The resolve-native allocation may have relocated the young promise
        // body; refresh the handle from the rooted `promise_value`.
        let promise = promise_value
            .as_promise()
            .expect("rooted promise survives allocation");
        let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
        reject_roots.extend_from_slice(value_roots);
        reject_roots.push(&promise_value);
        reject_roots.push(&resolve);
        let reject =
            make_reject_native_runtime_rooted(interp, promise, &reject_roots, slice_roots)?;
        let promise = promise_value
            .as_promise()
            .expect("rooted promise survives allocation");
        Ok((promise, resolve, reject))
    }

    pub(crate) fn construct_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &HoltStack,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_stack_rooted(interp, stack, value_roots, slice_roots)?;
        let promise_value = Value::promise(promise);
        let mut resolve_roots = Vec::with_capacity(value_roots.len() + 1);
        resolve_roots.extend_from_slice(value_roots);
        resolve_roots.push(&promise_value);
        let resolve = make_resolve_native_stack_rooted(
            interp,
            stack,
            promise,
            self.context.clone(),
            &resolve_roots,
            slice_roots,
        )?;
        let promise = promise_value
            .as_promise()
            .expect("rooted promise survives allocation");
        let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
        reject_roots.extend_from_slice(value_roots);
        reject_roots.push(&promise_value);
        reject_roots.push(&resolve);
        let reject =
            make_reject_native_stack_rooted(interp, stack, promise, &reject_roots, slice_roots)?;
        let promise = promise_value
            .as_promise()
            .expect("rooted promise survives allocation");
        Ok((promise, resolve, reject))
    }

    pub(crate) fn construct_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_native_rooted(ctx, value_roots, slice_roots)?;
        let promise_value = Value::promise(promise);
        let mut resolve_roots = Vec::with_capacity(value_roots.len() + 1);
        resolve_roots.extend_from_slice(value_roots);
        resolve_roots.push(&promise_value);
        let resolve = make_resolve_native_native_rooted(
            ctx,
            promise,
            self.context.clone(),
            &resolve_roots,
            slice_roots,
        )?;
        let promise = promise_value
            .as_promise()
            .expect("rooted promise survives allocation");
        let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
        reject_roots.extend_from_slice(value_roots);
        reject_roots.push(&promise_value);
        reject_roots.push(&resolve);
        let reject = make_reject_native_native_rooted(ctx, promise, &reject_roots, slice_roots)?;
        let promise = promise_value
            .as_promise()
            .expect("rooted promise survives allocation");
        Ok((promise, resolve, reject))
    }

    pub(crate) fn capability_runtime_rooted(
        &self,
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<PromiseCapability, otter_gc::OutOfMemory> {
        let (handle, resolve, reject) =
            self.construct_runtime_rooted(interp, value_roots, slice_roots)?;
        Ok(PromiseCapability {
            promise: Value::promise(handle),
            resolve,
            reject,
            context: self.context.clone(),
        })
    }

    pub(crate) fn capability_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &HoltStack,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<PromiseCapability, otter_gc::OutOfMemory> {
        let (handle, resolve, reject) =
            self.construct_stack_rooted(interp, stack, value_roots, slice_roots)?;
        Ok(PromiseCapability {
            promise: Value::promise(handle),
            resolve,
            reject,
            context: self.context.clone(),
        })
    }
}

/// Construct a pending promise while visiting the interpreter runtime roots and
/// caller-provided temporary roots.
pub fn pending_runtime_rooted(
    interp: &mut Interpreter,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
    PromiseBuilder::new().pending_runtime_rooted(interp, value_roots, slice_roots)
}

fn visit_runtime_roots(
    visitor: &mut dyn FnMut(*mut RawGc),
    roots: &[*mut RawGc],
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) {
    for &slot in roots {
        visitor(slot);
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

fn native_value_with_captures_native_rooted<F>(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    captures: smallvec::SmallVec<[Value; 4]>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let new_target = ctx.new_target().cloned();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        crate::runtime_cx::visit_native_roots(
            visitor,
            &roots,
            &this_value,
            new_target.as_ref(),
            value_roots,
            slice_roots,
        );
    };
    native_value_with_captures_unchecked_with_roots(
        ctx.heap_mut(),
        name,
        captures,
        &mut external_visit,
        call,
    )
}

fn trace_captures(
    captures: &smallvec::SmallVec<[Value; 4]>,
) -> Arc<crate::native_function::NativeTraceFn> {
    let captures = captures.clone();
    Arc::new(move |visitor: &mut SlotVisitor<'_>| {
        for capture in captures.iter() {
            capture.trace_value_slots(visitor);
        }
    })
}

fn promise_native_runtime<F>(
    interp: &mut Interpreter,
    name: &'static str,
    length: u8,
    captures: smallvec::SmallVec<[Value; 4]>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let trace = trace_captures(&captures);
    let roots = interp.collect_runtime_roots();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        trace,
        &mut external_visit,
        call,
    )
}

fn promise_native_stack<F>(
    interp: &mut Interpreter,
    stack: &HoltStack,
    name: &'static str,
    length: u8,
    captures: smallvec::SmallVec<[Value; 4]>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let trace = trace_captures(&captures);
    let roots = interp.collect_allocation_roots(stack);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        trace,
        &mut external_visit,
        call,
    )
}

fn promise_native_ctx<F>(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    length: u8,
    captures: smallvec::SmallVec<[Value; 4]>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let trace = trace_captures(&captures);
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let new_target = ctx.new_target().cloned();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        crate::runtime_cx::visit_native_roots(
            visitor,
            &roots,
            &this_value,
            new_target.as_ref(),
            value_roots,
            slice_roots,
        );
    };
    traced_native_value_with_length(
        ctx.heap_mut(),
        name,
        length,
        captures,
        trace,
        &mut external_visit,
        call,
    )
}

/// Rebuild an element function's capability from its LIVE captures.
/// The captures live inside the native function's GC body and are
/// traced (and rewritten on relocation) with it; the `cap` clones the
/// body closure captured at build time are plain Rust copies that go
/// stale on the first moving collection — reading them is the
/// Promise-combinator use-after-move family.
fn capability_from_captures(captures: &[Value], template: &PromiseCapability) -> PromiseCapability {
    PromiseCapability {
        promise: captures.first().copied().unwrap_or(template.promise),
        resolve: captures.get(1).copied().unwrap_or(template.resolve),
        reject: captures.get(2).copied().unwrap_or(template.reject),
        context: template.context.clone(),
    }
}

fn promise_element_function<F>(
    interp: &mut Interpreter,
    name: &'static str,
    length: u8,
    captures: smallvec::SmallVec<[Value; 4]>,
    trace: Arc<crate::native_function::NativeTraceFn>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let roots = interp.collect_runtime_roots();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        trace,
        &mut external_visit,
        call,
    )
}

/// Dispatch a `Promise.<method>(args...)` static call. Routes
/// the typed [`PromiseMethod`] emitted by the compiler.
pub fn statics_call(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Option<Value>,
    method: otter_bytecode::method_id::PromiseMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    use otter_bytecode::method_id::PromiseMethod as M;
    let constructor = match constructor {
        Some(constructor) => constructor,
        None => builtin_promise_constructor(interp)?,
    };
    if !is_builtin_promise_constructor(interp, &constructor) {
        return match method {
            M::Resolve => static_resolve_generic(interp, context, constructor, args),
            M::Reject => static_reject_generic(interp, context, constructor, args),
            M::All => static_all_generic(interp, context, constructor, args),
            M::Race => static_race_generic(interp, context, constructor, args),
            M::AllSettled => static_all_settled_generic(interp, context, constructor, args),
            M::Any => static_any_generic(interp, context, constructor, args),
            M::WithResolvers => static_with_resolvers_generic(interp, context, constructor),
            M::Try => static_try_generic(interp, context, constructor, args),
            M::AllKeyed => {
                static_all_keyed_generic(interp, context, constructor, args, KeyedVariant::All)
            }
            M::AllSettledKeyed => static_all_keyed_generic(
                interp,
                context,
                constructor,
                args,
                KeyedVariant::AllSettled,
            ),
        };
    }
    match method {
        M::Resolve => static_resolve(interp, context, constructor, args),
        M::Reject => Ok(Value::promise(static_reject(interp, args)?)),
        M::All => static_all_generic(interp, context, constructor, args),
        M::Race => static_race_generic(interp, context, constructor, args),
        M::AllSettled => static_all_settled_generic(interp, context, constructor, args),
        M::Any => static_any_generic(interp, context, constructor, args),
        M::WithResolvers => static_with_resolvers(interp, context),
        M::Try => static_try_generic(interp, context, constructor, args),
        M::AllKeyed => {
            static_all_keyed_generic(interp, context, constructor, args, KeyedVariant::All)
        }
        M::AllSettledKeyed => {
            static_all_keyed_generic(interp, context, constructor, args, KeyedVariant::AllSettled)
        }
    }
}

/// Dispatch a `promise.<name>(args...)` instance-method call.
/// Branches on `then` / `catch` / `finally`; everything else
/// surfaces as `UnknownIntrinsic` upstream.
pub fn prototype_call(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    name: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    match name {
        "then" => method_then(interp, context, promise, args),
        "catch" => Ok(method_catch(interp, context, promise, args)),
        "finally" => method_finally_value(
            interp,
            context,
            Value::promise(*promise),
            args.first().cloned().unwrap_or(Value::undefined()),
        ),
        other => Err(NativeError::TypeError {
            name: "Promise.prototype",
            reason: format!("method `{other}` is not defined"),
        }),
    }
}

/// §27.2.5.1 / §27.2.5.3 helper — `Invoke(receiver, "then", « on_fulfilled,
/// on_rejected »)`. Reads `.then` via ordinary property semantics
/// (firing accessor `[[Get]]` if present) and calls it with the
/// supplied receiver. Used by `Promise.prototype.catch` and
/// `Promise.prototype.finally` so user-supplied `.then` overrides
/// (including monkey-patches on plain thenables) are observable.
pub fn invoke_then(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<Value, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Promise.prototype",
            reason: "missing execution context".to_string(),
        })?;
    let (interp, _) = ctx.interp_mut_and_context();
    invoke_then_interp(interp, &exec, receiver, on_fulfilled, on_rejected)
}

fn invoke_then_interp(
    interp: &mut Interpreter,
    exec: &ExecutionContext,
    receiver: Value,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.prototype";
    let then = get_callable_property(interp, exec, receiver, "then", NAME)?;
    interp
        .run_callable_sync(exec, &then, receiver, smallvec![on_fulfilled, on_rejected])
        .map_err(|err| promise_vm_error(interp, NAME, err))
}

/// §27.2.5.3 `Promise.prototype.finally(onFinally)`.
///
/// 1. Let promise be the this value.
/// 2. If Type(promise) is not Object, throw TypeError.
/// 3. Let C = SpeciesConstructor(promise, %Promise%).
/// 4. If IsCallable(onFinally) is false, thenFinally = catchFinally = onFinally.
///    Else build the spec'd thenFinally / catchFinally closures.
/// 5. Return ? Invoke(promise, "then", « thenFinally, catchFinally »).
///
/// The catchFinally closure has to *throw* the original rejection
/// reason verbatim (per spec a `thrower` function). It does so by
/// stashing the value on [`Interpreter::set_pending_uncaught_throw`]
/// and returning `NativeError::Thrown`; the surrounding microtask
/// drain consumes that slot to settle the downstream promise with
/// identity preserved.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.prototype.finally>
///
/// Public entry point for `Promise.prototype.finally` invoked via
/// ordinary native dispatch (`Promise.prototype.finally.call(obj,
/// ...)`). Threads through to [`method_finally_value`] so any
/// receiver, not just a `Value::Promise`, can be processed.
pub fn method_finally_invoke(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    let context = ctx.execution_context().cloned();
    let (interp, _) = ctx.interp_mut_and_context();
    method_finally_value(interp, context, receiver, on_finally)
}

fn method_finally_value(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    receiver: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.prototype.finally";
    if !receiver.is_object_type() {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "`this` is not an Object".to_string(),
        });
    }
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;
    if !crate::is_callable_value(&on_finally) {
        return invoke_then_interp(interp, &exec, receiver, on_finally, on_finally);
    }
    let default_ctor = builtin_promise_constructor(interp)?;
    let c = species_constructor_runtime(interp, &exec, &receiver, &default_ctor, NAME)?;
    let then_finally = make_then_finally(interp, &exec, c, on_finally)?;
    let catch_finally = make_catch_finally(interp, &exec, c, on_finally)?;
    invoke_then_interp(interp, &exec, receiver, then_finally, catch_finally)
}

fn make_then_finally(
    interp: &mut Interpreter,
    exec: &ExecutionContext,
    constructor: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![constructor, on_finally];
    let trace = trace_captures(&captures);
    let exec_for_call = exec.clone();
    let constructor_root = constructor;
    let on_finally_root = on_finally;
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&constructor_root, &on_finally_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        1,
        captures,
        trace,
        &mut external_visit,
        move |ctx, args, captures| {
            let c = captures[0];
            let on_finally = captures[1];
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let result = {
                let (interp, _) = ctx.interp_mut_and_context();
                interp
                    .run_callable_sync(
                        &exec_for_call,
                        &on_finally,
                        Value::undefined(),
                        SmallVec::new(),
                    )
                    .map_err(|err| promise_vm_error(interp, "Promise.prototype.finally", err))?
            };
            let resolved = {
                let (interp, _) = ctx.interp_mut_and_context();
                let resolve_fn = get_promise_resolve(interp, &exec_for_call, &c)?;
                call_promise_resolve(interp, &exec_for_call, &resolve_fn, &c, result)?
            };
            let value_thunk = make_value_thunk(ctx, value)?;
            invoke_then(ctx, resolved, value_thunk, Value::undefined())
        },
    )
    .map_err(|_| oom_native("Promise.prototype.finally"))
}

fn make_catch_finally(
    interp: &mut Interpreter,
    exec: &ExecutionContext,
    constructor: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![constructor, on_finally];
    let trace = trace_captures(&captures);
    let exec_for_call = exec.clone();
    let constructor_root = constructor;
    let on_finally_root = on_finally;
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&constructor_root, &on_finally_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        1,
        captures,
        trace,
        &mut external_visit,
        move |ctx, args, captures| {
            let c = captures[0];
            let on_finally = captures[1];
            let reason = args.first().cloned().unwrap_or(Value::undefined());
            let result = {
                let (interp, _) = ctx.interp_mut_and_context();
                interp
                    .run_callable_sync(
                        &exec_for_call,
                        &on_finally,
                        Value::undefined(),
                        SmallVec::new(),
                    )
                    .map_err(|err| promise_vm_error(interp, "Promise.prototype.finally", err))?
            };
            let resolved = {
                let (interp, _) = ctx.interp_mut_and_context();
                let resolve_fn = get_promise_resolve(interp, &exec_for_call, &c)?;
                call_promise_resolve(interp, &exec_for_call, &resolve_fn, &c, result)?
            };
            let thrower = make_thrower(ctx, reason)?;
            invoke_then(ctx, resolved, thrower, Value::undefined())
        },
    )
    .map_err(|_| oom_native("Promise.prototype.finally"))
}

fn make_value_thunk(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![value];
    let trace = trace_captures(&captures);
    let value_root = value;
    let (interp, _) = ctx.interp_mut_and_context();
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&value_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        0,
        captures,
        trace,
        &mut external_visit,
        move |_ctx, _args, captures| Ok(captures[0]),
    )
    .map_err(|_| oom_native("Promise.prototype.finally"))
}

fn make_thrower(ctx: &mut NativeCtx<'_>, reason: Value) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![reason];
    let trace = trace_captures(&captures);
    let reason_root = reason;
    let (interp, _) = ctx.interp_mut_and_context();
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&reason_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        0,
        captures,
        trace,
        &mut external_visit,
        move |ctx, _args, captures| {
            let reason = captures[0];
            // Stash the original Value so the microtask drain can
            // settle the chained promise with identity preserved.
            // NativeError::Thrown alone would render the reason as
            // a string and lose object/Symbol identity.
            ctx.interp_mut().set_pending_uncaught_throw(reason);
            Err(NativeError::Thrown {
                name: "Promise.prototype.finally",
                message: String::new(),
            })
        },
    )
    .map_err(|_| oom_native("Promise.prototype.finally"))
}

// -- statics --------------------------------------------------------

fn is_builtin_promise_constructor(interp: &Interpreter, constructor: &Value) -> bool {
    constructor
        .as_native_function()
        .is_some_and(|native| native.name(interp.gc_heap()) == "Promise")
}

fn builtin_promise_constructor(interp: &Interpreter) -> Result<Value, NativeError> {
    crate::object::get(*interp.global_this(), interp.gc_heap(), "Promise").ok_or_else(|| {
        NativeError::TypeError {
            name: "Promise",
            reason: "Promise constructor is not installed".to_string(),
        }
    })
}

fn promise_vm_error(
    interp: &crate::Interpreter,
    name: &'static str,
    err: crate::VmError,
) -> NativeError {
    match err {
        crate::VmError::Uncaught => {
            let value = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                _ => Default::default(),
            };
            NativeError::Thrown {
                name,
                message: value.into(),
            }
        }
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

/// §27.2.1.5 `NewPromiseCapability(C)` for non-intrinsic
/// constructors.
///
/// The built-in `%Promise%` fast path still uses [`PromiseBuilder`]
/// directly. This path preserves the observable constructor/executor
/// protocol for `Promise.<static>.call(C, ...)`: validate
/// `IsConstructor(C)`, construct with a single executor argument,
/// reject duplicate executor calls with non-`undefined` resolve /
/// reject values, and require callable captured functions.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-newpromisecapability>
fn new_generic_promise_capability(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<PromiseCapability, NativeError> {
    let exec = context.ok_or_else(|| NativeError::TypeError {
        name: "Promise",
        reason: "missing execution context".to_string(),
    })?;
    if !crate::is_constructor_runtime(&constructor, &exec, interp.gc_heap()) {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "this value is not a constructor".to_string(),
        });
    }
    let state = CapabilityExecutorState::new();
    let trace_state = {
        let state = state.clone();
        Arc::new(move |visitor: &mut SlotVisitor<'_>| state.trace(visitor))
    };
    let state_for_call = state.clone();
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.extend_from_slice(value_roots);
    roots.push(&constructor);
    // §27.2.1.5.1 — the GetCapabilitiesExecutor has length 2.
    let executor = {
        let runtime_roots = interp.collect_runtime_roots();
        let roots_slice = roots.as_slice();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &runtime_roots, roots_slice, slice_roots);
        };
        crate::native_function::traced_native_value_with_length(
            interp.gc_heap_mut(),
            "",
            2,
            SmallVec::new(),
            trace_state,
            &mut external_visit,
            move |_ctx, args, _captures| state_for_call.call(args),
        )?
    };
    let promise = interp
        .run_construct_sync(&exec, &constructor, constructor, smallvec![executor])
        .map_err(|err| promise_vm_error(interp, "Promise", err))?;
    let resolve = (*state.resolve.borrow()).unwrap_or(Value::undefined());
    let reject = (*state.reject.borrow()).unwrap_or(Value::undefined());
    if !crate::is_callable_value(&resolve) {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "promise capability resolve is not callable".to_string(),
        });
    }
    if !crate::is_callable_value(&reject) {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "promise capability reject is not callable".to_string(),
        });
    }
    Ok(PromiseCapability {
        promise,
        resolve,
        reject,
        context: Some(exec),
    })
}

fn call_capability_function(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    function: &Value,
    value: Value,
) -> Result<(), NativeError> {
    let exec = cap.context.as_ref().ok_or_else(|| NativeError::TypeError {
        name: "Promise",
        reason: "missing execution context".to_string(),
    })?;
    interp
        .run_callable_sync(exec, function, Value::undefined(), smallvec![value])
        .map_err(|err| promise_vm_error(interp, "Promise", err))?;
    Ok(())
}

fn call_capability_resolve(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    value: Value,
) -> Result<(), NativeError> {
    call_capability_function(interp, cap, &cap.resolve, value)
}

fn call_capability_reject(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    reason: Value,
) -> Result<(), NativeError> {
    call_capability_function(interp, cap, &cap.reject, reason)
}

fn native_error_rejection_value(interp: &mut Interpreter, err: NativeError) -> Value {
    if let NativeError::Thrown { message, .. } = err {
        let heap = interp.gc_heap_mut();
        return Value::string(
            crate::JsString::from_str(&message, heap).unwrap_or_else(|_| {
                crate::JsString::from_str("", heap).expect("empty string allocates")
            }),
        );
    }
    let vm_error = crate::native_to_vm_error(interp, err);
    crate::error_ops::vm_err_to_value(interp, &vm_error)
}

fn native_error_rejection_value_preserving_throw(
    interp: &mut Interpreter,
    err: NativeError,
) -> Value {
    if matches!(err, NativeError::Thrown { .. })
        && let Some(value) = interp.take_pending_uncaught_throw()
    {
        return value;
    }
    native_error_rejection_value(interp, err)
}

fn reject_capability_error(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    err: NativeError,
) -> Result<Value, NativeError> {
    let reason = native_error_rejection_value_preserving_throw(interp, err);
    call_capability_reject(interp, cap, reason)?;
    Ok(cap.promise)
}

/// Read an own/inherited property by string key without callability check.
///
/// Invokes any accessor `[[Get]]` exactly once per §10.1.8.1.
fn get_property_runtime(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    receiver: Value,
    key: &'static str,
    name: &'static str,
) -> Result<Value, NativeError> {
    let property_key = crate::VmPropertyKey::String(key);
    match interp
        .ordinary_get_value(context, receiver, receiver, &property_key, 0)
        .map_err(|err| promise_vm_error(interp, name, err))?
    {
        crate::VmGetOutcome::Value(value) => Ok(value),
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(interp, name, err)),
    }
}

/// Read an own/inherited property by symbol key without callability check.
fn get_symbol_property_runtime(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    receiver: Value,
    sym: crate::symbol::JsSymbol,
    name: &'static str,
) -> Result<Value, NativeError> {
    let property_key = crate::VmPropertyKey::Symbol(sym);
    match interp
        .ordinary_get_value(context, receiver, receiver, &property_key, 0)
        .map_err(|err| promise_vm_error(interp, name, err))?
    {
        crate::VmGetOutcome::Value(value) => Ok(value),
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(interp, name, err)),
    }
}

/// §7.3.21 `SpeciesConstructor(O, defaultConstructor)` — picks the
/// constructor to use when an algorithm needs a fresh instance derived
/// from `O`. Returns `defaultConstructor` when `O.constructor` is
/// `undefined`, throws `TypeError` if `constructor` is a non-object,
/// returns `C` when `C[@@species]` is `null`/`undefined`, and otherwise
/// returns `C[@@species]` after validating it is a constructor.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-speciesconstructor>
fn species_constructor_runtime(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    obj: &Value,
    default_ctor: &Value,
    name: &'static str,
) -> Result<Value, NativeError> {
    let c = get_property_runtime(interp, context, *obj, "constructor", name)?;
    if c.is_undefined() {
        return Ok(*default_ctor);
    }
    if !c.is_object_type() {
        return Err(NativeError::TypeError {
            name,
            reason: "constructor is not an Object".to_string(),
        });
    }
    let species_sym = interp
        .well_known_symbols()
        .get(crate::symbol::WellKnown::Species);
    let s = get_symbol_property_runtime(interp, context, c, species_sym, name)?;
    if s.is_undefined() || s.is_null() {
        return Ok(c);
    }
    if crate::is_constructor_runtime(&s, context, interp.gc_heap()) {
        return Ok(s);
    }
    Err(NativeError::TypeError {
        name,
        reason: "Symbol.species is not a constructor".to_string(),
    })
}

fn get_callable_property(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    receiver: Value,
    key: &'static str,
    name: &'static str,
) -> Result<Value, NativeError> {
    let property_key = crate::VmPropertyKey::String(key);
    let value = match interp
        .ordinary_get_value(context, receiver, receiver, &property_key, 0)
        .map_err(|err| promise_vm_error(interp, name, err))?
    {
        crate::VmGetOutcome::Value(value) => value,
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(interp, name, err))?,
    };
    if !interp.is_callable_runtime(&value) {
        return Err(NativeError::TypeError {
            name,
            reason: format!("{key} is not callable"),
        });
    }
    Ok(value)
}

fn get_promise_resolve(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    constructor: &Value,
) -> Result<Value, NativeError> {
    get_callable_property(interp, context, *constructor, "resolve", "Promise.resolve")
}

fn call_promise_resolve(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    resolve_fn: &Value,
    constructor: &Value,
    value: Value,
) -> Result<Value, NativeError> {
    interp
        .run_callable_sync(context, resolve_fn, *constructor, smallvec![value])
        .map_err(|err| promise_vm_error(interp, "Promise.resolve", err))
}

fn attach_then_value(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    promise: Value,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<(), NativeError> {
    let then = get_callable_property(interp, context, promise, "then", "Promise combinator")?;
    interp
        .run_callable_sync(
            context,
            &then,
            promise,
            smallvec![on_fulfilled, on_rejected],
        )
        .map_err(|err| promise_vm_error(interp, "Promise combinator", err))?;
    Ok(())
}

fn static_resolve(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::undefined());
    if let Some(p) = value.as_promise() {
        if let Some(exec) = context.as_ref() {
            let value_constructor =
                get_property_runtime(interp, exec, value, "constructor", "Promise.resolve")?;
            if crate::abstract_ops::same_value(&value_constructor, &constructor, interp.gc_heap()) {
                return Ok(Value::promise(p));
            }
        } else {
            return Ok(Value::promise(p));
        }
    }
    // §27.2.4.7 PromiseResolve — settle a fresh promise through its
    // resolve function rather than fulfilling directly, so a thenable
    // value is adopted (§27.2.1.3.2) instead of becoming the
    // fulfillment value verbatim.
    let cap = PromiseBuilder::with_optional_context(context).capability_runtime_rooted(
        interp,
        &[&value],
        &[args],
    )?;
    call_capability_resolve(interp, &cap, value)?;
    Ok(cap.promise)
}

fn static_reject(interp: &mut Interpreter, args: &[Value]) -> Result<JsPromiseHandle, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::undefined());
    Ok(PromiseBuilder::new().rejected_runtime_rooted(interp, reason, &[], &[args])?)
}

fn static_resolve_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::undefined());
    let cap = new_generic_promise_capability(interp, context, constructor, &[&value], &[args])?;
    call_capability_resolve(interp, &cap, value)?;
    Ok(cap.promise)
}

fn static_reject_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::undefined());
    let cap = new_generic_promise_capability(interp, context, constructor, &[&reason], &[args])?;
    call_capability_reject(interp, &cap, reason)?;
    Ok(cap.promise)
}

/// §27.2.4.6 `Promise.try(callbackfn, ...args)`.
///
/// 1. Let C be the this value.
/// 2. If C is not an Object, throw TypeError.
/// 3. Let promiseCapability = NewPromiseCapability(C).
/// 4. Let status = Completion(Call(callbackfn, undefined, args)).
/// 5. If status is an abrupt completion: Call(reject, undefined,
///    «status.value»).
/// 6. Else: Call(resolve, undefined, «status.value»).
/// 7. Return promiseCapability.[[Promise]].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.try>
fn static_try_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.try";
    if !constructor.is_object_type() {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "Promise.try `this` is not an Object".to_string(),
        });
    }
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;
    let callbackfn = args.first().cloned().unwrap_or(Value::undefined());
    let forwarded: SmallVec<[Value; 8]> = if args.len() > 1 {
        args[1..].iter().cloned().collect()
    } else {
        SmallVec::new()
    };
    let cap = new_generic_promise_capability(
        interp,
        Some(exec.clone()),
        constructor,
        &[&callbackfn],
        &[args],
    )?;
    let call_result = interp.run_callable_sync(&exec, &callbackfn, Value::undefined(), forwarded);
    match call_result {
        Ok(value) => {
            call_capability_resolve(interp, &cap, value)?;
        }
        Err(crate::VmError::Uncaught) => {
            let reason = crate::error_ops::vm_err_to_value(interp, &crate::VmError::Uncaught);
            call_capability_reject(interp, &cap, reason)?;
        }
        Err(other) => {
            let reason = crate::error_ops::vm_err_to_value(interp, &other);
            call_capability_reject(interp, &cap, reason)?;
        }
    }
    Ok(cap.promise)
}

#[derive(Clone, Copy)]
enum KeyedVariant {
    All,
    AllSettled,
}

impl KeyedVariant {
    const fn name(self) -> &'static str {
        match self {
            Self::All => "Promise.allKeyed",
            Self::AllSettled => "Promise.allSettledKeyed",
        }
    }
}

fn static_all_keyed_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
    variant: KeyedVariant,
) -> Result<Value, NativeError> {
    let name = variant.name();
    let cap = new_generic_promise_capability(interp, context.clone(), constructor, &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let promises = args.first().cloned().unwrap_or(Value::undefined());
    if !promises.is_object_type() {
        return reject_capability_error(
            interp,
            &cap,
            NativeError::TypeError {
                name,
                reason: "promises argument is not an Object".to_string(),
            },
        );
    }
    let all_keys = match interp.own_property_keys_value(&exec, &promises) {
        Ok(keys) => keys,
        Err(err) => {
            let native = promise_vm_error(interp, name, err);
            return reject_capability_error(interp, &cap, native);
        }
    };

    (|| -> Result<Value, NativeError> {
        let slots = PromiseSlots::new_keyed(
            interp,
            &[
                &cap.promise,
                &cap.resolve,
                &cap.reject,
                &promise_resolve,
                &constructor,
                &promises,
            ],
            &[args, all_keys.as_slice()],
        )?;
        let slots_root = slots.array_value();
        let keys_root = slots.keys_value().unwrap_or(Value::undefined());
        interp.push_iteration_anchor(slots_root);
        interp.push_iteration_anchor(keys_root);
        let _slots_fields_root = register_combinator_root(
            interp,
            Some(&slots),
            &cap,
            &[&constructor, &promise_resolve, &promises],
        );
        for key in all_keys {
            let Some(vm_key) = vm_property_key_from_value(&key, interp.gc_heap()) else {
                continue;
            };
            let desc = match interp.ordinary_get_own_property_descriptor_value_runtime_rooted(
                &exec,
                promises,
                &vm_key,
                0,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &promises,
                    &slots_root,
                    &keys_root,
                    &key,
                ],
                &[args],
            ) {
                Ok(desc) => desc,
                Err(err) => {
                    let native = promise_vm_error(interp, name, err);
                    return reject_capability_error(interp, &cap, native);
                }
            };
            if !desc.as_ref().is_some_and(|desc| desc.enumerable()) {
                continue;
            }
            let next_value = match keyed_get(interp, &exec, promises, &vm_key, name) {
                Ok(value) => value,
                Err(err) => return reject_capability_error(interp, &cap, err),
            };
            let i = slots.reserve_keyed_slot(
                interp,
                key,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &promises,
                    &slots_root,
                    &keys_root,
                    &key,
                    &next_value,
                ],
                &[args],
            )?;
            let value_anchor_base = interp.push_iteration_anchor(next_value) - 1;
            let entry_promise_result =
                call_promise_resolve(interp, &exec, &promise_resolve, &constructor, next_value);
            interp.pop_iteration_anchors_to(value_anchor_base);
            let entry_promise = match entry_promise_result {
                Ok(value) => value,
                Err(err) => return reject_capability_error(interp, &cap, err),
            };
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise) - 1;
            let on_fulfill = keyed_element_function(
                interp,
                slots.clone(),
                cap.clone(),
                variant,
                true,
                i,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &promises,
                    &entry_promise,
                    &slots_root,
                    &keys_root,
                ],
                &[args],
            )?;
            let on_reject = match variant {
                KeyedVariant::All => cap.reject,
                KeyedVariant::AllSettled => keyed_element_function(
                    interp,
                    slots.clone(),
                    cap.clone(),
                    variant,
                    false,
                    i,
                    &[
                        &cap.promise,
                        &cap.resolve,
                        &cap.reject,
                        &on_fulfill,
                        &promise_resolve,
                        &constructor,
                        &promises,
                        &entry_promise,
                        &slots_root,
                        &keys_root,
                    ],
                    &[args],
                )?,
            };
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, on_fulfill, on_reject);
            interp.pop_iteration_anchors_to(entry_anchor_base);
            if let Err(err) = attach_result {
                return reject_capability_error(interp, &cap, err);
            }
        }
        if slots.finish_iteration() {
            resolve_keyed_slots_runtime(
                interp,
                &cap,
                &slots,
                name,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &promises,
                ],
                &[args],
            )?;
        }
        Ok(cap.promise)
    })()
}

fn vm_property_key_from_value(
    key: &Value,
    heap: &otter_gc::GcHeap,
) -> Option<crate::VmPropertyKey<'static>> {
    if let Some(s) = key.as_string(heap) {
        return Some(crate::VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
    }
    if let Some(sym) = key.as_symbol(heap) {
        return Some(crate::VmPropertyKey::Symbol(sym));
    }
    None
}

fn keyed_get(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    receiver: Value,
    key: &crate::VmPropertyKey<'_>,
    name: &'static str,
) -> Result<Value, NativeError> {
    match interp
        .ordinary_get_value(context, receiver, receiver, key, 0)
        .map_err(|err| promise_vm_error(interp, name, err))?
    {
        crate::VmGetOutcome::Value(value) => Ok(value),
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(interp, name, err)),
    }
}

fn keyed_element_function(
    interp: &mut Interpreter,
    slots: Arc<PromiseSlots>,
    cap: PromiseCapability,
    variant: KeyedVariant,
    fulfilled: bool,
    index: usize,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, NativeError> {
    let name = variant.name();
    let trace_slots = {
        let slots = slots.clone();
        let cap = cap.clone();
        Arc::new(move |visitor: &mut SlotVisitor<'_>| {
            slots.trace(visitor);
            cap.promise.trace_value_slots(visitor);
            cap.resolve.trace_value_slots(visitor);
            cap.reject.trace_value_slots(visitor);
        })
    };
    promise_element_function(
        interp,
        "",
        1,
        smallvec![cap.promise, cap.resolve, cap.reject],
        trace_slots,
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            let cap = capability_from_captures(captures, &cap);
            let payload = args.first().cloned().unwrap_or(Value::undefined());
            let value = match variant {
                KeyedVariant::All => payload,
                KeyedVariant::AllSettled => build_settled_record(fulfilled, payload, ctx)?,
            };
            if slots.fill(ctx.heap_mut(), index, value) {
                resolve_keyed_slots_native(ctx, &cap, &slots, name)?;
            }
            Ok(Value::undefined())
        },
    )
    .map_err(|_| oom_native(name))
}

fn resolve_keyed_slots_runtime(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    slots: &PromiseSlots,
    name: &'static str,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<(), NativeError> {
    let keys = slots.collect_keys(interp.gc_heap());
    let values = slots.collect_values(interp.gc_heap());
    let result =
        create_keyed_result_runtime(interp, name, &keys, &values, value_roots, slice_roots)?;
    call_capability_resolve(interp, cap, result)
}

fn resolve_keyed_slots_native(
    ctx: &mut NativeCtx<'_>,
    cap: &PromiseCapability,
    slots: &PromiseSlots,
    name: &'static str,
) -> Result<(), NativeError> {
    let keys = slots.collect_keys(ctx.heap());
    let values = slots.collect_values(ctx.heap());
    let result = create_keyed_result_native(ctx, name, &keys, &values)?;
    let interp = ctx.interp_mut();
    call_capability_resolve(interp, cap, result)
}

fn create_keyed_result_runtime(
    interp: &mut Interpreter,
    name: &'static str,
    keys: &[Value],
    values: &[Value],
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, NativeError> {
    let mut all_slice_roots = Vec::with_capacity(slice_roots.len() + 2);
    all_slice_roots.extend_from_slice(slice_roots);
    all_slice_roots.push(keys);
    all_slice_roots.push(values);
    let obj = interp
        .alloc_runtime_rooted_object_with_roots(value_roots, all_slice_roots.as_slice())
        .map_err(|_| oom_native(name))?;
    define_keyed_result_properties(obj, interp.gc_heap_mut(), keys, values, name)?;
    Ok(Value::object(obj))
}

fn create_keyed_result_native(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    keys: &[Value],
    values: &[Value],
) -> Result<Value, NativeError> {
    let obj = ctx
        .alloc_object_with_roots(&[], &[keys, values])
        .map_err(|_| oom_native(name))?;
    define_keyed_result_properties(obj, ctx.heap_mut(), keys, values, name)?;
    Ok(Value::object(obj))
}

fn define_keyed_result_properties(
    obj: crate::object::JsObject,
    heap: &mut otter_gc::GcHeap,
    keys: &[Value],
    values: &[Value],
    name: &'static str,
) -> Result<(), NativeError> {
    for (key, value) in keys.iter().zip(values.iter()) {
        let desc = crate::object::PropertyDescriptor::data(*value, true, true, true);
        let ok = if let Some(s) = key.as_string(heap) {
            let key = s.to_lossy_string(heap);
            crate::object::define_own_property(obj, heap, &key, desc)
        } else if let Some(sym) = key.as_symbol(heap) {
            crate::object::define_own_symbol_property(obj, heap, sym, desc)
        } else {
            true
        };
        if !ok {
            return Err(NativeError::TypeError {
                name,
                reason: "failed to define keyed result property".to_string(),
            });
        }
    }
    Ok(())
}

fn static_all_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let cap = new_generic_promise_capability(interp, context.clone(), constructor, &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.all",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            let native = promise_vm_error(interp, "Promise.all", err);
            return reject_capability_error(interp, &cap, native);
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator) - 1;
    interp.push_iteration_anchor(next_method);
    let outcome = (|| -> Result<Value, NativeError> {
        let slots = PromiseSlots::new(
            interp,
            &[
                &cap.promise,
                &cap.resolve,
                &cap.reject,
                &promise_resolve,
                &constructor,
                &iterable,
                &iterator,
                &next_method,
            ],
            &[args],
        )?;
        let slots_root = slots.array_value();
        interp.push_iteration_anchor(slots_root);
        let _slots_fields_root = register_combinator_root(
            interp,
            Some(&slots),
            &cap,
            &[
                &constructor,
                &promise_resolve,
                &iterable,
                &iterator,
                &next_method,
            ],
        );
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    let native = promise_vm_error(interp, "Promise.all", err);
                    return reject_capability_error(interp, &cap, native);
                }
            };
            let i = slots.reserve_slot(
                interp,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &iterable,
                    &iterator,
                    &next_method,
                    &slots_root,
                    &next_value,
                ],
                &[args],
            )?;
            let value_anchor_base = interp.push_iteration_anchor(next_value) - 1;
            let entry_promise_result =
                call_promise_resolve(interp, &exec, &promise_resolve, &constructor, next_value);
            interp.pop_iteration_anchors_to(value_anchor_base);
            let entry_promise = match entry_promise_result {
                Ok(value) => value,
                Err(err) => {
                    let _ = interp.iterator_close_sync(&exec, &iterator);
                    return reject_capability_error(interp, &cap, err);
                }
            };
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise) - 1;
            let cap_for_fulfill = cap.clone();
            let slots_for_trace = slots.clone();
            let trace_slots = Arc::new(move |visitor: &mut SlotVisitor<'_>| {
                slots_for_trace.trace(visitor);
                cap_for_fulfill.promise.trace_value_slots(visitor);
                cap_for_fulfill.resolve.trace_value_slots(visitor);
                cap_for_fulfill.reject.trace_value_slots(visitor);
            });
            let cap_for_fulfill = cap.clone();
            let slots_for_fulfill = slots.clone();
            let on_fulfill = promise_element_function(
                interp,
                "",
                1,
                smallvec![cap.promise, cap.resolve, cap.reject],
                trace_slots,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &entry_promise,
                    &slots_root,
                ],
                &[args],
                move |ctx, args, captures| {
                    let cap = capability_from_captures(captures, &cap_for_fulfill);
                    let v = args.first().cloned().unwrap_or(Value::undefined());
                    if slots_for_fulfill.fill(ctx.heap_mut(), i, v) {
                        let collected = slots_for_fulfill.collect_values(ctx.heap());
                        let arr = ctx.array_from_elements_with_roots(
                            collected.iter().cloned(),
                            &[&cap.promise, &cap.resolve, &cap.reject],
                            &[collected.as_slice()],
                        )?;
                        let interp = ctx.interp_mut();
                        call_capability_resolve(interp, &cap, Value::array(arr))?;
                    }
                    Ok(Value::undefined())
                },
            )?;
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, on_fulfill, cap.reject);
            interp.pop_iteration_anchors_to(entry_anchor_base);
            if let Err(err) = attach_result {
                let _ = interp.iterator_close_sync(&exec, &iterator);
                return reject_capability_error(interp, &cap, err);
            }
        }
        if slots.finish_iteration() {
            let collected = slots.collect_values(interp.gc_heap());
            let arr = interp
                .alloc_runtime_rooted_array_from_values(
                    collected.iter().cloned(),
                    &[
                        &cap.promise,
                        &cap.resolve,
                        &cap.reject,
                        &promise_resolve,
                        &constructor,
                        &iterable,
                        &iterator,
                        &next_method,
                        &slots_root,
                    ],
                    &[args, collected.as_slice()],
                )
                .map_err(|_| oom_native("Promise.all"))?;
            if let Err(err) = call_capability_resolve(interp, &cap, Value::array(arr)) {
                return reject_capability_error(interp, &cap, err);
            }
        }
        {}
        Ok(cap.promise)
    })();
    interp.pop_iteration_anchors_to(anchor_base);
    outcome
}

fn static_race_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let cap = new_generic_promise_capability(interp, context.clone(), constructor, &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.race",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            let native = promise_vm_error(interp, "Promise.race", err);
            return reject_capability_error(interp, &cap, native);
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator) - 1;
    interp.push_iteration_anchor(next_method);
    let _locals_root = register_combinator_root(
        interp,
        None,
        &cap,
        &[
            &constructor,
            &promise_resolve,
            &iterable,
            &iterator,
            &next_method,
        ],
    );
    let outcome = (|| -> Result<Value, NativeError> {
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    let native = promise_vm_error(interp, "Promise.race", err);
                    return reject_capability_error(interp, &cap, native);
                }
            };
            let value_anchor_base = interp.push_iteration_anchor(next_value) - 1;
            let entry_promise_result =
                call_promise_resolve(interp, &exec, &promise_resolve, &constructor, next_value);
            interp.pop_iteration_anchors_to(value_anchor_base);
            let entry_promise = match entry_promise_result {
                Ok(value) => value,
                Err(err) => {
                    let _ = interp.iterator_close_sync(&exec, &iterator);
                    return reject_capability_error(interp, &cap, err);
                }
            };
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise) - 1;
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, cap.resolve, cap.reject);
            interp.pop_iteration_anchors_to(entry_anchor_base);
            if let Err(err) = attach_result {
                let _ = interp.iterator_close_sync(&exec, &iterator);
                return reject_capability_error(interp, &cap, err);
            }
        }
        Ok(cap.promise)
    })();
    interp.pop_iteration_anchors_to(anchor_base);
    outcome
}

fn static_all_settled_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let cap = new_generic_promise_capability(interp, context.clone(), constructor, &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.allSettled",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            let native = promise_vm_error(interp, "Promise.allSettled", err);
            return reject_capability_error(interp, &cap, native);
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator) - 1;
    interp.push_iteration_anchor(next_method);
    let _heap = interp.gc_heap_mut();
    let outcome = (|| -> Result<Value, NativeError> {
        let slots = PromiseSlots::new(
            interp,
            &[
                &cap.promise,
                &cap.resolve,
                &cap.reject,
                &promise_resolve,
                &constructor,
                &iterable,
                &iterator,
                &next_method,
            ],
            &[args],
        )?;
        let slots_root = slots.array_value();
        interp.push_iteration_anchor(slots_root);
        let _slots_fields_root = register_combinator_root(
            interp,
            Some(&slots),
            &cap,
            &[
                &constructor,
                &promise_resolve,
                &iterable,
                &iterator,
                &next_method,
            ],
        );
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    let native = promise_vm_error(interp, "Promise.allSettled", err);
                    return reject_capability_error(interp, &cap, native);
                }
            };
            let i = slots.reserve_slot(
                interp,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &iterable,
                    &iterator,
                    &next_method,
                    &slots_root,
                    &next_value,
                ],
                &[args],
            )?;
            let value_anchor_base = interp.push_iteration_anchor(next_value) - 1;
            let entry_promise_result =
                call_promise_resolve(interp, &exec, &promise_resolve, &constructor, next_value);
            interp.pop_iteration_anchors_to(value_anchor_base);
            let entry_promise = match entry_promise_result {
                Ok(value) => value,
                Err(err) => {
                    let _ = interp.iterator_close_sync(&exec, &iterator);
                    return reject_capability_error(interp, &cap, err);
                }
            };
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise) - 1;
            let on_fulfill = {
                let slots = slots.clone();
                let cap = cap.clone();
                let promise_root = cap.promise;
                let resolve_root = cap.resolve;
                let reject_root = cap.reject;
                let trace_slots = {
                    let slots = slots.clone();
                    let cap = cap.clone();
                    Arc::new(move |visitor: &mut SlotVisitor<'_>| {
                        slots.trace(visitor);
                        cap.promise.trace_value_slots(visitor);
                        cap.resolve.trace_value_slots(visitor);
                        cap.reject.trace_value_slots(visitor);
                    })
                };
                promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![cap.promise, cap.resolve, cap.reject],
                    trace_slots,
                    &[
                        &promise_root,
                        &resolve_root,
                        &reject_root,
                        &promise_resolve,
                        &constructor,
                        &entry_promise,
                        &slots_root,
                    ],
                    &[args],
                    move |ctx, args, captures| {
                        let cap = capability_from_captures(captures, &cap);
                        let v = args.first().cloned().unwrap_or(Value::undefined());
                        let record = build_settled_record(true, v, ctx)?;
                        if slots.fill(ctx.heap_mut(), i, record) {
                            let collected = slots.collect_values(ctx.heap());
                            let arr = ctx.array_from_elements_with_roots(
                                collected.iter().cloned(),
                                &[&cap.promise, &cap.resolve, &cap.reject],
                                &[collected.as_slice()],
                            )?;
                            let interp = ctx.interp_mut();
                            call_capability_resolve(interp, &cap, Value::array(arr))?;
                        }
                        Ok(Value::undefined())
                    },
                )?
            };
            let on_reject = {
                let slots = slots.clone();
                let cap = cap.clone();
                let promise_root = cap.promise;
                let resolve_root = cap.resolve;
                let reject_root = cap.reject;
                let trace_slots = {
                    let slots = slots.clone();
                    let cap = cap.clone();
                    Arc::new(move |visitor: &mut SlotVisitor<'_>| {
                        slots.trace(visitor);
                        cap.promise.trace_value_slots(visitor);
                        cap.resolve.trace_value_slots(visitor);
                        cap.reject.trace_value_slots(visitor);
                    })
                };
                promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![cap.promise, cap.resolve, cap.reject],
                    trace_slots,
                    &[
                        &promise_root,
                        &resolve_root,
                        &reject_root,
                        &on_fulfill,
                        &promise_resolve,
                        &constructor,
                        &entry_promise,
                        &slots_root,
                    ],
                    &[args],
                    move |ctx, args, captures| {
                        let cap = capability_from_captures(captures, &cap);
                        let r = args.first().cloned().unwrap_or(Value::undefined());
                        let record = build_settled_record(false, r, ctx)?;
                        if slots.fill(ctx.heap_mut(), i, record) {
                            let collected = slots.collect_values(ctx.heap());
                            let arr = ctx.array_from_elements_with_roots(
                                collected.iter().cloned(),
                                &[&cap.promise, &cap.resolve, &cap.reject],
                                &[collected.as_slice()],
                            )?;
                            let interp = ctx.interp_mut();
                            call_capability_resolve(interp, &cap, Value::array(arr))?;
                        }
                        Ok(Value::undefined())
                    },
                )?
            };
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, on_fulfill, on_reject);
            interp.pop_iteration_anchors_to(entry_anchor_base);
            if let Err(err) = attach_result {
                let _ = interp.iterator_close_sync(&exec, &iterator);
                return reject_capability_error(interp, &cap, err);
            }
        }
        if slots.finish_iteration() {
            let collected = slots.collect_values(interp.gc_heap());
            let arr = interp
                .alloc_runtime_rooted_array_from_values(
                    collected.iter().cloned(),
                    &[
                        &cap.promise,
                        &cap.resolve,
                        &cap.reject,
                        &promise_resolve,
                        &constructor,
                        &iterable,
                        &iterator,
                        &next_method,
                        &slots_root,
                    ],
                    &[args, collected.as_slice()],
                )
                .map_err(|_| oom_native("Promise.allSettled"))?;
            if let Err(err) = call_capability_resolve(interp, &cap, Value::array(arr)) {
                return reject_capability_error(interp, &cap, err);
            }
        }
        Ok(cap.promise)
    })();
    interp.pop_iteration_anchors_to(anchor_base);
    outcome
}

fn build_settled_record(
    fulfilled: bool,
    payload: Value,
    ctx: &mut NativeCtx<'_>,
) -> Result<Value, NativeError> {
    let status_text = if fulfilled { "fulfilled" } else { "rejected" };
    let status = crate::JsString::from_str(status_text, ctx.heap_mut()).map_err(|e| {
        NativeError::TypeError {
            name: "Promise",
            reason: format!("string allocation failed: {e}"),
        }
    })?;
    let key = if fulfilled { "value" } else { "reason" };
    let obj = ctx.alloc_object().map_err(|_| NativeError::TypeError {
        name: "Promise",
        reason: "out of memory".to_string(),
    })?;
    ctx.set_property_with_roots(obj, "status", Value::string(status), &[&payload], &[])
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    ctx.set_property(obj, key, payload)
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    Ok(Value::object(obj))
}

fn make_aggregate_error_runtime_rooted(
    interp: &mut Interpreter,
    registry: &ErrorClassRegistry,
    errors: Vec<Value>,
) -> Result<Value, NativeError> {
    let message = aggregate_error_message(interp.gc_heap_mut())?;
    let obj = interp
        .alloc_runtime_rooted_object_with_roots(&[&message], &[&errors])
        .map_err(|_| oom_native("Promise.any"))?;
    {
        let gc_heap = interp.gc_heap_for_cx_mut();
        crate::object::set_prototype(
            obj,
            gc_heap,
            Some(registry.prototype(ErrorKind::AggregateError)),
        );
    }
    let mut errors_root = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for value in &errors {
            value.trace_value_slots(visitor);
        }
    };
    interp
        .set_property_with_extra_roots(obj, "message", message, &mut errors_root)
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    let obj_value = Value::object(obj);
    let arr = interp
        .alloc_runtime_rooted_array_from_values(
            errors.iter().cloned(),
            &[&obj_value],
            &[errors.as_slice()],
        )
        .map_err(|_| oom_native("Promise.any"))?;
    interp
        .set_property(obj, "errors", Value::array(arr))
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    Ok(Value::object(obj))
}

fn make_aggregate_error_native_rooted(
    ctx: &mut NativeCtx<'_>,
    registry: &ErrorClassRegistry,
    errors: Vec<Value>,
) -> Result<Value, NativeError> {
    // Handle-scope discipline: every intermediate (message string,
    // error instance, errors array) is arena-parked, so the shape
    // transitions each property define performs cannot strand a
    // sibling. The prototype resolves through the registry at use
    // time; the raw read is parked before the next allocation.
    let proto = Value::object(registry.prototype(ErrorKind::AggregateError));
    ctx.scope(|ctx, s| {
        let mut cx = crate::marshal::MarshalCx::new(ctx, s);
        let proto = cx.park(proto);
        let errors_array = {
            let array = cx
                .array(errors.len())
                .map_err(|err| err.into_native("Promise.any"))?;
            for (index, error) in errors.iter().enumerate() {
                let element = cx.park(*error);
                cx.set_index(array, index, element)
                    .map_err(|err| err.into_native("Promise.any"))?;
            }
            array
        };
        let message = cx
            .string("All promises were rejected")
            .map_err(|err| err.into_native("Promise.any"))?;
        let instance = cx.object().map_err(|err| err.into_native("Promise.any"))?;
        {
            let raw_instance = cx.escape(instance);
            let raw_proto = cx.escape(proto);
            if let (Some(object), Some(proto)) = (raw_instance.as_object(), raw_proto.as_object()) {
                crate::object::set_prototype(object, cx.heap_mut(), Some(proto));
            }
        }
        cx.set(instance, "message", message)
            .map_err(|err| err.into_native("Promise.any"))?;
        cx.set(instance, "errors", errors_array)
            .map_err(|err| err.into_native("Promise.any"))?;
        Ok(cx.escape(instance))
    })
}

fn aggregate_error_message(heap: &mut otter_gc::GcHeap) -> Result<Value, NativeError> {
    Ok(Value::string(
        JsString::from_str("All promises were rejected", heap).map_err(|e| {
            NativeError::TypeError {
                name: "Promise",
                reason: format!("string allocation failed: {e}"),
            }
        })?,
    ))
}

fn oom_native(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn alloc_object_result_with_object_proto(
    interp: &mut Interpreter,
    name: &'static str,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<crate::object::JsObject, NativeError> {
    match interp.constructor_prototype_value("Object") {
        Ok(v) if v.is_object() => interp
            .alloc_runtime_rooted_object_with_proto(
                v.as_object().unwrap(),
                value_roots,
                slice_roots,
            )
            .map_err(|_| oom_native(name)),
        _ => interp
            .alloc_runtime_rooted_object_with_roots(value_roots, slice_roots)
            .map_err(|_| oom_native(name)),
    }
}

fn static_any_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let cap = new_generic_promise_capability(interp, context.clone(), constructor, &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.any",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            let native = promise_vm_error(interp, "Promise.any", err);
            return reject_capability_error(interp, &cap, native);
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator) - 1;
    interp.push_iteration_anchor(next_method);
    let _heap = interp.gc_heap_mut();
    let registry = interp.error_classes_clone();
    let outcome = (|| -> Result<Value, NativeError> {
        let errors = PromiseSlots::new(
            interp,
            &[
                &cap.promise,
                &cap.resolve,
                &cap.reject,
                &promise_resolve,
                &constructor,
                &iterable,
                &iterator,
                &next_method,
            ],
            &[args],
        )?;
        let errors_root = errors.array_value();
        interp.push_iteration_anchor(errors_root);
        let _errors_fields_root = register_combinator_root(
            interp,
            Some(&errors),
            &cap,
            &[
                &constructor,
                &promise_resolve,
                &iterable,
                &iterator,
                &next_method,
            ],
        );
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    let native = promise_vm_error(interp, "Promise.any", err);
                    return reject_capability_error(interp, &cap, native);
                }
            };
            let i = errors.reserve_slot(
                interp,
                &[
                    &cap.promise,
                    &cap.resolve,
                    &cap.reject,
                    &promise_resolve,
                    &constructor,
                    &iterable,
                    &iterator,
                    &next_method,
                    &errors_root,
                    &next_value,
                ],
                &[args],
            )?;
            let value_anchor_base = interp.push_iteration_anchor(next_value) - 1;
            let entry_promise_result =
                call_promise_resolve(interp, &exec, &promise_resolve, &constructor, next_value);
            interp.pop_iteration_anchors_to(value_anchor_base);
            let entry_promise = match entry_promise_result {
                Ok(value) => value,
                Err(err) => {
                    let _ = interp.iterator_close_sync(&exec, &iterator);
                    return reject_capability_error(interp, &cap, err);
                }
            };
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise) - 1;
            let on_reject = {
                let errors = errors.clone();
                let registry = registry.clone();
                let cap = cap.clone();
                let promise_root = cap.promise;
                let resolve_root = cap.resolve;
                let reject_root = cap.reject;
                let trace_errors = {
                    let errors = errors.clone();
                    let cap = cap.clone();
                    Arc::new(move |visitor: &mut SlotVisitor<'_>| {
                        errors.trace(visitor);
                        cap.promise.trace_value_slots(visitor);
                        cap.resolve.trace_value_slots(visitor);
                        cap.reject.trace_value_slots(visitor);
                    })
                };
                promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![cap.promise, cap.resolve, cap.reject],
                    trace_errors,
                    &[
                        &promise_root,
                        &resolve_root,
                        &reject_root,
                        &promise_resolve,
                        &constructor,
                        &entry_promise,
                        &errors_root,
                    ],
                    &[args],
                    move |ctx, args, captures| {
                        let cap = capability_from_captures(captures, &cap);
                        let reason = args.first().cloned().unwrap_or(Value::undefined());
                        if errors.fill(ctx.heap_mut(), i, reason) {
                            let collected = errors.collect_values(ctx.heap());
                            let agg =
                                make_aggregate_error_native_rooted(ctx, &registry, collected)?;
                            let interp = ctx.interp_mut();
                            call_capability_reject(interp, &cap, agg)?;
                        }
                        Ok(Value::undefined())
                    },
                )?
            };
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, cap.resolve, on_reject);
            interp.pop_iteration_anchors_to(entry_anchor_base);
            if let Err(err) = attach_result {
                let _ = interp.iterator_close_sync(&exec, &iterator);
                return reject_capability_error(interp, &cap, err);
            }
        }
        if errors.finish_iteration() {
            let collected = errors.collect_values(interp.gc_heap());
            let agg = make_aggregate_error_runtime_rooted(interp, &registry, collected)?;
            call_capability_reject(interp, &cap, agg)?;
        }
        Ok(cap.promise)
    })();
    interp.pop_iteration_anchors_to(anchor_base);
    outcome
}

/// §27.2.4.6 `Promise.withResolvers()` — returns
/// `{ promise, resolve, reject }` over a fresh pending promise.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.withResolvers>
fn static_with_resolvers(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
) -> Result<Value, NativeError> {
    let cap = PromiseBuilder::with_optional_context(context).capability_runtime_rooted(
        interp,
        &[],
        &[],
    )?;
    let obj = alloc_object_result_with_object_proto(
        interp,
        "Promise.withResolvers",
        &[&cap.promise, &cap.resolve, &cap.reject],
        &[],
    )?;
    let mut cap_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        cap.promise.trace_value_slots(visitor);
        cap.resolve.trace_value_slots(visitor);
        cap.reject.trace_value_slots(visitor);
    };
    interp
        .set_property_with_extra_roots(obj, "promise", cap.promise, &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    let mut cap_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        cap.promise.trace_value_slots(visitor);
        cap.resolve.trace_value_slots(visitor);
        cap.reject.trace_value_slots(visitor);
    };
    interp
        .set_property_with_extra_roots(obj, "resolve", cap.resolve, &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    let mut cap_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        cap.promise.trace_value_slots(visitor);
        cap.resolve.trace_value_slots(visitor);
        cap.reject.trace_value_slots(visitor);
    };
    interp
        .set_property_with_extra_roots(obj, "reject", cap.reject, &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    Ok(Value::object(obj))
}

fn static_with_resolvers_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
) -> Result<Value, NativeError> {
    let cap = new_generic_promise_capability(interp, context, constructor, &[], &[])?;
    let obj = alloc_object_result_with_object_proto(
        interp,
        "Promise.withResolvers",
        &[&cap.promise, &cap.resolve, &cap.reject],
        &[],
    )?;
    let mut cap_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        cap.promise.trace_value_slots(visitor);
        cap.resolve.trace_value_slots(visitor);
        cap.reject.trace_value_slots(visitor);
    };
    interp
        .set_property_with_extra_roots(obj, "promise", cap.promise, &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    let mut cap_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        cap.promise.trace_value_slots(visitor);
        cap.resolve.trace_value_slots(visitor);
        cap.reject.trace_value_slots(visitor);
    };
    interp
        .set_property_with_extra_roots(obj, "resolve", cap.resolve, &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    let mut cap_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        cap.promise.trace_value_slots(visitor);
        cap.resolve.trace_value_slots(visitor);
        cap.reject.trace_value_slots(visitor);
    };
    interp
        .set_property_with_extra_roots(obj, "reject", cap.reject, &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    Ok(Value::object(obj))
}

// -- prototype methods ---------------------------------------------

/// §27.2.5.4 `Promise.prototype.then(onFulfilled, onRejected)`.
///
/// 1. Let promise be the this value.
/// 2. If IsPromise(promise) is false, throw TypeError.
/// 3. Let C = SpeciesConstructor(promise, %Promise%).
/// 4. Let resultCapability = NewPromiseCapability(C).
/// 5. Return PerformPromiseThen(promise, onFulfilled, onRejected,
///    resultCapability).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.prototype.then>
fn method_then(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    args: &[Value],
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.prototype.then";
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;
    let promise_root = Value::promise(*promise);
    let default_ctor = builtin_promise_constructor(interp)?;
    let c = species_constructor_runtime(interp, &exec, &promise_root, &default_ctor, NAME)?;

    let on_fulfilled = match args.first() {
        Some(v) if crate::is_callable_value(v) => Some(*v),
        _ => None,
    };
    let on_rejected = match args.get(1) {
        Some(v) if crate::is_callable_value(v) => Some(*v),
        _ => None,
    };

    let mut roots: Vec<&Value> = vec![&promise_root, &c];
    if let Some(value) = &on_fulfilled {
        roots.push(value);
    }
    if let Some(value) = &on_rejected {
        roots.push(value);
    }
    let capability = if is_builtin_promise_constructor(interp, &c) {
        PromiseBuilder::with_optional_context(context.clone())
            .capability_runtime_rooted(interp, &roots, &[])
            .map_err(|_| oom_native(NAME))?
    } else {
        new_generic_promise_capability(interp, context.clone(), c, &roots, &[])?
    };

    let outcome = promise.perform_then_with_context(
        interp.gc_heap_mut(),
        on_fulfilled,
        on_rejected,
        capability.clone(),
        context,
    );
    if let Some(job) = outcome.immediate_job {
        interp.microtasks_mut().enqueue(job);
    }
    Ok(capability.promise)
}

fn method_catch(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    args: &[Value],
) -> Value {
    let on_rejected = match args.first() {
        Some(v) if crate::is_callable_value(v) => Some(*v),
        _ => None,
    };
    perform_then_with_handlers(interp, context, promise, None, on_rejected)
}

// -- helpers -------------------------------------------------------

fn perform_then_with_handlers(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
) -> Value {
    let promise_root = Value::promise(*promise);
    let mut value_roots = vec![&promise_root];
    if let Some(value) = &on_fulfilled {
        value_roots.push(value);
    }
    if let Some(value) = &on_rejected {
        value_roots.push(value);
    }
    let capability = match PromiseBuilder::with_optional_context(context.clone())
        .capability_runtime_rooted(interp, &value_roots, &[])
    {
        Ok(cap) => cap,
        Err(_) => return Value::undefined(),
    };
    let outcome: PromiseThenOutcome = promise.perform_then_with_context(
        interp.gc_heap_mut(),
        on_fulfilled,
        on_rejected,
        capability.clone(),
        context,
    );
    if let Some(job) = outcome.immediate_job {
        interp.microtasks_mut().enqueue(job);
    }
    capability.promise
}

fn attach_then(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
) {
    // Reusable "result-of-then" path that the combinators don't
    // expose to user code. We still need a capability so the
    // reaction has somewhere to settle, even if we never read it.
    let promise_root = Value::promise(*promise);
    let mut value_roots = vec![&promise_root];
    if let Some(value) = &on_fulfilled {
        value_roots.push(value);
    }
    if let Some(value) = &on_rejected {
        value_roots.push(value);
    }
    let capability = match PromiseBuilder::with_optional_context(context.clone())
        .capability_runtime_rooted(interp, &value_roots, &[])
    {
        Ok(cap) => cap,
        Err(_) => return,
    };
    let outcome = promise.perform_then_with_context(
        interp.gc_heap_mut(),
        on_fulfilled,
        on_rejected,
        capability,
        context,
    );
    if let Some(job) = outcome.immediate_job {
        interp.microtasks_mut().enqueue(job);
    }
}

/// Read the settled promise handle from a settle-native's GC-traced captures.
///
/// The promise is stored as `captures[0]` (a `Value::promise`) so the moving
/// collector rewrites its offset when the young promise body relocates. The
/// resolve/reject pair is built by allocating two native-function bodies, each
/// of which scavenges; a handle closed over by value at construction time would
/// be left pointing at the body's pre-move slot — since reused by another young
/// object — so settling it would fault on a foreign payload. Reading the handle
/// back from the traced capture keeps it current.
fn settle_native_promise(captures: &[Value]) -> JsPromiseHandle {
    captures
        .first()
        .copied()
        .and_then(Value::as_promise)
        .expect("promise settle native function captures the promise handle at index 0")
}

fn make_resolve_native_runtime_rooted(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    context: Option<ExecutionContext>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    promise_native_runtime(
        interp,
        "",
        1,
        smallvec![Value::promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            resolve_native_body(
                ctx,
                args,
                settle_native_promise(captures),
                &captured_context,
            )
        },
    )
}

fn make_resolve_native_stack_rooted(
    interp: &mut Interpreter,
    stack: &HoltStack,
    promise: JsPromiseHandle,
    context: Option<ExecutionContext>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    promise_native_stack(
        interp,
        stack,
        "",
        1,
        smallvec![Value::promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            resolve_native_body(
                ctx,
                args,
                settle_native_promise(captures),
                &captured_context,
            )
        },
    )
}

fn make_resolve_native_native_rooted(
    ctx: &mut NativeCtx<'_>,
    promise: JsPromiseHandle,
    context: Option<ExecutionContext>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    promise_native_ctx(
        ctx,
        "",
        1,
        smallvec![Value::promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            resolve_native_body(
                ctx,
                args,
                settle_native_promise(captures),
                &captured_context,
            )
        },
    )
}

fn resolve_native_body(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    promise: JsPromiseHandle,
    captured_context: &Option<ExecutionContext>,
) -> Result<Value, NativeError> {
    let context = ctx
        .execution_context()
        .cloned()
        .or_else(|| captured_context.clone());
    let pending = {
        let interp = ctx.interp_mut();
        matches!(promise.state(interp.gc_heap()), PromiseState::Pending)
    };
    if !pending {
        return Ok(Value::undefined());
    }

    let value = args.first().cloned().unwrap_or(Value::undefined());
    if let Some(inner) = value.as_promise() {
        let value_root = Value::promise(inner);
        let (on_fulfill, on_reject) =
            make_resolve_adoption_handlers_native_rooted(ctx, promise, &[&value_root], &[args])?;
        let interp = ctx.interp_mut();
        attach_then(interp, context, &inner, Some(on_fulfill), Some(on_reject));
        return Ok(Value::undefined());
    }

    // §27.2.1.3.2 Promise Resolve Functions steps 8-13 — any object
    // with a callable `then` is a thenable: read `then` (firing an
    // accessor and rejecting on its throw), then schedule a
    // PromiseResolveThenableJob that calls `then(resolve, reject)`
    // with this promise's resolving functions. A native promise
    // takes the faster `attach_then` path above; this branch covers
    // user-defined thenables (`Promise.resolve(obj)`, `await obj`).
    if value.is_object_type()
        && let Some(exec) = context.clone()
    {
        let then = {
            let interp = ctx.interp_mut();
            match get_property_runtime(interp, &exec, value, "then", "Promise resolve") {
                Ok(then) => then,
                Err(err) => {
                    let reason = native_error_rejection_value_preserving_throw(interp, err);
                    let jobs = promise.reject(interp.gc_heap_mut(), reason);
                    drain_jobs(interp, jobs);
                    return Ok(Value::undefined());
                }
            }
        };
        if crate::is_callable_value(&then) {
            let value_root = value;
            let then_root = then;
            let (on_fulfill, on_reject) = make_resolve_adoption_handlers_native_rooted(
                ctx,
                promise,
                &[&value_root, &then_root],
                &[args],
            )?;
            let job =
                make_resolve_thenable_job(ctx, value, then, on_fulfill, on_reject, exec.clone())?;
            ctx.interp_mut().microtasks_mut().enqueue(crate::Microtask {
                callee: job,
                this_value: Value::undefined(),
                args: SmallVec::new(),
                context: Some(exec),
                result_capability: None,
                kind: crate::microtask::MicrotaskKind::Call,
            });
            return Ok(Value::undefined());
        }
    }

    let interp = ctx.interp_mut();
    let jobs = promise.fulfill(interp.gc_heap_mut(), value);
    drain_jobs(interp, jobs);
    Ok(Value::undefined())
}

/// Resolve a promise from interpreter code without requiring a [`NativeCtx`].
///
/// This is the same core path needed by async function frame completion:
/// returning a native promise must adopt that promise instead of fulfilling the
/// async function's promise with the promise object itself.
pub(crate) fn resolve_promise_from_interpreter(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    value: Value,
    context: Option<ExecutionContext>,
) -> Result<(), crate::VmError> {
    if !matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
        return Ok(());
    }

    if let Some(inner) = value.as_promise() {
        let value_root = Value::promise(inner);
        let (on_fulfill, on_reject) =
            make_resolve_adoption_handlers_runtime_rooted(interp, promise, &[&value_root], &[])
                .map_err(crate::oom_to_vm)?;
        attach_then(interp, context, &inner, Some(on_fulfill), Some(on_reject));
        return Ok(());
    }

    let jobs = promise.fulfill(interp.gc_heap_mut(), value);
    drain_jobs(interp, jobs);
    Ok(())
}

/// §27.2.1.3.2 PromiseResolveThenableJob — a native that calls
/// `then.call(thenable, resolve, reject)` and, if that call throws,
/// rejects the promise with the abrupt completion's value. Enqueued
/// as a microtask so the thenable's `then` runs on a later tick, per
/// spec, rather than synchronously during resolution.
fn make_resolve_thenable_job(
    ctx: &mut NativeCtx<'_>,
    thenable: Value,
    then: Value,
    on_fulfill: Value,
    on_reject: Value,
    exec: ExecutionContext,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![thenable, then, on_fulfill, on_reject];
    native_value_with_captures_native_rooted(
        ctx,
        "PromiseResolveThenableJob",
        captures,
        &[&thenable, &then, &on_fulfill, &on_reject],
        &[],
        move |ctx, _args, captures| {
            let thenable = captures[0];
            let then = captures[1];
            let on_fulfill = captures[2];
            let on_reject = captures[3];
            let interp = ctx.interp_mut();
            match interp.run_callable_sync(&exec, &then, thenable, smallvec![on_fulfill, on_reject])
            {
                Ok(_) => Ok(Value::undefined()),
                Err(err) => {
                    // §27.2.1.3.2 — an abrupt `then` call rejects the
                    // promise with the thrown value, preserving its
                    // identity (a user `throw obj` keeps `obj`).
                    let reason = interp
                        .take_pending_uncaught_throw()
                        .unwrap_or_else(|| crate::error_ops::vm_err_to_value(interp, &err));
                    let _ = interp.run_callable_sync(
                        &exec,
                        &on_reject,
                        Value::undefined(),
                        smallvec![reason],
                    );
                    Ok(Value::undefined())
                }
            }
        },
    )
    .map_err(|_| oom_native("PromiseResolveThenableJob"))
}

fn make_resolve_adoption_handlers_runtime_rooted(
    interp: &mut Interpreter,
    resolver: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<(Value, Value), otter_gc::OutOfMemory> {
    let resolver_value = Value::promise(resolver);
    let mut fulfill_roots = Vec::with_capacity(value_roots.len() + 1);
    fulfill_roots.extend_from_slice(value_roots);
    fulfill_roots.push(&resolver_value);
    let on_fulfill = promise_native_runtime(
        interp,
        "Promise resolve adopt fulfill",
        1,
        smallvec![resolver_value],
        &fulfill_roots,
        slice_roots,
        move |ctx, args, captures| {
            let resolver = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            let v = args.first().cloned().unwrap_or(Value::undefined());
            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
            drain_jobs(interp, jobs);
            Ok(Value::undefined())
        },
    )?;

    let resolver_reject_value = Value::promise(resolver);
    let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
    reject_roots.extend_from_slice(value_roots);
    reject_roots.push(&resolver_reject_value);
    reject_roots.push(&on_fulfill);
    let on_reject = promise_native_runtime(
        interp,
        "Promise resolve adopt reject",
        1,
        smallvec![resolver_reject_value],
        &reject_roots,
        slice_roots,
        move |ctx, args, captures| {
            let resolver = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            let reason = args.first().cloned().unwrap_or(Value::undefined());
            let jobs = resolver.reject(interp.gc_heap_mut(), reason);
            drain_jobs(interp, jobs);
            Ok(Value::undefined())
        },
    )?;

    Ok((on_fulfill, on_reject))
}

fn make_resolve_adoption_handlers_native_rooted(
    ctx: &mut NativeCtx<'_>,
    resolver: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<(Value, Value), otter_gc::OutOfMemory> {
    let resolver_value = Value::promise(resolver);
    let mut fulfill_roots = Vec::with_capacity(value_roots.len() + 1);
    fulfill_roots.extend_from_slice(value_roots);
    fulfill_roots.push(&resolver_value);
    let on_fulfill = native_value_with_captures_native_rooted(
        ctx,
        "Promise resolve adopt fulfill",
        smallvec![resolver_value],
        &fulfill_roots,
        slice_roots,
        move |ctx, args, captures| {
            let resolver = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            let v = args.first().cloned().unwrap_or(Value::undefined());
            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
            drain_jobs(interp, jobs);
            Ok(Value::undefined())
        },
    )?;

    let resolver_reject_value = Value::promise(resolver);
    let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
    reject_roots.extend_from_slice(value_roots);
    reject_roots.push(&resolver_reject_value);
    reject_roots.push(&on_fulfill);
    let on_reject = native_value_with_captures_native_rooted(
        ctx,
        "Promise resolve adopt reject",
        smallvec![resolver_reject_value],
        &reject_roots,
        slice_roots,
        move |ctx, args, captures| {
            let resolver = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            let reason = args.first().cloned().unwrap_or(Value::undefined());
            let jobs = resolver.reject(interp.gc_heap_mut(), reason);
            drain_jobs(interp, jobs);
            Ok(Value::undefined())
        },
    )?;

    Ok((on_fulfill, on_reject))
}

fn make_reject_native_runtime_rooted(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_runtime(
        interp,
        "",
        1,
        smallvec![Value::promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            let promise = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let reason = args.first().cloned().unwrap_or(Value::undefined());
                let jobs = promise.reject(interp.gc_heap_mut(), reason);
                drain_jobs(interp, jobs);
            }
            Ok(Value::undefined())
        },
    )
}

fn make_reject_native_stack_rooted(
    interp: &mut Interpreter,
    stack: &HoltStack,
    promise: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_stack(
        interp,
        stack,
        "",
        1,
        smallvec![Value::promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            let promise = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let reason = args.first().cloned().unwrap_or(Value::undefined());
                let jobs = promise.reject(interp.gc_heap_mut(), reason);
                drain_jobs(interp, jobs);
            }
            Ok(Value::undefined())
        },
    )
}

fn make_reject_native_native_rooted(
    ctx: &mut NativeCtx<'_>,
    promise: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_ctx(
        ctx,
        "",
        1,
        smallvec![Value::promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, captures| {
            let promise = settle_native_promise(captures);
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let reason = args.first().cloned().unwrap_or(Value::undefined());
                let jobs = promise.reject(interp.gc_heap_mut(), reason);
                drain_jobs(interp, jobs);
            }
            Ok(Value::undefined())
        },
    )
}

fn drain_jobs(interp: &mut Interpreter, jobs: PromiseSettleJobs) {
    for j in jobs.jobs {
        interp.microtasks_mut().enqueue(j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumberValue;
    use crate::runtime_cx::NativeCallInfo;
    use otter_bytecode::{BytecodeModule, SourceKind};

    /// Minimal execution context for paths that invoke a capability's
    /// native resolve/reject closure — `call_capability_function` runs
    /// it through `run_callable_sync`, which requires a `Some(context)`
    /// even when the body needs no module functions.
    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "promise-dispatch-test".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn aggregate_error_runtime_builder_uses_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let registry = interp.error_classes_clone();
        let errors = vec![Value::number_i32(1)];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = make_aggregate_error_runtime_rooted(&mut interp, &registry, errors)
            .expect("aggregate error");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise.any AggregateError runtime path should allocate object and errors array in young space"
        );
        let Some(obj) = result.as_object() else {
            panic!("expected object");
        };
        assert!(crate::object::get(obj, interp.gc_heap(), "errors").is_some_and(|v| v.is_array()));
    }

    #[test]
    fn aggregate_error_native_builder_uses_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let registry = interp.error_classes_clone();
        let errors = vec![Value::number_i32(2)];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = {
            let mut ctx = NativeCtx::new(&mut interp);
            make_aggregate_error_native_rooted(&mut ctx, &registry, errors)
                .expect("aggregate error")
        };

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise.any AggregateError native path should allocate object and errors array in young space"
        );
        let Some(obj) = result.as_object() else {
            panic!("expected object");
        };
        assert!(crate::object::get(obj, interp.gc_heap(), "errors").is_some_and(|v| v.is_array()));
    }

    #[test]
    fn promise_static_resolve_uses_runtime_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let args = [Value::number_i32(7)];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let constructor = Value::undefined();
        let promise_value = static_resolve(&mut interp, Some(empty_context()), constructor, &args)
            .expect("Promise.resolve");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise.resolve should allocate non-promise results through runtime-rooted young allocation"
        );
        let Some(promise) = promise_value.as_promise() else {
            panic!("expected promise");
        };
        let state = promise.state(interp.gc_heap());
        match state {
            PromiseState::Fulfilled(v) => assert!(v.is_number()),
            _ => panic!("expected fulfilled with number, got {state:?}"),
        }
    }

    #[test]
    fn promise_capability_uses_runtime_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let cap = PromiseBuilder::new()
            .capability_runtime_rooted(&mut interp, &[], &[])
            .expect("capability");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise capability creation should allocate promise and closures through runtime roots"
        );
        assert!(cap.promise.is_promise());
        assert!(cap.resolve.is_native_function());
        assert!(cap.reject.is_native_function());
    }

    #[test]
    fn promise_constructor_builder_uses_native_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        let executor = Value::number_i32(17);
        let args = vec![executor];

        let (handle, resolve, reject) = {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(
                    Value::number(NumberValue::from_i32(1)),
                    Some(Value::number(NumberValue::from_i32(2))),
                ),
            );
            PromiseBuilder::new()
                .construct_native_rooted(&mut ctx, &[&executor], &[args.as_slice()])
                .expect("native-rooted promise constructor plumbing")
        };

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "native Promise constructor plumbing should allocate through root-aware young allocation"
        );
        assert!(matches!(
            handle.state(interp.gc_heap()),
            PromiseState::Pending
        ));
        assert!(interp.is_callable_runtime(&resolve));
        assert!(interp.is_callable_runtime(&reject));
    }
}

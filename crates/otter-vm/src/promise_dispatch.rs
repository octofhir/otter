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

use crate::activation_stack::ActivationStack;
use crate::error_classes::{ErrorClassRegistry, ErrorKind};
use crate::execution_context::ExecutionContext;
use crate::native_function::{
    NativeError, native_value_with_captures_unchecked_with_roots, traced_native_value_with_length,
};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
    PromiseThenOutcome,
};
use crate::{Interpreter, Local, NativeCtx, Value};
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

#[derive(Clone, Copy)]
struct CapabilityHandles<'scope> {
    promise: Local<'scope>,
    resolve: Local<'scope>,
    reject: Local<'scope>,
}

impl<'scope> CapabilityHandles<'scope> {
    fn park(
        interp: &mut Interpreter,
        scope: &'scope crate::handles::HandleScope,
        capability: &PromiseCapability,
    ) -> Self {
        Self {
            promise: interp.scoped_value(scope, capability.promise),
            resolve: interp.scoped_value(scope, capability.resolve),
            reject: interp.scoped_value(scope, capability.reject),
        }
    }

    fn current(self, interp: &Interpreter, context: Option<ExecutionContext>) -> PromiseCapability {
        PromiseCapability {
            promise: interp.escape_scoped(self.promise),
            resolve: interp.escape_scoped(self.resolve),
            reject: interp.escape_scoped(self.reject),
            context,
        }
    }

    fn refresh(self, interp: &Interpreter, capability: &mut PromiseCapability) {
        capability.promise = interp.escape_scoped(self.promise);
        capability.resolve = interp.escape_scoped(self.resolve);
        capability.reject = interp.escape_scoped(self.reject);
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
    fn new_scoped<'scope>(
        interp: &mut Interpreter,
        scope: &'scope crate::handles::HandleScope,
    ) -> Result<(Arc<Self>, Local<'scope>), NativeError> {
        let values = interp
            .scoped_array(scope, 0)
            .map_err(|_| oom_native("Promise combinator"))?;
        let slots = Arc::new(Self {
            values: Cell::new(interp.escape_scoped(values)),
            keys: None,
            remaining: Cell::new(1),
        });
        Ok((slots, values))
    }

    fn new_keyed_scoped<'scope>(
        interp: &mut Interpreter,
        scope: &'scope crate::handles::HandleScope,
    ) -> Result<(Arc<Self>, Local<'scope>, Local<'scope>), NativeError> {
        let values = interp
            .scoped_array(scope, 0)
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        let keys = interp
            .scoped_array(scope, 0)
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        let slots = Arc::new(Self {
            values: Cell::new(interp.escape_scoped(values)),
            keys: Some(Cell::new(interp.escape_scoped(keys))),
            remaining: Cell::new(1),
        });
        Ok((slots, values, keys))
    }

    fn refresh_scoped(&self, interp: &Interpreter, values: Local<'_>, keys: Option<Local<'_>>) {
        self.values.set(interp.escape_scoped(values));
        if let (Some(slot), Some(keys)) = (&self.keys, keys) {
            slot.set(interp.escape_scoped(keys));
        }
    }

    fn reserve_slot_scoped(
        &self,
        interp: &mut Interpreter,
        values: Local<'_>,
    ) -> Result<usize, NativeError> {
        self.refresh_scoped(interp, values, None);
        let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let len = crate::array::push_with_roots(
            self.values_array(),
            interp.gc_heap_mut(),
            Value::hole(),
            &mut no_extra_roots,
        )
        .map_err(|_| oom_native("Promise combinator"))?;
        self.refresh_scoped(interp, values, None);
        self.remaining.set(self.remaining.get().saturating_add(1));
        Ok(len - 1)
    }

    fn reserve_keyed_slot_scoped(
        &self,
        interp: &mut Interpreter,
        values: Local<'_>,
        keys: Local<'_>,
        key: Local<'_>,
    ) -> Result<usize, NativeError> {
        self.refresh_scoped(interp, values, Some(keys));
        let key = interp.escape_scoped(key);
        let Some(keys_array) = self.keys_array() else {
            return Err(NativeError::TypeError {
                name: "Promise keyed combinator",
                reason: "missing keyed slots".to_string(),
            });
        };
        let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        crate::array::push_with_roots(keys_array, interp.gc_heap_mut(), key, &mut no_extra_roots)
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        self.refresh_scoped(interp, values, Some(keys));
        let len = crate::array::push_with_roots(
            self.values_array(),
            interp.gc_heap_mut(),
            Value::hole(),
            &mut no_extra_roots,
        )
        .map_err(|_| oom_native("Promise keyed combinator"))?;
        self.refresh_scoped(interp, values, Some(keys));
        self.remaining.set(self.remaining.get().saturating_add(1));
        Ok(len - 1)
    }

    fn materialize_array_scoped<'scope>(
        &self,
        interp: &mut Interpreter,
        scope: &'scope crate::handles::HandleScope,
        values: Local<'scope>,
        name: &'static str,
    ) -> Result<Local<'scope>, NativeError> {
        self.refresh_scoped(interp, values, None);
        let elements = self
            .collect_values(interp.gc_heap())
            .into_iter()
            .map(|value| interp.scoped_value(scope, value))
            .collect::<Vec<_>>();
        let result = interp
            .scoped_array(scope, elements.len())
            .map_err(|_| oom_native(name))?;
        for (index, value) in elements.into_iter().enumerate() {
            interp
                .scoped_set_index(scope, result, index, value)
                .map_err(|_| oom_native(name))?;
        }
        self.refresh_scoped(interp, values, None);
        Ok(result)
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

    fn fill(&self, heap: &mut otter_gc::GcHeap, index: usize, value: Value) -> bool {
        let did_fill = crate::array::with_elements_rewrite(self.values_array(), heap, |elements| {
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
        stack: &ActivationStack,
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
        stack: &ActivationStack,
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
        stack: &ActivationStack,
        reason: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        let promise = JsPromiseHandle::rejected_with_roots(
            interp.gc_heap_mut(),
            reason,
            &mut external_visit,
        )?;
        interp.note_born_rejection(promise);
        Ok(promise)
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
        let promise = JsPromiseHandle::rejected_with_roots(
            interp.gc_heap_mut(),
            reason,
            &mut external_visit,
        )?;
        interp.note_born_rejection(promise);
        Ok(promise)
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
        let root_base = interp.json_root_push(Value::promise(promise));
        let result = (|| {
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives allocation");
            let resolve = make_resolve_native_runtime_rooted(
                interp,
                promise,
                self.context.clone(),
                value_roots,
                slice_roots,
            )?;
            let resolve_root = interp.json_root_push(resolve);
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives resolve allocation");
            let reject =
                make_reject_native_runtime_rooted(interp, promise, value_roots, slice_roots)?;
            Ok((
                interp
                    .json_root_get(root_base)
                    .as_promise()
                    .expect("rooted promise survives reject allocation"),
                interp.json_root_get(resolve_root),
                reject,
            ))
        })();
        interp.json_root_pop_to(root_base);
        result
    }

    pub(crate) fn construct_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &ActivationStack,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_stack_rooted(interp, stack, value_roots, slice_roots)?;
        let root_base = interp.json_root_push(Value::promise(promise));
        let result = (|| {
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives allocation");
            let resolve = make_resolve_native_stack_rooted(
                interp,
                stack,
                promise,
                self.context.clone(),
                value_roots,
                slice_roots,
            )?;
            let resolve_root = interp.json_root_push(resolve);
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives resolve allocation");
            let reject =
                make_reject_native_stack_rooted(interp, stack, promise, value_roots, slice_roots)?;
            Ok((
                interp
                    .json_root_get(root_base)
                    .as_promise()
                    .expect("rooted promise survives reject allocation"),
                interp.json_root_get(resolve_root),
                reject,
            ))
        })();
        interp.json_root_pop_to(root_base);
        result
    }

    pub(crate) fn construct_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_native_rooted(ctx, value_roots, slice_roots)?;
        let root_base = ctx.interp_mut().json_root_push(Value::promise(promise));
        let result = (|| {
            let promise = ctx
                .interp_mut()
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives allocation");
            let resolve = make_resolve_native_native_rooted(
                ctx,
                promise,
                self.context.clone(),
                value_roots,
                slice_roots,
            )?;
            let resolve_root = ctx.interp_mut().json_root_push(resolve);
            let promise = ctx
                .interp_mut()
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives resolve allocation");
            let reject = make_reject_native_native_rooted(ctx, promise, value_roots, slice_roots)?;
            Ok((
                ctx.interp_mut()
                    .json_root_get(root_base)
                    .as_promise()
                    .expect("rooted promise survives reject allocation"),
                ctx.interp_mut().json_root_get(resolve_root),
                reject,
            ))
        })();
        ctx.interp_mut().json_root_pop_to(root_base);
        result
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
        stack: &ActivationStack,
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
    stack: &ActivationStack,
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
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    traced_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        trace,
        &mut no_extra_roots,
        call,
    )
}

/// Dispatch a `Promise.<method>(args...)` static call. Routes
/// the typed [`PromiseMethod`] emitted by the compiler.
pub fn statics_call(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
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
            M::Resolve => static_resolve_generic(interp, stack, context, constructor, args),
            M::Reject => static_reject_generic(interp, stack, context, constructor, args),
            M::All => static_all_generic(interp, stack, context, constructor, args),
            M::Race => static_race_generic(interp, stack, context, constructor, args),
            M::AllSettled => static_all_settled_generic(interp, stack, context, constructor, args),
            M::Any => static_any_generic(interp, stack, context, constructor, args),
            M::WithResolvers => static_with_resolvers_generic(interp, stack, context, constructor),
            M::Try => static_try_generic(interp, stack, context, constructor, args),
            M::AllKeyed => static_all_keyed_generic(
                interp,
                stack,
                context,
                constructor,
                args,
                KeyedVariant::All,
            ),
            M::AllSettledKeyed => static_all_keyed_generic(
                interp,
                stack,
                context,
                constructor,
                args,
                KeyedVariant::AllSettled,
            ),
        };
    }
    match method {
        M::Resolve => static_resolve(interp, stack, context, constructor, args),
        M::Reject => Ok(Value::promise(static_reject(interp, args)?)),
        M::All => static_all_generic(interp, stack, context, constructor, args),
        M::Race => static_race_generic(interp, stack, context, constructor, args),
        M::AllSettled => static_all_settled_generic(interp, stack, context, constructor, args),
        M::Any => static_any_generic(interp, stack, context, constructor, args),
        M::WithResolvers => static_with_resolvers(interp, context),
        M::Try => static_try_generic(interp, stack, context, constructor, args),
        M::AllKeyed => {
            static_all_keyed_generic(interp, stack, context, constructor, args, KeyedVariant::All)
        }
        M::AllSettledKeyed => static_all_keyed_generic(
            interp,
            stack,
            context,
            constructor,
            args,
            KeyedVariant::AllSettled,
        ),
    }
}

/// Dispatch a `promise.<name>(args...)` instance-method call.
/// Branches on `then` / `catch` / `finally`; everything else
/// surfaces as `UnknownIntrinsic` upstream.
pub fn prototype_call(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    name: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    match name {
        "then" => method_then(interp, stack, context, promise, args),
        "catch" => Ok(method_catch(interp, context, promise, args)),
        "finally" => method_finally_value(
            interp,
            stack,
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
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let on_fulfilled = scope.value(on_fulfilled);
        let on_rejected = scope.value(on_rejected);
        let receiver_raw = scope.raw(receiver);
        let on_fulfilled_raw = scope.raw(on_fulfilled);
        let on_rejected_raw = scope.raw(on_rejected);
        let result = scope.with_turn_parts(|interp, stack| {
            invoke_then_interp(
                interp,
                stack,
                &exec,
                receiver_raw,
                on_fulfilled_raw,
                on_rejected_raw,
            )
        })?;
        let result = scope.value(result);
        Ok(scope.finish(result))
    })
}

fn invoke_then_interp(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    exec: &ExecutionContext,
    receiver: Value,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.prototype";
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let on_fulfilled = interp.scoped_value(scope, on_fulfilled);
        let on_rejected = interp.scoped_value(scope, on_rejected);
        let receiver_raw = interp.escape_scoped(receiver);
        let then = get_callable_property(interp, stack, exec, receiver_raw, "then", NAME)?;
        let then = interp.scoped_value(scope, then);
        let then = interp.escape_scoped(then);
        let receiver = interp.escape_scoped(receiver);
        let on_fulfilled = interp.escape_scoped(on_fulfilled);
        let on_rejected = interp.escape_scoped(on_rejected);
        interp
            .run_callable_sync_rooted(
                stack,
                exec,
                &then,
                receiver,
                smallvec![on_fulfilled, on_rejected],
            )
            .map_err(|err| promise_vm_error(interp, NAME, err))
    })
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
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let on_finally = scope.value(on_finally);
        let receiver_raw = scope.raw(receiver);
        let on_finally_raw = scope.raw(on_finally);
        let result = scope.with_turn_parts(|interp, stack| {
            method_finally_value(interp, stack, context, receiver_raw, on_finally_raw)
        })?;
        let result = scope.value(result);
        Ok(scope.finish(result))
    })
}

fn method_finally_value(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
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
    let default_ctor = builtin_promise_constructor(interp)?;
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let on_finally = interp.scoped_value(scope, on_finally);
        let default_ctor = interp.scoped_value(scope, default_ctor);
        if !crate::is_callable_value(&interp.escape_scoped(on_finally)) {
            let receiver = interp.escape_scoped(receiver);
            let on_finally = interp.escape_scoped(on_finally);
            return invoke_then_interp(interp, stack, &exec, receiver, on_finally, on_finally);
        }
        let receiver_raw = interp.escape_scoped(receiver);
        let default_ctor_raw = interp.escape_scoped(default_ctor);
        let constructor = species_constructor_runtime(
            interp,
            stack,
            &exec,
            &receiver_raw,
            &default_ctor_raw,
            NAME,
        )?;
        let constructor = interp.scoped_value(scope, constructor);
        let then_finally = make_then_finally(
            interp,
            &exec,
            interp.escape_scoped(constructor),
            interp.escape_scoped(on_finally),
        )?;
        let then_finally = interp.scoped_value(scope, then_finally);
        let catch_finally = make_catch_finally(
            interp,
            &exec,
            interp.escape_scoped(constructor),
            interp.escape_scoped(on_finally),
        )?;
        let catch_finally = interp.scoped_value(scope, catch_finally);
        invoke_then_interp(
            interp,
            stack,
            &exec,
            interp.escape_scoped(receiver),
            interp.escape_scoped(then_finally),
            interp.escape_scoped(catch_finally),
        )
    })
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
            ctx.scope(|mut scope| {
                let c = scope.value(captures[0]);
                let on_finally = scope.value(captures[1]);
                let value = scope.argument(args, 0);
                let undefined = scope.undefined();
                let result = scope.call(on_finally, undefined, &[])?;
                let c_raw = scope.raw(c);
                let resolve_fn = scope.with_turn_parts(|interp, stack| {
                    get_promise_resolve(interp, stack, &exec_for_call, &c_raw)
                })?;
                let resolve_fn = scope.value(resolve_fn);
                let resolved = scope.call(resolve_fn, c, &[result])?;
                let value_raw = scope.raw(value);
                let value_thunk = make_value_thunk(scope.context(), value_raw)?;
                let value_thunk = scope.value(value_thunk);
                let resolved_raw = scope.raw(resolved);
                let value_thunk_raw = scope.raw(value_thunk);
                let result = invoke_then(
                    scope.context(),
                    resolved_raw,
                    value_thunk_raw,
                    Value::undefined(),
                )?;
                let result = scope.value(result);
                Ok(scope.finish(result))
            })
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
            ctx.scope(|mut scope| {
                let c = scope.value(captures[0]);
                let on_finally = scope.value(captures[1]);
                let reason = scope.argument(args, 0);
                let undefined = scope.undefined();
                let result = scope.call(on_finally, undefined, &[])?;
                let c_raw = scope.raw(c);
                let resolve_fn = scope.with_turn_parts(|interp, stack| {
                    get_promise_resolve(interp, stack, &exec_for_call, &c_raw)
                })?;
                let resolve_fn = scope.value(resolve_fn);
                let resolved = scope.call(resolve_fn, c, &[result])?;
                let reason_raw = scope.raw(reason);
                let thrower = make_thrower(scope.context(), reason_raw)?;
                let thrower = scope.value(thrower);
                let resolved_raw = scope.raw(resolved);
                let thrower_raw = scope.raw(thrower);
                let result = invoke_then(
                    scope.context(),
                    resolved_raw,
                    thrower_raw,
                    Value::undefined(),
                )?;
                let result = scope.value(result);
                Ok(scope.finish(result))
            })
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
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: &mut Value,
) -> Result<PromiseCapability, NativeError> {
    let exec = context.ok_or_else(|| NativeError::TypeError {
        name: "Promise",
        reason: "missing execution context".to_string(),
    })?;
    if !crate::is_constructor_runtime(constructor, &exec, interp.gc_heap()) {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "this value is not a constructor".to_string(),
        });
    }
    interp.with_handle_scope(|interp, scope| {
        let constructor_handle = interp.scoped_value(scope, *constructor);
        let state = CapabilityExecutorState::new();
        let trace_state = {
            let state = state.clone();
            Arc::new(move |visitor: &mut SlotVisitor<'_>| state.trace(visitor))
        };
        let state_for_call = state.clone();
        let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        // §27.2.1.5.1 — the GetCapabilitiesExecutor has length 2.
        let executor = crate::native_function::traced_native_value_with_length(
            interp.gc_heap_mut(),
            "",
            2,
            SmallVec::new(),
            trace_state,
            &mut no_extra_roots,
            move |_ctx, args, _captures| state_for_call.call(args),
        )?;
        let executor = interp.scoped_value(scope, executor);
        let constructor_raw = interp.escape_scoped(constructor_handle);
        let executor_raw = interp.escape_scoped(executor);
        let promise = interp
            .run_construct_sync_rooted(
                stack,
                &exec,
                &constructor_raw,
                constructor_raw,
                smallvec![executor_raw],
            )
            .map_err(|err| promise_vm_error(interp, "Promise", err))?;
        let promise = interp.scoped_value(scope, promise);
        *constructor = interp.escape_scoped(constructor_handle);
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
            promise: interp.escape_scoped(promise),
            resolve,
            reject,
            context: Some(exec),
        })
    })
}

fn call_capability_function(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    use_reject: bool,
    value: Value,
) -> Result<(), NativeError> {
    let exec = cap.context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise",
        reason: "missing execution context".to_string(),
    })?;
    interp.with_handle_scope(|interp, scope| {
        let handles = CapabilityHandles::park(interp, scope, cap);
        let value = interp.scoped_value(scope, value);
        let function = if use_reject {
            handles.reject
        } else {
            handles.resolve
        };
        let function = interp.escape_scoped(function);
        let value = interp.escape_scoped(value);
        let result = interp
            .run_callable_sync_rooted(
                stack,
                &exec,
                &function,
                Value::undefined(),
                smallvec![value],
            )
            .map_err(|err| promise_vm_error(interp, "Promise", err));
        handles.refresh(interp, cap);
        result.map(|_| ())
    })
}

fn call_capability_resolve(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    value: Value,
) -> Result<(), NativeError> {
    call_capability_function(interp, stack, cap, false, value)
}

fn call_capability_reject(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    reason: Value,
) -> Result<(), NativeError> {
    call_capability_function(interp, stack, cap, true, reason)
}

fn call_capability_resolve_native(
    ctx: &mut NativeCtx<'_>,
    cap: &PromiseCapability,
    value: Value,
) -> Result<(), NativeError> {
    ctx.scope(|mut scope| {
        let promise = scope.value(cap.promise);
        let resolve = scope.value(cap.resolve);
        let reject = scope.value(cap.reject);
        let value = scope.value(value);
        let mut live_cap = PromiseCapability {
            promise: scope.raw(promise),
            resolve: scope.raw(resolve),
            reject: scope.raw(reject),
            context: cap.context.clone(),
        };
        let value = scope.raw(value);
        scope.with_turn_parts(|interp, stack| {
            call_capability_resolve(interp, stack, &mut live_cap, value)
        })
    })
}

fn call_capability_reject_native(
    ctx: &mut NativeCtx<'_>,
    cap: &PromiseCapability,
    reason: Value,
) -> Result<(), NativeError> {
    ctx.scope(|mut scope| {
        let promise = scope.value(cap.promise);
        let resolve = scope.value(cap.resolve);
        let reject = scope.value(cap.reject);
        let reason = scope.value(reason);
        let mut live_cap = PromiseCapability {
            promise: scope.raw(promise),
            resolve: scope.raw(resolve),
            reject: scope.raw(reject),
            context: cap.context.clone(),
        };
        let reason = scope.raw(reason);
        scope.with_turn_parts(|interp, stack| {
            call_capability_reject(interp, stack, &mut live_cap, reason)
        })
    })
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
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    err: NativeError,
) -> Result<Value, NativeError> {
    let reason = native_error_rejection_value_preserving_throw(interp, err);
    call_capability_reject(interp, stack, cap, reason)?;
    Ok(cap.promise)
}

/// Read an own/inherited property by string key without callability check.
///
/// Invokes any accessor `[[Get]]` exactly once per §10.1.8.1.
fn get_property_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    receiver: Value,
    key: &'static str,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let receiver_raw = interp.escape_scoped(receiver);
        let property_key = crate::VmPropertyKey::String(key);
        match interp
            .ordinary_get_value(stack, context, receiver_raw, receiver_raw, &property_key, 0)
            .map_err(|err| promise_vm_error(interp, name, err))?
        {
            crate::VmGetOutcome::Value(value) => Ok(value),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let getter = interp.scoped_value(scope, getter);
                let getter = interp.escape_scoped(getter);
                let receiver = interp.escape_scoped(receiver);
                interp
                    .run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(|err| promise_vm_error(interp, name, err))
            }
        }
    })
}

/// Read an own/inherited property by symbol key without callability check.
fn get_symbol_property_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    receiver: Value,
    sym: crate::symbol::JsSymbol,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let receiver_raw = interp.escape_scoped(receiver);
        let property_key = crate::VmPropertyKey::Symbol(sym);
        match interp
            .ordinary_get_value(stack, context, receiver_raw, receiver_raw, &property_key, 0)
            .map_err(|err| promise_vm_error(interp, name, err))?
        {
            crate::VmGetOutcome::Value(value) => Ok(value),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let getter = interp.scoped_value(scope, getter);
                let getter = interp.escape_scoped(getter);
                let receiver = interp.escape_scoped(receiver);
                interp
                    .run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(|err| promise_vm_error(interp, name, err))
            }
        }
    })
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
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    obj: &Value,
    default_ctor: &Value,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let obj = interp.scoped_value(scope, *obj);
        let default_ctor = interp.scoped_value(scope, *default_ctor);
        let obj_raw = interp.escape_scoped(obj);
        let c = get_property_runtime(interp, stack, context, obj_raw, "constructor", name)?;
        let c = interp.scoped_value(scope, c);
        let c_raw = interp.escape_scoped(c);
        if c_raw.is_undefined() {
            return Ok(interp.escape_scoped(default_ctor));
        }
        if !c_raw.is_object_type() {
            return Err(NativeError::TypeError {
                name,
                reason: "constructor is not an Object".to_string(),
            });
        }
        let species_sym = interp
            .well_known_symbols()
            .get(crate::symbol::WellKnown::Species);
        let s = get_symbol_property_runtime(interp, stack, context, c_raw, species_sym, name)?;
        let s = interp.scoped_value(scope, s);
        let s_raw = interp.escape_scoped(s);
        if s_raw.is_undefined() || s_raw.is_null() {
            return Ok(interp.escape_scoped(c));
        }
        if crate::is_constructor_runtime(&s_raw, context, interp.gc_heap()) {
            return Ok(s_raw);
        }
        Err(NativeError::TypeError {
            name,
            reason: "Symbol.species is not a constructor".to_string(),
        })
    })
}

fn get_callable_property(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    receiver: Value,
    key: &'static str,
    name: &'static str,
) -> Result<Value, NativeError> {
    let value = get_property_runtime(interp, stack, context, receiver, key, name)?;
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
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    constructor: &Value,
) -> Result<Value, NativeError> {
    get_callable_property(
        interp,
        stack,
        context,
        *constructor,
        "resolve",
        "Promise.resolve",
    )
}

fn call_promise_resolve(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    resolve_fn: &Value,
    constructor: &Value,
    value: Value,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let resolve_fn = interp.scoped_value(scope, *resolve_fn);
        let constructor = interp.scoped_value(scope, *constructor);
        let value = interp.scoped_value(scope, value);
        let resolve_fn = interp.escape_scoped(resolve_fn);
        let constructor = interp.escape_scoped(constructor);
        let value = interp.escape_scoped(value);
        interp
            .run_callable_sync_rooted(stack, context, &resolve_fn, constructor, smallvec![value])
            .map_err(|err| promise_vm_error(interp, "Promise.resolve", err))
    })
}

fn attach_then_value(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    promise: Value,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<(), NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let promise = interp.scoped_value(scope, promise);
        let on_fulfilled = interp.scoped_value(scope, on_fulfilled);
        let on_rejected = interp.scoped_value(scope, on_rejected);
        let promise_raw = interp.escape_scoped(promise);
        let then = get_callable_property(
            interp,
            stack,
            context,
            promise_raw,
            "then",
            "Promise combinator",
        )?;
        let then = interp.scoped_value(scope, then);
        let then = interp.escape_scoped(then);
        let promise = interp.escape_scoped(promise);
        let on_fulfilled = interp.escape_scoped(on_fulfilled);
        let on_rejected = interp.escape_scoped(on_rejected);
        interp
            .run_callable_sync_rooted(
                stack,
                context,
                &then,
                promise,
                smallvec![on_fulfilled, on_rejected],
            )
            .map_err(|err| promise_vm_error(interp, "Promise combinator", err))?;
        Ok(())
    })
}

fn static_resolve(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let value = interp.scoped_value(scope, value);
        let constructor = interp.scoped_value(scope, constructor);
        if interp.escape_scoped(value).is_promise() {
            if let Some(exec) = context.as_ref() {
                let value_raw = interp.escape_scoped(value);
                let value_constructor = get_property_runtime(
                    interp,
                    stack,
                    exec,
                    value_raw,
                    "constructor",
                    "Promise.resolve",
                )?;
                let value_constructor = interp.scoped_value(scope, value_constructor);
                if crate::abstract_ops::same_value(
                    &interp.escape_scoped(value_constructor),
                    &interp.escape_scoped(constructor),
                    interp.gc_heap(),
                ) {
                    return Ok(interp.escape_scoped(value));
                }
            } else {
                return Ok(interp.escape_scoped(value));
            }
        }
        // §27.2.4.7 PromiseResolve — settle a fresh promise through its
        // resolve function rather than fulfilling directly, so a thenable
        // value is adopted instead of becoming the fulfillment value verbatim.
        let cap = PromiseBuilder::with_optional_context(context.clone())
            .capability_runtime_rooted(interp, &[], &[])?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let value = interp.escape_scoped(value);
        let mut cap = cap_handles.current(interp, context.clone());
        call_capability_resolve(interp, stack, &mut cap, value)?;
        Ok(cap_handles.current(interp, context).promise)
    })
}

fn static_reject(interp: &mut Interpreter, args: &[Value]) -> Result<JsPromiseHandle, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::undefined());
    Ok(PromiseBuilder::new().rejected_runtime_rooted(interp, reason, &[], &[args])?)
}

fn static_resolve_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    mut constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let value = interp.scoped_value(scope, value);
        let constructor_handle = interp.scoped_value(scope, constructor);
        constructor = interp.escape_scoped(constructor_handle);
        let cap = new_generic_promise_capability(interp, stack, context.clone(), &mut constructor)?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let value = interp.escape_scoped(value);
        let mut cap = cap_handles.current(interp, context.clone());
        call_capability_resolve(interp, stack, &mut cap, value)?;
        Ok(cap_handles.current(interp, context).promise)
    })
}

fn static_reject_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    mut constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let reason = interp.scoped_value(scope, reason);
        let constructor_handle = interp.scoped_value(scope, constructor);
        constructor = interp.escape_scoped(constructor_handle);
        let cap = new_generic_promise_capability(interp, stack, context.clone(), &mut constructor)?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let reason = interp.escape_scoped(reason);
        let mut cap = cap_handles.current(interp, context.clone());
        call_capability_reject(interp, stack, &mut cap, reason)?;
        Ok(cap_handles.current(interp, context).promise)
    })
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
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    mut constructor: Value,
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
    interp.with_handle_scope(|interp, scope| {
        let constructor_handle = interp.scoped_value(scope, constructor);
        let callback = interp.scoped_value(
            scope,
            args.first().copied().unwrap_or_else(Value::undefined),
        );
        let forwarded = args
            .iter()
            .skip(1)
            .copied()
            .map(|value| interp.scoped_value(scope, value))
            .collect::<Vec<_>>();
        constructor = interp.escape_scoped(constructor_handle);
        let cap =
            new_generic_promise_capability(interp, stack, Some(exec.clone()), &mut constructor)?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let callback = interp.escape_scoped(callback);
        let forwarded: SmallVec<[Value; 8]> = forwarded
            .into_iter()
            .map(|value| interp.escape_scoped(value))
            .collect();
        let call_result =
            interp.run_callable_sync_rooted(stack, &exec, &callback, Value::undefined(), forwarded);
        let mut cap = cap_handles.current(interp, Some(exec.clone()));
        match call_result {
            Ok(value) => call_capability_resolve(interp, stack, &mut cap, value)?,
            Err(crate::VmError::Uncaught) => {
                let reason = crate::error_ops::vm_err_to_value(interp, &crate::VmError::Uncaught);
                call_capability_reject(interp, stack, &mut cap, reason)?;
            }
            Err(other) => {
                let reason = crate::error_ops::vm_err_to_value(interp, &other);
                call_capability_reject(interp, stack, &mut cap, reason)?;
            }
        }
        Ok(cap_handles.current(interp, Some(exec)).promise)
    })
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
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
    variant: KeyedVariant,
) -> Result<Value, NativeError> {
    let name = variant.name();
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let promises = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let constructor = interp.scoped_value(scope, constructor);
        let promises = interp.scoped_value(scope, promises);
        let mut constructor_current = interp.escape_scoped(constructor);
        let cap = new_generic_promise_capability(
            interp,
            stack,
            context.clone(),
            &mut constructor_current,
        )?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        if !interp.escape_scoped(promises).is_object_type() {
            let mut cap = cap_handles.current(interp, context.clone());
            return reject_capability_error(
                interp,
                stack,
                &mut cap,
                NativeError::TypeError {
                    name,
                    reason: "promises argument is not an Object".to_string(),
                },
            );
        }
        let constructor_raw = interp.escape_scoped(constructor);
        let promise_resolve = match get_promise_resolve(interp, stack, &exec, &constructor_raw) {
            Ok(value) => interp.scoped_value(scope, value),
            Err(err) => {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        };
        let promises_raw = interp.escape_scoped(promises);
        let all_keys = match interp.own_property_keys_value(stack, &exec, &promises_raw) {
            Ok(keys) => keys
                .into_iter()
                .map(|key| interp.scoped_value(scope, key))
                .collect::<Vec<_>>(),
            Err(err) => {
                let native = promise_vm_error(interp, name, err);
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let (slots, slots_handle, keys_handle) = PromiseSlots::new_keyed_scoped(interp, scope)?;

        for key in all_keys {
            let key_raw = interp.escape_scoped(key);
            let Some(vm_key) = vm_property_key_from_value(&key_raw, interp.gc_heap()) else {
                continue;
            };
            let promises_raw = interp.escape_scoped(promises);
            let desc = match interp.ordinary_get_own_property_descriptor_value(
                stack,
                &exec,
                promises_raw,
                &vm_key,
                0,
            ) {
                Ok(desc) => desc,
                Err(err) => {
                    let native = promise_vm_error(interp, name, err);
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, native);
                }
            };
            if !desc.as_ref().is_some_and(|desc| desc.enumerable()) {
                continue;
            }
            let promises_raw = interp.escape_scoped(promises);
            let next_value = match keyed_get(interp, stack, &exec, promises_raw, &vm_key, name) {
                Ok(value) => interp.scoped_value(scope, value),
                Err(err) => {
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err);
                }
            };
            let i = slots.reserve_keyed_slot_scoped(interp, slots_handle, keys_handle, key)?;
            let promise_resolve_raw = interp.escape_scoped(promise_resolve);
            let constructor_raw = interp.escape_scoped(constructor);
            let next_value_raw = interp.escape_scoped(next_value);
            let entry_promise = match call_promise_resolve(
                interp,
                stack,
                &exec,
                &promise_resolve_raw,
                &constructor_raw,
                next_value_raw,
            ) {
                Ok(value) => interp.scoped_value(scope, value),
                Err(err) => {
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err);
                }
            };
            slots.refresh_scoped(interp, slots_handle, Some(keys_handle));
            let live_cap = cap_handles.current(interp, context.clone());
            let on_fulfill =
                keyed_element_function(interp, slots.clone(), live_cap.clone(), variant, true, i)?;
            let on_fulfill = interp.scoped_value(scope, on_fulfill);
            let on_reject = match variant {
                KeyedVariant::All => cap_handles.reject,
                KeyedVariant::AllSettled => {
                    let live_cap = cap_handles.current(interp, context.clone());
                    let on_reject = keyed_element_function(
                        interp,
                        slots.clone(),
                        live_cap.clone(),
                        variant,
                        false,
                        i,
                    )?;
                    interp.scoped_value(scope, on_reject)
                }
            };
            let entry_promise_raw = interp.escape_scoped(entry_promise);
            let on_fulfill_raw = interp.escape_scoped(on_fulfill);
            let on_reject_raw = interp.escape_scoped(on_reject);
            if let Err(err) = attach_then_value(
                interp,
                stack,
                &exec,
                entry_promise_raw,
                on_fulfill_raw,
                on_reject_raw,
            ) {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        }
        slots.refresh_scoped(interp, slots_handle, Some(keys_handle));
        if slots.finish_iteration() {
            let mut cap = cap_handles.current(interp, context.clone());
            resolve_keyed_slots_runtime(interp, stack, &mut cap, &slots, name, &[], &[])?;
        }
        Ok(cap_handles.current(interp, context).promise)
    })
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
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    receiver: Value,
    key: &crate::VmPropertyKey<'_>,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let receiver_raw = interp.escape_scoped(receiver);
        match interp
            .ordinary_get_value(stack, context, receiver_raw, receiver_raw, key, 0)
            .map_err(|err| promise_vm_error(interp, name, err))?
        {
            crate::VmGetOutcome::Value(value) => Ok(value),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let getter = interp.scoped_value(scope, getter);
                let getter = interp.escape_scoped(getter);
                let receiver = interp.escape_scoped(receiver);
                interp
                    .run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(|err| promise_vm_error(interp, name, err))
            }
        }
    })
}

fn keyed_element_function(
    interp: &mut Interpreter,
    slots: Arc<PromiseSlots>,
    cap: PromiseCapability,
    variant: KeyedVariant,
    fulfilled: bool,
    index: usize,
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

fn settled_element_function(
    interp: &mut Interpreter,
    slots: Arc<PromiseSlots>,
    cap: PromiseCapability,
    fulfilled: bool,
    index: usize,
) -> Result<Value, NativeError> {
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
        move |ctx, args, captures| {
            let cap = capability_from_captures(captures, &cap);
            let payload = args.first().cloned().unwrap_or(Value::undefined());
            let record = build_settled_record(fulfilled, payload, ctx)?;
            if slots.fill(ctx.heap_mut(), index, record) {
                let collected = slots.collect_values(ctx.heap());
                let arr = ctx.array_from_elements_with_roots(
                    collected.iter().cloned(),
                    &[&cap.promise, &cap.resolve, &cap.reject],
                    &[collected.as_slice()],
                )?;
                call_capability_resolve_native(ctx, &cap, Value::array(arr))?;
            }
            Ok(Value::undefined())
        },
    )
    .map_err(|_| oom_native("Promise.allSettled"))
}

fn resolve_keyed_slots_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    slots: &PromiseSlots,
    name: &'static str,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<(), NativeError> {
    let keys = slots.collect_keys(interp.gc_heap());
    let values = slots.collect_values(interp.gc_heap());
    let result =
        create_keyed_result_runtime(interp, name, &keys, &values, value_roots, slice_roots)?;
    call_capability_resolve(interp, stack, cap, result)
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
    call_capability_resolve_native(ctx, cap, result)
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
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.all",
        reason: "missing execution context".to_string(),
    })?;
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let constructor = interp.scoped_value(scope, constructor);
        let iterable = interp.scoped_value(scope, iterable);
        let mut constructor_current = interp.escape_scoped(constructor);
        let cap = new_generic_promise_capability(
            interp,
            stack,
            context.clone(),
            &mut constructor_current,
        )?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let constructor_raw = interp.escape_scoped(constructor);
        let promise_resolve = match get_promise_resolve(interp, stack, &exec, &constructor_raw) {
            Ok(value) => interp.scoped_value(scope, value),
            Err(err) => {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        };
        let iterable_raw = interp.escape_scoped(iterable);
        let (iterator, next_method) = match interp.get_iterator_sync(stack, &exec, &iterable_raw) {
            Ok((iterator, next)) => (
                interp.scoped_value(scope, iterator),
                interp.scoped_value(scope, next),
            ),
            Err(err) => {
                let native = promise_vm_error(interp, "Promise.all", err);
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let (slots, slots_handle) = PromiseSlots::new_scoped(interp, scope)?;
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = promise_vm_error(interp, "Promise.all", err);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let i = slots.reserve_slot_scoped(interp, slots_handle)?;
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    &exec,
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                slots.refresh_scoped(interp, slots_handle, None);
                let live_cap = cap_handles.current(interp, context.clone());
                let cap_for_fulfill = live_cap.clone();
                let slots_for_trace = slots.clone();
                let trace_slots = Arc::new(move |visitor: &mut SlotVisitor<'_>| {
                    slots_for_trace.trace(visitor);
                    cap_for_fulfill.promise.trace_value_slots(visitor);
                    cap_for_fulfill.resolve.trace_value_slots(visitor);
                    cap_for_fulfill.reject.trace_value_slots(visitor);
                });
                let cap_for_fulfill = live_cap.clone();
                let slots_for_fulfill = slots.clone();
                let on_fulfill = promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![live_cap.promise, live_cap.resolve, live_cap.reject],
                    trace_slots,
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
                            call_capability_resolve_native(ctx, &cap, Value::array(arr))?;
                        }
                        Ok(Value::undefined())
                    },
                )?;
                let on_fulfill = interp.scoped_value(iteration_scope, on_fulfill);
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfill = interp.escape_scoped(on_fulfill);
                let on_reject = interp.escape_scoped(cap_handles.reject);
                if let Err(err) =
                    attach_then_value(interp, stack, &exec, entry_promise, on_fulfill, on_reject)
                {
                    let iterator_raw = interp.escape_scoped(iterator);
                    let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        slots.refresh_scoped(interp, slots_handle, None);
        if slots.finish_iteration() {
            let result =
                slots.materialize_array_scoped(interp, scope, slots_handle, "Promise.all")?;
            let result = interp.escape_scoped(result);
            let mut cap = cap_handles.current(interp, context.clone());
            if let Err(err) = call_capability_resolve(interp, stack, &mut cap, result) {
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        }
        Ok(cap_handles.current(interp, context).promise)
    })
}

fn static_race_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.race",
        reason: "missing execution context".to_string(),
    })?;
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let constructor = interp.scoped_value(scope, constructor);
        let iterable = interp.scoped_value(scope, iterable);
        let mut constructor_current = interp.escape_scoped(constructor);
        let cap = new_generic_promise_capability(
            interp,
            stack,
            context.clone(),
            &mut constructor_current,
        )?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let constructor_raw = interp.escape_scoped(constructor);
        let promise_resolve = match get_promise_resolve(interp, stack, &exec, &constructor_raw) {
            Ok(value) => interp.scoped_value(scope, value),
            Err(err) => {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        };
        let iterable_raw = interp.escape_scoped(iterable);
        let (iterator, next_method) = match interp.get_iterator_sync(stack, &exec, &iterable_raw) {
            Ok((iterator, next)) => (
                interp.scoped_value(scope, iterator),
                interp.scoped_value(scope, next),
            ),
            Err(err) => {
                let native = promise_vm_error(interp, "Promise.race", err);
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = promise_vm_error(interp, "Promise.race", err);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    &exec,
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfilled = interp.escape_scoped(cap_handles.resolve);
                let on_rejected = interp.escape_scoped(cap_handles.reject);
                if let Err(err) = attach_then_value(
                    interp,
                    stack,
                    &exec,
                    entry_promise,
                    on_fulfilled,
                    on_rejected,
                ) {
                    let iterator_raw = interp.escape_scoped(iterator);
                    let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        Ok(cap_handles.current(interp, context).promise)
    })
}

fn static_all_settled_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.allSettled",
        reason: "missing execution context".to_string(),
    })?;
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    interp.with_handle_scope(|interp, scope| {
        let constructor = interp.scoped_value(scope, constructor);
        let iterable = interp.scoped_value(scope, iterable);
        let mut constructor_current = interp.escape_scoped(constructor);
        let cap = new_generic_promise_capability(
            interp,
            stack,
            context.clone(),
            &mut constructor_current,
        )?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let constructor_raw = interp.escape_scoped(constructor);
        let promise_resolve = match get_promise_resolve(interp, stack, &exec, &constructor_raw) {
            Ok(value) => interp.scoped_value(scope, value),
            Err(err) => {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        };
        let iterable_raw = interp.escape_scoped(iterable);
        let (iterator, next_method) = match interp.get_iterator_sync(stack, &exec, &iterable_raw) {
            Ok((iterator, next)) => (
                interp.scoped_value(scope, iterator),
                interp.scoped_value(scope, next),
            ),
            Err(err) => {
                let native = promise_vm_error(interp, "Promise.allSettled", err);
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let (slots, slots_handle) = PromiseSlots::new_scoped(interp, scope)?;
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = promise_vm_error(interp, "Promise.allSettled", err);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let i = slots.reserve_slot_scoped(interp, slots_handle)?;
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    &exec,
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                slots.refresh_scoped(interp, slots_handle, None);
                let live_cap = cap_handles.current(interp, context.clone());
                let on_fulfill =
                    settled_element_function(interp, slots.clone(), live_cap.clone(), true, i)?;
                let on_fulfill = interp.scoped_value(iteration_scope, on_fulfill);
                let live_cap = cap_handles.current(interp, context.clone());
                let on_reject =
                    settled_element_function(interp, slots.clone(), live_cap.clone(), false, i)?;
                let on_reject = interp.scoped_value(iteration_scope, on_reject);
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfill = interp.escape_scoped(on_fulfill);
                let on_reject = interp.escape_scoped(on_reject);
                if let Err(err) =
                    attach_then_value(interp, stack, &exec, entry_promise, on_fulfill, on_reject)
                {
                    let iterator_raw = interp.escape_scoped(iterator);
                    let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        slots.refresh_scoped(interp, slots_handle, None);
        if slots.finish_iteration() {
            let result = slots.materialize_array_scoped(
                interp,
                scope,
                slots_handle,
                "Promise.allSettled",
            )?;
            let result = interp.escape_scoped(result);
            let mut cap = cap_handles.current(interp, context.clone());
            if let Err(err) = call_capability_resolve(interp, stack, &mut cap, result) {
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        }
        Ok(cap_handles.current(interp, context).promise)
    })
}

fn build_settled_record(
    fulfilled: bool,
    payload: Value,
    ctx: &mut NativeCtx<'_>,
) -> Result<Value, NativeError> {
    let status_text = if fulfilled { "fulfilled" } else { "rejected" };
    let key = if fulfilled { "value" } else { "reason" };
    ctx.scope(|mut scope| {
        let payload = scope.value(payload);
        let status = scope.string(status_text)?;
        let object = scope.object()?;
        scope.set(object, "status", status)?;
        scope.set(object, key, payload)?;
        Ok(scope.finish(object))
    })
}

fn make_aggregate_error_runtime_rooted(
    interp: &mut Interpreter,
    registry: &ErrorClassRegistry,
    errors: Vec<Value>,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let errors = errors
            .into_iter()
            .map(|value| interp.scoped_value(scope, value))
            .collect::<Vec<_>>();
        let prototype = interp.scoped_value(
            scope,
            Value::object(registry.prototype(ErrorKind::AggregateError)),
        );
        let message = interp
            .scoped_string(scope, "All promises were rejected")
            .map_err(|_| oom_native("Promise.any"))?;
        let object = interp
            .scoped_object(scope)
            .map_err(|_| oom_native("Promise.any"))?;
        interp
            .scoped_set_prototype(scope, object, Some(prototype))
            .map_err(|err| NativeError::TypeError {
                name: "Promise.any",
                reason: err.to_string(),
            })?;
        interp
            .scoped_set(scope, object, "message", message)
            .map_err(|err| NativeError::TypeError {
                name: "Promise.any",
                reason: err.to_string(),
            })?;
        let errors_array = interp
            .scoped_array(scope, errors.len())
            .map_err(|_| oom_native("Promise.any"))?;
        for (index, error) in errors.into_iter().enumerate() {
            interp
                .scoped_set_index(scope, errors_array, index, error)
                .map_err(|_| oom_native("Promise.any"))?;
        }
        interp
            .scoped_set(scope, object, "errors", errors_array)
            .map_err(|err| NativeError::TypeError {
                name: "Promise.any",
                reason: err.to_string(),
            })?;
        Ok(interp.escape_scoped(object))
    })
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
    ctx.scope(|scope| {
        let mut cx = crate::marshal::MarshalCx::new(scope);
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

fn oom_native(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn capability_record_scoped(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let cap = CapabilityHandles::park(interp, scope, cap);
        let object = interp.scoped_object(scope).map_err(|_| oom_native(name))?;
        interp
            .scoped_set(scope, object, "promise", cap.promise)
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })?;
        interp
            .scoped_set(scope, object, "resolve", cap.resolve)
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })?;
        interp
            .scoped_set(scope, object, "reject", cap.reject)
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })?;
        Ok(interp.escape_scoped(object))
    })
}

fn static_any_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.any",
        reason: "missing execution context".to_string(),
    })?;
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    let registry = interp.error_classes_clone();
    interp.with_handle_scope(|interp, scope| {
        let constructor = interp.scoped_value(scope, constructor);
        let iterable = interp.scoped_value(scope, iterable);
        let mut constructor_current = interp.escape_scoped(constructor);
        let cap = new_generic_promise_capability(
            interp,
            stack,
            context.clone(),
            &mut constructor_current,
        )?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let constructor_raw = interp.escape_scoped(constructor);
        let promise_resolve = match get_promise_resolve(interp, stack, &exec, &constructor_raw) {
            Ok(value) => interp.scoped_value(scope, value),
            Err(err) => {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        };
        let iterable_raw = interp.escape_scoped(iterable);
        let (iterator, next_method) = match interp.get_iterator_sync(stack, &exec, &iterable_raw) {
            Ok((iterator, next)) => (
                interp.scoped_value(scope, iterator),
                interp.scoped_value(scope, next),
            ),
            Err(err) => {
                let native = promise_vm_error(interp, "Promise.any", err);
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let (errors, errors_handle) = PromiseSlots::new_scoped(interp, scope)?;
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = promise_vm_error(interp, "Promise.any", err);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let i = errors.reserve_slot_scoped(interp, errors_handle)?;
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    &exec,
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                errors.refresh_scoped(interp, errors_handle, None);
                let live_cap = cap_handles.current(interp, context.clone());
                let errors_for_call = errors.clone();
                let registry_for_call = registry.clone();
                let cap_for_call = live_cap.clone();
                let trace_errors = {
                    let errors = errors.clone();
                    let cap = live_cap.clone();
                    Arc::new(move |visitor: &mut SlotVisitor<'_>| {
                        errors.trace(visitor);
                        cap.promise.trace_value_slots(visitor);
                        cap.resolve.trace_value_slots(visitor);
                        cap.reject.trace_value_slots(visitor);
                    })
                };
                let on_reject = promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![live_cap.promise, live_cap.resolve, live_cap.reject],
                    trace_errors,
                    move |ctx, args, captures| {
                        let cap = capability_from_captures(captures, &cap_for_call);
                        let reason = args.first().cloned().unwrap_or(Value::undefined());
                        if errors_for_call.fill(ctx.heap_mut(), i, reason) {
                            let collected = errors_for_call.collect_values(ctx.heap());
                            let agg = make_aggregate_error_native_rooted(
                                ctx,
                                &registry_for_call,
                                collected,
                            )?;
                            call_capability_reject_native(ctx, &cap, agg)?;
                        }
                        Ok(Value::undefined())
                    },
                )?;
                let on_reject = interp.scoped_value(iteration_scope, on_reject);
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfilled = interp.escape_scoped(cap_handles.resolve);
                let on_reject = interp.escape_scoped(on_reject);
                if let Err(err) =
                    attach_then_value(interp, stack, &exec, entry_promise, on_fulfilled, on_reject)
                {
                    let iterator_raw = interp.escape_scoped(iterator);
                    let _ = interp.iterator_close_sync(stack, &exec, &iterator_raw);
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        errors.refresh_scoped(interp, errors_handle, None);
        if errors.finish_iteration() {
            let collected = errors.collect_values(interp.gc_heap());
            let agg = make_aggregate_error_runtime_rooted(interp, &registry, collected)?;
            let mut cap = cap_handles.current(interp, context.clone());
            call_capability_reject(interp, stack, &mut cap, agg)?;
        }
        Ok(cap_handles.current(interp, context).promise)
    })
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
    capability_record_scoped(interp, &cap, "Promise.withResolvers")
}

fn static_with_resolvers_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    mut constructor: Value,
) -> Result<Value, NativeError> {
    let cap = new_generic_promise_capability(interp, stack, context, &mut constructor)?;
    capability_record_scoped(interp, &cap, "Promise.withResolvers")
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
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    args: &[Value],
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.prototype.then";
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;
    let promise = Value::promise(*promise);
    let default_ctor = builtin_promise_constructor(interp)?;
    interp.with_handle_scope(|interp, scope| {
        let promise = interp.scoped_value(scope, promise);
        let default_ctor = interp.scoped_value(scope, default_ctor);
        let on_fulfilled = args
            .first()
            .copied()
            .filter(crate::is_callable_value)
            .map(|value| interp.scoped_value(scope, value));
        let on_rejected = args
            .get(1)
            .copied()
            .filter(crate::is_callable_value)
            .map(|value| interp.scoped_value(scope, value));
        let promise_raw = interp.escape_scoped(promise);
        let default_ctor_raw = interp.escape_scoped(default_ctor);
        let c = species_constructor_runtime(
            interp,
            stack,
            &exec,
            &promise_raw,
            &default_ctor_raw,
            NAME,
        )?;
        let c = interp.scoped_value(scope, c);
        let c_raw = interp.escape_scoped(c);
        let capability = if is_builtin_promise_constructor(interp, &c_raw) {
            PromiseBuilder::with_optional_context(context.clone())
                .capability_runtime_rooted(interp, &[], &[])
                .map_err(|_| oom_native(NAME))?
        } else {
            let mut constructor = c_raw;
            new_generic_promise_capability(interp, stack, context.clone(), &mut constructor)?
        };
        let capability_handles = CapabilityHandles::park(interp, scope, &capability);
        let promise = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("Promise.prototype.then receiver remains a promise");
        let on_fulfilled = on_fulfilled.map(|value| interp.escape_scoped(value));
        let on_rejected = on_rejected.map(|value| interp.escape_scoped(value));
        let capability = capability_handles.current(interp, context.clone());
        let outcome = promise.perform_then_with_context(
            interp.gc_heap_mut(),
            on_fulfilled,
            on_rejected,
            capability,
            context.clone(),
        );
        if let Some(job) = outcome.immediate_job {
            interp.microtasks_mut().enqueue(job);
        }
        Ok(capability_handles.current(interp, context).promise)
    })
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
    interp.with_handle_scope(|interp, scope| {
        let promise = interp.scoped_value(scope, Value::promise(*promise));
        let on_fulfilled = on_fulfilled.map(|value| interp.scoped_value(scope, value));
        let on_rejected = on_rejected.map(|value| interp.scoped_value(scope, value));
        let capability = match PromiseBuilder::with_optional_context(context.clone())
            .capability_runtime_rooted(interp, &[], &[])
        {
            Ok(capability) => capability,
            Err(_) => return Value::undefined(),
        };
        let capability_handles = CapabilityHandles::park(interp, scope, &capability);
        let promise = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("then receiver remains rooted");
        let on_fulfilled = on_fulfilled.map(|value| interp.escape_scoped(value));
        let on_rejected = on_rejected.map(|value| interp.escape_scoped(value));
        let capability = capability_handles.current(interp, context.clone());
        let outcome: PromiseThenOutcome = promise.perform_then_with_context(
            interp.gc_heap_mut(),
            on_fulfilled,
            on_rejected,
            capability,
            context.clone(),
        );
        if let Some(job) = outcome.immediate_job {
            interp.microtasks_mut().enqueue(job);
        }
        capability_handles.current(interp, context).promise
    })
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
    interp.with_handle_scope(|interp, scope| {
        let promise = interp.scoped_value(scope, Value::promise(*promise));
        let on_fulfilled = on_fulfilled.map(|value| interp.scoped_value(scope, value));
        let on_rejected = on_rejected.map(|value| interp.scoped_value(scope, value));
        let capability = match PromiseBuilder::with_optional_context(context.clone())
            .capability_runtime_rooted(interp, &[], &[])
        {
            Ok(capability) => capability,
            Err(_) => return,
        };
        let capability_handles = CapabilityHandles::park(interp, scope, &capability);
        let promise = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("adopted promise remains rooted");
        let on_fulfilled = on_fulfilled.map(|value| interp.escape_scoped(value));
        let on_rejected = on_rejected.map(|value| interp.escape_scoped(value));
        let capability = capability_handles.current(interp, context.clone());
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
    });
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
    stack: &ActivationStack,
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
    ctx.scope(|mut scope| {
        let promise = scope.value(Value::promise(promise));
        let value = scope.argument(args, 0);
        let promise_handle = scope
            .raw(promise)
            .as_promise()
            .expect("resolve function keeps its captured promise rooted");
        if !matches!(
            promise_handle.state(scope.context().heap()),
            PromiseState::Pending
        ) {
            return Ok(Value::undefined());
        }

        if scope.raw(value).is_promise() {
            let promise_handle = scope
                .raw(promise)
                .as_promise()
                .expect("resolver promise remains rooted");
            let value_root = scope.raw(value);
            let (on_fulfill, on_reject) = make_resolve_adoption_handlers_native_rooted(
                scope.context(),
                promise_handle,
                &[&value_root],
                &[],
            )?;
            let on_fulfill = scope.value(on_fulfill);
            let on_reject = scope.value(on_reject);
            let inner = scope
                .raw(value)
                .as_promise()
                .expect("adopted promise remains rooted");
            let on_fulfill = scope.raw(on_fulfill);
            let on_reject = scope.raw(on_reject);
            attach_then(
                scope.context().interp_mut(),
                context.clone(),
                &inner,
                Some(on_fulfill),
                Some(on_reject),
            );
            return Ok(Value::undefined());
        }

        // §27.2.1.3.2 Promise Resolve Functions steps 8-13 — any object
        // with a callable `then` is a thenable: read `then` (firing an
        // accessor and rejecting on its throw), then enqueue the job.
        if scope.raw(value).is_object_type()
            && let Some(exec) = context.clone()
        {
            let value_raw = scope.raw(value);
            let then = scope.with_turn_parts(|interp, stack| {
                get_property_runtime(interp, stack, &exec, value_raw, "then", "Promise resolve")
            });
            let then = match then {
                Ok(then) => scope.value(then),
                Err(err) => {
                    let promise = scope
                        .raw(promise)
                        .as_promise()
                        .expect("resolver promise remains rooted on getter throw");
                    let interp = scope.context().interp_mut();
                    let reason = native_error_rejection_value_preserving_throw(interp, err);
                    let jobs = promise.reject(interp.gc_heap_mut(), reason);
                    drain_jobs(interp, jobs);
                    return Ok(Value::undefined());
                }
            };
            if scope.is_callable(then) {
                let promise_handle = scope
                    .raw(promise)
                    .as_promise()
                    .expect("resolver promise remains rooted");
                let value_raw = scope.raw(value);
                let then_raw = scope.raw(then);
                let (on_fulfill, on_reject) = make_resolve_adoption_handlers_native_rooted(
                    scope.context(),
                    promise_handle,
                    &[&value_raw, &then_raw],
                    &[],
                )?;
                let on_fulfill = scope.value(on_fulfill);
                let on_reject = scope.value(on_reject);
                let value_raw = scope.raw(value);
                let then_raw = scope.raw(then);
                let on_fulfill_raw = scope.raw(on_fulfill);
                let on_reject_raw = scope.raw(on_reject);
                let job = make_resolve_thenable_job(
                    scope.context(),
                    value_raw,
                    then_raw,
                    on_fulfill_raw,
                    on_reject_raw,
                    exec.clone(),
                )?;
                let job = scope.value(job);
                let job = scope.raw(job);
                scope
                    .context()
                    .interp_mut()
                    .microtasks_mut()
                    .enqueue(crate::Microtask {
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

        let promise = scope
            .raw(promise)
            .as_promise()
            .expect("resolver promise remains rooted before fulfillment");
        let value = scope.raw(value);
        let interp = scope.context().interp_mut();
        let jobs = promise.fulfill(interp.gc_heap_mut(), value);
        drain_jobs(interp, jobs);
        Ok(Value::undefined())
    })
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
    interp.with_handle_scope(|interp, scope| {
        let promise = interp.scoped_value(scope, Value::promise(promise));
        let value = interp.scoped_value(scope, value);
        let promise_handle = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("async resolver promise remains rooted");
        if !matches!(
            promise_handle.state(interp.gc_heap()),
            PromiseState::Pending
        ) {
            return Ok(());
        }

        if interp.escape_scoped(value).is_promise() {
            let promise_handle = interp
                .escape_scoped(promise)
                .as_promise()
                .expect("async resolver promise remains rooted");
            let (on_fulfill, on_reject) =
                make_resolve_adoption_handlers_runtime_rooted(interp, promise_handle, &[], &[])
                    .map_err(crate::oom_to_vm)?;
            let on_fulfill = interp.scoped_value(scope, on_fulfill);
            let on_reject = interp.scoped_value(scope, on_reject);
            let inner = interp
                .escape_scoped(value)
                .as_promise()
                .expect("adopted async promise remains rooted");
            attach_then(
                interp,
                context,
                &inner,
                Some(interp.escape_scoped(on_fulfill)),
                Some(interp.escape_scoped(on_reject)),
            );
            return Ok(());
        }

        let promise = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("async resolver promise remains rooted before fulfillment");
        let value = interp.escape_scoped(value);
        let jobs = promise.fulfill(interp.gc_heap_mut(), value);
        drain_jobs(interp, jobs);
        Ok(())
    })
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
    ctx.native_value(
        "PromiseResolveThenableJob",
        captures,
        move |ctx, _args, captures| {
            ctx.scope(|mut scope| {
                let thenable = scope.value(captures[0]);
                let then = scope.value(captures[1]);
                let on_fulfill = scope.value(captures[2]);
                let on_reject = scope.value(captures[3]);
                let thenable_raw = scope.raw(thenable);
                let then_raw = scope.raw(then);
                let on_fulfill_raw = scope.raw(on_fulfill);
                let on_reject_raw = scope.raw(on_reject);
                let call_result = scope.with_turn_parts(|interp, stack| {
                    interp.run_callable_sync_rooted(
                        stack,
                        &exec,
                        &then_raw,
                        thenable_raw,
                        smallvec![on_fulfill_raw, on_reject_raw],
                    )
                });
                match call_result {
                    Ok(_) => Ok(Value::undefined()),
                    Err(err) => {
                        // §27.2.1.3.2 — an abrupt `then` call rejects the
                        // promise with the thrown value, preserving its
                        // identity (a user `throw obj` keeps `obj`).
                        let reason = scope.with_turn_parts(|interp, _| {
                            interp
                                .take_pending_uncaught_throw()
                                .unwrap_or_else(|| crate::error_ops::vm_err_to_value(interp, &err))
                        });
                        let reason = scope.value(reason);
                        let on_reject = scope.raw(on_reject);
                        let reason = scope.raw(reason);
                        let _ = scope.with_turn_parts(|interp, stack| {
                            interp.run_callable_sync_rooted(
                                stack,
                                &exec,
                                &on_reject,
                                Value::undefined(),
                                smallvec![reason],
                            )
                        });
                        Ok(Value::undefined())
                    }
                }
            })
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
    stack: &ActivationStack,
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
    interp.note_settle_rejection(&jobs);
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
        if std::env::var_os("OTTER_GC_STRESS").is_none() {
            assert!(
                after > before,
                "Promise.any AggregateError runtime path should allocate object and errors array in young space"
            );
        }
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

        let result = NativeCtx::with_host_context(
            &mut interp,
            NativeCallInfo::call(Value::undefined()),
            None,
            |ctx| {
                make_aggregate_error_native_rooted(ctx, &registry, errors).expect("aggregate error")
            },
        );

        let after = interp.gc_heap().stats().new_allocated_bytes;
        if std::env::var_os("OTTER_GC_STRESS").is_none() {
            assert!(
                after > before,
                "Promise.any AggregateError native path should allocate object and errors array in young space"
            );
        }
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
        let mut stack = ActivationStack::new();
        let promise_value = interp
            .with_runtime_turn(&mut stack, |mut turn| {
                turn.with_parts(|interp, stack| {
                    static_resolve(interp, stack, Some(empty_context()), constructor, &args)
                })
            })
            .expect("Promise.resolve");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        if std::env::var_os("OTTER_GC_STRESS").is_none() {
            assert!(
                after > before,
                "Promise.resolve should allocate non-promise results through runtime-rooted young allocation"
            );
        }
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
        if std::env::var_os("OTTER_GC_STRESS").is_none() {
            assert!(
                after > before,
                "Promise capability creation should allocate promise and closures through runtime roots"
            );
        }
        assert!(cap.promise.is_promise());
        assert!(cap.resolve.is_native_function());
        assert!(cap.reject.is_native_function());
        assert_ne!(
            cap.resolve, cap.reject,
            "moving collection must not alias resolve and reject closures"
        );
    }

    #[test]
    fn promise_constructor_builder_uses_native_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        let executor = Value::number_i32(17);
        let args = vec![executor];

        let (handle, resolve, reject) = NativeCtx::with_host_context(
            &mut interp,
            NativeCallInfo::construct(
                Value::number(NumberValue::from_i32(1)),
                Some(Value::number(NumberValue::from_i32(2))),
            ),
            None,
            |ctx| {
                PromiseBuilder::new()
                    .construct_native_rooted(ctx, &[&executor], &[args.as_slice()])
                    .expect("native-rooted promise constructor plumbing")
            },
        );

        let after = interp.gc_heap().stats().new_allocated_bytes;
        if std::env::var_os("OTTER_GC_STRESS").is_none() {
            assert!(
                after > before,
                "native Promise constructor plumbing should allocate through root-aware young allocation"
            );
        }
        assert!(matches!(
            handle.state(interp.gc_heap()),
            PromiseState::Pending
        ));
        assert!(interp.is_callable_runtime(&resolve));
        assert!(interp.is_callable_runtime(&reject));
    }
}

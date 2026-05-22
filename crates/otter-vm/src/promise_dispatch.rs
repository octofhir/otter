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
//!   `JsPromiseHandle::clone()` (Rc-shared body). They are
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
use crate::native_function::{
    NativeError, native_value_with_captures_unchecked_with_roots, traced_native_value_with_length,
};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
    PromiseThenOutcome,
};
use crate::string::JsString;
use crate::{Frame, Interpreter, NativeCtx, Value};
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::{SmallVec, smallvec};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct PromiseSlots {
    values: crate::array::JsArray,
    keys: Option<crate::array::JsArray>,
    remaining: Cell<usize>,
}

struct CapabilityExecutorState {
    resolve: RefCell<Option<Value>>,
    reject: RefCell<Option<Value>>,
}

impl CapabilityExecutorState {
    fn new() -> Rc<Self> {
        Rc::new(Self {
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
        let resolve = args.first().cloned().unwrap_or(Value::Undefined);
        let reject = args.get(1).cloned().unwrap_or(Value::Undefined);
        if !matches!(resolve, Value::Undefined) {
            *self.resolve.borrow_mut() = Some(resolve);
        }
        if !matches!(reject, Value::Undefined) {
            *self.reject.borrow_mut() = Some(reject);
        }
        Ok(Value::Undefined)
    }
}

impl PromiseSlots {
    fn new(
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Rc<Self>, NativeError> {
        let values = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                value_roots,
                slice_roots,
            )
            .map_err(|_| oom_native("Promise combinator"))?;
        Ok(Rc::new(Self {
            values,
            keys: None,
            remaining: Cell::new(1),
        }))
    }

    fn new_keyed(
        interp: &mut Interpreter,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Rc<Self>, NativeError> {
        let values = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                value_roots,
                slice_roots,
            )
            .map_err(|_| oom_native("Promise keyed combinator"))?;
        let values_root = Value::Array(values);
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
        Ok(Rc::new(Self {
            values,
            keys: Some(keys),
            remaining: Cell::new(1),
        }))
    }

    fn trace(&self, visitor: &mut SlotVisitor<'_>) {
        self.array_value().trace_value_slots(visitor);
        if let Some(keys) = self.keys {
            Value::Array(keys).trace_value_slots(visitor);
        }
    }

    fn array_value(&self) -> Value {
        Value::Array(self.values)
    }

    fn keys_value(&self) -> Option<Value> {
        self.keys.map(Value::Array)
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
            self.values,
            interp.gc_heap_mut(),
            Value::Hole,
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
        let Some(keys) = self.keys else {
            return Err(NativeError::TypeError {
                name: "Promise keyed combinator",
                reason: "missing keyed slots".to_string(),
            });
        };
        let key_root = key.clone();
        let values_root = self.array_value();
        let keys_root = Value::Array(keys);
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
            self.values,
            interp.gc_heap_mut(),
            Value::Hole,
            &mut external_visit,
        )
        .map_err(|_| oom_native("Promise keyed combinator"))?;
        self.remaining.set(self.remaining.get().saturating_add(1));
        Ok(len - 1)
    }

    fn fill(&self, heap: &mut otter_gc::GcHeap, index: usize, value: Value) -> bool {
        let did_fill = crate::array::with_elements_mut(self.values, heap, |elements| {
            let Some(slot) = elements.get_mut(index) else {
                return false;
            };
            if !matches!(slot, Value::Hole) {
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
        crate::array::with_elements(self.values, heap, |elements| {
            elements
                .iter()
                .map(|slot| {
                    if matches!(slot, Value::Hole) {
                        Value::Undefined
                    } else {
                        slot.clone()
                    }
                })
                .collect()
        })
    }

    fn collect_keys(&self, heap: &otter_gc::GcHeap) -> Vec<Value> {
        let Some(keys) = self.keys else {
            return Vec::new();
        };
        crate::array::with_elements(keys, heap, |elements| elements.to_vec())
    }
}

/// Boa-style helper for constructing promises and capabilities with
/// an explicit VM execution context.
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

    pub(crate) fn fulfilled_runtime_rooted(
        &self,
        interp: &mut Interpreter,
        value: Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        let roots = interp.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
        };
        JsPromiseHandle::fulfilled_with_roots(interp.gc_heap_mut(), value, &mut external_visit)
    }

    pub(crate) fn pending_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &SmallVec<[Frame; 8]>,
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
        stack: &SmallVec<[Frame; 8]>,
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
        stack: &SmallVec<[Frame; 8]>,
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
        let this_value = ctx.this_value().clone();
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
        let promise_value = Value::Promise(promise);
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
        let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
        reject_roots.extend_from_slice(value_roots);
        reject_roots.push(&promise_value);
        reject_roots.push(&resolve);
        let reject =
            make_reject_native_runtime_rooted(interp, promise, &reject_roots, slice_roots)?;
        Ok((promise, resolve, reject))
    }

    pub(crate) fn construct_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &SmallVec<[Frame; 8]>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_stack_rooted(interp, stack, value_roots, slice_roots)?;
        let promise_value = Value::Promise(promise);
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
        let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
        reject_roots.extend_from_slice(value_roots);
        reject_roots.push(&promise_value);
        reject_roots.push(&resolve);
        let reject =
            make_reject_native_stack_rooted(interp, stack, promise, &reject_roots, slice_roots)?;
        Ok((promise, resolve, reject))
    }

    pub(crate) fn construct_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending_native_rooted(ctx, value_roots, slice_roots)?;
        let promise_value = Value::Promise(promise);
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
        let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
        reject_roots.extend_from_slice(value_roots);
        reject_roots.push(&promise_value);
        reject_roots.push(&resolve);
        let reject = make_reject_native_native_rooted(ctx, promise, &reject_roots, slice_roots)?;
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
            promise: Value::Promise(handle),
            resolve,
            reject,
            context: self.context.clone(),
        })
    }

    pub(crate) fn capability_stack_rooted(
        &self,
        interp: &mut Interpreter,
        stack: &SmallVec<[Frame; 8]>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<PromiseCapability, otter_gc::OutOfMemory> {
        let (handle, resolve, reject) =
            self.construct_stack_rooted(interp, stack, value_roots, slice_roots)?;
        Ok(PromiseCapability {
            promise: Value::Promise(handle),
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
    let this_value = ctx.this_value().clone();
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
) -> Rc<crate::native_function::NativeTraceFn> {
    let captures = captures.clone();
    Rc::new(move |visitor: &mut SlotVisitor<'_>| {
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
    stack: &SmallVec<[Frame; 8]>,
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
    let this_value = ctx.this_value().clone();
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

fn promise_element_function<F>(
    interp: &mut Interpreter,
    name: &'static str,
    length: u8,
    captures: smallvec::SmallVec<[Value; 4]>,
    trace: Rc<crate::native_function::NativeTraceFn>,
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
        M::Reject => Ok(Value::Promise(static_reject(interp, args)?)),
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
            Value::Promise(*promise),
            args.first().cloned().unwrap_or(Value::Undefined),
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
    let then = get_callable_property(interp, exec, receiver.clone(), "then", NAME)?;
    interp
        .run_callable_sync(exec, &then, receiver, smallvec![on_fulfilled, on_rejected])
        .map_err(|err| promise_vm_error(NAME, err))
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
    if !is_object_like(&receiver) {
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
        return invoke_then_interp(interp, &exec, receiver, on_finally.clone(), on_finally);
    }
    let default_ctor = builtin_promise_constructor(interp)?;
    let c = species_constructor_runtime(interp, &exec, &receiver, &default_ctor, NAME)?;
    let then_finally = make_then_finally(interp, &exec, c.clone(), on_finally.clone())?;
    let catch_finally = make_catch_finally(interp, &exec, c, on_finally)?;
    invoke_then_interp(interp, &exec, receiver, then_finally, catch_finally)
}

fn make_then_finally(
    interp: &mut Interpreter,
    exec: &ExecutionContext,
    constructor: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![constructor.clone(), on_finally.clone()];
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
            let c = captures[0].clone();
            let on_finally = captures[1].clone();
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            let result = {
                let (interp, _) = ctx.interp_mut_and_context();
                interp
                    .run_callable_sync(
                        &exec_for_call,
                        &on_finally,
                        Value::Undefined,
                        SmallVec::new(),
                    )
                    .map_err(|err| promise_vm_error("Promise.prototype.finally", err))?
            };
            let resolved = {
                let (interp, _) = ctx.interp_mut_and_context();
                let resolve_fn = get_promise_resolve(interp, &exec_for_call, &c)?;
                call_promise_resolve(interp, &exec_for_call, &resolve_fn, &c, result)?
            };
            let value_thunk = make_value_thunk(ctx, value)?;
            invoke_then(ctx, resolved, value_thunk, Value::Undefined)
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
    let captures: SmallVec<[Value; 4]> = smallvec![constructor.clone(), on_finally.clone()];
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
            let c = captures[0].clone();
            let on_finally = captures[1].clone();
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            let result = {
                let (interp, _) = ctx.interp_mut_and_context();
                interp
                    .run_callable_sync(
                        &exec_for_call,
                        &on_finally,
                        Value::Undefined,
                        SmallVec::new(),
                    )
                    .map_err(|err| promise_vm_error("Promise.prototype.finally", err))?
            };
            let resolved = {
                let (interp, _) = ctx.interp_mut_and_context();
                let resolve_fn = get_promise_resolve(interp, &exec_for_call, &c)?;
                call_promise_resolve(interp, &exec_for_call, &resolve_fn, &c, result)?
            };
            let thrower = make_thrower(ctx, reason)?;
            invoke_then(ctx, resolved, thrower, Value::Undefined)
        },
    )
    .map_err(|_| oom_native("Promise.prototype.finally"))
}

fn make_value_thunk(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![value.clone()];
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
        move |_ctx, _args, captures| Ok(captures[0].clone()),
    )
    .map_err(|_| oom_native("Promise.prototype.finally"))
}

fn make_thrower(ctx: &mut NativeCtx<'_>, reason: Value) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![reason.clone()];
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
            let reason = captures[0].clone();
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
    matches!(
        constructor,
        Value::NativeFunction(native) if native.name(interp.gc_heap()) == "Promise"
    )
}

fn builtin_promise_constructor(interp: &Interpreter) -> Result<Value, NativeError> {
    crate::object::get(*interp.global_this(), interp.gc_heap(), "Promise").ok_or_else(|| {
        NativeError::TypeError {
            name: "Promise",
            reason: "Promise constructor is not installed".to_string(),
        }
    })
}

fn promise_vm_error(name: &'static str, err: crate::VmError) -> NativeError {
    match err {
        crate::VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
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
        Rc::new(move |visitor: &mut SlotVisitor<'_>| state.trace(visitor))
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
        .run_construct_sync(
            &exec,
            &constructor,
            constructor.clone(),
            smallvec![executor.clone()],
        )
        .map_err(|err| promise_vm_error("Promise", err))?;
    let resolve = state.resolve.borrow().clone().unwrap_or(Value::Undefined);
    let reject = state.reject.borrow().clone().unwrap_or(Value::Undefined);
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
        .run_callable_sync(exec, function, Value::Undefined, smallvec![value])
        .map_err(|err| promise_vm_error("Promise", err))?;
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

fn native_error_rejection_value(err: NativeError, heap: &mut otter_gc::GcHeap) -> Value {
    if let NativeError::Thrown { message, .. } = err {
        return Value::String(
            crate::JsString::from_str(&message, heap).unwrap_or_else(|_| {
                crate::JsString::from_str("", heap).expect("empty string allocates")
            }),
        );
    }
    let vm_error = crate::native_to_vm_error(err);
    crate::error_ops::vm_err_to_value(&vm_error, heap)
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
    native_error_rejection_value(err, interp.gc_heap_mut())
}

fn reject_capability_error(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    err: NativeError,
) -> Result<Value, NativeError> {
    let reason = native_error_rejection_value_preserving_throw(interp, err);
    call_capability_reject(interp, cap, reason)?;
    Ok(cap.promise.clone())
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
        .ordinary_get_value(
            context,
            receiver.clone(),
            receiver.clone(),
            &property_key,
            0,
        )
        .map_err(|err| promise_vm_error(name, err))?
    {
        crate::VmGetOutcome::Value(value) => Ok(value),
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(name, err)),
    }
}

/// Read an own/inherited property by symbol key without callability check.
fn get_symbol_property_runtime(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    receiver: Value,
    sym: &crate::symbol::JsSymbol,
    name: &'static str,
) -> Result<Value, NativeError> {
    let property_key = crate::VmPropertyKey::Symbol(sym.clone());
    match interp
        .ordinary_get_value(
            context,
            receiver.clone(),
            receiver.clone(),
            &property_key,
            0,
        )
        .map_err(|err| promise_vm_error(name, err))?
    {
        crate::VmGetOutcome::Value(value) => Ok(value),
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(name, err)),
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
    let c = get_property_runtime(interp, context, obj.clone(), "constructor", name)?;
    if matches!(c, Value::Undefined) {
        return Ok(default_ctor.clone());
    }
    if !is_object_like(&c) {
        return Err(NativeError::TypeError {
            name,
            reason: "constructor is not an Object".to_string(),
        });
    }
    let species_sym = interp
        .well_known_symbols()
        .get(crate::symbol::WellKnown::Species);
    let s = get_symbol_property_runtime(interp, context, c.clone(), &species_sym, name)?;
    if matches!(s, Value::Undefined | Value::Null) {
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

fn is_object_like(value: &Value) -> bool {
    matches!(
        value,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure(_)
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Proxy(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Promise(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
    )
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
        .ordinary_get_value(
            context,
            receiver.clone(),
            receiver.clone(),
            &property_key,
            0,
        )
        .map_err(|err| promise_vm_error(name, err))?
    {
        crate::VmGetOutcome::Value(value) => value,
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver.clone(), SmallVec::new())
            .map_err(|err| promise_vm_error(name, err))?,
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
    get_callable_property(
        interp,
        context,
        constructor.clone(),
        "resolve",
        "Promise.resolve",
    )
}

fn call_promise_resolve(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    resolve_fn: &Value,
    constructor: &Value,
    value: Value,
) -> Result<Value, NativeError> {
    interp
        .run_callable_sync(context, resolve_fn, constructor.clone(), smallvec![value])
        .map_err(|err| promise_vm_error("Promise.resolve", err))
}

fn attach_then_value(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    promise: Value,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<(), NativeError> {
    let then = get_callable_property(
        interp,
        context,
        promise.clone(),
        "then",
        "Promise combinator",
    )?;
    interp
        .run_callable_sync(
            context,
            &then,
            promise,
            smallvec![on_fulfilled, on_rejected],
        )
        .map_err(|err| promise_vm_error("Promise combinator", err))?;
    Ok(())
}

fn static_resolve(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if let Value::Promise(p) = &value {
        if let Some(exec) = context.as_ref() {
            let value_constructor = get_property_runtime(
                interp,
                exec,
                value.clone(),
                "constructor",
                "Promise.resolve",
            )?;
            if crate::abstract_ops::same_value(&value_constructor, &constructor, interp.gc_heap()) {
                return Ok(Value::Promise(*p));
            }
        } else {
            return Ok(Value::Promise(*p));
        }
    }
    Ok(Value::Promise(
        PromiseBuilder::new().fulfilled_runtime_rooted(interp, value, &[], &[args])?,
    ))
}

fn static_reject(interp: &mut Interpreter, args: &[Value]) -> Result<JsPromiseHandle, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(PromiseBuilder::new().rejected_runtime_rooted(interp, reason, &[], &[args])?)
}

fn static_resolve_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
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
    let reason = args.first().cloned().unwrap_or(Value::Undefined);
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
    if !is_object_like(&constructor) {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "Promise.try `this` is not an Object".to_string(),
        });
    }
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
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
    let call_result = interp.run_callable_sync(&exec, &callbackfn, Value::Undefined, forwarded);
    match call_result {
        Ok(value) => {
            call_capability_resolve(interp, &cap, value)?;
        }
        Err(crate::VmError::Uncaught { value }) => {
            let reason = crate::error_ops::vm_err_to_value(
                &crate::VmError::Uncaught { value },
                interp.gc_heap_mut(),
            );
            call_capability_reject(interp, &cap, reason)?;
        }
        Err(other) => {
            let reason = crate::error_ops::vm_err_to_value(&other, interp.gc_heap_mut());
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
    let cap =
        new_generic_promise_capability(interp, context.clone(), constructor.clone(), &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let promises = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_object_like(&promises) {
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
        Err(err) => return reject_capability_error(interp, &cap, promise_vm_error(name, err)),
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
        let keys_root = slots.keys_value().unwrap_or(Value::Undefined);
        interp.push_iteration_anchor(slots_root.clone());
        interp.push_iteration_anchor(keys_root.clone());
        for key in all_keys {
            let Some(vm_key) = vm_property_key_from_value(&key, interp.gc_heap()) else {
                continue;
            };
            let desc = match interp.ordinary_get_own_property_descriptor_value_runtime_rooted(
                &exec,
                promises.clone(),
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
                    return reject_capability_error(interp, &cap, promise_vm_error(name, err));
                }
            };
            if !desc.as_ref().is_some_and(|desc| desc.enumerable()) {
                continue;
            }
            let next_value = match keyed_get(interp, &exec, promises.clone(), &vm_key, name) {
                Ok(value) => value,
                Err(err) => return reject_capability_error(interp, &cap, err),
            };
            let i = slots.reserve_keyed_slot(
                interp,
                key.clone(),
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
            let value_anchor_base = interp.push_iteration_anchor(next_value.clone()) - 1;
            let entry_promise_result =
                call_promise_resolve(interp, &exec, &promise_resolve, &constructor, next_value);
            interp.pop_iteration_anchors_to(value_anchor_base);
            let entry_promise = match entry_promise_result {
                Ok(value) => value,
                Err(err) => return reject_capability_error(interp, &cap, err),
            };
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise.clone()) - 1;
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
                KeyedVariant::All => cap.reject.clone(),
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
        Ok(cap.promise.clone())
    })()
}

fn vm_property_key_from_value(
    key: &Value,
    heap: &otter_gc::GcHeap,
) -> Option<crate::VmPropertyKey<'static>> {
    match key {
        Value::String(s) => Some(crate::VmPropertyKey::OwnedString(s.to_lossy_string(heap))),
        Value::Symbol(sym) => Some(crate::VmPropertyKey::Symbol(sym.clone())),
        _ => None,
    }
}

fn keyed_get(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    receiver: Value,
    key: &crate::VmPropertyKey<'_>,
    name: &'static str,
) -> Result<Value, NativeError> {
    match interp
        .ordinary_get_value(context, receiver.clone(), receiver.clone(), key, 0)
        .map_err(|err| promise_vm_error(name, err))?
    {
        crate::VmGetOutcome::Value(value) => Ok(value),
        crate::VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, receiver, SmallVec::new())
            .map_err(|err| promise_vm_error(name, err)),
    }
}

fn keyed_element_function(
    interp: &mut Interpreter,
    slots: Rc<PromiseSlots>,
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
        Rc::new(move |visitor: &mut SlotVisitor<'_>| {
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
        smallvec![cap.promise.clone(), cap.resolve.clone(), cap.reject.clone()],
        trace_slots,
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let payload = args.first().cloned().unwrap_or(Value::Undefined);
            let value = match variant {
                KeyedVariant::All => payload,
                KeyedVariant::AllSettled => build_settled_record(fulfilled, payload, ctx)?,
            };
            if slots.fill(ctx.heap_mut(), index, value) {
                resolve_keyed_slots_native(ctx, &cap, &slots, name)?;
            }
            Ok(Value::Undefined)
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
    Ok(Value::Object(obj))
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
    Ok(Value::Object(obj))
}

fn define_keyed_result_properties(
    obj: crate::object::JsObject,
    heap: &mut otter_gc::GcHeap,
    keys: &[Value],
    values: &[Value],
    name: &'static str,
) -> Result<(), NativeError> {
    for (key, value) in keys.iter().zip(values.iter()) {
        let desc = crate::object::PropertyDescriptor::data(value.clone(), true, true, true);
        let ok = match key {
            Value::String(s) => {
                let key = s.to_lossy_string(heap);
                crate::object::define_own_property(obj, heap, &key, desc)
            }
            Value::Symbol(sym) => crate::object::define_own_symbol_property(obj, heap, sym, desc),
            _ => true,
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
    let cap =
        new_generic_promise_capability(interp, context.clone(), constructor.clone(), &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.all",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            return reject_capability_error(interp, &cap, promise_vm_error("Promise.all", err));
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator.clone()) - 1;
    interp.push_iteration_anchor(next_method.clone());
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
        interp.push_iteration_anchor(slots_root.clone());
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    return reject_capability_error(
                        interp,
                        &cap,
                        promise_vm_error("Promise.all", err),
                    );
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
            let value_anchor_base = interp.push_iteration_anchor(next_value.clone()) - 1;
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
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise.clone()) - 1;
            let cap_for_fulfill = cap.clone();
            let slots_for_trace = slots.clone();
            let trace_slots = Rc::new(move |visitor: &mut SlotVisitor<'_>| {
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
                smallvec![cap.promise.clone(), cap.resolve.clone(), cap.reject.clone()],
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
                move |ctx, args, _captures| {
                    let v = args.first().cloned().unwrap_or(Value::Undefined);
                    if slots_for_fulfill.fill(ctx.heap_mut(), i, v) {
                        let collected = slots_for_fulfill.collect_values(ctx.heap());
                        let arr = ctx.array_from_elements_with_roots(
                            collected.iter().cloned(),
                            &[
                                &cap_for_fulfill.promise,
                                &cap_for_fulfill.resolve,
                                &cap_for_fulfill.reject,
                            ],
                            &[collected.as_slice()],
                        )?;
                        let interp = ctx.interp_mut();
                        call_capability_resolve(interp, &cap_for_fulfill, Value::Array(arr))?;
                    }
                    Ok(Value::Undefined)
                },
            )?;
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, on_fulfill, cap.reject.clone());
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
            if let Err(err) = call_capability_resolve(interp, &cap, Value::Array(arr)) {
                return reject_capability_error(interp, &cap, err);
            }
        }
        Ok(cap.promise.clone())
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
    let cap =
        new_generic_promise_capability(interp, context.clone(), constructor.clone(), &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.race",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            return reject_capability_error(interp, &cap, promise_vm_error("Promise.race", err));
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator.clone()) - 1;
    interp.push_iteration_anchor(next_method.clone());
    let outcome = (|| -> Result<Value, NativeError> {
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    return reject_capability_error(
                        interp,
                        &cap,
                        promise_vm_error("Promise.race", err),
                    );
                }
            };
            let value_anchor_base = interp.push_iteration_anchor(next_value.clone()) - 1;
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
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise.clone()) - 1;
            let attach_result = attach_then_value(
                interp,
                &exec,
                entry_promise,
                cap.resolve.clone(),
                cap.reject.clone(),
            );
            interp.pop_iteration_anchors_to(entry_anchor_base);
            if let Err(err) = attach_result {
                let _ = interp.iterator_close_sync(&exec, &iterator);
                return reject_capability_error(interp, &cap, err);
            }
        }
        Ok(cap.promise.clone())
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
    let cap =
        new_generic_promise_capability(interp, context.clone(), constructor.clone(), &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.allSettled",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            return reject_capability_error(
                interp,
                &cap,
                promise_vm_error("Promise.allSettled", err),
            );
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator.clone()) - 1;
    interp.push_iteration_anchor(next_method.clone());
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
        interp.push_iteration_anchor(slots_root.clone());
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    return reject_capability_error(
                        interp,
                        &cap,
                        promise_vm_error("Promise.allSettled", err),
                    );
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
            let value_anchor_base = interp.push_iteration_anchor(next_value.clone()) - 1;
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
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise.clone()) - 1;
            let on_fulfill = {
                let slots = slots.clone();
                let cap = cap.clone();
                let promise_root = cap.promise.clone();
                let resolve_root = cap.resolve.clone();
                let reject_root = cap.reject.clone();
                let trace_slots = {
                    let slots = slots.clone();
                    let cap = cap.clone();
                    Rc::new(move |visitor: &mut SlotVisitor<'_>| {
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
                    smallvec![cap.promise.clone(), cap.resolve.clone(), cap.reject.clone()],
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
                    move |ctx, args, _captures| {
                        let v = args.first().cloned().unwrap_or(Value::Undefined);
                        let record = build_settled_record(true, v, ctx)?;
                        if slots.fill(ctx.heap_mut(), i, record) {
                            let collected = slots.collect_values(ctx.heap());
                            let arr = ctx.array_from_elements_with_roots(
                                collected.iter().cloned(),
                                &[&cap.promise, &cap.resolve, &cap.reject],
                                &[collected.as_slice()],
                            )?;
                            let interp = ctx.interp_mut();
                            call_capability_resolve(interp, &cap, Value::Array(arr))?;
                        }
                        Ok(Value::Undefined)
                    },
                )?
            };
            let on_reject = {
                let slots = slots.clone();
                let cap = cap.clone();
                let promise_root = cap.promise.clone();
                let resolve_root = cap.resolve.clone();
                let reject_root = cap.reject.clone();
                let trace_slots = {
                    let slots = slots.clone();
                    let cap = cap.clone();
                    Rc::new(move |visitor: &mut SlotVisitor<'_>| {
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
                    smallvec![cap.promise.clone(), cap.resolve.clone(), cap.reject.clone()],
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
                    move |ctx, args, _captures| {
                        let r = args.first().cloned().unwrap_or(Value::Undefined);
                        let record = build_settled_record(false, r, ctx)?;
                        if slots.fill(ctx.heap_mut(), i, record) {
                            let collected = slots.collect_values(ctx.heap());
                            let arr = ctx.array_from_elements_with_roots(
                                collected.iter().cloned(),
                                &[&cap.promise, &cap.resolve, &cap.reject],
                                &[collected.as_slice()],
                            )?;
                            let interp = ctx.interp_mut();
                            call_capability_resolve(interp, &cap, Value::Array(arr))?;
                        }
                        Ok(Value::Undefined)
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
            if let Err(err) = call_capability_resolve(interp, &cap, Value::Array(arr)) {
                return reject_capability_error(interp, &cap, err);
            }
        }
        Ok(cap.promise.clone())
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
    ctx.set_property_with_roots(obj, "status", Value::String(status), &[&payload], &[])
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    ctx.set_property(obj, key, payload)
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    Ok(Value::Object(obj))
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
    let obj_value = Value::Object(obj);
    let arr = interp
        .alloc_runtime_rooted_array_from_values(
            errors.iter().cloned(),
            &[&obj_value],
            &[errors.as_slice()],
        )
        .map_err(|_| oom_native("Promise.any"))?;
    interp
        .set_property(obj, "errors", Value::Array(arr))
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    Ok(Value::Object(obj))
}

fn make_aggregate_error_native_rooted(
    ctx: &mut NativeCtx<'_>,
    registry: &ErrorClassRegistry,
    errors: Vec<Value>,
) -> Result<Value, NativeError> {
    let message = aggregate_error_message(ctx.heap_mut())?;
    let obj = ctx
        .alloc_object_with_roots(&[&message], &[&errors])
        .map_err(|_| oom_native("Promise.any"))?;
    {
        let gc_heap = ctx.heap_mut();
        crate::object::set_prototype(
            obj,
            gc_heap,
            Some(registry.prototype(ErrorKind::AggregateError)),
        );
    }
    ctx.set_property_with_roots(obj, "message", message, &[], &[&errors])
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    let obj_value = Value::Object(obj);
    let arr = ctx
        .array_from_elements_with_roots(errors.iter().cloned(), &[&obj_value], &[errors.as_slice()])
        .map_err(|_| oom_native("Promise.any"))?;
    ctx.set_property(obj, "errors", Value::Array(arr))
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    Ok(Value::Object(obj))
}

fn aggregate_error_message(heap: &mut otter_gc::GcHeap) -> Result<Value, NativeError> {
    Ok(Value::String(
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
        Ok(Value::Object(proto)) => interp
            .alloc_runtime_rooted_object_with_proto(proto, value_roots, slice_roots)
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
    let cap =
        new_generic_promise_capability(interp, context.clone(), constructor.clone(), &[], &[args])?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.any",
        reason: "missing execution context".to_string(),
    })?;
    let promise_resolve = match get_promise_resolve(interp, &exec, &constructor) {
        Ok(value) => value,
        Err(err) => return reject_capability_error(interp, &cap, err),
    };
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let (iterator, next_method) = match interp.get_iterator_sync(&exec, &iterable) {
        Ok(record) => record,
        Err(err) => {
            return reject_capability_error(interp, &cap, promise_vm_error("Promise.any", err));
        }
    };
    let anchor_base = interp.push_iteration_anchor(iterator.clone()) - 1;
    interp.push_iteration_anchor(next_method.clone());
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
        interp.push_iteration_anchor(errors_root.clone());
        loop {
            let next_value = match interp.iterator_step_sync(&exec, &iterator, &next_method) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(err) => {
                    return reject_capability_error(
                        interp,
                        &cap,
                        promise_vm_error("Promise.any", err),
                    );
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
            let value_anchor_base = interp.push_iteration_anchor(next_value.clone()) - 1;
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
            let entry_anchor_base = interp.push_iteration_anchor(entry_promise.clone()) - 1;
            let on_reject = {
                let errors = errors.clone();
                let registry = registry.clone();
                let cap = cap.clone();
                let promise_root = cap.promise.clone();
                let resolve_root = cap.resolve.clone();
                let reject_root = cap.reject.clone();
                let trace_errors = {
                    let errors = errors.clone();
                    let cap = cap.clone();
                    Rc::new(move |visitor: &mut SlotVisitor<'_>| {
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
                    smallvec![cap.promise.clone(), cap.resolve.clone(), cap.reject.clone()],
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
                    move |ctx, args, _captures| {
                        let reason = args.first().cloned().unwrap_or(Value::Undefined);
                        if errors.fill(ctx.heap_mut(), i, reason) {
                            let collected = errors.collect_values(ctx.heap());
                            let agg =
                                make_aggregate_error_native_rooted(ctx, &registry, collected)?;
                            let interp = ctx.interp_mut();
                            call_capability_reject(interp, &cap, agg)?;
                        }
                        Ok(Value::Undefined)
                    },
                )?
            };
            let attach_result =
                attach_then_value(interp, &exec, entry_promise, cap.resolve.clone(), on_reject);
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
        Ok(cap.promise.clone())
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
        .set_property_with_extra_roots(obj, "promise", cap.promise.clone(), &mut cap_roots)
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
        .set_property_with_extra_roots(obj, "resolve", cap.resolve.clone(), &mut cap_roots)
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
        .set_property_with_extra_roots(obj, "reject", cap.reject.clone(), &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    Ok(Value::Object(obj))
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
        .set_property_with_extra_roots(obj, "promise", cap.promise.clone(), &mut cap_roots)
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
        .set_property_with_extra_roots(obj, "resolve", cap.resolve.clone(), &mut cap_roots)
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
        .set_property_with_extra_roots(obj, "reject", cap.reject.clone(), &mut cap_roots)
        .map_err(|err| NativeError::TypeError {
            name: "Promise.withResolvers",
            reason: err.to_string(),
        })?;
    Ok(Value::Object(obj))
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
    let promise_root = Value::Promise(*promise);
    let default_ctor = builtin_promise_constructor(interp)?;
    let c = species_constructor_runtime(interp, &exec, &promise_root, &default_ctor, NAME)?;

    let on_fulfilled = match args.first() {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
        _ => None,
    };
    let on_rejected = match args.get(1) {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
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
        new_generic_promise_capability(interp, context.clone(), c.clone(), &roots, &[])?
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
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
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
    let promise_root = Value::Promise(*promise);
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
        Err(_) => return Value::Undefined,
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
    let promise_root = Value::Promise(*promise);
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
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| resolve_native_body(ctx, args, promise, &captured_context),
    )
}

fn make_resolve_native_stack_rooted(
    interp: &mut Interpreter,
    stack: &SmallVec<[Frame; 8]>,
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
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| resolve_native_body(ctx, args, promise, &captured_context),
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
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| resolve_native_body(ctx, args, promise, &captured_context),
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
        return Ok(Value::Undefined);
    }

    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if let Value::Promise(inner) = value {
        let value_root = Value::Promise(inner);
        let (on_fulfill, on_reject) =
            make_resolve_adoption_handlers_native_rooted(ctx, promise, &[&value_root], &[args])?;
        let interp = ctx.interp_mut();
        attach_then(interp, context, &inner, Some(on_fulfill), Some(on_reject));
        return Ok(Value::Undefined);
    }

    let interp = ctx.interp_mut();
    let jobs = promise.fulfill(interp.gc_heap_mut(), value);
    drain_jobs(interp, jobs);
    Ok(Value::Undefined)
}

fn make_resolve_adoption_handlers_native_rooted(
    ctx: &mut NativeCtx<'_>,
    resolver: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<(Value, Value), otter_gc::OutOfMemory> {
    let resolver_value = Value::Promise(resolver);
    let mut fulfill_roots = Vec::with_capacity(value_roots.len() + 1);
    fulfill_roots.extend_from_slice(value_roots);
    fulfill_roots.push(&resolver_value);
    let on_fulfill = native_value_with_captures_native_rooted(
        ctx,
        "Promise resolve adopt fulfill",
        smallvec![resolver_value.clone()],
        &fulfill_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let interp = ctx.interp_mut();
            let v = args.first().cloned().unwrap_or(Value::Undefined);
            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
            drain_jobs(interp, jobs);
            Ok(Value::Undefined)
        },
    )?;

    let resolver_for_reject = resolver;
    let resolver_reject_value = Value::Promise(resolver_for_reject);
    let mut reject_roots = Vec::with_capacity(value_roots.len() + 2);
    reject_roots.extend_from_slice(value_roots);
    reject_roots.push(&resolver_reject_value);
    reject_roots.push(&on_fulfill);
    let on_reject = native_value_with_captures_native_rooted(
        ctx,
        "Promise resolve adopt reject",
        smallvec![resolver_reject_value.clone()],
        &reject_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let interp = ctx.interp_mut();
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            let jobs = resolver_for_reject.reject(interp.gc_heap_mut(), reason);
            drain_jobs(interp, jobs);
            Ok(Value::Undefined)
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
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                let jobs = promise.reject(interp.gc_heap_mut(), reason);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
        },
    )
}

fn make_reject_native_stack_rooted(
    interp: &mut Interpreter,
    stack: &SmallVec<[Frame; 8]>,
    promise: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_stack(
        interp,
        stack,
        "",
        1,
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                let jobs = promise.reject(interp.gc_heap_mut(), reason);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
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
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                let jobs = promise.reject(interp.gc_heap_mut(), reason);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
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

    #[test]
    fn aggregate_error_runtime_builder_uses_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let registry = interp.error_classes_clone();
        let errors = vec![Value::Number(NumberValue::from_i32(1))];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = make_aggregate_error_runtime_rooted(&mut interp, &registry, errors)
            .expect("aggregate error");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise.any AggregateError runtime path should allocate object and errors array in young space"
        );
        let Value::Object(obj) = result else {
            panic!("expected object");
        };
        assert!(matches!(
            crate::object::get(obj, interp.gc_heap(), "errors"),
            Some(Value::Array(_))
        ));
    }

    #[test]
    fn aggregate_error_native_builder_uses_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let registry = interp.error_classes_clone();
        let errors = vec![Value::Number(NumberValue::from_i32(2))];
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
        let Value::Object(obj) = result else {
            panic!("expected object");
        };
        assert!(matches!(
            crate::object::get(obj, interp.gc_heap(), "errors"),
            Some(Value::Array(_))
        ));
    }

    #[test]
    fn promise_static_resolve_uses_runtime_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let args = [Value::Number(NumberValue::from_i32(7))];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let constructor = Value::Undefined;
        let promise_value =
            static_resolve(&mut interp, None, constructor, &args).expect("Promise.resolve");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise.resolve should allocate non-promise results through runtime-rooted young allocation"
        );
        let Value::Promise(promise) = promise_value else {
            panic!("expected promise");
        };
        assert!(matches!(
            promise.state(interp.gc_heap()),
            PromiseState::Fulfilled(Value::Number(_))
        ));
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
        assert!(matches!(cap.promise, Value::Promise(_)));
        assert!(matches!(cap.resolve, Value::NativeFunction(_)));
        assert!(matches!(cap.reject, Value::NativeFunction(_)));
    }

    #[test]
    fn promise_constructor_builder_uses_native_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        let executor = Value::Number(NumberValue::from_i32(17));
        let args = vec![executor.clone()];

        let (handle, resolve, reject) = {
            let mut ctx = NativeCtx::new_with_call_info(
                &mut interp,
                NativeCallInfo::construct(
                    Value::Number(NumberValue::from_i32(1)),
                    Some(Value::Number(NumberValue::from_i32(2))),
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

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
//! - [`construct`] — `new Promise(executor)` body.
//! - [`statics_call`] — dispatcher for `Promise.<name>(args...)`
//!   (`resolve`, `reject`, `all`, `race`).
//! - [`prototype_call`] — dispatcher for
//!   `promise.<name>(args...)` (`then`, `catch`, `finally`).
//! - [`PromiseBuilder`] / [`make_capability`] —
//!   `NewPromiseCapability` (§27.2.1.5).
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
    NativeError, native_value_with_captures_unchecked,
    native_value_with_captures_unchecked_with_roots, native_value_with_trace_unchecked_with_roots,
};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
    PromiseThenOutcome,
};
use crate::string::{JsString, StringHeap};
use crate::{Frame, Interpreter, Microtask, NativeCtx, Value};
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::{SmallVec, smallvec};
use std::cell::{Cell, OnceCell};
use std::rc::Rc;

struct PromiseSlots {
    values: Vec<OnceCell<Value>>,
    remaining: Cell<usize>,
}

impl PromiseSlots {
    fn new(total: usize) -> Rc<Self> {
        let values = std::iter::repeat_with(OnceCell::new).take(total).collect();
        Rc::new(Self {
            values,
            remaining: Cell::new(total),
        })
    }

    fn trace(&self, visitor: &mut SlotVisitor<'_>) {
        for value in self.values.iter().filter_map(OnceCell::get) {
            value.trace_value_slots(visitor);
        }
    }

    fn fill(&self, index: usize, value: Value) -> bool {
        if self.values[index].set(value).is_err() {
            return false;
        }
        let count = self.remaining.get().saturating_sub(1);
        self.remaining.set(count);
        count == 0
    }

    fn collect_values(&self) -> Vec<Value> {
        self.values
            .iter()
            .map(|slot| slot.get().cloned().unwrap_or(Value::Undefined))
            .collect()
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

    /// Construct a fresh pending promise handle.
    pub fn pending(
        &self,
        heap: &mut otter_gc::GcHeap,
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        JsPromiseHandle::pending(heap)
    }

    /// Construct a pre-fulfilled promise handle.
    pub fn fulfilled(
        &self,
        heap: &mut otter_gc::GcHeap,
        value: Value,
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        JsPromiseHandle::fulfilled(heap, value)
    }

    /// Construct a pre-rejected promise handle.
    pub fn rejected(
        &self,
        heap: &mut otter_gc::GcHeap,
        reason: Value,
    ) -> Result<JsPromiseHandle, otter_gc::OutOfMemory> {
        JsPromiseHandle::rejected(heap, reason)
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

    /// Foundation `Promise` constructor body. Builds a pending
    /// promise, hands native resolve/reject to the executor, and
    /// returns the promise value.
    ///
    /// The executor itself is invoked by the caller (the VM
    /// dispatcher) — this function only produces the value plumbing.
    pub fn construct(
        &self,
        heap: &mut otter_gc::GcHeap,
    ) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
        let promise = self.pending(heap)?;
        let resolve = make_resolve_native(heap, promise, self.context.clone())?;
        let reject = make_reject_native(heap, promise)?;
        Ok((promise, resolve, reject))
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

    /// `NewPromiseCapability` — produce the `{promise, resolve,
    /// reject}` triple over a fresh pending promise.
    pub fn capability(
        &self,
        heap: &mut otter_gc::GcHeap,
    ) -> Result<PromiseCapability, otter_gc::OutOfMemory> {
        let (handle, resolve, reject) = self.construct(heap)?;
        Ok(PromiseCapability {
            promise: Value::Promise(handle),
            resolve,
            reject,
            context: self.context.clone(),
        })
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

fn native_value_with_captures_runtime_rooted<F>(
    interp: &mut Interpreter,
    name: &'static str,
    captures: smallvec::SmallVec<[Value; 4]>,
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
    native_value_with_captures_unchecked_with_roots(
        interp.gc_heap_mut(),
        name,
        captures,
        &mut external_visit,
        call,
    )
}

fn native_value_with_captures_stack_rooted<F>(
    interp: &mut Interpreter,
    stack: &SmallVec<[Frame; 8]>,
    name: &'static str,
    captures: smallvec::SmallVec<[Value; 4]>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let roots = interp.collect_allocation_roots(stack);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
    };
    native_value_with_captures_unchecked_with_roots(
        interp.gc_heap_mut(),
        name,
        captures,
        &mut external_visit,
        call,
    )
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

fn native_value_with_trace_runtime_rooted<F>(
    interp: &mut Interpreter,
    name: &'static str,
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
    native_value_with_trace_unchecked_with_roots(
        interp.gc_heap_mut(),
        name,
        captures,
        trace,
        &mut external_visit,
        call,
    )
}

/// Foundation `Promise` constructor body. Builds a pending
/// promise, hands native resolve/reject to the executor, and
/// returns the promise value.
///
/// The executor itself is invoked by the caller (the VM
/// dispatcher) — this function only produces the value plumbing.
pub fn construct(
    heap: &mut otter_gc::GcHeap,
) -> Result<(JsPromiseHandle, Value, Value), otter_gc::OutOfMemory> {
    PromiseBuilder::new().construct(heap)
}

/// `NewPromiseCapability` — produce the `{promise, resolve,
/// reject}` triple over a fresh pending promise.
pub fn make_capability(
    heap: &mut otter_gc::GcHeap,
) -> Result<PromiseCapability, otter_gc::OutOfMemory> {
    PromiseBuilder::new().capability(heap)
}

/// Dispatch a `Promise.<method>(args...)` static call. Routes
/// the typed [`PromiseMethod`] emitted by the compiler.
pub fn statics_call(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    method: otter_bytecode::method_id::PromiseMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    use otter_bytecode::method_id::PromiseMethod as M;
    match method {
        M::Resolve => Ok(Value::Promise(static_resolve(interp, args)?)),
        M::Reject => Ok(Value::Promise(static_reject(interp, args)?)),
        M::All => static_all(interp, context, args),
        M::Race => static_race(interp, context, args),
        M::AllSettled => static_all_settled(interp, context, args),
        M::Any => static_any(interp, context, args),
        M::WithResolvers => static_with_resolvers(interp, context),
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
        "then" => Ok(method_then(interp, context, promise, args)),
        "catch" => Ok(method_catch(interp, context, promise, args)),
        "finally" => Ok(method_finally(interp, context, promise, args)),
        other => Err(NativeError::TypeError {
            name: "Promise.prototype",
            reason: format!("method `{other}` is not defined"),
        }),
    }
}

// -- statics --------------------------------------------------------

fn static_resolve(
    interp: &mut Interpreter,
    args: &[Value],
) -> Result<JsPromiseHandle, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    // Spec: if `value` is already a Promise we'd return it
    // unchanged. Foundation matches that for our concrete handle.
    if let Value::Promise(p) = &value {
        return Ok(*p);
    }
    Ok(PromiseBuilder::new().fulfilled_runtime_rooted(interp, value, &[], &[args])?)
}

fn static_reject(interp: &mut Interpreter, args: &[Value]) -> Result<JsPromiseHandle, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(PromiseBuilder::new().rejected_runtime_rooted(interp, reason, &[], &[args])?)
}

fn static_all(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries =
        match args.first() {
            Some(Value::Array(arr)) => {
                crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
            }
            _ => {
                // Foundation: only array iterables. Generic iterables
                // arrive once `Symbol.iterator` is in.
                return Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context.clone())
                        .rejected_runtime_rooted(interp, Value::Undefined, &[], &[args])?,
                ));
            }
        };
    let result = PromiseBuilder::with_optional_context(context.clone()).pending_runtime_rooted(
        interp,
        &[],
        &[args, entries.as_slice()],
    )?;
    if entries.is_empty() {
        // Spec: empty iterable resolves immediately with [].
        let result_value = Value::Promise(result);
        let arr = match interp.alloc_runtime_rooted_array_from_values(
            std::iter::empty::<Value>(),
            &[&result_value],
            &[],
        ) {
            Ok(arr) => arr,
            Err(_) => {
                return Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context.clone())
                        .rejected_runtime_rooted(
                            interp,
                            Value::Undefined,
                            &[&result_value],
                            &[args],
                        )?,
                ));
            }
        };
        let jobs = result.fulfill(interp.gc_heap_mut(), Value::Array(arr));
        for j in jobs.jobs {
            interp.microtasks_mut().enqueue(j);
        }
        return Ok(Value::Promise(result));
    }
    // Track per-slot fulfillment via shared Rust state that each
    // per-element resolver mutates. The native function bodies
    // install trace hooks over this state, so any fulfilled GC
    // values remain live while the combinator is pending.
    let slots = PromiseSlots::new(entries.len());
    for (i, entry) in entries.iter().cloned().enumerate() {
        let slots = slots.clone();
        let result_clone = result;
        let result_root = Value::Promise(result);
        let entry_promise = match entry {
            Value::Promise(p) => p,
            other => PromiseBuilder::with_optional_context(context.clone())
                .fulfilled_runtime_rooted(
                    interp,
                    other,
                    &[&result_root],
                    &[args, entries.as_slice()],
                )?,
        };
        let trace_slots = {
            let slots = slots.clone();
            Rc::new(move |visitor: &mut SlotVisitor<'_>| slots.trace(visitor))
        };
        let on_fulfill = native_value_with_trace_runtime_rooted(
            interp,
            "Promise.all element",
            smallvec![Value::Promise(result_clone)],
            trace_slots,
            &[&result_root],
            &[args, entries.as_slice()],
            move |ctx, args, _captures| {
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                if slots.fill(i, v) {
                    let collected = slots.collect_values();
                    let arr = ctx.array_from_elements(collected)?;
                    let interp = ctx.interp_mut();
                    let jobs = result_clone.fulfill(interp.gc_heap_mut(), Value::Array(arr));
                    for j in jobs.jobs {
                        interp.microtasks_mut().enqueue(j);
                    }
                }
                Ok(Value::Undefined)
            },
        )?;
        let result_for_reject = result;
        let on_reject = native_value_with_captures_runtime_rooted(
            interp,
            "Promise.all reject",
            smallvec![Value::Promise(result_for_reject)],
            &[&result_root, &on_fulfill],
            &[args, entries.as_slice()],
            move |ctx, args, _captures| {
                let interp = ctx.interp_mut();
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                let jobs = result_for_reject.reject(interp.gc_heap_mut(), reason);
                for j in jobs.jobs {
                    interp.microtasks_mut().enqueue(j);
                }
                Ok(Value::Undefined)
            },
        )?;
        attach_then(
            interp,
            context.clone(),
            &entry_promise,
            Some(on_fulfill),
            Some(on_reject),
        );
    }
    Ok(Value::Promise(result))
}

fn static_race(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries =
        match args.first() {
            Some(Value::Array(arr)) => {
                crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
            }
            _ => {
                return Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context.clone())
                        .rejected_runtime_rooted(interp, Value::Undefined, &[], &[args])?,
                ));
            }
        };
    let result = PromiseBuilder::with_optional_context(context.clone()).pending_runtime_rooted(
        interp,
        &[],
        &[args, entries.as_slice()],
    )?;
    for entry in entries.iter().cloned() {
        let result_root = Value::Promise(result);
        let entry_promise = match entry {
            Value::Promise(p) => p,
            other => PromiseBuilder::with_optional_context(context.clone())
                .fulfilled_runtime_rooted(
                    interp,
                    other,
                    &[&result_root],
                    &[args, entries.as_slice()],
                )?,
        };
        let result_for_fulfill = result;
        let on_fulfill = native_value_with_captures_runtime_rooted(
            interp,
            "Promise.race fulfill",
            smallvec![Value::Promise(result_for_fulfill)],
            &[&result_root],
            &[args, entries.as_slice()],
            move |ctx, args, _captures| {
                let interp = ctx.interp_mut();
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                let jobs = result_for_fulfill.fulfill(interp.gc_heap_mut(), v);
                for j in jobs.jobs {
                    interp.microtasks_mut().enqueue(j);
                }
                Ok(Value::Undefined)
            },
        )?;
        let result_for_reject = result;
        let on_reject = native_value_with_captures_runtime_rooted(
            interp,
            "Promise.race reject",
            smallvec![Value::Promise(result_for_reject)],
            &[&result_root, &on_fulfill],
            &[args, entries.as_slice()],
            move |ctx, args, _captures| {
                let interp = ctx.interp_mut();
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                let jobs = result_for_reject.reject(interp.gc_heap_mut(), reason);
                for j in jobs.jobs {
                    interp.microtasks_mut().enqueue(j);
                }
                Ok(Value::Undefined)
            },
        )?;
        attach_then(
            interp,
            context.clone(),
            &entry_promise,
            Some(on_fulfill),
            Some(on_reject),
        );
    }
    Ok(Value::Promise(result))
}

/// §27.2.4.2 `Promise.allSettled(iterable)` — settle with an array
/// of `{status: "fulfilled", value}` / `{status: "rejected",
/// reason}` records once every input promise settles.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.allsettled>
fn static_all_settled(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries =
        match args.first() {
            Some(Value::Array(arr)) => {
                crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
            }
            _ => {
                return Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context.clone())
                        .rejected_runtime_rooted(interp, Value::Undefined, &[], &[args])?,
                ));
            }
        };
    let result = PromiseBuilder::with_optional_context(context.clone()).pending_runtime_rooted(
        interp,
        &[],
        &[args, entries.as_slice()],
    )?;
    if entries.is_empty() {
        let result_value = Value::Promise(result);
        let arr = match interp.alloc_runtime_rooted_array_from_values(
            std::iter::empty::<Value>(),
            &[&result_value],
            &[],
        ) {
            Ok(arr) => arr,
            Err(_) => {
                return Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context.clone())
                        .rejected_runtime_rooted(
                            interp,
                            Value::Undefined,
                            &[&result_value],
                            &[args],
                        )?,
                ));
            }
        };
        let jobs = result.fulfill(interp.gc_heap_mut(), Value::Array(arr));
        for j in jobs.jobs {
            interp.microtasks_mut().enqueue(j);
        }
        return Ok(Value::Promise(result));
    }
    let slots = PromiseSlots::new(entries.len());
    let heap = interp.string_heap_clone();
    for (i, entry) in entries.iter().cloned().enumerate() {
        let result_root = Value::Promise(result);
        let entry_promise = match entry {
            Value::Promise(p) => p,
            other => PromiseBuilder::with_optional_context(context.clone())
                .fulfilled_runtime_rooted(
                    interp,
                    other,
                    &[&result_root],
                    &[args, entries.as_slice()],
                )?,
        };
        let on_fulfill = {
            let slots = slots.clone();
            let heap = heap.clone();
            let trace_slots = {
                let slots = slots.clone();
                Rc::new(move |visitor: &mut SlotVisitor<'_>| slots.trace(visitor))
            };
            native_value_with_trace_runtime_rooted(
                interp,
                "Promise.allSettled fulfill",
                smallvec![Value::Promise(result)],
                trace_slots,
                &[&result_root],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let v = args.first().cloned().unwrap_or(Value::Undefined);
                    let record = build_settled_record(true, v, &heap, ctx).map_err(|e| {
                        NativeError::TypeError {
                            name: "Promise",
                            reason: format!("string allocation failed: {e}"),
                        }
                    })?;
                    finalize_settled(&slots, &result, i, record, ctx)?;
                    Ok(Value::Undefined)
                },
            )?
        };
        let on_reject = {
            let slots = slots.clone();
            let heap = heap.clone();
            let trace_slots = {
                let slots = slots.clone();
                Rc::new(move |visitor: &mut SlotVisitor<'_>| slots.trace(visitor))
            };
            native_value_with_trace_runtime_rooted(
                interp,
                "Promise.allSettled reject",
                smallvec![Value::Promise(result)],
                trace_slots,
                &[&result_root, &on_fulfill],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let r = args.first().cloned().unwrap_or(Value::Undefined);
                    let record = build_settled_record(false, r, &heap, ctx).map_err(|e| {
                        NativeError::TypeError {
                            name: "Promise",
                            reason: format!("string allocation failed: {e}"),
                        }
                    })?;
                    finalize_settled(&slots, &result, i, record, ctx)?;
                    Ok(Value::Undefined)
                },
            )?
        };
        attach_then(
            interp,
            context.clone(),
            &entry_promise,
            Some(on_fulfill),
            Some(on_reject),
        );
    }
    Ok(Value::Promise(result))
}

fn build_settled_record(
    fulfilled: bool,
    payload: Value,
    heap: &std::sync::Arc<crate::string::StringHeap>,
    ctx: &mut NativeCtx<'_>,
) -> Result<Value, crate::string::StringError> {
    let status_text = if fulfilled { "fulfilled" } else { "rejected" };
    let status = crate::JsString::from_str(status_text, heap)?;
    let key = if fulfilled { "value" } else { "reason" };
    let obj = ctx
        .alloc_object()
        .map_err(|_| crate::string::StringError::OutOfMemory {
            requested_bytes: 0,
            heap_limit_bytes: 0,
        })?;
    let gc_heap = ctx.heap_mut();
    crate::object::set(obj, gc_heap, "status", Value::String(status));
    crate::object::set(obj, gc_heap, key, payload);
    Ok(Value::Object(obj))
}

fn finalize_settled(
    slots: &PromiseSlots,
    result: &JsPromiseHandle,
    index: usize,
    record: Value,
    ctx: &mut NativeCtx<'_>,
) -> Result<(), NativeError> {
    if slots.fill(index, record) {
        let collected = slots.collect_values();
        let arr = ctx.array_from_elements(collected)?;
        let interp = ctx.interp_mut();
        let jobs = result.fulfill(interp.gc_heap_mut(), Value::Array(arr));
        for j in jobs.jobs {
            interp.microtasks_mut().enqueue(j);
        }
    }
    Ok(())
}

fn make_aggregate_error_runtime_rooted(
    interp: &mut Interpreter,
    registry: &ErrorClassRegistry,
    string_heap: &StringHeap,
    errors: Vec<Value>,
) -> Result<Value, NativeError> {
    let message = aggregate_error_message(string_heap)?;
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
        crate::object::set(obj, gc_heap, "message", message);
    }
    let obj_value = Value::Object(obj);
    let arr = interp
        .alloc_runtime_rooted_array_from_values(errors, &[&obj_value], &[])
        .map_err(|_| oom_native("Promise.any"))?;
    crate::object::set(
        obj,
        interp.gc_heap_for_cx_mut(),
        "errors",
        Value::Array(arr),
    );
    Ok(Value::Object(obj))
}

fn make_aggregate_error_native_rooted(
    ctx: &mut NativeCtx<'_>,
    registry: &ErrorClassRegistry,
    string_heap: &StringHeap,
    errors: Vec<Value>,
) -> Result<Value, NativeError> {
    let message = aggregate_error_message(string_heap)?;
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
        crate::object::set(obj, gc_heap, "message", message);
    }
    let obj_value = Value::Object(obj);
    let arr = ctx
        .array_from_elements_with_roots(errors, &[&obj_value], &[])
        .map_err(|_| oom_native("Promise.any"))?;
    crate::object::set(obj, ctx.heap_mut(), "errors", Value::Array(arr));
    Ok(Value::Object(obj))
}

fn aggregate_error_message(string_heap: &StringHeap) -> Result<Value, NativeError> {
    Ok(Value::String(
        JsString::from_str("All promises were rejected", string_heap).map_err(|e| {
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

/// §27.2.4.3 `Promise.any(iterable)` — short-circuits on the first
/// fulfillment; rejects with `AggregateError` once every input
/// rejects.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.any>
fn static_any(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries =
        match args.first() {
            Some(Value::Array(arr)) => {
                crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
            }
            _ => {
                return Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context.clone())
                        .rejected_runtime_rooted(interp, Value::Undefined, &[], &[args])?,
                ));
            }
        };
    let result = PromiseBuilder::with_optional_context(context.clone()).pending_runtime_rooted(
        interp,
        &[],
        &[args, entries.as_slice()],
    )?;
    if entries.is_empty() {
        // Spec: empty iterable rejects with an AggregateError whose
        // `errors` array is empty.
        let registry = interp.error_classes_clone();
        let string_heap = interp.string_heap_clone();
        let agg = make_aggregate_error_runtime_rooted(interp, &registry, &string_heap, Vec::new())?;
        let jobs = result.reject(interp.gc_heap_mut(), agg);
        for j in jobs.jobs {
            interp.microtasks_mut().enqueue(j);
        }
        return Ok(Value::Promise(result));
    }
    let errors = PromiseSlots::new(entries.len());
    let heap = interp.string_heap_clone();
    let registry = interp.error_classes_clone();
    for (i, entry) in entries.iter().cloned().enumerate() {
        let result_root = Value::Promise(result);
        let entry_promise = match entry {
            Value::Promise(p) => p,
            other => PromiseBuilder::with_optional_context(context.clone())
                .fulfilled_runtime_rooted(
                    interp,
                    other,
                    &[&result_root],
                    &[args, entries.as_slice()],
                )?,
        };
        let on_fulfill = {
            native_value_with_captures_runtime_rooted(
                interp,
                "Promise.any fulfill",
                smallvec![Value::Promise(result)],
                &[&result_root],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let interp = ctx.interp_mut();
                    let v = args.first().cloned().unwrap_or(Value::Undefined);
                    let jobs = result.fulfill(interp.gc_heap_mut(), v);
                    for j in jobs.jobs {
                        interp.microtasks_mut().enqueue(j);
                    }
                    Ok(Value::Undefined)
                },
            )?
        };
        let on_reject = {
            let errors = errors.clone();
            let heap = heap.clone();
            let registry = registry.clone();
            let trace_errors = {
                let errors = errors.clone();
                Rc::new(move |visitor: &mut SlotVisitor<'_>| errors.trace(visitor))
            };
            native_value_with_trace_runtime_rooted(
                interp,
                "Promise.any reject",
                smallvec![Value::Promise(result)],
                trace_errors,
                &[&result_root, &on_fulfill],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let reason = args.first().cloned().unwrap_or(Value::Undefined);
                    if errors.fill(i, reason) {
                        let collected = errors.collect_values();
                        let agg =
                            make_aggregate_error_native_rooted(ctx, &registry, &heap, collected)?;
                        let interp = ctx.interp_mut();
                        let jobs = result.reject(interp.gc_heap_mut(), agg);
                        for j in jobs.jobs {
                            interp.microtasks_mut().enqueue(j);
                        }
                    }
                    Ok(Value::Undefined)
                },
            )?
        };
        attach_then(
            interp,
            context.clone(),
            &entry_promise,
            Some(on_fulfill),
            Some(on_reject),
        );
    }
    Ok(Value::Promise(result))
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
    let obj = match interp
        .alloc_runtime_rooted_object_with_roots(&[&cap.promise, &cap.resolve, &cap.reject], &[])
    {
        Ok(o) => o,
        Err(_) => return Ok(Value::Undefined),
    };
    let gc_heap = interp.gc_heap_for_cx_mut();
    crate::object::set(obj, gc_heap, "promise", cap.promise);
    crate::object::set(obj, gc_heap, "resolve", cap.resolve);
    crate::object::set(obj, gc_heap, "reject", cap.reject);
    Ok(Value::Object(obj))
}

// -- prototype methods ---------------------------------------------

fn method_then(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    args: &[Value],
) -> Value {
    let on_fulfilled = match args.first() {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
        _ => None,
    };
    let on_rejected = match args.get(1) {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
        _ => None,
    };
    perform_then_with_handlers(interp, context, promise, on_fulfilled, on_rejected)
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

fn method_finally(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    promise: &JsPromiseHandle,
    args: &[Value],
) -> Value {
    // Spec §27.2.5.3 — when `onFinally` is not callable, fall back
    // to a no-op `then` that propagates the original settlement.
    let on_finally = match args.first() {
        Some(v) if crate::is_callable_value(v) => v.clone(),
        _ => return perform_then_with_handlers(interp, context, promise, None, None),
    };
    // Build wrapper reactions that:
    // 1. Invoke `onFinally()` synchronously via a microtask.
    // 2. Forward the original fulfilment value / rejection reason
    //    through the chained promise (returning a fresh rejected
    //    promise re-throws through the resolve adoption path).
    // Foundation simplification: we don't await onFinally's return
    // value (the spec calls for that for thenable returns); the
    // common case of a synchronous cleanup is preserved.
    let then_handler = {
        let on_finally = on_finally.clone();
        match native_value_with_captures_unchecked(
            interp.gc_heap_mut(),
            "Promise.prototype.finally then",
            smallvec![on_finally.clone()],
            move |ctx, args, _captures| {
                let context = ctx.execution_context().cloned();
                let interp = ctx.interp_mut();
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                interp.microtasks_mut().enqueue(Microtask {
                    callee: on_finally.clone(),
                    this_value: Value::Undefined,
                    args: smallvec![],
                    context: context.clone(),
                    result_capability: None,
                    kind: crate::microtask::MicrotaskKind::Call,
                });
                Ok(value)
            },
        ) {
            Ok(value) => value,
            Err(_) => return perform_then_with_handlers(interp, context, promise, None, None),
        }
    };
    let catch_handler = {
        match native_value_with_captures_unchecked(
            interp.gc_heap_mut(),
            "Promise.prototype.finally catch",
            smallvec![on_finally.clone()],
            move |ctx, args, _captures| {
                let context = ctx.execution_context().cloned();
                let interp = ctx.interp_mut();
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                interp.microtasks_mut().enqueue(Microtask {
                    callee: on_finally.clone(),
                    this_value: Value::Undefined,
                    args: smallvec![],
                    context: context.clone(),
                    result_capability: None,
                    kind: crate::microtask::MicrotaskKind::Call,
                });
                // Re-raise the original rejection through the chained
                // promise. The resolve-native adopts the returned
                // promise's state, so a rejected handle propagates as
                // expected.
                Ok(Value::Promise(
                    PromiseBuilder::with_optional_context(context)
                        .rejected(interp.gc_heap_mut(), reason)?,
                ))
            },
        ) {
            Ok(value) => value,
            Err(_) => return perform_then_with_handlers(interp, context, promise, None, None),
        }
    };
    perform_then_with_handlers(
        interp,
        context,
        promise,
        Some(then_handler),
        Some(catch_handler),
    )
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

fn make_resolve_native(
    heap: &mut otter_gc::GcHeap,
    promise: JsPromiseHandle,
    context: Option<ExecutionContext>,
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    native_value_with_captures_unchecked(
        heap,
        "Promise resolve",
        smallvec![Value::Promise(promise)],
        move |ctx, args, _captures| {
            let context = ctx
                .execution_context()
                .cloned()
                .or_else(|| captured_context.clone());
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                // If the resolved value is itself a promise, adopt its
                // state. Spec §27.2.1.4 step 8: schedule a job that
                // forwards. Foundation: fulfill directly with the
                // inner value once that promise settles.
                if let Value::Promise(inner) = &value {
                    let resolver = promise;
                    let on_fulfill = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt fulfill",
                        smallvec![Value::Promise(resolver)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let v = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    let resolver_for_reject = promise;
                    let on_reject = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt reject",
                        smallvec![Value::Promise(resolver_for_reject)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let reason = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver_for_reject.reject(interp.gc_heap_mut(), reason);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    attach_then(interp, context, inner, Some(on_fulfill), Some(on_reject));
                    return Ok(Value::Undefined);
                }
                let jobs = promise.fulfill(interp.gc_heap_mut(), value);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
        },
    )
}

fn make_resolve_native_runtime_rooted(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    context: Option<ExecutionContext>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    native_value_with_captures_runtime_rooted(
        interp,
        "Promise resolve",
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let context = ctx
                .execution_context()
                .cloned()
                .or_else(|| captured_context.clone());
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                if let Value::Promise(inner) = &value {
                    let resolver = promise;
                    let on_fulfill = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt fulfill",
                        smallvec![Value::Promise(resolver)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let v = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    let resolver_for_reject = promise;
                    let on_reject = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt reject",
                        smallvec![Value::Promise(resolver_for_reject)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let reason = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver_for_reject.reject(interp.gc_heap_mut(), reason);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    attach_then(interp, context, inner, Some(on_fulfill), Some(on_reject));
                    return Ok(Value::Undefined);
                }
                let jobs = promise.fulfill(interp.gc_heap_mut(), value);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
        },
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
    native_value_with_captures_stack_rooted(
        interp,
        stack,
        "Promise resolve",
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let context = ctx
                .execution_context()
                .cloned()
                .or_else(|| captured_context.clone());
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                if let Value::Promise(inner) = &value {
                    let resolver = promise;
                    let on_fulfill = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt fulfill",
                        smallvec![Value::Promise(resolver)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let v = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    let resolver_for_reject = promise;
                    let on_reject = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt reject",
                        smallvec![Value::Promise(resolver_for_reject)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let reason = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver_for_reject.reject(interp.gc_heap_mut(), reason);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    attach_then(interp, context, inner, Some(on_fulfill), Some(on_reject));
                    return Ok(Value::Undefined);
                }
                let jobs = promise.fulfill(interp.gc_heap_mut(), value);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
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
    native_value_with_captures_native_rooted(
        ctx,
        "Promise resolve",
        smallvec![Value::Promise(promise)],
        value_roots,
        slice_roots,
        move |ctx, args, _captures| {
            let context = ctx
                .execution_context()
                .cloned()
                .or_else(|| captured_context.clone());
            let interp = ctx.interp_mut();
            if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                if let Value::Promise(inner) = &value {
                    let resolver = promise;
                    let on_fulfill = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt fulfill",
                        smallvec![Value::Promise(resolver)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let v = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver.fulfill(interp.gc_heap_mut(), v);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    let resolver_for_reject = promise;
                    let on_reject = native_value_with_captures_unchecked(
                        interp.gc_heap_mut(),
                        "Promise resolve adopt reject",
                        smallvec![Value::Promise(resolver_for_reject)],
                        move |ctx, args, _captures| {
                            let interp = ctx.interp_mut();
                            let reason = args.first().cloned().unwrap_or(Value::Undefined);
                            let jobs = resolver_for_reject.reject(interp.gc_heap_mut(), reason);
                            drain_jobs(interp, jobs);
                            Ok(Value::Undefined)
                        },
                    )?;
                    attach_then(interp, context, inner, Some(on_fulfill), Some(on_reject));
                    return Ok(Value::Undefined);
                }
                let jobs = promise.fulfill(interp.gc_heap_mut(), value);
                drain_jobs(interp, jobs);
            }
            Ok(Value::Undefined)
        },
    )
}

fn make_reject_native(
    heap: &mut otter_gc::GcHeap,
    promise: JsPromiseHandle,
) -> Result<Value, otter_gc::OutOfMemory> {
    native_value_with_captures_unchecked(
        heap,
        "Promise reject",
        smallvec![Value::Promise(promise)],
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

fn make_reject_native_runtime_rooted(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    native_value_with_captures_runtime_rooted(
        interp,
        "Promise reject",
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
    native_value_with_captures_stack_rooted(
        interp,
        stack,
        "Promise reject",
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
    native_value_with_captures_native_rooted(
        ctx,
        "Promise reject",
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
        let strings = interp.string_heap_clone();
        let errors = vec![Value::Number(NumberValue::from_i32(1))];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = make_aggregate_error_runtime_rooted(&mut interp, &registry, &strings, errors)
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
        let strings = interp.string_heap_clone();
        let errors = vec![Value::Number(NumberValue::from_i32(2))];
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = {
            let mut ctx = NativeCtx::new(&mut interp);
            make_aggregate_error_native_rooted(&mut ctx, &registry, &strings, errors)
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

        let promise = static_resolve(&mut interp, &args).expect("Promise.resolve");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Promise.resolve should allocate non-promise results through runtime-rooted young allocation"
        );
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

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
    NativeError, native_value_with_captures_unchecked_with_roots,
    native_value_with_trace_unchecked_with_roots, traced_native_value_with_length,
};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
    PromiseThenOutcome,
};
use crate::string::{JsString, StringHeap};
use crate::{Frame, Interpreter, Microtask, NativeCtx, Value};
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::{SmallVec, smallvec};
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

struct PromiseSlots {
    values: Vec<OnceCell<Value>>,
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

    pub(crate) fn rejected_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        reason: Value,
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
        JsPromiseHandle::rejected_with_roots(ctx.heap_mut(), reason, &mut external_visit)
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
        };
    }
    match method {
        M::Resolve => Ok(Value::Promise(static_resolve(interp, args)?)),
        M::Reject => Ok(Value::Promise(static_reject(interp, args)?)),
        M::All => static_all_generic(interp, context, constructor, args),
        M::Race => static_race_generic(interp, context, constructor, args),
        M::AllSettled => static_all_settled_generic(interp, context, constructor, args),
        M::Any => static_any_generic(interp, context, constructor, args),
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
    let executor = native_value_with_trace_runtime_rooted(
        interp,
        "Promise capability executor",
        SmallVec::new(),
        trace_state,
        roots.as_slice(),
        slice_roots,
        move |_ctx, args, _captures| state_for_call.call(args),
    )?;
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

fn invoke_constructor_resolve(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    constructor: &Value,
    value: Value,
) -> Result<Value, NativeError> {
    let resolve = get_callable_property(
        interp,
        context,
        constructor.clone(),
        "resolve",
        "Promise.resolve",
    )?;
    interp
        .run_callable_sync(context, &resolve, constructor.clone(), smallvec![value])
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

fn static_all_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries = match args.first() {
        Some(Value::Array(arr)) => {
            crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
        }
        _ => Vec::new(),
    };
    let cap = new_generic_promise_capability(
        interp,
        context.clone(),
        constructor.clone(),
        &[],
        &[args, entries.as_slice()],
    )?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.all",
        reason: "missing execution context".to_string(),
    })?;
    if entries.is_empty() {
        let arr = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                &[&cap.promise, &cap.resolve, &cap.reject],
                &[args],
            )
            .map_err(|_| oom_native("Promise.all"))?;
        call_capability_resolve(interp, &cap, Value::Array(arr))?;
        return Ok(cap.promise);
    }
    let slots = PromiseSlots::new(entries.len());
    for (i, entry) in entries.iter().cloned().enumerate() {
        let entry_promise = invoke_constructor_resolve(interp, &exec, &constructor, entry)?;
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
            &[&cap.promise, &cap.resolve, &cap.reject],
            &[args, entries.as_slice()],
            move |ctx, args, _captures| {
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                if slots_for_fulfill.fill(i, v) {
                    let collected = slots_for_fulfill.collect_values();
                    let arr = ctx.array_from_elements(collected)?;
                    let interp = ctx.interp_mut();
                    call_capability_resolve(interp, &cap_for_fulfill, Value::Array(arr))?;
                }
                Ok(Value::Undefined)
            },
        )?;
        attach_then_value(interp, &exec, entry_promise, on_fulfill, cap.reject.clone())?;
    }
    Ok(cap.promise)
}

fn static_race_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries = match args.first() {
        Some(Value::Array(arr)) => {
            crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
        }
        _ => Vec::new(),
    };
    let cap = new_generic_promise_capability(
        interp,
        context.clone(),
        constructor.clone(),
        &[],
        &[args, entries.as_slice()],
    )?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.race",
        reason: "missing execution context".to_string(),
    })?;
    for entry in entries.iter().cloned() {
        let entry_promise = invoke_constructor_resolve(interp, &exec, &constructor, entry)?;
        attach_then_value(
            interp,
            &exec,
            entry_promise,
            cap.resolve.clone(),
            cap.reject.clone(),
        )?;
    }
    Ok(cap.promise)
}

fn static_all_settled_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries = match args.first() {
        Some(Value::Array(arr)) => {
            crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
        }
        _ => Vec::new(),
    };
    let cap = new_generic_promise_capability(
        interp,
        context.clone(),
        constructor.clone(),
        &[],
        &[args, entries.as_slice()],
    )?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.allSettled",
        reason: "missing execution context".to_string(),
    })?;
    if entries.is_empty() {
        let arr = interp
            .alloc_runtime_rooted_array_from_values(
                std::iter::empty::<Value>(),
                &[&cap.promise, &cap.resolve, &cap.reject],
                &[args],
            )
            .map_err(|_| oom_native("Promise.allSettled"))?;
        call_capability_resolve(interp, &cap, Value::Array(arr))?;
        return Ok(cap.promise);
    }
    let slots = PromiseSlots::new(entries.len());
    let heap = interp.string_heap_clone();
    for (i, entry) in entries.iter().cloned().enumerate() {
        let entry_promise = invoke_constructor_resolve(interp, &exec, &constructor, entry)?;
        let on_fulfill = {
            let slots = slots.clone();
            let heap = heap.clone();
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
                &[&promise_root, &resolve_root, &reject_root],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let v = args.first().cloned().unwrap_or(Value::Undefined);
                    let record = build_settled_record(true, v, &heap, ctx)?;
                    if slots.fill(i, record) {
                        let collected = slots.collect_values();
                        let arr = ctx.array_from_elements(collected)?;
                        let interp = ctx.interp_mut();
                        call_capability_resolve(interp, &cap, Value::Array(arr))?;
                    }
                    Ok(Value::Undefined)
                },
            )?
        };
        let on_reject = {
            let slots = slots.clone();
            let heap = heap.clone();
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
                &[&promise_root, &resolve_root, &reject_root, &on_fulfill],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let r = args.first().cloned().unwrap_or(Value::Undefined);
                    let record = build_settled_record(false, r, &heap, ctx)?;
                    if slots.fill(i, record) {
                        let collected = slots.collect_values();
                        let arr = ctx.array_from_elements(collected)?;
                        let interp = ctx.interp_mut();
                        call_capability_resolve(interp, &cap, Value::Array(arr))?;
                    }
                    Ok(Value::Undefined)
                },
            )?
        };
        attach_then_value(interp, &exec, entry_promise, on_fulfill, on_reject)?;
    }
    Ok(cap.promise)
}

fn build_settled_record(
    fulfilled: bool,
    payload: Value,
    heap: &std::sync::Arc<crate::string::StringHeap>,
    ctx: &mut NativeCtx<'_>,
) -> Result<Value, NativeError> {
    let status_text = if fulfilled { "fulfilled" } else { "rejected" };
    let status =
        crate::JsString::from_str(status_text, heap).map_err(|e| NativeError::TypeError {
            name: "Promise",
            reason: format!("string allocation failed: {e}"),
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
        .alloc_runtime_rooted_array_from_values(errors, &[&obj_value], &[])
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
    }
    ctx.set_property_with_roots(obj, "message", message, &[], &[&errors])
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
    let obj_value = Value::Object(obj);
    let arr = ctx
        .array_from_elements_with_roots(errors, &[&obj_value], &[])
        .map_err(|_| oom_native("Promise.any"))?;
    ctx.set_property(obj, "errors", Value::Array(arr))
        .map_err(|err| NativeError::TypeError {
            name: "Promise",
            reason: err.to_string(),
        })?;
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

fn static_any_generic(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let entries = match args.first() {
        Some(Value::Array(arr)) => {
            crate::array::with_elements(*arr, interp.gc_heap(), |elements| elements.to_vec())
        }
        _ => Vec::new(),
    };
    let cap = new_generic_promise_capability(
        interp,
        context.clone(),
        constructor.clone(),
        &[],
        &[args, entries.as_slice()],
    )?;
    let exec = context.clone().ok_or_else(|| NativeError::TypeError {
        name: "Promise.any",
        reason: "missing execution context".to_string(),
    })?;
    if entries.is_empty() {
        let registry = interp.error_classes_clone();
        let string_heap = interp.string_heap_clone();
        let agg = make_aggregate_error_runtime_rooted(interp, &registry, &string_heap, Vec::new())?;
        call_capability_reject(interp, &cap, agg)?;
        return Ok(cap.promise);
    }
    let errors = PromiseSlots::new(entries.len());
    let heap = interp.string_heap_clone();
    let registry = interp.error_classes_clone();
    for (i, entry) in entries.iter().cloned().enumerate() {
        let entry_promise = invoke_constructor_resolve(interp, &exec, &constructor, entry)?;
        let on_reject = {
            let errors = errors.clone();
            let heap = heap.clone();
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
                &[&promise_root, &resolve_root, &reject_root],
                &[args, entries.as_slice()],
                move |ctx, args, _captures| {
                    let reason = args.first().cloned().unwrap_or(Value::Undefined);
                    if errors.fill(i, reason) {
                        let collected = errors.collect_values();
                        let agg =
                            make_aggregate_error_native_rooted(ctx, &registry, &heap, collected)?;
                        let interp = ctx.interp_mut();
                        call_capability_reject(interp, &cap, agg)?;
                    }
                    Ok(Value::Undefined)
                },
            )?
        };
        attach_then_value(interp, &exec, entry_promise, cap.resolve.clone(), on_reject)?;
    }
    Ok(cap.promise)
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
    let obj = interp
        .alloc_runtime_rooted_object_with_roots(&[&cap.promise, &cap.resolve, &cap.reject], &[])
        .map_err(|_| oom_native("Promise.withResolvers"))?;
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
    let promise_root = Value::Promise(*promise);
    let then_handler = {
        let on_finally_root = on_finally.clone();
        let on_finally_call = on_finally.clone();
        match native_value_with_captures_runtime_rooted(
            interp,
            "Promise.prototype.finally then",
            smallvec![on_finally_root.clone()],
            &[&promise_root, &on_finally_root],
            &[args],
            move |ctx, args, _captures| {
                let context = ctx.execution_context().cloned();
                let interp = ctx.interp_mut();
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                interp.microtasks_mut().enqueue(Microtask {
                    callee: on_finally_call.clone(),
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
        let on_finally_root = on_finally.clone();
        let on_finally_call = on_finally.clone();
        match native_value_with_captures_runtime_rooted(
            interp,
            "Promise.prototype.finally catch",
            smallvec![on_finally_root.clone()],
            &[&promise_root, &on_finally_root, &then_handler],
            &[args],
            move |ctx, args, _captures| {
                let context = ctx.execution_context().cloned();
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                let reason_root = reason.clone();
                let rejected = PromiseBuilder::with_optional_context(context.clone())
                    .rejected_native_rooted(
                        ctx,
                        reason,
                        &[&on_finally_call, &reason_root],
                        &[args],
                    )?;
                let interp = ctx.interp_mut();
                interp.microtasks_mut().enqueue(Microtask {
                    callee: on_finally_call.clone(),
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
                Ok(Value::Promise(rejected))
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
        "Promise resolve",
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
        "Promise resolve",
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
        "Promise resolve",
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
        "Promise reject",
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
        "Promise reject",
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
        "Promise reject",
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

//! `Promise` constructor + statics + prototype dispatch.
//!
//! Connects source-admitted capability construction, settlement and jobs:
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
//! - Native `resolve` / `reject` closures retain the promise and their
//!   shared AlreadyResolved cell in collector-traced captures. They read
//!   the current promise after every collecting allocation and settle
//!   at most once per spec §27.2.1.4 / §27.2.1.7.
//! - A reaction records its admitted source Option and callable-owned
//!   realm before queue publication. Native-only work keeps source None;
//!   bytecode callbacks resolve their exact defining function owner.
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
use crate::native_function::{NativeError, local_native_value_with_length};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
};
use crate::runtime_activation::CommittedValueError;
use crate::{Interpreter, Local, NativeCtx, Value};
use otter_gc::raw::RawGc;
use smallvec::{SmallVec, smallvec};
use std::cell::Cell;
use std::sync::Arc;

/// Shared countdown for one combinator run.
///
/// The values / keys arrays the old struct carried in traced `Cell`s
/// now ride each element function's capture list, where the native
/// body traces and relocates them. The only state genuinely shared
/// between the outer loop and the element functions is this counter —
/// not a JS value, so the `Arc` needs no trace hook and owns nothing
/// the GC has to know about.
struct PromiseSlots {
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

/// The GetCapabilitiesExecutor's shared state is an ordinary JS object
/// carried in the executor's capture list, so the native body's own
/// capture tracing covers it — no side trace hook, nothing for the GC
/// to reach outside the heap. §27.2.1.5.1 stores `[[Resolve]]` /
/// `[[Reject]]` as internal slots; two data properties on a plain
/// object express the same thing.
mod capability_executor_state {
    use super::{NativeCtx, NativeError, Value};

    pub(super) const RESOLVE: &str = "resolve";
    pub(super) const REJECT: &str = "reject";

    /// §27.2.1.5.1 GetCapabilitiesExecutor step 2-5, against the state
    /// object in `captures[0]`.
    pub(super) fn call(
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
        captures: &[Value],
    ) -> Result<Value, NativeError> {
        let state = captures[0]
            .as_object()
            .expect("capability executor state is an object");
        let heap = ctx.heap_mut();
        if crate::object::get(state, heap, RESOLVE).is_some() {
            return Err(NativeError::TypeError {
                name: "Promise",
                reason: "promise capability executor already has a resolve function".to_string(),
            });
        }
        if crate::object::get(state, heap, REJECT).is_some() {
            return Err(NativeError::TypeError {
                name: "Promise",
                reason: "promise capability executor already has a reject function".to_string(),
            });
        }
        let resolve = args.first().cloned().unwrap_or(Value::undefined());
        let reject = args.get(1).cloned().unwrap_or(Value::undefined());
        // Each store can grow the state's storage and move young values, so
        // the state and both functions ride scope handles across them.
        ctx.scope(|mut scope| {
            let state = scope.value(Value::object(state));
            let resolve = scope.value(resolve);
            let reject = scope.value(reject);
            if !scope.is_undefined(resolve) {
                scope.set(state, RESOLVE, resolve)?;
            }
            if !scope.is_undefined(reject) {
                scope.set(state, REJECT, reject)?;
            }
            Ok(Value::undefined())
        })
    }
}

impl PromiseSlots {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            remaining: Cell::new(1),
        })
    }

    /// One more pending element function.
    fn add_pending(&self) {
        self.remaining.set(self.remaining.get().saturating_add(1));
    }

    /// One element (or the iteration itself) settled; `true` when it
    /// was the last.
    fn settle_one(&self) -> bool {
        let count = self.remaining.get().saturating_sub(1);
        self.remaining.set(count);
        count == 0
    }
}

/// The array a combinator capture slot holds.
fn capture_array(value: Value) -> crate::array::JsArray {
    value
        .as_array()
        .expect("combinator capture always holds an array")
}

/// Append a hole slot to the values array and count it pending.
fn reserve_slot_scoped(
    slots: &PromiseSlots,
    interp: &mut Interpreter,
    values: Local<'_>,
) -> Result<usize, NativeError> {
    let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    let len = crate::array::push_with_roots(
        capture_array(interp.escape_scoped(values)),
        interp.gc_heap_mut(),
        Value::hole(),
        &mut no_extra_roots,
    )
    .map_err(NativeError::from)?;
    slots.add_pending();
    Ok(len - 1)
}

/// Append `key` to the keys array and a hole to the values array.
fn reserve_keyed_slot_scoped(
    slots: &PromiseSlots,
    interp: &mut Interpreter,
    values: Local<'_>,
    keys: Local<'_>,
    key: Local<'_>,
) -> Result<usize, NativeError> {
    let key = interp.escape_scoped(key);
    let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    crate::array::push_with_roots(
        capture_array(interp.escape_scoped(keys)),
        interp.gc_heap_mut(),
        key,
        &mut no_extra_roots,
    )
    .map_err(NativeError::from)?;
    let len = crate::array::push_with_roots(
        capture_array(interp.escape_scoped(values)),
        interp.gc_heap_mut(),
        Value::hole(),
        &mut no_extra_roots,
    )
    .map_err(NativeError::from)?;
    slots.add_pending();
    Ok(len - 1)
}

/// Copy the values array into a fresh dense result array.
fn materialize_array_scoped<'scope>(
    interp: &mut Interpreter,
    scope: &'scope crate::handles::HandleScope,
    values: Local<'scope>,
    name: &'static str,
) -> Result<Local<'scope>, NativeError> {
    let elements = collect_values(
        interp.gc_heap(),
        capture_array(interp.escape_scoped(values)),
    )
    .into_iter()
    .map(|value| interp.scoped_value(scope, value))
    .collect::<Vec<_>>();
    let result = interp
        .scoped_array(scope, elements.len())
        .map_err(|error| CommittedValueError::JavaScript(error).into_native(interp, name))?;
    for (index, value) in elements.into_iter().enumerate() {
        interp
            .scoped_set_index(scope, result, index, value)
            .map_err(|error| CommittedValueError::JavaScript(error).into_native(interp, name))?;
    }
    Ok(result)
}

/// Fill one hole slot; `true` when it was the last pending element.
fn fill_slot(
    slots: &PromiseSlots,
    heap: &mut otter_gc::GcHeap,
    values: crate::array::JsArray,
    index: usize,
    value: Value,
) -> bool {
    let did_fill = crate::array::with_elements_rewrite(values, heap, |elements| {
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
    slots.settle_one()
}

fn collect_values(heap: &otter_gc::GcHeap, values: crate::array::JsArray) -> Vec<Value> {
    crate::array::with_elements(values, heap, |elements| {
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

fn collect_keys(heap: &otter_gc::GcHeap, keys: crate::array::JsArray) -> Vec<Value> {
    crate::array::with_elements(keys, heap, |elements| elements.to_vec())
}

/// Root-aware helper for constructing ECMA-262 §27.2.1.5
/// `NewPromiseCapability` records with an explicit optional source.
/// Each method routes through the appropriate root walker
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

    /// Retain the admitted source for later settlement, or None for genuine
    /// native-only work. Bytecode children resolve their own exact FunctionID.
    #[must_use]
    pub fn with_context(context: Option<ExecutionContext>) -> Self {
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
        interp.note_born_rejection(promise, self.context.as_ref());
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
        interp.note_born_rejection(promise, self.context.as_ref());
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
            let already_resolved = alloc_already_resolved_cell(interp.gc_heap_mut())?;
            let flag_root = interp.json_root_push(already_resolved);
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives allocation");
            let already_resolved = interp.json_root_get(flag_root);
            let resolve = make_resolve_native_runtime_rooted(
                interp,
                promise,
                already_resolved,
                self.context.clone(),
                value_roots,
                slice_roots,
            )?;
            let resolve_root = interp.json_root_push(resolve);
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives resolve allocation");
            let already_resolved = interp.json_root_get(flag_root);
            let reject = make_reject_native_runtime_rooted(
                interp,
                promise,
                already_resolved,
                value_roots,
                slice_roots,
            )?;
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
            let already_resolved = alloc_already_resolved_cell(interp.gc_heap_mut())?;
            let flag_root = interp.json_root_push(already_resolved);
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives allocation");
            let already_resolved = interp.json_root_get(flag_root);
            let resolve = make_resolve_native_stack_rooted(
                interp,
                stack,
                promise,
                already_resolved,
                self.context.clone(),
                value_roots,
                slice_roots,
            )?;
            let resolve_root = interp.json_root_push(resolve);
            let promise = interp
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives resolve allocation");
            let already_resolved = interp.json_root_get(flag_root);
            let reject = make_reject_native_stack_rooted(
                interp,
                stack,
                promise,
                already_resolved,
                value_roots,
                slice_roots,
            )?;
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
            let already_resolved = alloc_already_resolved_cell(ctx.interp_mut().gc_heap_mut())?;
            let flag_root = ctx.interp_mut().json_root_push(already_resolved);
            let promise = ctx
                .interp_mut()
                .json_root_get(root_base)
                .as_promise()
                .expect("rooted promise survives allocation");
            let already_resolved = ctx.interp_mut().json_root_get(flag_root);
            let resolve = make_resolve_native_native_rooted(
                ctx,
                promise,
                already_resolved,
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
            let already_resolved = ctx.interp_mut().json_root_get(flag_root);
            let reject = make_reject_native_native_rooted(
                ctx,
                promise,
                already_resolved,
                value_roots,
                slice_roots,
            )?;
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

impl Interpreter {
    /// `Get(promise, "constructor")` — an ordinary read, so a user-defined
    /// accessor runs where the spec says it does.
    fn promise_resolve_constructor_of(
        &mut self,
        stack: &mut crate::activation_stack::ActivationStack,
        context: Option<&ExecutionContext>,
        value: Value,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let receiver = interp.scoped_value(scope, value);
            let value = interp.escape_scoped(receiver);
            match interp.ordinary_get_value(
                stack,
                context,
                value,
                value,
                &crate::VmPropertyKey::String("constructor"),
                0,
            )? {
                crate::VmGetOutcome::Value(found) => Ok(found),
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    // Proxy GetMethod can collect before exposing the target's
                    // accessor. Its receiver must come from the current root.
                    let getter = interp.scoped_value(scope, getter);
                    interp
                        .run_callable_sync_rooted(
                            stack,
                            context,
                            &interp.escape_scoped(getter),
                            interp.escape_scoped(receiver),
                            smallvec::SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)
                }
            }
        })
    }

    /// §27.2.4.7 `PromiseResolve(%Promise%, value)` — the promise a value
    /// stands for.
    ///
    /// The capability's own resolve function is what runs, because that is
    /// where the thenable job lives: a plain object with a `then` method is
    /// adopted exactly once, which a direct fulfilment would skip.
    ///
    /// # Errors
    /// Propagates whatever the value's `then` threw.
    pub(crate) fn promise_resolve_value(
        &mut self,
        stack: &mut crate::activation_stack::ActivationStack,
        context: Option<&ExecutionContext>,
        value: Value,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let value = interp.scoped_value(scope, value);
            if interp.escape_scoped(value).is_promise() {
                let constructor = interp.promise_resolve_constructor_of(
                    stack,
                    context,
                    interp.escape_scoped(value),
                )?;
                let promise_constructor =
                    crate::object::get(interp.global_this, &interp.gc_heap, "Promise")
                        .unwrap_or_else(Value::undefined);
                if crate::abstract_ops::same_value(
                    &constructor,
                    &promise_constructor,
                    &interp.gc_heap,
                ) {
                    return Ok(interp.escape_scoped(value));
                }
            }
            let capability = PromiseBuilder::with_context(context.cloned())
                .capability_stack_rooted(interp, stack, &[], &[])
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let promise = interp.scoped_value(scope, capability.promise);
            let resolve = interp.scoped_value(scope, capability.resolve);
            interp
                .run_callable_sync_rooted(
                    stack,
                    context,
                    &interp.escape_scoped(resolve),
                    Value::undefined(),
                    smallvec![interp.escape_scoped(value)],
                )
                .map_err(CommittedValueError::completed_call)?;
            Ok(interp.escape_scoped(promise))
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
    let roots = interp.collect_runtime_roots();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
    };
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        &mut external_visit,
        call,
    )?;
    Ok(interp.stamp_native_creation_realm(value))
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
    let roots = interp.collect_allocation_roots(stack);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &roots, value_roots, slice_roots);
    };
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        &mut external_visit,
        call,
    )?;
    Ok(interp.stamp_native_creation_realm(value))
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
    let value = local_native_value_with_length(
        ctx.heap_mut(),
        name,
        length,
        captures,
        &mut external_visit,
        call,
    )?;
    Ok(ctx.interp_mut().stamp_native_creation_realm(value))
}

/// Rebuild an element function's capability from its LIVE captures.
/// The captures live inside the native function's GC body and are
/// traced (and rewritten on relocation) with it; the `cap` clones the
/// body closure captured at build time are plain Rust copies that go
/// stale on the first moving collection — reading them is the
/// Promise-combinator use-after-move family.
/// The capability an element function captured, read from the traced
/// captures slab. The collector rewrites the slab in place when it moves the
/// capability's cells, so callers read it after their last allocation.
fn capability_from_captures(captures: &[Value], template: &PromiseCapability) -> PromiseCapability {
    // SAFETY: each index is in bounds of the live slab; the volatile reads
    // keep a pre-allocation load of the shared slice from being reused.
    let read = |index: usize, fallback: Value| {
        if index < captures.len() {
            unsafe { std::ptr::read_volatile(captures.as_ptr().add(index)) }
        } else {
            fallback
        }
    };
    PromiseCapability {
        promise: read(0, template.promise),
        resolve: read(1, template.resolve),
        reject: read(2, template.reject),
        context: template.context.clone(),
    }
}

fn promise_element_function<F>(
    interp: &mut Interpreter,
    name: &'static str,
    length: u8,
    captures: smallvec::SmallVec<[Value; 4]>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        name,
        length,
        captures,
        &mut no_extra_roots,
        call,
    )?;
    Ok(interp.stamp_native_creation_realm(value))
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
        M::Reject => Ok(Value::promise(static_reject(interp, context, args)?)),
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
        "catch" => method_catch(interp, context, promise, args),
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
/// `Invoke(receiver, "then", args)` passing exactly the given arguments —
/// the §27.2.5.3.1/.2 finally closures pass only their thunk, `catch`
/// passes « undefined, onRejected », and a patched `then` can observe
/// `arguments.length`.
pub fn invoke_then(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    const NAME: &str = "Promise.prototype";
    let exec = ctx.execution_context().cloned();
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let args: SmallVec<[_; 2]> = args.iter().map(|arg| scope.value(*arg)).collect();
        let receiver_raw = scope.raw(receiver);
        let args_raw: SmallVec<[Value; 2]> = args.iter().map(|arg| scope.raw(*arg)).collect();
        let result = scope.with_turn_parts(|interp, stack| {
            interp.with_handle_scope(|interp, scope| {
                let receiver = interp.scoped_value(scope, receiver_raw);
                let args: SmallVec<[_; 2]> = args_raw
                    .iter()
                    .map(|arg| interp.scoped_value(scope, *arg))
                    .collect();
                let receiver_raw = interp.escape_scoped(receiver);
                let then = get_callable_property(
                    interp,
                    stack,
                    exec.as_ref(),
                    receiver_raw,
                    "then",
                    NAME,
                )?;
                let then = interp.scoped_value(scope, then);
                let then = interp.escape_scoped(then);
                let receiver = interp.escape_scoped(receiver);
                let args: SmallVec<[Value; 8]> =
                    args.iter().map(|arg| interp.escape_scoped(*arg)).collect();
                interp
                    .run_callable_sync_rooted(stack, exec.as_ref(), &then, receiver, args)
                    .map_err(|err| crate::native_function::vm_to_native_error(interp, err, NAME))
            })
        })?;
        let result = scope.value(result);
        Ok(scope.finish(result))
    })
}

fn invoke_then_interp(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    exec: Option<&ExecutionContext>,
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
            .map_err(|err| crate::native_function::vm_to_native_error(interp, err, NAME))
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
    let exec = context.clone();
    let default_ctor = builtin_promise_constructor(interp)?;
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let on_finally = interp.scoped_value(scope, on_finally);
        let default_ctor = interp.scoped_value(scope, default_ctor);
        if !crate::is_callable_value(&interp.escape_scoped(on_finally)) {
            let receiver = interp.escape_scoped(receiver);
            let on_finally = interp.escape_scoped(on_finally);
            return invoke_then_interp(
                interp,
                stack,
                exec.as_ref(),
                receiver,
                on_finally,
                on_finally,
            );
        }
        let receiver_raw = interp.escape_scoped(receiver);
        let default_ctor_raw = interp.escape_scoped(default_ctor);
        let constructor = species_constructor_runtime(
            interp,
            stack,
            exec.as_ref(),
            &receiver_raw,
            &default_ctor_raw,
            NAME,
        )?;
        let constructor = interp.scoped_value(scope, constructor);
        let then_finally = make_then_finally(
            interp,
            exec.as_ref(),
            interp.escape_scoped(constructor),
            interp.escape_scoped(on_finally),
        )?;
        let then_finally = interp.scoped_value(scope, then_finally);
        let catch_finally = make_catch_finally(
            interp,
            exec.as_ref(),
            interp.escape_scoped(constructor),
            interp.escape_scoped(on_finally),
        )?;
        let catch_finally = interp.scoped_value(scope, catch_finally);
        invoke_then_interp(
            interp,
            stack,
            exec.as_ref(),
            interp.escape_scoped(receiver),
            interp.escape_scoped(then_finally),
            interp.escape_scoped(catch_finally),
        )
    })
}

fn make_then_finally(
    interp: &mut Interpreter,
    exec: Option<&ExecutionContext>,
    constructor: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![constructor, on_finally];
    let exec_for_call = exec.cloned();
    let constructor_root = constructor;
    let on_finally_root = on_finally;
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&constructor_root, &on_finally_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        1,
        captures,
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
                    get_promise_resolve(interp, stack, exec_for_call.as_ref(), &c_raw)
                })?;
                let resolve_fn = scope.value(resolve_fn);
                let resolved = scope.call(resolve_fn, c, &[result])?;
                let value_raw = scope.raw(value);
                let value_thunk = make_value_thunk(scope.context(), value_raw)?;
                let value_thunk = scope.value(value_thunk);
                let resolved_raw = scope.raw(resolved);
                let value_thunk_raw = scope.raw(value_thunk);
                let result = invoke_then(scope.context(), resolved_raw, &[value_thunk_raw])?;
                let result = scope.value(result);
                Ok(scope.finish(result))
            })
        },
    )?;
    Ok(interp.stamp_native_creation_realm(value))
}

fn make_catch_finally(
    interp: &mut Interpreter,
    exec: Option<&ExecutionContext>,
    constructor: Value,
    on_finally: Value,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![constructor, on_finally];
    let exec_for_call = exec.cloned();
    let constructor_root = constructor;
    let on_finally_root = on_finally;
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&constructor_root, &on_finally_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        1,
        captures,
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
                    get_promise_resolve(interp, stack, exec_for_call.as_ref(), &c_raw)
                })?;
                let resolve_fn = scope.value(resolve_fn);
                let resolved = scope.call(resolve_fn, c, &[result])?;
                let reason_raw = scope.raw(reason);
                let thrower = make_thrower(scope.context(), reason_raw)?;
                let thrower = scope.value(thrower);
                let resolved_raw = scope.raw(resolved);
                let thrower_raw = scope.raw(thrower);
                let result = invoke_then(scope.context(), resolved_raw, &[thrower_raw])?;
                let result = scope.value(result);
                Ok(scope.finish(result))
            })
        },
    )?;
    Ok(interp.stamp_native_creation_realm(value))
}

fn make_value_thunk(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![value];
    let value_root = value;
    let (interp, _) = ctx.interp_mut_and_context();
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&value_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        0,
        captures,
        &mut external_visit,
        move |_ctx, _args, captures| Ok(captures[0]),
    )?;
    Ok(interp.stamp_native_creation_realm(value))
}

fn make_thrower(ctx: &mut NativeCtx<'_>, reason: Value) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![reason];
    let reason_root = reason;
    let (interp, _) = ctx.interp_mut_and_context();
    let runtime_roots = interp.collect_runtime_roots();
    let value_roots: &[&Value] = &[&reason_root];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_runtime_roots(visitor, &runtime_roots, value_roots, &[]);
    };
    let value = local_native_value_with_length(
        interp.gc_heap_mut(),
        "",
        0,
        captures,
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
    )?;
    Ok(interp.stamp_native_creation_realm(value))
}

// -- statics --------------------------------------------------------

fn is_builtin_promise_constructor(interp: &Interpreter, constructor: &Value) -> bool {
    constructor
        .as_native_function()
        .is_some_and(|native| native.name_is(interp.gc_heap(), "Promise"))
}

fn builtin_promise_constructor(interp: &Interpreter) -> Result<Value, NativeError> {
    crate::object::get(*interp.global_this(), interp.gc_heap(), "Promise").ok_or_else(|| {
        NativeError::TypeError {
            name: "Promise",
            reason: "Promise constructor is not installed".to_string(),
        }
    })
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
    let exec = context.ok_or(NativeError::InvalidOperand)?;
    if !crate::is_constructor_runtime(constructor, &exec, interp.gc_heap()) {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "this value is not a constructor".to_string(),
        });
    }
    interp.with_handle_scope(|interp, scope| {
        let constructor_handle = interp.scoped_value(scope, *constructor);
        // The executor's shared state is a plain object in its capture
        // list; the native body traces it, so no side hook is needed.
        let state_obj = {
            let mut no_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
            crate::object::alloc_dictionary_object_with_roots(interp.gc_heap_mut(), &mut no_roots)?
        };
        let state_handle = interp.scoped_value(scope, Value::object(state_obj));
        let state_raw = interp.escape_scoped(state_handle);
        let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        // §27.2.1.5.1 — the GetCapabilitiesExecutor has length 2. The
        // state value rides the capture list, which the allocation
        // itself roots.
        let executor = crate::native_function::local_native_value_with_length(
            interp.gc_heap_mut(),
            "",
            2,
            smallvec![state_raw],
            &mut no_extra_roots,
            capability_executor_state::call,
        )?;
        let executor = interp.stamp_native_creation_realm(executor);
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
                0,
            )
            .map_err(|err| crate::native_function::vm_to_native_error(interp, err, "Promise"))?;
        let promise = interp.scoped_value(scope, promise);
        *constructor = interp.escape_scoped(constructor_handle);
        let state = interp
            .escape_scoped(state_handle)
            .as_object()
            .expect("capability executor state is an object");
        let resolve =
            crate::object::get(state, interp.gc_heap(), capability_executor_state::RESOLVE)
                .unwrap_or(Value::undefined());
        let reject = crate::object::get(state, interp.gc_heap(), capability_executor_state::REJECT)
            .unwrap_or(Value::undefined());
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
    let exec = cap.context.clone();
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
                exec.as_ref(),
                &function,
                Value::undefined(),
                smallvec![value],
            )
            .map_err(|err| crate::native_function::vm_to_native_error(interp, err, "Promise"));
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

/// Materialize a native abrupt completion with the current rooted turn.
/// Existing thrown values are consumed by the one VM throwable owner.
fn native_error_rejection_value(
    interp: &mut Interpreter,
    stack: &ActivationStack,
    err: NativeError,
) -> Result<Value, NativeError> {
    crate::error_ops::native_error_to_throwable_with_stack(interp, stack, None, err)
        .map_err(|error| crate::native_function::vm_to_native_error(interp, error, "Promise"))
}

fn reject_capability_error(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    err: NativeError,
) -> Result<Value, NativeError> {
    // Materializing the reason allocates; the capability rides handles
    // across it so a moved reject function is re-read before the call.
    let reason = interp.with_handle_scope(|interp, scope| {
        let handles = CapabilityHandles::park(interp, scope, cap);
        let reason = native_error_rejection_value(interp, stack, err);
        handles.refresh(interp, cap);
        reason
    })?;
    call_capability_reject(interp, stack, cap, reason)?;
    Ok(cap.promise)
}

/// Read an own/inherited property by string key without callability check.
///
/// Invokes any accessor `[[Get]]` exactly once per §10.1.8.1.
fn get_property_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<&ExecutionContext>,
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
            .map_err(|err| err.into_native(interp, name))?
        {
            crate::VmGetOutcome::Value(value) => Ok(value),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let getter = interp.scoped_value(scope, getter);
                let getter = interp.escape_scoped(getter);
                let receiver = interp.escape_scoped(receiver);
                interp
                    .run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(|err| crate::native_function::vm_to_native_error(interp, err, name))
            }
        }
    })
}

/// Read an own/inherited property by symbol key without callability check.
fn get_symbol_property_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<&ExecutionContext>,
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
            .map_err(|err| err.into_native(interp, name))?
        {
            crate::VmGetOutcome::Value(value) => Ok(value),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let getter = interp.scoped_value(scope, getter);
                let getter = interp.escape_scoped(getter);
                let receiver = interp.escape_scoped(receiver);
                interp
                    .run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(|err| crate::native_function::vm_to_native_error(interp, err, name))
            }
        }
    })
}

/// §7.3.21 `SpeciesConstructor(O, defaultConstructor)` — picks the
/// constructor to use when an algorithm needs a fresh instance derived
/// from `O`. Returns `defaultConstructor` when `O.constructor` is
/// `undefined`, throws `TypeError` if `constructor` is a non-object,
/// returns `defaultConstructor` when `C[@@species]` is `null`/`undefined`, and otherwise
/// returns `C[@@species]` after validating it is a constructor.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-speciesconstructor>
fn species_constructor_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<&ExecutionContext>,
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
            return Ok(interp.escape_scoped(default_ctor));
        }
        if is_builtin_promise_constructor(interp, &s_raw) {
            return Ok(s_raw);
        }
        let source = interp
            .callable_context(context, s_raw)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))?;
        let source = source.ok_or(NativeError::InvalidOperand)?;
        if crate::is_constructor_runtime(&s_raw, &source, interp.gc_heap()) {
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
    context: Option<&ExecutionContext>,
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
    context: Option<&ExecutionContext>,
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
    context: Option<&ExecutionContext>,
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
            .map_err(|err| {
                crate::native_function::vm_to_native_error(interp, err, "Promise.resolve")
            })
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
            let value_raw = interp.escape_scoped(value);
            let value_constructor = get_property_runtime(
                interp,
                stack,
                context.as_ref(),
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
        }
        // §27.2.4.7 PromiseResolve — settle a fresh promise through its
        // resolve function rather than fulfilling directly, so a thenable
        // value is adopted instead of becoming the fulfillment value verbatim.
        let cap = PromiseBuilder::with_context(context.clone()).capability_runtime_rooted(
            interp,
            &[],
            &[],
        )?;
        let cap_handles = CapabilityHandles::park(interp, scope, &cap);
        let value = interp.escape_scoped(value);
        let mut cap = cap_handles.current(interp, context.clone());
        call_capability_resolve(interp, stack, &mut cap, value)?;
        Ok(cap_handles.current(interp, context).promise)
    })
}

fn static_reject(
    interp: &mut Interpreter,
    context: Option<ExecutionContext>,
    args: &[Value],
) -> Result<JsPromiseHandle, NativeError> {
    let reason = args.first().cloned().unwrap_or(Value::undefined());
    Ok(
        PromiseBuilder::with_context(context).rejected_runtime_rooted(
            interp,
            reason,
            &[],
            &[args],
        )?,
    )
}

fn static_resolve_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    mut constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args.first().cloned().unwrap_or(Value::undefined());
    // §27.2.4.7 step 1 — a non-object receiver throws before the
    // pass-through check can compare against it.
    if !constructor.is_object_type() {
        return Err(NativeError::TypeError {
            name: "Promise.resolve",
            reason: "`this` is not an Object".to_string(),
        });
    }
    interp.with_handle_scope(|interp, scope| {
        let value = interp.scoped_value(scope, value);
        let constructor_handle = interp.scoped_value(scope, constructor);
        // §27.2.4.7 PromiseResolve step 2 — a promise whose `constructor`
        // is C passes through unchanged: no fresh capability, no extra
        // `then` tick.
        if interp.escape_scoped(value).is_promise() {
            let value_raw = interp.escape_scoped(value);
            let value_constructor = get_property_runtime(
                interp,
                stack,
                context.as_ref(),
                value_raw,
                "constructor",
                "Promise.resolve",
            )?;
            let value_constructor = interp.scoped_value(scope, value_constructor);
            if crate::abstract_ops::same_value(
                &interp.escape_scoped(value_constructor),
                &interp.escape_scoped(constructor_handle),
                interp.gc_heap(),
            ) {
                return Ok(interp.escape_scoped(value));
            }
        }
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
    let exec = context.clone().ok_or(NativeError::InvalidOperand)?;
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
        let call_result = interp.run_callable_sync_rooted(
            stack,
            Some(&exec),
            &callback,
            Value::undefined(),
            forwarded,
        );
        let mut cap = cap_handles.current(interp, Some(exec.clone()));
        match call_result {
            Ok(value) => call_capability_resolve(interp, stack, &mut cap, value)?,
            Err(other) => {
                // Call has already exhausted the callback's source handlers.
                // A completed engine/control failure cannot start rejection
                // work or be projected as a direct-source catchable OOM.
                if other.is_fatal() || matches!(other, crate::VmError::OutOfMemory { .. }) {
                    return Err(crate::native_function::vm_to_native_error(
                        interp,
                        other,
                        "Promise.try",
                    ));
                }
                let reason = interp
                    .vm_error_to_throwable_with_stack_roots(Some(&exec), stack, &other)
                    .map_err(|error| {
                        crate::native_function::vm_to_native_error(interp, error, "Promise.try")
                    })?;
                // Rendering the reason allocates; re-read the capability.
                let mut cap = cap_handles.current(interp, Some(exec.clone()));
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
    let exec = context.clone().ok_or(NativeError::InvalidOperand)?;
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
        let promise_resolve =
            match get_promise_resolve(interp, stack, Some(&exec), &constructor_raw) {
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
                let native = err.into_native(interp, name);
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let slots = PromiseSlots::new();
        let slots_handle = interp.scoped_array(scope, 0).map_err(|error| {
            CommittedValueError::JavaScript(error).into_native(interp, "Promise keyed combinator")
        })?;
        let keys_handle = interp.scoped_array(scope, 0).map_err(|error| {
            CommittedValueError::JavaScript(error).into_native(interp, "Promise keyed combinator")
        })?;

        for key in all_keys {
            let key_raw = interp.escape_scoped(key);
            let Some(vm_key) = vm_property_key_from_value(&key_raw, interp.gc_heap()) else {
                continue;
            };
            let promises_raw = interp.escape_scoped(promises);
            let desc = match interp.ordinary_get_own_property_descriptor_value(
                stack,
                Some(&exec),
                promises_raw,
                &vm_key,
                0,
            ) {
                Ok(desc) => desc,
                Err(err) => {
                    let native = err.into_native(interp, name);
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, native);
                }
            };
            if !desc.as_ref().is_some_and(|desc| desc.enumerable()) {
                continue;
            }
            let promises_raw = interp.escape_scoped(promises);
            let next_value =
                match keyed_get(interp, stack, Some(&exec), promises_raw, &vm_key, name) {
                    Ok(value) => interp.scoped_value(scope, value),
                    Err(err) => {
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err);
                    }
                };
            let i = reserve_keyed_slot_scoped(&slots, interp, slots_handle, keys_handle, key)?;
            let promise_resolve_raw = interp.escape_scoped(promise_resolve);
            let constructor_raw = interp.escape_scoped(constructor);
            let next_value_raw = interp.escape_scoped(next_value);
            let entry_promise = match call_promise_resolve(
                interp,
                stack,
                Some(&exec),
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
            let live_cap = cap_handles.current(interp, context.clone());
            let values_raw = interp.escape_scoped(slots_handle);
            let keys_raw = interp.escape_scoped(keys_handle);
            let on_fulfill = keyed_element_function(
                interp,
                slots.clone(),
                live_cap.clone(),
                values_raw,
                keys_raw,
                variant,
                true,
                i,
            )?;
            let on_fulfill = interp.scoped_value(scope, on_fulfill);
            let on_reject = match variant {
                KeyedVariant::All => cap_handles.reject,
                KeyedVariant::AllSettled => {
                    let live_cap = cap_handles.current(interp, context.clone());
                    // The fulfill element function above allocated; re-read
                    // both arrays from their scope handles.
                    let values_raw = interp.escape_scoped(slots_handle);
                    let keys_raw = interp.escape_scoped(keys_handle);
                    let on_reject = keyed_element_function(
                        interp,
                        slots.clone(),
                        live_cap.clone(),
                        values_raw,
                        keys_raw,
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
                Some(&exec),
                entry_promise_raw,
                on_fulfill_raw,
                on_reject_raw,
            ) {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, err);
            }
        }
        if slots.settle_one() {
            let mut cap = cap_handles.current(interp, context.clone());
            let values_raw = interp.escape_scoped(slots_handle);
            let keys_raw = interp.escape_scoped(keys_handle);
            // IfAbruptRejectPromise — a throwing capability resolve (or an
            // abrupt result build) rejects the capability instead of
            // escaping the combinator.
            if let Err(error) =
                resolve_keyed_slots_runtime(interp, stack, &mut cap, values_raw, keys_raw, name)
            {
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, error);
            }
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
    context: Option<&ExecutionContext>,
    receiver: Value,
    key: &crate::VmPropertyKey<'_>,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        let receiver_raw = interp.escape_scoped(receiver);
        match interp
            .ordinary_get_value(stack, context, receiver_raw, receiver_raw, key, 0)
            .map_err(|err| err.into_native(interp, name))?
        {
            crate::VmGetOutcome::Value(value) => Ok(value),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let getter = interp.scoped_value(scope, getter);
                let getter = interp.escape_scoped(getter);
                let receiver = interp.escape_scoped(receiver);
                interp
                    .run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(|err| crate::native_function::vm_to_native_error(interp, err, name))
            }
        }
    })
}

fn keyed_element_function(
    interp: &mut Interpreter,
    slots: Arc<PromiseSlots>,
    cap: PromiseCapability,
    values: Value,
    keys: Value,
    variant: KeyedVariant,
    fulfilled: bool,
    index: usize,
) -> Result<Value, NativeError> {
    let name = variant.name();
    promise_element_function(
        interp,
        "",
        1,
        smallvec![cap.promise, cap.resolve, cap.reject, values, keys],
        move |ctx, args, captures| {
            // The record build and the slot fill both allocate; the captures
            // slab is a traced root the collector rewrites in place, so every
            // handle is re-read from it after each allocating step.
            let payload = args.first().cloned().unwrap_or(Value::undefined());
            let value = match variant {
                KeyedVariant::All => payload,
                KeyedVariant::AllSettled => build_settled_record(fulfilled, payload, ctx)?,
            };
            let values = capture_array(captures[3]);
            if fill_slot(&slots, ctx.heap_mut(), values, index, value) {
                let values = capture_array(captures[3]);
                let keys = capture_array(captures[4]);
                let result = ctx
                    .with_turn_parts(|interp, _| create_keyed_result(interp, name, values, keys))?;
                // The result build moved the young capability functions; the
                // traced captures slab holds them.
                let cap = capability_from_captures(captures, &cap);
                call_capability_resolve_native(ctx, &cap, result)?;
            }
            Ok(Value::undefined())
        },
    )
    .map_err(NativeError::from)
}

fn settled_element_function(
    interp: &mut Interpreter,
    slots: Arc<PromiseSlots>,
    cap: PromiseCapability,
    values: Value,
    fulfilled: bool,
    index: usize,
) -> Result<Value, NativeError> {
    promise_element_function(
        interp,
        "",
        1,
        smallvec![cap.promise, cap.resolve, cap.reject, values],
        move |ctx, args, captures| {
            // The record build, the slot fill, and the result-array build all
            // allocate; the captures slab is a traced root the collector
            // rewrites in place, so every handle is re-read from it after
            // each allocating step. The capability values stay alive through
            // the slab, so the array build needs no extra roots.
            let payload = args.first().cloned().unwrap_or(Value::undefined());
            let record = build_settled_record(fulfilled, payload, ctx)?;
            let values = capture_array(captures[3]);
            if fill_slot(&slots, ctx.heap_mut(), values, index, record) {
                let values = capture_array(captures[3]);
                let collected = collect_values(ctx.heap(), values);
                let arr = ctx.array_from_elements_with_roots(
                    collected.iter().cloned(),
                    &[],
                    &[collected.as_slice()],
                )?;
                let cap = capability_from_captures(captures, &cap);
                call_capability_resolve_native(ctx, &cap, Value::array(arr))?;
            }
            Ok(Value::undefined())
        },
    )
    .map_err(NativeError::from)
}

fn resolve_keyed_slots_runtime(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    cap: &mut PromiseCapability,
    values: Value,
    keys: Value,
    name: &'static str,
) -> Result<(), NativeError> {
    // The result build allocates; the capability rides handles across it.
    let result = interp.with_handle_scope(|interp, scope| {
        let handles = CapabilityHandles::park(interp, scope, cap);
        let result = create_keyed_result(interp, name, capture_array(values), capture_array(keys));
        handles.refresh(interp, cap);
        result
    })?;
    call_capability_resolve(interp, stack, cap, result)
}

/// The await-dictionary result: `OrdinaryObjectCreate(null)` with one data
/// property per settled key, in key order.
fn create_keyed_result(
    interp: &mut Interpreter,
    name: &'static str,
    values: crate::array::JsArray,
    keys: crate::array::JsArray,
) -> Result<Value, NativeError> {
    let keys = collect_keys(interp.gc_heap(), keys);
    let values = collect_values(interp.gc_heap(), values);
    interp.with_handle_scope(|interp, scope| {
        // Every pair is parked before the first allocation, and each define
        // re-reads the receiver and its pair: a define that grows the
        // object's storage moves young cells.
        let pairs: Vec<_> = keys
            .into_iter()
            .zip(values)
            .map(|(key, value)| {
                (
                    interp.scoped_value(scope, key),
                    interp.scoped_value(scope, value),
                )
            })
            .collect();
        let object = interp
            .scoped_object(scope)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))?;
        let mut raw = interp
            .escape_scoped(object)
            .as_object()
            .expect("keyed result is an object");
        if !crate::object::set_prototype(&mut raw, interp.gc_heap_mut(), None)? {
            return Err(NativeError::TypeError {
                name,
                reason: "result prototype rejected".to_owned(),
            });
        }
        for (key, value) in pairs {
            let key = interp.escape_scoped(key);
            let value = interp.escape_scoped(value);
            let raw = interp
                .escape_scoped(object)
                .as_object()
                .expect("keyed result is an object");
            let heap = interp.gc_heap_mut();
            let desc = crate::object::PropertyDescriptor::data(value, true, true, true);
            let ok = if let Some(s) = key.as_string(heap) {
                let key = s.to_lossy_string(heap);
                crate::object::define_own_property(raw, heap, &key, desc)?
            } else if let Some(sym) = key.as_symbol(heap) {
                crate::object::define_own_symbol_property(raw, heap, sym, desc)?
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
        Ok(interp.escape_scoped(object))
    })
}

fn static_all_generic(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<ExecutionContext>,
    constructor: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let exec = context.clone().ok_or(NativeError::InvalidOperand)?;
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
        let promise_resolve =
            match get_promise_resolve(interp, stack, Some(&exec), &constructor_raw) {
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
                let native = err.into_native(interp, "Promise.all");
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let slots = PromiseSlots::new();
        let slots_handle = interp.scoped_array(scope, 0).map_err(|error| {
            CommittedValueError::JavaScript(error).into_native(interp, "Promise combinator")
        })?;
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = err.into_native(interp, "Promise.all");
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let i = reserve_slot_scoped(&slots, interp, slots_handle)?;
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    Some(&exec),
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        if err.is_fatal() {
                            return Err(err);
                        }
                        interp
                            .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                            .map_err(|error| error.into_native(interp, "Promise combinator"))?;
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                let live_cap = cap_handles.current(interp, context.clone());
                let cap_for_fulfill = live_cap.clone();
                let slots_for_fulfill = slots.clone();
                let values_raw = interp.escape_scoped(slots_handle);
                let on_fulfill = promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![
                        live_cap.promise,
                        live_cap.resolve,
                        live_cap.reject,
                        values_raw
                    ],
                    move |ctx, args, captures| {
                        let values = capture_array(captures[3]);
                        let v = args.first().cloned().unwrap_or(Value::undefined());
                        if fill_slot(&slots_for_fulfill, ctx.heap_mut(), values, i, v) {
                            let collected = collect_values(ctx.heap(), values);
                            let arr = ctx.array_from_elements_with_roots(
                                collected.iter().cloned(),
                                &[],
                                &[collected.as_slice()],
                            )?;
                            // The array build moved the young capability
                            // functions; the traced captures slab holds them.
                            let cap = capability_from_captures(captures, &cap_for_fulfill);
                            call_capability_resolve_native(ctx, &cap, Value::array(arr))?;
                        }
                        Ok(Value::undefined())
                    },
                )?;
                let on_fulfill = interp.scoped_value(iteration_scope, on_fulfill);
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfill = interp.escape_scoped(on_fulfill);
                let on_reject = interp.escape_scoped(cap_handles.reject);
                if let Err(err) = attach_then_value(
                    interp,
                    stack,
                    Some(&exec),
                    entry_promise,
                    on_fulfill,
                    on_reject,
                ) {
                    let iterator_raw = interp.escape_scoped(iterator);
                    if err.is_fatal() {
                        return Err(err);
                    }
                    interp
                        .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                        .map_err(|error| error.into_native(interp, "Promise combinator"))?;
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        if slots.settle_one() {
            let result = materialize_array_scoped(interp, scope, slots_handle, "Promise.all")?;
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
    let exec = context.clone().ok_or(NativeError::InvalidOperand)?;
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
        let promise_resolve =
            match get_promise_resolve(interp, stack, Some(&exec), &constructor_raw) {
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
                let native = err.into_native(interp, "Promise.race");
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
                        let native = err.into_native(interp, "Promise.race");
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
                    Some(&exec),
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        if err.is_fatal() {
                            return Err(err);
                        }
                        interp
                            .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                            .map_err(|error| error.into_native(interp, "Promise combinator"))?;
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
                    Some(&exec),
                    entry_promise,
                    on_fulfilled,
                    on_rejected,
                ) {
                    let iterator_raw = interp.escape_scoped(iterator);
                    if err.is_fatal() {
                        return Err(err);
                    }
                    interp
                        .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                        .map_err(|error| error.into_native(interp, "Promise combinator"))?;
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
    let exec = context.clone().ok_or(NativeError::InvalidOperand)?;
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
        let promise_resolve =
            match get_promise_resolve(interp, stack, Some(&exec), &constructor_raw) {
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
                let native = err.into_native(interp, "Promise.allSettled");
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let slots = PromiseSlots::new();
        let slots_handle = interp.scoped_array(scope, 0).map_err(|error| {
            CommittedValueError::JavaScript(error).into_native(interp, "Promise combinator")
        })?;
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = err.into_native(interp, "Promise.allSettled");
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let i = reserve_slot_scoped(&slots, interp, slots_handle)?;
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    Some(&exec),
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        if err.is_fatal() {
                            return Err(err);
                        }
                        interp
                            .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                            .map_err(|error| error.into_native(interp, "Promise combinator"))?;
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                let live_cap = cap_handles.current(interp, context.clone());
                let values_raw = interp.escape_scoped(slots_handle);
                let on_fulfill = settled_element_function(
                    interp,
                    slots.clone(),
                    live_cap.clone(),
                    values_raw,
                    true,
                    i,
                )?;
                let on_fulfill = interp.scoped_value(iteration_scope, on_fulfill);
                let live_cap = cap_handles.current(interp, context.clone());
                let values_raw = interp.escape_scoped(slots_handle);
                let on_reject = settled_element_function(
                    interp,
                    slots.clone(),
                    live_cap.clone(),
                    values_raw,
                    false,
                    i,
                )?;
                let on_reject = interp.scoped_value(iteration_scope, on_reject);
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfill = interp.escape_scoped(on_fulfill);
                let on_reject = interp.escape_scoped(on_reject);
                if let Err(err) = attach_then_value(
                    interp,
                    stack,
                    Some(&exec),
                    entry_promise,
                    on_fulfill,
                    on_reject,
                ) {
                    let iterator_raw = interp.escape_scoped(iterator);
                    if err.is_fatal() {
                        return Err(err);
                    }
                    interp
                        .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                        .map_err(|error| error.into_native(interp, "Promise combinator"))?;
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        if slots.settle_one() {
            let result =
                materialize_array_scoped(interp, scope, slots_handle, "Promise.allSettled")?;
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
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "Promise.any")
            })?;
        let object = interp.scoped_object(scope).map_err(|error| {
            crate::native_function::vm_to_native_error(interp, error, "Promise.any")
        })?;
        interp
            .scoped_set_prototype(scope, object, Some(prototype))
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "Promise.any")
            })?;
        interp
            .scoped_set(scope, object, "message", message)
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "Promise.any")
            })?;
        let errors_array = interp.scoped_array(scope, errors.len()).map_err(|error| {
            CommittedValueError::JavaScript(error).into_native(interp, "Promise.any")
        })?;
        for (index, error) in errors.into_iter().enumerate() {
            interp
                .scoped_set_index(scope, errors_array, index, error)
                .map_err(|error| {
                    CommittedValueError::JavaScript(error).into_native(interp, "Promise.any")
                })?;
        }
        interp
            .scoped_set(scope, object, "errors", errors_array)
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "Promise.any")
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
        // Park every reason before the first allocation moves them.
        let errors: Vec<_> = errors.into_iter().map(|error| cx.park(error)).collect();
        let errors_array = {
            let array = cx
                .array(errors.len())
                .map_err(|err| err.into_native("Promise.any"))?;
            for (index, element) in errors.into_iter().enumerate() {
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
            if let (Some(mut object), Some(proto)) =
                (raw_instance.as_object(), raw_proto.as_object())
                && !crate::object::set_prototype(&mut object, cx.heap_mut(), Some(proto))?
            {
                return Err(NativeError::TypeError {
                    name: "Promise.any",
                    reason: "error prototype rejected".to_owned(),
                });
            }
        }
        cx.set(instance, "message", message)
            .map_err(|err| err.into_native("Promise.any"))?;
        cx.set(instance, "errors", errors_array)
            .map_err(|err| err.into_native("Promise.any"))?;
        Ok(cx.escape(instance))
    })
}

fn capability_record_scoped(
    interp: &mut Interpreter,
    cap: &PromiseCapability,
    name: &'static str,
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let cap = CapabilityHandles::park(interp, scope, cap);
        let object = interp
            .scoped_object(scope)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))?;
        interp
            .scoped_set(scope, object, "promise", cap.promise)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))?;
        interp
            .scoped_set(scope, object, "resolve", cap.resolve)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))?;
        interp
            .scoped_set(scope, object, "reject", cap.reject)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))?;
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
    let exec = context.clone().ok_or(NativeError::InvalidOperand)?;
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
        let promise_resolve =
            match get_promise_resolve(interp, stack, Some(&exec), &constructor_raw) {
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
                let native = err.into_native(interp, "Promise.any");
                let mut cap = cap_handles.current(interp, context.clone());
                return reject_capability_error(interp, stack, &mut cap, native);
            }
        };
        let errors = PromiseSlots::new();
        let errors_handle = interp.scoped_array(scope, 0).map_err(|error| {
            CommittedValueError::JavaScript(error).into_native(interp, "Promise combinator")
        })?;
        loop {
            let iterator_raw = interp.escape_scoped(iterator);
            let next_method_raw = interp.escape_scoped(next_method);
            let next_value =
                match interp.iterator_step_sync(stack, &exec, &iterator_raw, &next_method_raw) {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        let native = err.into_native(interp, "Promise.any");
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, native);
                    }
                };
            // An abrupt completion inside the per-element step already closed the
            // iterator and settled the capability; it must also end the combinator
            // loop, which an infinite iterator otherwise spins in forever.
            if let Some(settled) = interp.with_handle_scope(|interp, iteration_scope| {
                let next_value = interp.scoped_value(iteration_scope, next_value);
                let i = reserve_slot_scoped(&errors, interp, errors_handle)?;
                let promise_resolve_raw = interp.escape_scoped(promise_resolve);
                let constructor_raw = interp.escape_scoped(constructor);
                let next_value_raw = interp.escape_scoped(next_value);
                let entry_promise = match call_promise_resolve(
                    interp,
                    stack,
                    Some(&exec),
                    &promise_resolve_raw,
                    &constructor_raw,
                    next_value_raw,
                ) {
                    Ok(value) => interp.scoped_value(iteration_scope, value),
                    Err(err) => {
                        let iterator_raw = interp.escape_scoped(iterator);
                        if err.is_fatal() {
                            return Err(err);
                        }
                        interp
                            .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                            .map_err(|error| error.into_native(interp, "Promise combinator"))?;
                        let mut cap = cap_handles.current(interp, context.clone());
                        return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                    }
                };
                let live_cap = cap_handles.current(interp, context.clone());
                let errors_for_call = errors.clone();
                let registry_for_call = registry.clone();
                let cap_for_call = live_cap.clone();
                let errors_raw = interp.escape_scoped(errors_handle);
                let on_reject = promise_element_function(
                    interp,
                    "",
                    1,
                    smallvec![
                        live_cap.promise,
                        live_cap.resolve,
                        live_cap.reject,
                        errors_raw
                    ],
                    move |ctx, args, captures| {
                        let errors_arr = capture_array(captures[3]);
                        let reason = args.first().cloned().unwrap_or(Value::undefined());
                        if fill_slot(&errors_for_call, ctx.heap_mut(), errors_arr, i, reason) {
                            let collected = collect_values(ctx.heap(), errors_arr);
                            let agg = make_aggregate_error_native_rooted(
                                ctx,
                                &registry_for_call,
                                collected,
                            )?;
                            // The error build moved the young capability
                            // functions; the traced captures slab holds them.
                            let cap = capability_from_captures(captures, &cap_for_call);
                            call_capability_reject_native(ctx, &cap, agg)?;
                        }
                        Ok(Value::undefined())
                    },
                )?;
                let on_reject = interp.scoped_value(iteration_scope, on_reject);
                let entry_promise = interp.escape_scoped(entry_promise);
                let on_fulfilled = interp.escape_scoped(cap_handles.resolve);
                let on_reject = interp.escape_scoped(on_reject);
                if let Err(err) = attach_then_value(
                    interp,
                    stack,
                    Some(&exec),
                    entry_promise,
                    on_fulfilled,
                    on_reject,
                ) {
                    let iterator_raw = interp.escape_scoped(iterator);
                    if err.is_fatal() {
                        return Err(err);
                    }
                    interp
                        .iterator_close_discarding_completion(stack, Some(&exec), &iterator_raw)
                        .map_err(|error| error.into_native(interp, "Promise combinator"))?;
                    let mut cap = cap_handles.current(interp, context.clone());
                    return reject_capability_error(interp, stack, &mut cap, err).map(Some);
                }
                Ok(None)
            })? {
                return Ok(settled);
            }
        }
        if errors.settle_one() {
            let collected = collect_values(
                interp.gc_heap(),
                capture_array(interp.escape_scoped(errors_handle)),
            );
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
    let cap = PromiseBuilder::with_context(context).capability_runtime_rooted(interp, &[], &[])?;
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
    let exec = context.clone();
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
            exec.as_ref(),
            &promise_raw,
            &default_ctor_raw,
            NAME,
        )?;
        let c = interp.scoped_value(scope, c);
        let c_raw = interp.escape_scoped(c);
        let capability = if is_builtin_promise_constructor(interp, &c_raw) {
            PromiseBuilder::with_context(context.clone())
                .capability_runtime_rooted(interp, &[], &[])
                .map_err(NativeError::from)?
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
        let outcome = interp
            .register_promise_reactions(
                promise,
                on_fulfilled,
                on_rejected,
                capability,
                context.clone(),
            )
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "Promise.prototype")
            })?;
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
) -> Result<Value, NativeError> {
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
) -> Result<Value, NativeError> {
    interp.with_handle_scope(|interp, scope| {
        let promise = interp.scoped_value(scope, Value::promise(*promise));
        let on_fulfilled = on_fulfilled.map(|value| interp.scoped_value(scope, value));
        let on_rejected = on_rejected.map(|value| interp.scoped_value(scope, value));
        let capability = PromiseBuilder::with_context(context.clone()).capability_runtime_rooted(
            interp,
            &[],
            &[],
        )?;
        let capability_handles = CapabilityHandles::park(interp, scope, &capability);
        let promise = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("then receiver remains rooted");
        let on_fulfilled = on_fulfilled.map(|value| interp.escape_scoped(value));
        let on_rejected = on_rejected.map(|value| interp.escape_scoped(value));
        let capability = capability_handles.current(interp, context.clone());
        let outcome = interp
            .register_promise_reactions(
                promise,
                on_fulfilled,
                on_rejected,
                capability,
                context.clone(),
            )
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "Promise.prototype")
            })?;
        if let Some(job) = outcome.immediate_job {
            interp.microtasks_mut().enqueue(job);
        }
        Ok(capability_handles.current(interp, context).promise)
    })
}

fn attach_then_value(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: Option<&ExecutionContext>,
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
            .map_err(|err| {
                crate::native_function::vm_to_native_error(interp, err, "Promise combinator")
            })?;
        Ok(())
    })
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

/// §27.2.1.3 `[[AlreadyResolved]]` — one shared boolean cell per resolving-
/// function pair, packed as an opaque GC value so both natives trace it
/// through their captures. A fresh pair (Promise constructor, capability
/// executor, or PromiseResolveThenableJob) always gets its own cell.
fn alloc_already_resolved_cell(
    heap: &mut otter_gc::GcHeap,
) -> Result<Value, otter_gc::OutOfMemory> {
    let cell = crate::alloc_upvalue(heap, Value::boolean(false))?;
    Ok(Value::from_object_gc(cell.raw()))
}

/// Consume the pair's `[[AlreadyResolved]]` flag from `captures[1]`.
///
/// Returns `true` exactly once per cell — the call that flips it — so a
/// resolve or reject function whose pair already ran becomes a no-op even
/// while the promise is still pending on an in-flight thenable job.
fn consume_already_resolved(interp: &mut Interpreter, captures: &[Value]) -> bool {
    let cell = captures
        .get(1)
        .and_then(|flag| flag.as_raw_gc())
        .and_then(|raw| raw.checked_cast::<crate::UpvalueCellBody>())
        .expect("resolving pair captures its [[AlreadyResolved]] cell at index 1");
    if crate::read_upvalue(interp.gc_heap(), cell)
        .as_boolean()
        .unwrap_or(false)
    {
        return false;
    }
    crate::store_upvalue(interp.gc_heap_mut(), cell, Value::boolean(true));
    true
}

fn make_resolve_native_runtime_rooted(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    already_resolved: Value,
    context: Option<ExecutionContext>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    promise_native_runtime(
        interp,
        "",
        1,
        smallvec![Value::promise(promise), already_resolved],
        value_roots,
        slice_roots,
        move |ctx, args, captures| resolve_native_body(ctx, args, captures, &captured_context),
    )
}

fn make_resolve_native_stack_rooted(
    interp: &mut Interpreter,
    stack: &ActivationStack,
    promise: JsPromiseHandle,
    already_resolved: Value,
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
        smallvec![Value::promise(promise), already_resolved],
        value_roots,
        slice_roots,
        move |ctx, args, captures| resolve_native_body(ctx, args, captures, &captured_context),
    )
}

fn make_resolve_native_native_rooted(
    ctx: &mut NativeCtx<'_>,
    promise: JsPromiseHandle,
    already_resolved: Value,
    context: Option<ExecutionContext>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    let captured_context = context;
    promise_native_ctx(
        ctx,
        "",
        1,
        smallvec![Value::promise(promise), already_resolved],
        value_roots,
        slice_roots,
        move |ctx, args, captures| resolve_native_body(ctx, args, captures, &captured_context),
    )
}

/// §27.2.1.3.2 Promise Resolve Functions. The pair's `[[AlreadyResolved]]`
/// flag is consumed first, so a second call — or the constructor's
/// throw-after-resolve reject — is a no-op even while a thenable job for the
/// first resolution is still in flight.
fn resolve_native_body(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
    captured_context: &Option<ExecutionContext>,
) -> Result<Value, NativeError> {
    let promise = settle_native_promise(captures);
    if !consume_already_resolved(ctx.interp_mut(), captures) {
        return Ok(Value::undefined());
    }
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

        // §27.2.1.3.2 step 6 — resolving a promise with itself is a
        // TypeError, not a wait for something that can never arrive.
        if scope.raw(value) == scope.raw(promise) {
            let reason = scope
                .with_turn_parts(|interp, stack| {
                    interp
                        .make_error_instance_with_stack_roots(
                            stack,
                            crate::error_classes::ErrorKind::TypeError,
                            Some("Chaining cycle detected for promise".to_string()),
                            &Value::undefined(),
                        )
                        .map(Value::object)
                })
                .map_err(|error| {
                    crate::native_function::vm_to_native_error(
                        scope.context().interp_mut(),
                        error,
                        "Promise resolve",
                    )
                })?;
            // Materializing the cycle error may move the captured promise.
            let promise_handle = scope
                .raw(promise)
                .as_promise()
                .expect("resolver promise remains rooted after error allocation");
            let interp = scope.context().interp_mut();
            let jobs = promise_handle.reject(interp.gc_heap_mut(), reason);
            drain_jobs(interp, jobs, context.as_ref());
            return Ok(Value::undefined());
        }

        // §27.2.1.3.2 Promise Resolve Functions steps 8-13 — any object
        // with a callable `then` is a thenable: read `then` (firing an
        // accessor and rejecting on its throw), then enqueue the job. A
        // native promise takes the same observable path: the `then` read and
        // the job tick are both required (a custom `then` on the instance,
        // its class, or a patched %Promise.prototype.then% must win).
        if scope.raw(value).is_object_type() {
            let value_raw = scope.raw(value);
            let then = scope.with_turn_parts(|interp, stack| {
                get_property_runtime(
                    interp,
                    stack,
                    context.as_ref(),
                    value_raw,
                    "then",
                    "Promise resolve",
                )
            });
            let then = match then {
                Ok(then) => scope.value(then),
                Err(err) => {
                    let reason = scope.with_turn_parts(|interp, stack| {
                        native_error_rejection_value(interp, stack, err)
                    })?;
                    let promise = scope
                        .raw(promise)
                        .as_promise()
                        .expect("resolver promise remains rooted on getter throw");
                    let interp = scope.context().interp_mut();
                    let jobs = promise.reject(interp.gc_heap_mut(), reason);
                    drain_jobs(interp, jobs, context.as_ref());
                    return Ok(Value::undefined());
                }
            };
            if scope.is_callable(then) {
                let then_raw = scope.raw(then);
                let (job_context, realm_id) = scope
                    .with_turn_parts(|interp, _| {
                        let source = interp.callable_context(context.as_ref(), then_raw)?;
                        let realm =
                            interp.reaction_realm(Some(then_raw), interp.active_realm_id)?;
                        Ok::<_, crate::VmError>((source, realm))
                    })
                    .map_err(|error| {
                        crate::native_function::vm_to_native_error(
                            scope.context().interp_mut(),
                            error,
                            "Promise thenable",
                        )
                    })?;
                // §27.2.1.3.2 PromiseResolveThenableJob runs
                // CreateResolvingFunctions afresh: the handlers are FULL
                // resolve/reject functions with their own flag, so a
                // thenable that resolves with another thenable keeps
                // unwrapping instead of fulfilling with it verbatim.
                let already_resolved =
                    alloc_already_resolved_cell(scope.context().interp_mut().gc_heap_mut())?;
                let already_resolved = scope.value(already_resolved);
                let promise_handle = scope
                    .raw(promise)
                    .as_promise()
                    .expect("resolver promise remains rooted");
                let already_resolved_raw = scope.raw(already_resolved);
                let on_fulfill = make_resolve_native_native_rooted(
                    scope.context(),
                    promise_handle,
                    already_resolved_raw,
                    context.clone(),
                    &[],
                    &[],
                )?;
                let on_fulfill = scope.value(on_fulfill);
                let promise_handle = scope
                    .raw(promise)
                    .as_promise()
                    .expect("resolver promise remains rooted");
                let already_resolved_raw = scope.raw(already_resolved);
                let on_reject = make_reject_native_native_rooted(
                    scope.context(),
                    promise_handle,
                    already_resolved_raw,
                    &[],
                    &[],
                )?;
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
                    job_context.clone(),
                )?;
                let job = scope.value(job);
                let job = scope.raw(job);
                let async_context = scope.context().interp_mut().async_context();
                scope
                    .context()
                    .interp_mut()
                    .microtasks_mut()
                    .enqueue(crate::Microtask {
                        callee: job,
                        this_value: Value::undefined(),
                        args: SmallVec::new(),
                        context: job_context,
                        realm_id,
                        result_capability: None,
                        kind: crate::microtask::MicrotaskKind::Call,
                        async_context,
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
        drain_jobs(interp, jobs, context.as_ref());
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

        // §27.2.1.3.2 step 6 — resolving a promise with itself is a
        // TypeError, not a wait for something that can never arrive.
        if interp.escape_scoped(value) == interp.escape_scoped(promise) {
            let empty = ActivationStack::new();
            let reason = interp
                .make_error_instance_with_stack_roots(
                    &empty,
                    crate::error_classes::ErrorKind::TypeError,
                    Some("Chaining cycle detected for promise".to_string()),
                    &Value::undefined(),
                )
                .map(Value::object)?;
            // Read the rooted promise only after the collecting error build.
            let promise_handle = interp
                .escape_scoped(promise)
                .as_promise()
                .expect("async resolver promise remains rooted after error allocation");
            let jobs = promise_handle.reject(interp.gc_heap_mut(), reason);
            drain_jobs(interp, jobs, context.as_ref());
            return Ok(());
        }

        // §27.2.1.3.2 steps 8-13 — an ordinary object with a callable
        // `then` is a thenable and is adopted through the job queue, not
        // fulfilled as itself. An async function returning one must settle
        // on what that thenable resolves to.
        // Native-only source stays None; a bytecode getter/then resolves
        // its exact defining chunk through the canonical call owner.
        if interp.escape_scoped(value).is_object_type() {
            let raw_value = interp.escape_scoped(value);
            let mut probe = ActivationStack::new();
            let then = get_property_runtime(
                interp,
                &mut probe,
                context.as_ref(),
                raw_value,
                "then",
                "Promise resolve",
            );
            let then = match then {
                Ok(then) => then,
                Err(err) => {
                    let reason = native_error_rejection_value(interp, &probe, err)
                        .map_err(|error| crate::native_to_vm_error(interp, error))?;
                    let promise_handle = interp
                        .escape_scoped(promise)
                        .as_promise()
                        .expect("async resolver promise remains rooted");
                    let jobs = promise_handle.reject(interp.gc_heap_mut(), reason);
                    drain_jobs(interp, jobs, context.as_ref());
                    return Ok(());
                }
            };
            let then = interp.scoped_value(scope, then);
            if interp.is_callable_runtime(&interp.escape_scoped(then)) {
                let then_raw = interp.escape_scoped(then);
                let job_context = interp.callable_context(context.as_ref(), then_raw)?;
                let realm_id = interp.reaction_realm(Some(then_raw), interp.active_realm_id)?;
                // §27.2.1.3.2 PromiseResolveThenableJob runs
                // CreateResolvingFunctions afresh: the handlers are FULL
                // resolve/reject functions with their own flag, so nested
                // thenables keep unwrapping.
                let already_resolved =
                    alloc_already_resolved_cell(interp.gc_heap_mut()).map_err(crate::oom_to_vm)?;
                let already_resolved = interp.scoped_value(scope, already_resolved);
                let promise_handle = interp
                    .escape_scoped(promise)
                    .as_promise()
                    .expect("async resolver promise remains rooted after flag allocation");
                let already_resolved_raw = interp.escape_scoped(already_resolved);
                let on_fulfill = make_resolve_native_runtime_rooted(
                    interp,
                    promise_handle,
                    already_resolved_raw,
                    context.clone(),
                    &[],
                    &[],
                )
                .map_err(crate::oom_to_vm)?;
                let on_fulfill = interp.scoped_value(scope, on_fulfill);
                let promise_handle = interp
                    .escape_scoped(promise)
                    .as_promise()
                    .expect("async resolver promise remains rooted");
                let already_resolved_raw = interp.escape_scoped(already_resolved);
                let on_reject = make_reject_native_runtime_rooted(
                    interp,
                    promise_handle,
                    already_resolved_raw,
                    &[],
                    &[],
                )
                .map_err(crate::oom_to_vm)?;
                let on_reject = interp.scoped_value(scope, on_reject);
                let job = make_resolve_thenable_job_runtime_rooted(
                    interp,
                    interp.escape_scoped(value),
                    interp.escape_scoped(then),
                    interp.escape_scoped(on_fulfill),
                    interp.escape_scoped(on_reject),
                    job_context.clone(),
                )?;
                let async_context = interp.async_context();
                interp.microtasks.enqueue(crate::microtask::Microtask {
                    callee: job,
                    this_value: Value::undefined(),
                    args: smallvec::SmallVec::new(),
                    context: job_context,
                    realm_id,
                    result_capability: None,
                    kind: crate::microtask::MicrotaskKind::Call,
                    async_context,
                });
                return Ok(());
            }
        }

        let promise = interp
            .escape_scoped(promise)
            .as_promise()
            .expect("async resolver promise remains rooted before fulfillment");
        let value = interp.escape_scoped(value);
        let jobs = promise.fulfill(interp.gc_heap_mut(), value);
        drain_jobs(interp, jobs, context.as_ref());
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
    exec: Option<ExecutionContext>,
) -> Result<Value, NativeError> {
    let captures = smallvec![thenable, then, on_fulfill, on_reject];
    ctx.native_value(
        "PromiseResolveThenableJob",
        captures,
        move |ctx, _args, captures| resolve_thenable_job_body(ctx, captures, exec.as_ref()),
    )
    .map_err(NativeError::from)
}

/// Run one queued thenable invocation inside the job's installed source/realm.
/// Every capture and the original rejection are scoped across collecting calls.
fn resolve_thenable_job_body(
    ctx: &mut NativeCtx<'_>,
    captures: &[Value],
    exec: Option<&ExecutionContext>,
) -> Result<Value, NativeError> {
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
                exec,
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
                if err.is_fatal() || matches!(err, crate::VmError::OutOfMemory { .. }) {
                    return Err(crate::native_function::vm_to_native_error(
                        scope.context().interp_mut(),
                        err,
                        "Promise thenable",
                    ));
                }
                let reason = scope.with_turn_parts(|interp, stack| {
                    interp
                        .vm_error_to_throwable_with_stack_roots(exec, stack, &err)
                        .map_err(|error| {
                            crate::native_function::vm_to_native_error(
                                interp,
                                error,
                                "Promise thenable",
                            )
                        })
                })?;
                let reason = scope.value(reason);
                let on_reject = scope.raw(on_reject);
                let reason = scope.raw(reason);
                scope.with_turn_parts(|interp, stack| {
                    interp
                        .run_callable_sync_rooted(
                            stack,
                            exec,
                            &on_reject,
                            Value::undefined(),
                            smallvec![reason],
                        )
                        .map_err(|error| {
                            crate::native_function::vm_to_native_error(
                                interp,
                                error,
                                "Promise thenable",
                            )
                        })
                })?;
                Ok(Value::undefined())
            }
        }
    })
}

/// §27.2.1.3.2 PromiseResolveThenableJob, built from the interpreter
/// rather than from a native call frame.
fn make_resolve_thenable_job_runtime_rooted(
    interp: &mut Interpreter,
    thenable: Value,
    then: Value,
    on_fulfill: Value,
    on_reject: Value,
    exec: Option<ExecutionContext>,
) -> Result<Value, crate::VmError> {
    promise_native_runtime(
        interp,
        "PromiseResolveThenableJob",
        0,
        smallvec![thenable, then, on_fulfill, on_reject],
        &[],
        &[],
        move |ctx, _args, captures| resolve_thenable_job_body(ctx, captures, exec.as_ref()),
    )
    .map_err(crate::VmError::from)
}

fn make_reject_native_runtime_rooted(
    interp: &mut Interpreter,
    promise: JsPromiseHandle,
    already_resolved: Value,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_runtime(
        interp,
        "",
        1,
        smallvec![Value::promise(promise), already_resolved],
        value_roots,
        slice_roots,
        move |ctx, args, captures| Ok(reject_native_body(ctx, args, captures)),
    )
}

fn make_reject_native_stack_rooted(
    interp: &mut Interpreter,
    stack: &ActivationStack,
    promise: JsPromiseHandle,
    already_resolved: Value,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_stack(
        interp,
        stack,
        "",
        1,
        smallvec![Value::promise(promise), already_resolved],
        value_roots,
        slice_roots,
        move |ctx, args, captures| Ok(reject_native_body(ctx, args, captures)),
    )
}

fn make_reject_native_native_rooted(
    ctx: &mut NativeCtx<'_>,
    promise: JsPromiseHandle,
    already_resolved: Value,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    promise_native_ctx(
        ctx,
        "",
        1,
        smallvec![Value::promise(promise), already_resolved],
        value_roots,
        slice_roots,
        move |ctx, args, captures| Ok(reject_native_body(ctx, args, captures)),
    )
}

/// §27.2.1.3.1 Promise Reject Functions — consume the pair's
/// `[[AlreadyResolved]]` flag, then reject the captured promise.
fn reject_native_body(ctx: &mut NativeCtx<'_>, args: &[Value], captures: &[Value]) -> Value {
    let context = ctx.execution_context().cloned();
    let promise = settle_native_promise(captures);
    if !consume_already_resolved(ctx.interp_mut(), captures) {
        return Value::undefined();
    }
    let interp = ctx.interp_mut();
    if matches!(promise.state(interp.gc_heap()), PromiseState::Pending) {
        let reason = args.first().cloned().unwrap_or(Value::undefined());
        let jobs = promise.reject(interp.gc_heap_mut(), reason);
        drain_jobs(interp, jobs, context.as_ref());
    }
    Value::undefined()
}

fn drain_jobs(
    interp: &mut Interpreter,
    jobs: PromiseSettleJobs,
    context: Option<&ExecutionContext>,
) {
    interp.note_settle_rejection(&jobs, context);
    for j in jobs.jobs {
        interp.microtasks_mut().enqueue(j);
    }
}

#[cfg(test)]
#[path = "promise_dispatch/completed_tests.rs"]
mod completed_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumberValue;
    use crate::runtime_cx::NativeCallInfo;

    /// Explicit verifier-valid source fixture for tests that exercise admitted
    /// settlement. Native-only capabilities use None through the same owner.
    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("promise-dispatch-test"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture")
    }

    #[test]
    fn aggregate_error_runtime_builder_uses_rooted_young_allocation() {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
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
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
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
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
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
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
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
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
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

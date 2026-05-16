//! Method-call opcode helpers.
//!
//! `CallMethodValue` is the widest dynamic dispatch opcode in the interpreter:
//! it handles prototype intrinsics, generator and iterator helpers, collection
//! callbacks, object/function prototype intercepts, and ordinary property
//! method lookup before falling into the shared callable path.
//!
//! # Contents
//! - `CallMethodValue` executable operand decoding.
//! - Collection `forEach` callback dispatch.
//! - Callback-driven Array prototype methods.
//!
//! # Invariants
//! - Stack-modifying callback paths run before the dense in-frame match.
//! - Caller PC is advanced before synchronous callback dispatch where nested
//!   execution can re-enter the VM.
//! - Ordinary method lookup still funnels into `Interpreter::invoke`.
//!
//! # See also
//! - [`crate::call_ops`]
//! - [`crate::executable`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    BoundFunction, ExecutionContext, Frame, GeneratorResumeKind, Interpreter, IntrinsicArgs,
    JsArray, JsString, NumberValue, Value, VmError, VmGetOutcome, VmPropertyKey, array_prototype,
    bigint, binary, boolean_prototype, bound_function_object_prototype_intercept,
    build_array_cb_args, collections_prototype, date, descriptor_value, function_metadata, intl,
    intrinsic_to_vm_error, is_callable, native_function_object_prototype_intercept,
    native_to_vm_error, number, object_prototype_intercept,
    operand_decode::{const_operand, register_operand},
    promise_dispatch, property_key_from_arg, read_register, regexp_prototype, require_callable,
    string::prototype as string_prototype,
    symbol_prototype, temporal, weak_refs, write_register,
};

impl Interpreter {
    /// Handle `Op::CallMethodValue`: the universal method-call op.
    /// Branches by receiver kind:
    /// - `String` / `Array` — synchronous intrinsic-table dispatch.
    ///   Result lands in the destination register without pushing
    ///   a frame.
    /// - `Object` — load the property; raise `NotCallable` if the
    ///   resolved value is not a function; otherwise call it with
    ///   `this = receiver`.
    /// - `Function` / `Closure` / `BoundFunction` — only the
    ///   `call`, `apply`, and `bind` shapes are recognised; anything
    ///   else surfaces as `UnknownIntrinsic`.
    pub(crate) fn do_call_method_value(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let recv_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let top_idx = stack.len() - 1;
        let recv_value = read_register(&stack[top_idx], recv_reg)?.clone();
        let mut arg_values: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            arg_values.push(read_register(&stack[top_idx], r)?.clone());
        }

        // Promise.prototype dispatches separately because it
        // needs `&mut self` to enqueue microtasks.
        if let Value::Promise(p) = &recv_value {
            let promise = *p;
            let result = promise_dispatch::prototype_call(
                self,
                Some(context.clone()),
                &promise,
                &name,
                arg_values.as_slice(),
            )
            .map_err(native_to_vm_error)?;
            let top_idx = stack.len() - 1;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }

        // `forEach` on a collection requires a callback dispatch
        // that pushes a frame; lives outside the static intrinsic
        // table so it can drive `self.invoke`.
        if name == "forEach" && matches!(&recv_value, Value::Map(_) | Value::Set(_)) {
            return self.do_collection_for_each(stack, context, &recv_value, &arg_values, dst);
        }

        // Iterator-helpers proposal — when receiver is an iterator
        // value, route through the dedicated dispatcher that builds
        // lazy wrappers / drains for terminals.
        // <https://tc39.es/proposal-iterator-helpers/>
        if let Value::Iterator(rc) = &recv_value {
            let iter_rc = *rc;
            if self.iterator_helper_dispatch(stack, context, &iter_rc, &name, &arg_values, dst)? {
                return Ok(());
            }
        }

        // §27.5.3 Generator.prototype methods — `.next` / `.return`
        // / `.throw`. The receiver carries the suspended frame; the
        // resume helper drives a sub-dispatch until the next Yield
        // or completion.
        // <https://tc39.es/ecma262/#sec-generator-objects>
        if let Value::Generator(g) = &recv_value {
            let kind = match name {
                "next" => Some(GeneratorResumeKind::Next(
                    arg_values.first().cloned().unwrap_or(Value::Undefined),
                )),
                "return" => Some(GeneratorResumeKind::Return(
                    arg_values.first().cloned().unwrap_or(Value::Undefined),
                )),
                "throw" => Some(GeneratorResumeKind::Throw(
                    arg_values.first().cloned().unwrap_or(Value::Undefined),
                )),
                _ => None,
            };
            if let Some(kind) = kind {
                let g = *g;
                let is_async_gen = g.is_async(&self.gc_heap);
                if is_async_gen {
                    // §27.6.3 — async-generator method calls always
                    // return a Promise. Allocate the outer
                    // capability up front and stash it on
                    // `pending_request` so `Op::Yield` /
                    // `resume_generator` / the await-resume native
                    // can settle it from inside the dispatch loop.
                    let cap = promise_dispatch::PromiseBuilder::with_context(context.clone())
                        .capability_stack_rooted(
                            self,
                            stack,
                            &[&recv_value],
                            &[arg_values.as_slice()],
                        )?;
                    let promise = cap.promise.clone();
                    g.set_pending_request(&mut self.gc_heap, cap.clone());
                    let outcome = self.resume_generator(context, &g, kind);
                    match outcome {
                        Ok(_) => {
                            // resume_generator drained the request
                            // — either by Op::Yield, by completion,
                            // or it left the request pending while
                            // an `Op::Await` parked the body. In
                            // any case, the outer promise is the
                            // user-visible handle.
                        }
                        Err(err) => {
                            if let Some(thrown) = self.pending_generator_throw.take() {
                                if let Some(req) = g.take_pending_request(&mut self.gc_heap) {
                                    let request_context =
                                        req.context.clone().unwrap_or_else(|| context.clone());
                                    self.run_callable_sync(
                                        &request_context,
                                        &req.reject,
                                        Value::Undefined,
                                        smallvec::smallvec![thrown],
                                    )?;
                                }
                            } else {
                                g.clear_pending_request(&mut self.gc_heap);
                                return Err(err);
                            }
                        }
                    }
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, promise)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    return Ok(());
                }
                match self.resume_generator(context, &g, kind) {
                    Ok(result) => {
                        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                        write_register(frame, dst, result)?;
                        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        return Ok(());
                    }
                    Err(err) => {
                        // If the generator body unwound an
                        // uncaught throw, re-raise the *original*
                        // value on the caller's frame stack so a
                        // surrounding `try { gen.throw(x) } catch`
                        // observes the right payload.
                        if let Some(thrown) = self.pending_generator_throw.take() {
                            self.unwind_throw(stack, thrown)?;
                            return Ok(());
                        }
                        return Err(err);
                    }
                }
            }
        }

        // §23.1.3 callback-driven Array.prototype methods. The
        // intrinsic table can't drive callbacks, so the foundation
        // dispatches them here via `run_callable_sync`. Each method
        // matches its ECMA-262 algorithm with sloppy edge handling
        // (sparse holes, throwing comparators, length mutation
        // mid-walk) deferred to follow-ups.
        if let Value::Array(arr) = &recv_value
            && matches!(
                name,
                "forEach"
                    | "map"
                    | "filter"
                    | "reduce"
                    | "reduceRight"
                    | "find"
                    | "findIndex"
                    | "every"
                    | "some"
                    | "flatMap"
                    | "sort"
            )
            && self.array_callback_dispatch(stack, context, arr, &name, &arg_values, dst)?
        {
            return Ok(());
        }
        // Primitive prototypes go through the intrinsic table —
        // synchronous, no frame push, advance pc and write directly.
        let intrinsic = match &recv_value {
            Value::String(_) => string_prototype::lookup(&name),
            Value::Array(_) => array_prototype::lookup(&name),
            Value::Number(_) => number::prototype_lookup(&name),
            Value::Boolean(_) => boolean_prototype::lookup(&name),
            Value::BigInt(_) => bigint::prototype::lookup(&name),
            Value::Date(_) => date::prototype::lookup(&name),
            Value::RegExp(_) => regexp_prototype::lookup(&name),
            Value::Symbol(_) => symbol_prototype::lookup(&name),
            Value::Map(_) => collections_prototype::lookup_map(&name),
            Value::Set(_) => collections_prototype::lookup_set(&name),
            Value::WeakMap(_) => collections_prototype::lookup_weak_map(&name),
            Value::WeakSet(_) => collections_prototype::lookup_weak_set(&name),
            Value::WeakRef(_) => weak_refs::lookup_weak_ref(&name),
            Value::FinalizationRegistry(_) => weak_refs::lookup_finalization_registry(&name),
            Value::Temporal(_) => temporal::lookup_prototype(&recv_value, &name),
            Value::Intl(_) => intl::lookup_prototype(&recv_value, &name),
            Value::ArrayBuffer(_) => binary::array_buffer_prototype::lookup(&name),
            Value::DataView(_) => binary::data_view_prototype::lookup(&name),
            Value::TypedArray(_) => binary::typed_array_prototype::lookup(&name),
            _ => None,
        };
        if let Some(entry) = intrinsic {
            let small_args: SmallVec<[Value; 4]> = arg_values.iter().cloned().collect();
            let result = {
                let string_heap = self.string_heap.clone();
                let allocation_roots = self.collect_allocation_roots(stack);
                (entry.impl_fn)(&mut IntrinsicArgs {
                    receiver: &recv_value,
                    args: &small_args,
                    string_heap: &string_heap,
                    gc_heap: &mut self.gc_heap,
                    allocation_roots: allocation_roots.as_slice(),
                })
                .map_err(intrinsic_to_vm_error)?
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }

        // §20.1.3 Object.prototype methods that ordinary objects
        // inherit. Foundation has no installed Object.prototype yet,
        // so the runtime intercepts the canonical names directly when
        // the receiver is an ordinary `JsObject`. Once the prototype
        // tree is real (task 61 follow-up) these route through the
        // standard property lookup below.
        // <https://tc39.es/ecma262/#sec-properties-of-the-object-prototype-object>
        if let Value::Object(obj) = &recv_value {
            // Only intercept when the user hasn't overridden the
            // method via an own / inherited data property. This
            // keeps `Object.create({hasOwnProperty: () => 'shadow'})`
            // observable.
            if matches!(
                crate::object::lookup(*obj, &self.gc_heap, &name),
                crate::object::PropertyLookup::Absent
            ) && let Some(result) = object_prototype_intercept(
                obj,
                &name,
                &arg_values,
                &self.string_heap,
                &self.gc_heap,
                self.function_prototype_object().ok(),
            )? {
                let frame = &mut stack[top_idx];
                write_register(frame, dst, result)?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                return Ok(());
            }
        }
        // Functions / closures inherit Object.prototype-style
        // methods. Foundation routes the call through the user-
        // properties bag attached to the compiled function.
        if let Value::Function { function_id } | Value::Closure { function_id, .. } = &recv_value
            && matches!(
                name,
                "hasOwnProperty" | "propertyIsEnumerable" | "isPrototypeOf"
            )
        {
            let result = match name {
                "hasOwnProperty" => {
                    let key = property_key_from_arg(arg_values.first())?;
                    self.ordinary_function_own_property_descriptor(
                        Some(context),
                        *function_id,
                        &key,
                    )?
                    .is_some()
                }
                "propertyIsEnumerable" => {
                    let key = property_key_from_arg(arg_values.first())?;
                    self.ordinary_function_own_property_descriptor(
                        Some(context),
                        *function_id,
                        &key,
                    )?
                    .is_some_and(|desc| desc.enumerable())
                }
                "isPrototypeOf" => false,
                _ => unreachable!("guarded by method-name match"),
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, Value::Boolean(result))?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }
        if let Value::NativeFunction(native) = &recv_value
            && let Some(result) = native_function_object_prototype_intercept(
                native,
                &name,
                &arg_values,
                &self.gc_heap,
                &self.string_heap,
            )?
        {
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }
        if let Value::BoundFunction(bound) = &recv_value
            && let Some(result) =
                bound_function_object_prototype_intercept(bound, &name, &arg_values, &self.gc_heap)?
        {
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }

        // §20.2.3 Function.prototype canonical methods —
        // `call` / `apply` / `bind` / `toString`. They are
        // unconditionally available on any callable, even when the
        // receiver is a ClassConstructor whose statics object
        // hasn't installed them. The intercept runs before the
        // property-lookup so user-installed shadows take precedence
        // only when the receiver is a plain Object. Callable
        // receivers go straight here.
        // <https://tc39.es/ecma262/#sec-properties-of-the-function-prototype-object>
        if matches!(name, "call" | "apply" | "bind" | "toString")
            && self.is_callable_runtime(&recv_value)
        {
            return self.dispatch_function_method(
                stack,
                context,
                &recv_value,
                &name,
                arg_values,
                dst,
            );
        }

        // Property-bearing receivers — load the property first.
        // For class constructors, `prototype` resolves to the
        // instance prototype object (mirroring `Op::LoadProperty`'s
        // class shape) and other names walk the static side. Only
        // when the property lookup hands back a callable do we
        // dispatch with `this = recv`; missing or non-callable
        // properties surface as `NotCallable` so callers see the
        // same error as `obj.notFn()`.
        let lookup_via_property = match &recv_value {
            Value::Object(_) | Value::Proxy(_) => {
                let key = VmPropertyKey::String(name);
                match self.ordinary_get_value(
                    context,
                    recv_value.clone(),
                    recv_value.clone(),
                    &key,
                    0,
                )? {
                    VmGetOutcome::Value(value) => Some(value),
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        Some(self.run_callable_sync(context, &getter, recv_value.clone(), args)?)
                    }
                }
            }
            Value::ClassConstructor(c) => Some(if name == "prototype" {
                Value::Object(c.prototype(&self.gc_heap))
            } else {
                crate::object::get(c.statics(&self.gc_heap), &self.gc_heap, &name)
                    .unwrap_or(Value::Undefined)
            }),
            // §10.1.8 OrdinaryGet on a callable receiver — user
            // properties (e.g. `assert.sameValue = function(){}`)
            // resolve via the function-properties side table; the
            // fallback to `Function.prototype.{call,apply,bind}`
            // happens below if we hand back `Undefined`.
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let fid = *function_id;
                Some(self.function_property_get_stack_rooted(context, stack, fid, &name)?)
            }
            // Native callable receiver (e.g. global `Promise` /
            // `Map` constructors). Look up `name` on the function
            // object's own-property table so `Promise.all(...)`,
            // `Map.groupBy(...)`, etc. dispatch through ordinary
            // method invocation.
            Value::NativeFunction(native) => {
                match native.own_property_descriptor(&self.gc_heap, &self.string_heap, &name)? {
                    Some(desc) => Some(descriptor_value(&desc)),
                    None => None,
                }
            }
            _ => None,
        };
        if let Some(method) = lookup_via_property {
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].pc = stack[top_idx]
                .pc
                .checked_add(1)
                .ok_or(VmError::InvalidOperand)?;
            return self.invoke(stack, context, &method, recv_value.clone(), arg_values, dst);
        }

        // `Function.prototype.{call, apply, bind, toString}` on a
        // callable receiver that doesn't expose the method as a
        // property — fallback path.
        if matches!(name, "call" | "apply" | "bind" | "toString")
            && self.is_callable_runtime(&recv_value)
        {
            return self.dispatch_function_method(
                stack,
                context,
                &recv_value,
                &name,
                arg_values,
                dst,
            );
        }

        Err(VmError::UnknownIntrinsic {
            name: name.to_string(),
        })
    }

    /// Dispatch `call` / `apply` / `bind` on a callable receiver.
    /// Foundation handles only the literal-array shape of `apply`
    /// — non-array second arguments raise `TypeMismatch` so callers
    /// learn quickly that the foundation slice rejects dynamic
    /// argument arrays.
    /// Drive `Map.prototype.forEach` / `Set.prototype.forEach` —
    /// invoke the callback on each entry in insertion order.
    ///
    /// # Algorithm
    /// 1. Snapshot the entry list at call time (matches Spec
    ///    §24.1.3.5 / §24.2.3.6 — observable mutation during the
    ///    walk is captured by re-reading the live receiver, but the
    ///    snapshot still gates `index < snapshot.len()`).
    /// 2. For each entry, enqueue an inline call: every callback is
    ///    invoked synchronously through `self.invoke`. Because each
    ///    invoke pushes a frame and returns through the dispatch
    ///    loop, the foundation chains them by stashing the iteration
    ///    state in a tiny native closure that re-enters this helper.
    /// 3. Foundation simplification: rather than a re-entrant
    ///    chain, walk the snapshot here and synchronously invoke
    ///    each callback via a fresh dispatch_loop run on a new
    ///    stack. This matches the synchronous-callback model the
    ///    rest of the foundation already uses (see
    ///    [`Interpreter::run_callable_sync`]).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-map.prototype.foreach>
    /// - <https://tc39.es/ecma262/#sec-set.prototype.foreach>
    fn do_collection_for_each(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        recv: &Value,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let callee = match args.first() {
            Some(c) if is_callable(c) => c.clone(),
            _ => return Err(VmError::NotCallable),
        };
        let entries: Vec<(Value, Value)> = match recv {
            Value::Map(m) => crate::collections::map_entries(*m, &self.gc_heap),
            Value::Set(s) => crate::collections::set_values(*s, &self.gc_heap)
                .into_iter()
                .map(|v| (v.clone(), v))
                .collect(),
            _ => return Err(VmError::TypeMismatch),
        };
        // Advance pc *before* invoking the callbacks so each
        // callback returns to the next instruction in the caller
        // frame.
        let top_idx = stack.len() - 1;
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        // Write `undefined` into the dst slot — `forEach` returns
        // `undefined` synchronously, even if the callback chain
        // produces values.
        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
        let recv_for_callback = recv.clone();
        for (key, value) in entries {
            let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
            cb_args.push(value);
            cb_args.push(key);
            cb_args.push(recv_for_callback.clone());
            self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
        }
        Ok(())
    }

    /// Dispatch the §23.1.3 callback-driven Array prototype methods.
    /// Returns `Ok(true)` when the call was handled here (the
    /// dispatcher should fall through to the post-dispatch return),
    /// `Ok(false)` when the method is `sort` with no comparator
    /// (intrinsic-table path takes over).
    ///
    /// All callbacks run synchronously through
    /// [`Self::run_callable_sync`] — the foundation walks the array
    /// snapshot at call time, matching spec semantics for arrays
    /// whose length doesn't change mid-iteration.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.prototype.foreach>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.map>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.filter>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.reduce>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.find>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.findindex>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.every>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.some>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.flatmap>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.sort>
    #[allow(clippy::too_many_arguments)]
    fn array_callback_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        arr: &JsArray,
        name: &str,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<bool, VmError> {
        // `sort` without a comparator falls through to the intrinsic
        // table's lexicographic path. Comparator-driven sort is
        // handled here.
        if name == "sort" && matches!(args.first(), None | Some(Value::Undefined)) {
            return Ok(false);
        }

        let arr_value = Value::Array(*arr);
        // Snapshot the elements so callback-driven mutation of the
        // receiver does not corrupt iteration. Foundation matches
        // ECMA-262's "single-pass over indices 0..len" by capturing
        // length at entry; growing the array inside the callback
        // does not extend the walk (spec-compliant for `forEach` /
        // `map` / `filter`).
        let elements: Vec<Value> =
            crate::array::with_elements(*arr, &self.gc_heap, |elements| elements.to_vec());
        let len = elements.len();

        let top_idx = stack.len() - 1;
        // Advance pc up front so each synchronous callback returns to
        // the next caller instruction.
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;

        let result = match name {
            "forEach" => {
                let callee = require_callable(args.first())?;
                for (i, value) in elements.into_iter().enumerate() {
                    if matches!(value, Value::Hole) {
                        continue;
                    }
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                }
                Value::Undefined
            }
            "map" => {
                // §23.1.3.21: callback NOT invoked for holes; the
                // result array preserves holes at the same indices.
                let callee = require_callable(args.first())?;
                let mut out: Vec<Value> = Vec::with_capacity(len);
                for (i, value) in elements.into_iter().enumerate() {
                    if matches!(value, Value::Hole) {
                        out.push(Value::Hole);
                        continue;
                    }
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    out.push(self.run_callable_sync(
                        context,
                        &callee,
                        Value::Undefined,
                        cb_args,
                    )?);
                }
                let result = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    out.iter().cloned(),
                    &[&arr_value, &callee],
                    &[args.as_slice(), out.as_slice()],
                )?;
                Value::Array(result)
            }
            "filter" => {
                let callee = require_callable(args.first())?;
                let mut out: Vec<Value> = Vec::new();
                for (i, value) in elements.into_iter().enumerate() {
                    if matches!(value, Value::Hole) {
                        continue;
                    }
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let kept =
                        self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    if kept.to_boolean() {
                        out.push(crate::array::get(*arr, &self.gc_heap, i));
                    }
                }
                let result = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    out.iter().cloned(),
                    &[&arr_value, &callee],
                    &[args.as_slice(), out.as_slice()],
                )?;
                Value::Array(result)
            }
            "reduce" | "reduceRight" => {
                // §23.1.3.24 / §23.1.3.25: skip holes; if no
                // initialValue and every slot is a hole, raise
                // TypeError.
                let callee = require_callable(args.first())?;
                let has_init = args.len() >= 2;
                let initial = if has_init {
                    args[1].clone()
                } else {
                    Value::Undefined
                };
                let reverse = name == "reduceRight";
                let mut acc;
                let start_idx: i64;
                let step: i64 = if reverse { -1 } else { 1 };
                if has_init {
                    acc = initial;
                    start_idx = if reverse {
                        len.saturating_sub(1) as i64
                    } else {
                        0
                    };
                } else {
                    let mut seed_idx: Option<usize> = None;
                    if reverse {
                        for i in (0..len).rev() {
                            if !matches!(elements[i], Value::Hole) {
                                seed_idx = Some(i);
                                break;
                            }
                        }
                    } else {
                        for (i, value) in elements.iter().enumerate() {
                            if !matches!(value, Value::Hole) {
                                seed_idx = Some(i);
                                break;
                            }
                        }
                    }
                    let seed = seed_idx.ok_or(VmError::TypeMismatch)?;
                    acc = elements[seed].clone();
                    start_idx = seed as i64 + step;
                }
                let mut i = start_idx;
                while i >= 0 && (i as usize) < len {
                    if matches!(elements[i as usize], Value::Hole) {
                        i += step;
                        continue;
                    }
                    let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                    cb_args.push(acc.clone());
                    cb_args.push(elements[i as usize].clone());
                    cb_args.push(Value::Number(NumberValue::from_i32(i as i32)));
                    cb_args.push(arr_value.clone());
                    acc = self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    i += step;
                }
                acc
            }
            "find" => {
                // §23.1.3.10: holes are visited but produce
                // `undefined` for the callback's element argument.
                let callee = require_callable(args.first())?;
                let mut found = Value::Undefined;
                for (i, value) in elements.into_iter().enumerate() {
                    let elem = if matches!(value, Value::Hole) {
                        Value::Undefined
                    } else {
                        value
                    };
                    let cb_args = build_array_cb_args(&elem, i, &arr_value);
                    let hit =
                        self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    if hit.to_boolean() {
                        found = elem;
                        break;
                    }
                }
                found
            }
            "findIndex" => {
                // §23.1.3.11: same hole semantics as `find`.
                let callee = require_callable(args.first())?;
                let mut idx: i32 = -1;
                for (i, value) in elements.into_iter().enumerate() {
                    let elem = if matches!(value, Value::Hole) {
                        Value::Undefined
                    } else {
                        value
                    };
                    let cb_args = build_array_cb_args(&elem, i, &arr_value);
                    let hit =
                        self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    if hit.to_boolean() {
                        idx = i as i32;
                        break;
                    }
                }
                Value::Number(NumberValue::from_i32(idx))
            }
            "every" => {
                // §23.1.3.6: callback NOT invoked for holes.
                let callee = require_callable(args.first())?;
                let mut all = true;
                for (i, value) in elements.into_iter().enumerate() {
                    if matches!(value, Value::Hole) {
                        continue;
                    }
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let hit =
                        self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    if !hit.to_boolean() {
                        all = false;
                        break;
                    }
                }
                Value::Boolean(all)
            }
            "some" => {
                // §23.1.3.27: callback NOT invoked for holes.
                let callee = require_callable(args.first())?;
                let mut any = false;
                for (i, value) in elements.into_iter().enumerate() {
                    if matches!(value, Value::Hole) {
                        continue;
                    }
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let hit =
                        self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    if hit.to_boolean() {
                        any = true;
                        break;
                    }
                }
                Value::Boolean(any)
            }
            "flatMap" => {
                // §23.1.3.12: callback NOT invoked for holes; the
                // hole simply contributes nothing to the flattened
                // result.
                let callee = require_callable(args.first())?;
                let mut out: Vec<Value> = Vec::with_capacity(len);
                for (i, value) in elements.into_iter().enumerate() {
                    if matches!(value, Value::Hole) {
                        continue;
                    }
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let mapped =
                        self.run_callable_sync(context, &callee, Value::Undefined, cb_args)?;
                    match mapped {
                        Value::Array(inner) => {
                            crate::array::with_elements(inner, &self.gc_heap, |elements| {
                                out.extend(elements.iter().cloned());
                            });
                        }
                        other => out.push(other),
                    }
                }
                let result = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    out.iter().cloned(),
                    &[&arr_value, &callee],
                    &[args.as_slice(), out.as_slice()],
                )?;
                Value::Array(result)
            }
            "sort" => {
                // §23.1.3.30: SortIndexedProperties sorts only
                // present elements; holes (and any explicit
                // `undefined`s, but we keep those in the sort) are
                // pushed to the end of the array.
                let callee = require_callable(args.first())?;
                let mut buffer: Vec<Value> = Vec::with_capacity(elements.len());
                let mut hole_count: usize = 0;
                for v in elements {
                    if matches!(v, Value::Hole) {
                        hole_count += 1;
                    } else {
                        buffer.push(v);
                    }
                }
                // Manual insertion sort over the present-elements
                // snapshot — a closure-driven `sort_by` would have
                // to call back into the interpreter from inside
                // `Ord::cmp`. O(n²), correctness-first.
                let n = buffer.len();
                for i in 1..n {
                    let mut j = i;
                    while j > 0 {
                        let mut cmp_args: SmallVec<[Value; 8]> = SmallVec::new();
                        cmp_args.push(buffer[j - 1].clone());
                        cmp_args.push(buffer[j].clone());
                        let outcome =
                            self.run_callable_sync(context, &callee, Value::Undefined, cmp_args)?;
                        let order = match outcome {
                            Value::Number(n) => n.as_f64(),
                            _ => 0.0,
                        };
                        if order > 0.0 {
                            buffer.swap(j - 1, j);
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                }
                {
                    crate::array::with_elements_mut(*arr, &mut self.gc_heap, |elements| {
                        elements.clear();
                        elements.extend(buffer);
                        for _ in 0..hole_count {
                            elements.push(Value::Hole);
                        }
                    });
                }
                arr_value.clone()
            }
            _ => return Ok(false),
        };

        let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame_top, dst, result)?;
        Ok(true)
    }
    fn dispatch_function_method(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: &Value,
        name: &str,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        match name {
            "call" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                stack[top_idx].pc = stack[top_idx]
                    .pc
                    .checked_add(1)
                    .ok_or(VmError::InvalidOperand)?;
                self.invoke(stack, context, callee, this_value, forwarded, dst)
            }
            "apply" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                    Some(arg_array) => self.create_list_from_array_like(context, arg_array)?,
                };
                stack[top_idx].pc = stack[top_idx]
                    .pc
                    .checked_add(1)
                    .ok_or(VmError::InvalidOperand)?;
                self.invoke(stack, context, callee, this_value, forwarded, dst)
            }
            "bind" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &self.gc_heap,
                    &self.string_heap,
                    &self.function_user_props,
                    &self.function_deleted_metadata,
                );
                let metadata =
                    function_metadata::bound_create_metadata(&ctx, callee, bound_args.len())?;
                let callee_root = callee.clone();
                let this_root = this_value.clone();
                let bound_args_root = bound_args.clone();
                let roots = self.collect_allocation_roots(stack);
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    callee_root.trace_value_slots(visitor);
                    this_root.trace_value_slots(visitor);
                    for arg in &bound_args_root {
                        arg.trace_value_slots(visitor);
                    }
                };
                let bound = BoundFunction::new_with_metadata_and_roots(
                    &mut self.gc_heap,
                    callee.clone(),
                    this_value,
                    bound_args,
                    metadata,
                    &mut external_visit,
                )?;
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::BoundFunction(bound))?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(())
            }
            // §20.2.3.5 Function.prototype.toString — foundation
            // returns the canonical `function <name>() { [native
            // code] }` placeholder. Spec mandates a source-faithful
            // representation when source is available; the
            // foundation defers source preservation to a follow-up.
            // <https://tc39.es/ecma262/#sec-function.prototype.tostring>
            "toString" => {
                let ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &self.gc_heap,
                    &self.string_heap,
                    &self.function_user_props,
                    &self.function_deleted_metadata,
                );
                let display = function_metadata::callable_to_string(&ctx, callee);
                let s = JsString::from_str(&display, &self.string_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::String(s))?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(())
            }
            _ => Err(VmError::UnknownIntrinsic {
                name: name.to_string(),
            }),
        }
    }
}

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
    JsString, NativeCallInfo, NativeCtx, NumberValue, Value, VmError, VmGetOutcome, VmPropertyKey,
    array_prototype, bigint, binary,
    boolean::prototype as boolean_prototype,
    bootstrap_collections, bound_function_object_prototype_intercept, build_array_cb_args,
    collections_prototype, date, descriptor_value, function_metadata, intl, intrinsic_to_vm_error,
    is_callable, native_function_object_prototype_intercept, native_to_vm_error, number,
    object_prototype_intercept,
    operand_decode::{const_operand, register_operand},
    promise_dispatch, property_key_from_arg, read_register, regexp_prototype, require_callable,
    string::prototype as string_prototype,
    symbol_prototype, weak_refs, write_register,
};

/// Object-like family that needs §7.1.1 `ToPrimitive` coercion before
/// numeric / string arithmetic. Mirrors the matches! variant list used
/// throughout `CallMethodValue` arg-coercion preambles: every object-
/// shaped value plus `RegExp` (which carries an expando bag).
fn needs_to_primitive(v: &Value) -> bool {
    v.is_object()
        || v.is_array()
        || v.is_function()
        || v.is_closure()
        || v.is_native_function()
        || v.is_bound_function()
        || v.is_class_constructor()
        || v.is_proxy()
        || v.is_regexp()
}

/// Clamp a `ToIntegerOrInfinity` result to an absolute index within
/// `[0, len]` per the relative-index convention shared by §23.2.3
/// `slice` / `subarray` (negative counts from the end, `±Infinity`
/// saturate to the bounds).
fn relative_index_clamp(relative: f64, len: i64) -> i64 {
    if relative < 0.0 {
        let v = len as f64 + relative;
        if v < 0.0 { 0 } else { v as i64 }
    } else {
        relative.min(len as f64) as i64
    }
}

impl Interpreter {
    /// §22.1.3 — pre-coerce the arguments of a `String.prototype`
    /// method in place: index-like operands run full `ToNumber`
    /// (`ToIntegerOrInfinity`'s first step, so Symbol / BigInt raise
    /// TypeError at the right slot and user `@@toPrimitive` / `valueOf`
    /// fire), and string operands run `ToPrimitive(String)`. Shared by
    /// the primitive-string fast path in `do_call_method_value` and the
    /// `.call` / property bridge so both invocation styles coerce
    /// identically. A `RegExp` argument to `match` / `matchAll` /
    /// `search` / `normalize` passes through unchanged for its
    /// `@@`-method.
    pub(crate) fn coerce_string_method_args(
        &mut self,
        context: &ExecutionContext,
        name: &str,
        args: &mut [Value],
    ) -> Result<(), VmError> {
        let (int_coerce, str_coerce): (&[usize], &[usize]) = match name {
            "indexOf" | "lastIndexOf" | "includes" | "startsWith" | "endsWith" => (&[1], &[0]),
            "slice" | "substring" | "substr" => (&[0, 1], &[]),
            "at" | "charAt" | "charCodeAt" | "codePointAt" => (&[0], &[]),
            "repeat" => (&[0], &[]),
            "padStart" | "padEnd" => (&[0], &[1]),
            "replace" | "replaceAll" => (&[], &[0]),
            "split" => (&[1], &[0]),
            "concat" => (&[], &[0, 1, 2, 3]),
            "match" | "matchAll" | "search" | "normalize" => (&[], &[0]),
            "anchor" | "fontcolor" | "fontsize" | "link" => (&[], &[0]),
            _ => (&[], &[]),
        };
        if int_coerce.is_empty() && str_coerce.is_empty() {
            return Ok(());
        }
        let regexp_pass_through = matches!(name, "match" | "matchAll" | "search" | "normalize");
        let is_non_primitive = |v: &Value| {
            v.is_object()
                || v.is_array()
                || v.is_function()
                || v.is_closure()
                || v.is_native_function()
                || v.is_bound_function()
                || v.is_class_constructor()
                || v.is_proxy()
                || (!regexp_pass_through && v.is_regexp())
        };
        for &idx in int_coerce {
            let Some(&v) = args.get(idx) else {
                continue;
            };
            // Skip primitives the intrinsic body already recognises
            // (`undefined` is the "absent" sentinel some §B.2.3.1
            // substr-style methods key on).
            if v.is_number() || v.is_boolean() || v.is_null() || v.is_undefined() {
                continue;
            }
            let coerced = self.coerce_to_number(context, &v)?;
            args[idx] = Value::number(coerced);
        }
        for &idx in str_coerce {
            let Some(&v) = args.get(idx) else {
                continue;
            };
            if !is_non_primitive(&v) {
                continue;
            }
            let primitive = self.evaluate_to_primitive(
                context,
                &v,
                crate::abstract_ops::ToPrimitiveHint::String,
            )?;
            args[idx] = primitive;
        }
        Ok(())
    }

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
        let caller_byte_len = self.current_byte_len;
        let top_idx = stack.len() - 1;
        let recv_value = *read_register(&stack[top_idx], recv_reg)?;
        let mut arg_values: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            arg_values.push(*read_register(&stack[top_idx], r)?);
        }

        // Promise.prototype dispatches separately because it
        // needs `&mut self` to enqueue microtasks. User-installed
        // expando overrides (`p.then = fn`) take priority and
        // route through the ordinary callable path so test262
        // can observe Symbol.species / custom-then plumbing.
        if let Some(p) = recv_value.as_promise() {
            let promise = p;
            if let Some(bag) = promise.expando(&self.gc_heap)
                && let Some(method) = crate::object::get(bag, &self.gc_heap, name)
                && self.is_callable_runtime(&method)
            {
                let top_idx = stack.len() - 1;
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return self.invoke(stack, context, &method, recv_value, arg_values, dst);
            }
            let result = promise_dispatch::prototype_call(
                self,
                Some(context.clone()),
                &promise,
                name,
                arg_values.as_slice(),
            )
            .map_err(native_to_vm_error)?;
            let top_idx = stack.len() - 1;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }

        // `forEach` on a collection requires a callback dispatch
        // that pushes a frame; lives outside the static intrinsic
        // table so it can drive `self.invoke`.
        if name == "forEach" && (recv_value.is_map() || recv_value.is_set()) {
            return self.do_collection_for_each(stack, context, &recv_value, &arg_values, dst);
        }

        // §24.2.4 Set methods use `GetSetRecord(other)`, so they
        // may call user-visible `other.has` / `other.keys`. Route
        // through the native context path instead of the synchronous
        // intrinsic table, which has no interpreter re-entry handle.
        // <https://tc39.es/ecma262/#sec-getsetrecord>
        if recv_value.is_set() && bootstrap_collections::is_set_method_name(name) {
            let result = {
                let mut ctx = NativeCtx::new_with_call_info_and_context(
                    self,
                    NativeCallInfo::call(recv_value),
                    Some(context.clone()),
                );
                bootstrap_collections::set_method_call(&mut ctx, name, &arg_values)
                    .map_err(native_to_vm_error)?
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }

        // Iterator-helpers proposal — when receiver is an iterator
        // value, route through the dedicated dispatcher that builds
        // lazy wrappers / drains for terminals.
        // <https://tc39.es/proposal-iterator-helpers/>
        if let Some(rc) = recv_value.as_iterator() {
            let iter_rc = rc;
            if self.iterator_helper_dispatch(stack, context, &iter_rc, name, &arg_values, dst)? {
                return Ok(());
            }
        }

        // §27.5.3 Generator.prototype methods — `.next` / `.return`
        // / `.throw`. The receiver carries the suspended frame; the
        // resume helper drives a sub-dispatch until the next Yield
        // or completion.
        // <https://tc39.es/ecma262/#sec-generator-objects>
        if let Some(g) = recv_value.as_generator() {
            let kind = match name {
                "next" => Some(GeneratorResumeKind::Next(
                    arg_values.first().cloned().unwrap_or(Value::undefined()),
                )),
                "return" => Some(GeneratorResumeKind::Return(
                    arg_values.first().cloned().unwrap_or(Value::undefined()),
                )),
                "throw" => Some(GeneratorResumeKind::Throw(
                    arg_values.first().cloned().unwrap_or(Value::undefined()),
                )),
                _ => None,
            };
            if let Some(kind) = kind {
                let is_async_gen = g.is_async(&self.gc_heap);
                if is_async_gen {
                    // §27.6.3 — async-generator method calls always
                    // return a Promise. Queue the request; only a
                    // suspended generator resumes immediately.
                    let cap = promise_dispatch::PromiseBuilder::with_context(context.clone())
                        .capability_stack_rooted(
                            self,
                            stack,
                            &[&recv_value],
                            &[arg_values.as_slice()],
                        )?;
                    let promise = cap.promise;

                    if g.async_state(&self.gc_heap)
                        == crate::generator::AsyncGeneratorState::Completed
                    {
                        match kind {
                            GeneratorResumeKind::Throw(reason) => {
                                self.async_generator_settle_capability(
                                    context,
                                    &cap,
                                    Err(reason),
                                    true,
                                )?;
                            }
                            GeneratorResumeKind::Next(_) => {
                                self.async_generator_settle_capability(
                                    context,
                                    &cap,
                                    Ok(Value::undefined()),
                                    true,
                                )?;
                            }
                            GeneratorResumeKind::Return(value) => {
                                self.async_generator_settle_capability(
                                    context,
                                    &cap,
                                    Ok(value),
                                    true,
                                )?;
                            }
                        }
                    } else {
                        let state = g.async_state(&self.gc_heap);
                        g.enqueue_async_request(&mut self.gc_heap, kind, cap.clone());
                        if matches!(
                            state,
                            crate::generator::AsyncGeneratorState::SuspendedStart
                                | crate::generator::AsyncGeneratorState::SuspendedYield
                        ) {
                            let resume = g
                                .front_async_resume(&self.gc_heap)
                                .ok_or(VmError::InvalidOperand)?;
                            self.resume_generator(context, &g, resume)?;
                        }
                    }
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, promise)?;
                    frame.advance_pc(caller_byte_len)?;
                    return Ok(());
                }
                match self.resume_generator(context, &g, kind) {
                    Ok(result) => {
                        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                        write_register(frame, dst, result)?;
                        frame.advance_pc(self.current_byte_len)?;
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

        // §27.1.2 — Generator receivers walk through
        // `Iterator.prototype` for the iterator-helpers proposal
        // surface (`map` / `filter` / `take` / `drop` / `flatMap` /
        // `toArray` / `forEach` / `reduce` / `some` / `every` /
        // `find`). The direct Generator-method branch above only
        // handles `next` / `return` / `throw`; everything else
        // resolves through the global Iterator constructor's
        // prototype slot. Found methods invoke with the Generator
        // as the receiver so the foundation's
        // `iterator_receiver` wraps it on entry.
        if recv_value.is_generator() {
            let iterator_proto = {
                let v = crate::object::get(self.global_this, &self.gc_heap, "Iterator");
                if let Some(ctor) = v.and_then(|v| v.as_object()) {
                    crate::object::get(ctor, &self.gc_heap, "prototype")
                } else if let Some(ctor) = v.and_then(|v| v.as_native_function()) {
                    ctor.own_property_descriptor(&mut self.gc_heap, "prototype")
                        .ok()
                        .flatten()
                        .and_then(|d| match d.kind {
                            crate::object::DescriptorKind::Data { value } => Some(value),
                            _ => None,
                        })
                } else {
                    None
                }
            };
            if let Some(proto) = iterator_proto.and_then(|v| v.as_object())
                && let Some(method) = crate::object::get(proto, &self.gc_heap, name)
                && self.is_callable_runtime(&method)
            {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                self.invoke(stack, context, &method, recv_value, arg_values, dst)?;
                return Ok(());
            }
        }

        // §23.1.3 callback-driven Array.prototype methods. The
        // intrinsic table can't drive callbacks, so the foundation
        // dispatches them here via `run_callable_sync`. Each method
        // matches its ECMA-262 algorithm with sloppy edge handling
        // (sparse holes, throwing comparators, length mutation
        // mid-walk) deferred to follow-ups.
        // Callback-driven `Array.prototype` methods on an Array receiver
        // funnel into the single live driver
        // (`array_callback_native_dispatch`), the same path the `.call` /
        // property bridge uses, so `arr.map(cb)` and
        // `Array.prototype.map.call(arr, cb)` run identical spec logic:
        // `len` read once, then a live `Get(O, k)` per index (a callback
        // mutating the receiver is observed). `sort` keeps its dedicated
        // comparator dispatcher.
        if recv_value.is_array()
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
            )
        {
            let result = {
                let mut ctx = NativeCtx::new_with_call_info_and_context(
                    self,
                    NativeCallInfo::call(recv_value),
                    Some(context.clone()),
                );
                array_prototype::array_callback_native_dispatch(name, &mut ctx, &arg_values)
                    .map_err(native_to_vm_error)?
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §23.1.3.30 — `sort` runs the re-entrant SortIndexedProperties
        // driver (comparator validity, generic receiver, Get/Set/Delete
        // + comparator re-entry, stable order).
        if recv_value.is_array() && name == "sort" {
            let comparefn = arg_values.first().copied().unwrap_or_else(Value::undefined);
            let result =
                self.array_sort(context, recv_value, comparefn, &[arg_values.as_slice()])?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §23.1.3.1 — `concat` on an Array receiver funnels into the
        // re-entrant driver (same path as `.call`), so spreadable
        // arguments, `@@isConcatSpreadable`, and array-like `length` /
        // indexed getters are observed.
        if recv_value.is_array() && name == "concat" {
            let result =
                self.array_concat(context, recv_value, &arg_values, &[arg_values.as_slice()])?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §23.1.3.26 / .34 — `shift` / `unshift` funnel through
        // re-entrant drivers so inherited indices and strict
        // length/set/delete failures match the `.call` bridge.
        if recv_value.is_array() && matches!(name, "shift" | "unshift") {
            let result = if name == "shift" {
                self.array_shift(context, recv_value, &[arg_values.as_slice()])
            } else {
                self.array_unshift(context, recv_value, &arg_values, &[arg_values.as_slice()])
            }?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §23.1.3.4 — `copyWithin` reads length, coerces indices, then
        // copies through live HasProperty/Get/Set/Delete.
        if recv_value.is_array() && name == "copyWithin" {
            let result =
                self.array_copy_within(context, recv_value, &arg_values, &[arg_values.as_slice()])?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §23.1.3.14 / .18 / .13 — `indexOf` / `lastIndexOf` /
        // `includes` on an Array receiver. The intrinsic-table impls
        // walk only the dense element store, so they miss inherited /
        // sparse indices and a getter that mutates the receiver
        // mid-search. Route through the single re-entrant driver
        // (shared with the `.call` path) while a context is in scope.
        if recv_value.is_array() && matches!(name, "indexOf" | "lastIndexOf" | "includes") {
            let search = arg_values.first().copied().unwrap_or_else(Value::undefined);
            let from_arg = arg_values.get(1).copied();
            let result = self.array_indexed_search(
                context,
                recv_value,
                name,
                search,
                from_arg,
                &[arg_values.as_slice()],
            )?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §22.1.3 — a primitive-string receiver routes every known
        // `String.prototype` method through the single re-entrant
        // `string_method_call`, the same path the `.call` / property
        // bridge uses. Receiver `ToString`, argument coercion (the
        // shared `coerce_string_method_args`), callable `replace`, and
        // `@@`-method delegation all live there now. Unknown names fall
        // through to the ordinary property / prototype walk below.
        if recv_value.is_string() && string_prototype::lookup(name).is_some() {
            let result = {
                let mut ctx = NativeCtx::new_with_call_info_and_context(
                    self,
                    NativeCallInfo::call(recv_value),
                    Some(context.clone()),
                );
                string_prototype::string_method_call(&mut ctx, name, &arg_values)
                    .map_err(native_to_vm_error)?
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §23.2.3.{8,11,12,13,14,15,17,18,21,22,28} — TypedArray
        // prototype callback methods. Same shape as the Array set
        // but routed through a TypedArray-specific dispatcher so
        // map / filter / etc. allocate a new TypedArray of the
        // receiver's kind instead of a plain Array.
        if let Some(t) = recv_value.as_typed_array(&self.gc_heap)
            && matches!(
                name,
                "forEach"
                    | "map"
                    | "filter"
                    | "find"
                    | "findIndex"
                    | "findLast"
                    | "findLastIndex"
                    | "every"
                    | "some"
                    | "reduce"
                    | "reduceRight"
            )
            && self.typed_array_callback_dispatch(stack, context, &t, name, &arg_values, dst)?
        {
            return Ok(());
        }
        // §23.2.3.26 `%TypedArray%.prototype.slice` allocates its
        // result through `TypedArraySpeciesCreate` and coerces its
        // `start` / `end` operands through `ToIntegerOrInfinity`
        // (which observes user `@@toPrimitive` / `valueOf`). The
        // intrinsic-table impl can do neither, so intercept here while
        // a re-entrant interpreter handle is in scope.
        if let Some(t) = recv_value.as_typed_array(&self.gc_heap)
            && name == "slice"
        {
            self.typed_array_slice_dispatch(stack, context, &t, &arg_values, dst)?;
            return Ok(());
        }
        // §23.2.3.27 `%TypedArray%.prototype.subarray` likewise coerces
        // its operands through `ToIntegerOrInfinity` and allocates its
        // result view through `TypedArraySpeciesCreate`.
        if let Some(t) = recv_value.as_typed_array(&self.gc_heap)
            && name == "subarray"
        {
            self.typed_array_subarray_dispatch(stack, context, &t, &arg_values, dst)?;
            return Ok(());
        }
        // §22.1.3.18 / §22.1.3.19 — `String.prototype.replace` and
        // `replaceAll` with a callable replaceValue dispatch through
        // the interpreter to invoke the callback. The intrinsic
        // table can't run callbacks (it lacks an
        // `ExecutionContext`), so intercept here before the table
        // lookup and route through the dedicated bridge.
        //
        // Wrapper objects (`new String("…")`) also reach this arm —
        // unwrap their `[[StringData]]` so the receiver flows in as
        // a primitive string for the callable-replace bridge.
        let string_recv: Option<Value> = if recv_value.is_string() {
            Some(recv_value)
        } else if let Some(obj) = recv_value.as_object() {
            crate::object::string_data(obj, &self.gc_heap).map(Value::string)
        } else {
            None
        };
        if let Some(string_recv) = string_recv
            && (name == "replace" || name == "replaceAll")
            && arg_values.len() >= 2
            && self.is_callable_runtime(&arg_values[1])
            && !arg_values.first().is_some_and(|v| v.is_regexp())
        {
            let recv_value = string_recv;
            // §22.1.3.18 step 7 — `searchString = ? ToString(searchValue)`.
            // Coerce non-String searchValues (null, undefined, numbers,
            // objects with `toString`) before handing the args to the
            // callable-replace bridge.
            let mut coerced_args = arg_values.clone();
            let needs_coerce = !coerced_args.first().is_some_and(|v| v.is_string());
            if needs_coerce {
                let original = coerced_args.first().cloned().unwrap_or(Value::undefined());
                let coerced = if original.is_undefined() {
                    "undefined".to_string()
                } else if original.is_null() {
                    "null".to_string()
                } else if let Some(b) = original.as_boolean() {
                    if b { "true" } else { "false" }.to_string()
                } else if let Some(n) = original.as_number() {
                    n.to_display_string()
                } else if let Some(b) = original.as_big_int() {
                    b.to_decimal_string(&self.gc_heap)
                } else if original.is_symbol() {
                    return Err(VmError::TypeError {
                        message: "Cannot convert a Symbol value to a string".to_string(),
                    });
                } else if original.is_object()
                    || original.is_array()
                    || original.is_function()
                    || original.is_closure()
                    || original.is_native_function()
                    || original.is_bound_function()
                    || original.is_class_constructor()
                    || original.is_proxy()
                {
                    let primitive = self.evaluate_to_primitive(
                        context,
                        &original,
                        crate::abstract_ops::ToPrimitiveHint::String,
                    )?;
                    if let Some(s) = primitive.as_string(&self.gc_heap) {
                        s.to_lossy_string(&self.gc_heap)
                    } else if let Some(n) = primitive.as_number() {
                        n.to_display_string()
                    } else if let Some(b) = primitive.as_boolean() {
                        if b { "true" } else { "false" }.to_string()
                    } else if primitive.is_null() {
                        "null".to_string()
                    } else if primitive.is_undefined() {
                        "undefined".to_string()
                    } else if let Some(b) = primitive.as_big_int() {
                        b.to_decimal_string(&self.gc_heap)
                    } else if primitive.is_symbol() {
                        return Err(VmError::TypeError {
                            message: "Cannot convert a Symbol value to a string".to_string(),
                        });
                    } else {
                        return Err(VmError::TypeMismatch);
                    }
                } else {
                    return Err(VmError::TypeMismatch);
                };
                if let Some(slot) = coerced_args.first_mut() {
                    *slot = Value::string(JsString::from_str(&coerced, self.gc_heap_mut())?);
                }
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            let result = self.dispatch_string_callable_replace(
                context,
                &recv_value,
                &coerced_args,
                name == "replaceAll",
            )?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            return Ok(());
        }
        // Primitive prototypes go through the intrinsic table —
        // synchronous, no frame push, advance pc and write directly.
        let intrinsic = if recv_value.is_string() {
            string_prototype::lookup(name)
        } else if recv_value.is_array() {
            array_prototype::lookup(name)
        } else if recv_value.is_number() {
            number::prototype_lookup(name)
        } else if recv_value.is_boolean() {
            boolean_prototype::lookup(name)
        } else if recv_value.is_big_int() {
            bigint::prototype::lookup(name)
        } else if recv_value
            .as_object()
            .is_some_and(|o| crate::object::date_data(o, &self.gc_heap).is_some())
        {
            // Date instances are ordinary objects with a
            // `[[DateValue]]` internal slot — when the receiver
            // is an Object we probe `crate::object::date_data` to
            // brand-check and route through the Date intrinsic
            // table.
            date::prototype::lookup(name)
        } else if recv_value.is_regexp() {
            regexp_prototype::lookup(name)
        } else if recv_value.is_symbol() {
            symbol_prototype::lookup(name)
        } else if recv_value.is_map() {
            collections_prototype::lookup_map(name)
        } else if recv_value.is_set() {
            collections_prototype::lookup_set(name)
        } else if recv_value.is_weak_map() {
            collections_prototype::lookup_weak_map(name)
        } else if recv_value.is_weak_set() {
            collections_prototype::lookup_weak_set(name)
        } else if recv_value.is_weak_ref() {
            weak_refs::lookup_weak_ref(name)
        } else if recv_value.is_finalization_registry() {
            weak_refs::lookup_finalization_registry(name)
        } else if recv_value.is_intl() {
            intl::lookup_prototype(&recv_value, &self.gc_heap, name)
        } else if recv_value.is_array_buffer() {
            binary::array_buffer_prototype::lookup(name)
        } else if recv_value.is_data_view() {
            binary::data_view_prototype::lookup(name)
        } else if recv_value.is_typed_array() {
            binary::typed_array_prototype::lookup(name)
        } else {
            None
        };
        if let Some(entry) = intrinsic {
            // §21.1.3.{3,4,5} — `Number.prototype.{toFixed,
            // toExponential, toPrecision}` start with
            // `ToIntegerOrInfinity` on their argument, which runs
            // `ToNumber` → `ToPrimitive(arg, "number")`. Non-
            // primitive arguments must observe `@@toPrimitive` /
            // `valueOf` / `toString`. Pre-coerce here before the
            // intrinsic so the spec ladder fires and Symbol / BigInt
            // surface the correct error class.
            let mut small_args: SmallVec<[Value; 4]> = arg_values.iter().cloned().collect();
            if recv_value.is_number() && matches!(name, "toFixed" | "toExponential" | "toPrecision")
            {
                for slot in small_args.iter_mut() {
                    if needs_to_primitive(slot) {
                        let primitive = self.evaluate_to_primitive(
                            context,
                            slot,
                            crate::abstract_ops::ToPrimitiveHint::Number,
                        )?;
                        *slot = primitive;
                    }
                }
            }
            // Pre-coerce integer-typed args through the
            // `ToNumber` → `ToPrimitive(Number)` ladder so the
            // intrinsic's `arg_signed_index` strict guard observes
            // user `@@toPrimitive` / `valueOf` / `toString` side
            // effects per spec rather than tripping
            // `TypeMismatch`. Each tuple lists the argument indices
            // whose `ToIntegerOrInfinity(…)` invocation lives at
            // the top of the algorithm header. Method receivers are
            // intentionally restricted to Array / Object — the
            // primitive-receiver short-circuit returns the unmodified
            // value before the intrinsic body runs.
            let int_coerce_indices: &[usize] = match name {
                // §23.1.3.14 / .17 / .15
                "indexOf" | "lastIndexOf" | "includes" => &[1],
                // §23.1.3.7 fill(value, start, end)
                "fill" => &[1, 2],
                // §23.1.3.26 slice(start, end)
                "slice" => &[0, 1],
                // §23.1.3.1 at(index)
                "at" => &[0],
                _ => &[],
            };
            if !int_coerce_indices.is_empty() && (recv_value.is_array() || recv_value.is_object()) {
                for &idx in int_coerce_indices {
                    let Some(slot) = small_args.get_mut(idx) else {
                        continue;
                    };
                    if !needs_to_primitive(slot) {
                        continue;
                    }
                    let primitive = self.evaluate_to_primitive(
                        context,
                        slot,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )?;
                    *slot = primitive;
                }
            }
            // §22.1.3.* String.prototype.* `position` / `start` /
            // `end` args run `ToIntegerOrInfinity(arg)`; searchString
            // operands run `ToString(arg)` which itself starts with
            // `ToPrimitive(arg, "string")`. Pre-coerce both shapes
            // when the receiver is a String primitive so user
            // `@@toPrimitive` / `valueOf` / `toString` fires per spec.
            // §24.3.1.{1,2} GetViewValue / SetViewValue on
            // `DataView.prototype.*` — pre-coerce `byteOffset` (and
            // setter `value`) through `ToPrimitive(Number)` so user
            // `@@toPrimitive` / `valueOf` / `toString` fire before
            // the intrinsic's strict numeric guard.
            if recv_value.is_data_view() {
                let dv_int_coerce: &[usize] = if name.starts_with("get") {
                    &[0]
                } else if name.starts_with("set") {
                    &[0, 1]
                } else {
                    &[]
                };
                for &idx in dv_int_coerce {
                    let Some(slot) = small_args.get_mut(idx) else {
                        continue;
                    };
                    if !needs_to_primitive(slot) {
                        continue;
                    }
                    let primitive = self.evaluate_to_primitive(
                        context,
                        slot,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )?;
                    *slot = primitive;
                }
            }
            // §23.2.3.{8,5,18,16,17} `fill` / `copyWithin` /
            // `includes` / `indexOf` / `lastIndexOf` open with
            // `ToNumber` / `ToIntegerOrInfinity` on their operands; the
            // intrinsic-table impls read raw `Value`s and cannot
            // re-enter. Pre-coerce here (a re-entrant interpreter
            // handle is in scope) so user `@@toPrimitive` / `valueOf`
            // fire in spec order — `fill` coerces its value (step 3)
            // before the `start` / `end` indices (steps 4-7) — and
            // abrupt completions surface.
            if recv_value.is_typed_array() {
                let is_bigint_kind = recv_value
                    .as_typed_array(&self.gc_heap)
                    .is_some_and(|t| t.kind().is_bigint());
                if name == "fill"
                    && let Some(value) = small_args.first().copied()
                {
                    if is_bigint_kind {
                        let b = crate::coerce::to_big_int_or_throw(self, context, &value)?;
                        small_args[0] = Value::big_int(b);
                    } else if !value.is_number() {
                        let n = self.coerce_to_number(context, &value)?;
                        small_args[0] = Value::number(n);
                    }
                }
                // NOTE: `copyWithin` is intentionally excluded — its
                // detached-during-coercion path (a `valueOf` that
                // detaches the backing buffer) hangs the conformance
                // runner today; re-enable once that path is fixed.
                let ta_int_coerce: &[usize] = match name {
                    "fill" => &[1, 2],
                    "copyWithin" => &[0, 1, 2],
                    "includes" | "indexOf" | "lastIndexOf" => &[1],
                    _ => &[],
                };
                for &idx in ta_int_coerce {
                    let Some(value) = small_args.get(idx).copied() else {
                        continue;
                    };
                    if value.is_number() || value.is_undefined() {
                        continue;
                    }
                    let n = self.coerce_to_number(context, &value)?;
                    small_args[idx] = Value::number(n);
                }
            }
            if recv_value.is_string() {
                self.coerce_string_method_args(context, name, &mut small_args)?;
            }
            // §22.2.7.1 / .2 `RegExp.prototype.exec` / `test` —
            // step `S = ? ToString(string)` on the argument. The
            // intrinsic's `arg_to_string_primitive` only inspects
            // slots, so an Object arg with a user `toString` was
            // stringified to `"[object Object]"` (matching nothing).
            // Pre-coerce through `ToPrimitive(String)` so the user
            // callback fires; the intrinsic then finishes ToString.
            if recv_value.is_regexp()
                && matches!(name, "exec" | "test")
                && let Some(slot) = small_args.get_mut(0)
                && (slot.is_object()
                    || slot.is_array()
                    || slot.is_function()
                    || slot.is_closure()
                    || slot.is_native_function()
                    || slot.is_bound_function()
                    || slot.is_class_constructor()
                    || slot.is_proxy()
                    || slot.is_regexp()
                    || slot.is_promise()
                    || slot.is_map()
                    || slot.is_set())
            {
                let primitive = self.evaluate_to_primitive(
                    context,
                    slot,
                    crate::abstract_ops::ToPrimitiveHint::String,
                )?;
                *slot = primitive;
            }
            // §21.4.4.x `Date.prototype.set*` — capture `t` from
            // `[[DateValue]]` BEFORE coercing args (step 3), run
            // `ToNumber` on every provided arg in declaration order
            // (steps 4–7), then restore captured `t` so the intrinsic
            // body sees the value spec step 3 captured — `ToNumber`
            // callbacks may have mutated `[[DateValue]]` via
            // `dt.setTime(...)`, but the spec NaN-check (step 8) and
            // component math operate on the captured value. The
            // intrinsic's final assignment in step 12 then overwrites
            // any in-callback mutation.
            if let Some(obj) = recv_value.as_object()
                && let Some(captured_t) = crate::object::date_data(obj, &self.gc_heap)
                && name.starts_with("set")
            {
                for slot in small_args.iter_mut() {
                    let coerced = self.coerce_to_number(context, slot)?;
                    *slot = Value::number(coerced);
                }
                // §21.4.4.{20..36} step 8 — `setMonth` / `setDate` /
                // `setHours` / `setMinutes` / `setSeconds` /
                // `setMilliseconds` (and UTC variants) **return
                // NaN without writing** when the captured time was
                // NaN, even though `ToNumber` callbacks may have
                // mutated `[[DateValue]]` mid-flight. `setFullYear`,
                // `setUTCFullYear`, `setTime` and Annex B `setYear`
                // always write through, so they fall into the
                // normal restore-and-dispatch path below.
                let nan_preserving = matches!(
                    name,
                    "setMonth"
                        | "setUTCMonth"
                        | "setDate"
                        | "setUTCDate"
                        | "setHours"
                        | "setUTCHours"
                        | "setMinutes"
                        | "setUTCMinutes"
                        | "setSeconds"
                        | "setUTCSeconds"
                        | "setMilliseconds"
                        | "setUTCMilliseconds"
                );
                if captured_t.is_nan() && nan_preserving {
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::number(NumberValue::from_f64(f64::NAN)))?;
                    frame.advance_pc(self.current_byte_len)?;
                    return Ok(());
                }
                crate::object::set_date_data(obj, &mut self.gc_heap, captured_t);
            }
            let result = {
                let allocation_roots = self.collect_allocation_roots(stack);
                (entry.impl_fn)(&mut IntrinsicArgs {
                    receiver: &recv_value,
                    args: &small_args,
                    gc_heap: &mut self.gc_heap,
                    allocation_roots: allocation_roots.as_slice(),
                })
                .map_err(intrinsic_to_vm_error)?
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }

        if let Some(obj) = recv_value.as_object()
            && self.object_prototype_object_opt() != Some(obj)
            && matches!(
                crate::object::lookup(obj, &self.gc_heap, name),
                crate::object::PropertyLookup::Absent
            )
            && let Some(result) = {
                let fn_proto = self.function_prototype_object().ok();
                object_prototype_intercept(&obj, name, &arg_values, &mut self.gc_heap, fn_proto)
            }?
        {
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }

        // Functions / closures inherit Object.prototype-style
        // methods. Foundation routes the call through the user-
        // properties bag attached to the compiled function.
        let fn_id_for_proto = recv_value.as_function().or_else(|| {
            recv_value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fn_id_for_proto
            && matches!(
                name,
                "hasOwnProperty" | "propertyIsEnumerable" | "isPrototypeOf"
            )
        {
            let result = match name {
                "hasOwnProperty" => {
                    let key = property_key_from_arg(arg_values.first(), &self.gc_heap)?;
                    if key == "prototype" {
                        let _ = self.function_property_get(context, function_id, "prototype")?;
                    }
                    self.ordinary_function_own_property_descriptor(
                        Some(context),
                        function_id,
                        &key,
                    )?
                    .is_some()
                }
                "propertyIsEnumerable" => {
                    let key = property_key_from_arg(arg_values.first(), &self.gc_heap)?;
                    if key == "prototype" {
                        let _ = self.function_property_get(context, function_id, "prototype")?;
                    }
                    self.ordinary_function_own_property_descriptor(
                        Some(context),
                        function_id,
                        &key,
                    )?
                    .is_some_and(|desc| desc.enumerable())
                }
                "isPrototypeOf" => false,
                _ => unreachable!("guarded by method-name match"),
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, Value::boolean(result))?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        if let Some(native) = recv_value.as_native_function()
            && let Some(result) = native_function_object_prototype_intercept(
                &native,
                name,
                &arg_values,
                &mut self.gc_heap,
            )?
        {
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        if let Some(bound) = recv_value.as_bound_function()
            && let Some(result) =
                bound_function_object_prototype_intercept(&bound, name, &arg_values, &self.gc_heap)?
        {
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §7.1.18 ToObject — `String.prototype.hasOwnProperty(idx)`,
        // `(0).propertyIsEnumerable("toString")`, etc. inherit
        // `Object.prototype.{hasOwnProperty, propertyIsEnumerable,
        // isPrototypeOf}` through the primitive wrapper chain. The
        // wrapper isn't materialized; we answer directly from the
        // primitive shape: String exposes integer indices in
        // `[0, length)` plus `"length"`; every other primitive has
        // no own properties.
        if matches!(
            name,
            "hasOwnProperty" | "propertyIsEnumerable" | "isPrototypeOf"
        ) && (recv_value.is_string()
            || recv_value.is_number()
            || recv_value.is_boolean()
            || recv_value.is_symbol()
            || recv_value.is_big_int())
        {
            let result = match name {
                "hasOwnProperty" | "propertyIsEnumerable" => {
                    let key = property_key_from_arg(arg_values.first(), &self.gc_heap)?;
                    if let Some(s) = recv_value.as_string(&self.gc_heap) {
                        if key == "length" {
                            // propertyIsEnumerable is false for
                            // String wrapper's `length`; hasOwn
                            // is true.
                            name == "hasOwnProperty"
                        } else if let Ok(idx) = key.parse::<u32>() {
                            idx < s.len()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                "isPrototypeOf" => false,
                _ => unreachable!("guarded by method-name match"),
            };
            let frame = &mut stack[top_idx];
            write_register(frame, dst, Value::boolean(result))?;
            frame.advance_pc(self.current_byte_len)?;
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
                name,
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
        let is_property_bearing = recv_value.is_object()
            || recv_value.is_proxy()
            || recv_value.is_array()
            || recv_value.is_regexp()
            || recv_value.is_map()
            || recv_value.is_set()
            || recv_value.is_weak_map()
            || recv_value.is_weak_set()
            || recv_value.is_weak_ref()
            || recv_value.is_finalization_registry()
            || recv_value.is_promise()
            || recv_value.is_array_buffer()
            || recv_value.is_data_view()
            || recv_value.is_typed_array()
            || recv_value.is_iterator();
        let lookup_via_property: Option<Value> = if is_property_bearing {
            // Property-bearing exotic receivers route through
            // `ordinary_get_value` so user-installed own properties
            // shadow the intrinsic-table miss path.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(context, recv_value, recv_value, &key, 0)? {
                VmGetOutcome::Value(value) => Some(value),
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    Some(self.run_callable_sync(context, &getter, recv_value, args)?)
                }
            }
        } else if let Some(c) = recv_value.as_class_constructor() {
            Some(if name == "prototype" {
                Value::object(c.prototype(&self.gc_heap))
            } else {
                // Go through the full `[[Get]]` ladder so accessor
                // descriptors on static members invoke their getter.
                let statics = Value::object(c.statics(&self.gc_heap));
                let key = VmPropertyKey::String(name);
                match self.ordinary_get_value(context, statics, statics, &key, 0)? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, statics, args)?
                    }
                }
            })
        } else if let Some(fid) = recv_value.as_function().or_else(|| {
            recv_value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            // §10.1.8 OrdinaryGet on a callable receiver — user
            // properties resolve via the function-properties side table.
            Some(self.function_property_get_stack_rooted(context, stack, fid, name)?)
        } else if let Some(native) = recv_value.as_native_function() {
            // Native callable receiver — look up `name` on the function
            // object's own-property table.
            native
                .own_property_descriptor(&mut self.gc_heap, name)?
                .map(|desc| descriptor_value(&desc))
        } else if recv_value.is_boolean()
            || recv_value.is_number()
            || recv_value.is_symbol()
            || recv_value.is_big_int()
            || recv_value.is_temporal()
        {
            // §7.1.18 ToObject — primitive receivers walk the
            // constructor's prototype to surface inherited
            // `Object.prototype.*` methods.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(context, recv_value, recv_value, &key, 0)? {
                VmGetOutcome::Value(value) if !value.is_undefined() => Some(value),
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    Some(self.run_callable_sync(context, &getter, recv_value, args)?)
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(method) = lookup_via_property {
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
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
                name,
                arg_values,
                dst,
            );
        }

        Err(VmError::UnknownIntrinsic {
            name: name.to_string(),
        })
    }

    /// §22.1.3.18 / §22.1.3.19 callable replaceValue path. Walks
    /// the receiver string's UTF-16 units, locates each
    /// non-overlapping match of the (String-coerced) needle, and
    /// invokes the callback with `(matched, position, fullString)`
    /// per spec step 6.h. Returns the spliced result string.
    pub(crate) fn dispatch_string_callable_replace(
        &mut self,
        context: &ExecutionContext,
        receiver: &Value,
        args: &SmallVec<[Value; 8]>,
        replace_all: bool,
    ) -> Result<Value, VmError> {
        use crate::string::JsString;
        let recv = receiver
            .as_string(&self.gc_heap)
            .ok_or(VmError::TypeMismatch)?;
        let needle = args
            .first()
            .and_then(|v| v.as_string(&self.gc_heap))
            .ok_or(VmError::TypeMismatch)?;
        let callback = args.get(1).cloned().unwrap_or(Value::undefined());
        let recv_units = recv.to_utf16_vec(&self.gc_heap);
        let needle_units = needle.to_utf16_vec(&self.gc_heap);
        let needle_len = needle_units.len();
        let recv_value = Value::string(recv);
        let mut out: Vec<u16> = Vec::with_capacity(recv_units.len());
        if needle_len == 0 {
            let positions: Vec<usize> = if replace_all {
                (0..=recv_units.len()).collect()
            } else {
                vec![0]
            };
            for pos in positions {
                let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    Value::string(needle),
                    Value::number_f64(pos as f64),
                    recv_value,
                ];
                let raw =
                    self.run_callable_sync(context, &callback, Value::undefined(), cb_args)?;
                let raw_string = if let Some(s) = raw.as_string(&self.gc_heap) {
                    s
                } else {
                    JsString::from_str(&raw.display_string(&self.gc_heap), &mut self.gc_heap)
                        .map_err(|_| VmError::TypeMismatch)?
                };
                out.extend_from_slice(&raw_string.to_utf16_vec(&self.gc_heap));
                if pos < recv_units.len() {
                    out.push(recv_units[pos]);
                }
            }
            return Ok(Value::string(
                JsString::from_utf16_units(&out, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?,
            ));
        }
        if recv_units.len() < needle_len {
            // Needle longer than receiver — no match possible.
            return Ok(Value::string(recv));
        }
        let last_start = recv_units.len() - needle_len;
        let mut cursor: usize = 0;
        while cursor <= last_start {
            if recv_units[cursor..cursor + needle_len] == needle_units[..] {
                let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    Value::string(needle),
                    Value::number_f64(cursor as f64),
                    recv_value,
                ];
                let raw =
                    self.run_callable_sync(context, &callback, Value::undefined(), cb_args)?;
                let raw_string = if let Some(s) = raw.as_string(&self.gc_heap) {
                    s
                } else {
                    JsString::from_str(&raw.display_string(&self.gc_heap), &mut self.gc_heap)
                        .map_err(|_| VmError::TypeMismatch)?
                };
                out.extend_from_slice(&raw_string.to_utf16_vec(&self.gc_heap));
                cursor += needle_len;
                if !replace_all {
                    break;
                }
            } else {
                out.push(recv_units[cursor]);
                cursor += 1;
            }
        }
        out.extend_from_slice(&recv_units[cursor..]);
        Ok(Value::string(
            JsString::from_utf16_units(&out, &mut self.gc_heap)
                .map_err(|_| VmError::TypeMismatch)?,
        ))
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
    /// 1. For each live Map/Set entry, enqueue an inline call: every callback is
    ///    invoked synchronously through `self.invoke`. Because each
    ///    invoke pushes a frame and returns through the dispatch
    ///    loop, the foundation chains them by stashing the iteration
    ///    state in a tiny native closure that re-enters this helper.
    /// 2. Foundation simplification: rather than a re-entrant
    ///    chain, walk the receiver here and synchronously invoke
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
            Some(c) if is_callable(c) => *c,
            _ => return Err(VmError::NotCallable),
        };
        // §24.1.3.5 / §24.2.3.6 step 4 — when `thisArg` is supplied,
        // bind it as the callback's `this`; otherwise let
        // `OrdinaryCallBindThis` default to undefined / globalObject.
        let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
        if !(recv.is_map() || recv.is_set()) {
            return Err(VmError::TypeMismatch);
        }
        // Advance pc *before* invoking the callbacks so each
        // callback returns to the next instruction in the caller
        // frame.
        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        // Write `undefined` into the dst slot — `forEach` returns
        // `undefined` synchronously, even if the callback chain
        // produces values.
        write_register(&mut stack[top_idx], dst, Value::undefined())?;
        let recv_for_callback = *recv;
        if let Some(m) = recv.as_map() {
            let mut index = 0;
            while index < crate::collections::map_raw_len(m, &self.gc_heap) {
                let Some((key, value)) = crate::collections::map_entry_at(m, &self.gc_heap, index)
                else {
                    index += 1;
                    continue;
                };
                index += 1;
                let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                cb_args.push(value);
                cb_args.push(key);
                cb_args.push(recv_for_callback);
                self.run_callable_sync(context, &callee, this_arg, cb_args)?;
            }
        } else if let Some(s) = recv.as_set() {
            let mut index = 0;
            while index < crate::collections::set_raw_len(s, &self.gc_heap) {
                let Some(value) = crate::collections::set_value_at(s, &self.gc_heap, index) else {
                    index += 1;
                    continue;
                };
                index += 1;
                let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                cb_args.push(value);
                cb_args.push(value);
                cb_args.push(recv_for_callback);
                self.run_callable_sync(context, &callee, this_arg, cb_args)?;
            }
        } else {
            unreachable!();
        }
        Ok(())
    }

    /// §23.2.3 TypedArray prototype callback methods —
    /// `forEach` / `map` / `filter` / `find` / `findIndex` /
    /// `findLast` / `findLastIndex` / `every` / `some` / `reduce` /
    /// `reduceRight`. Same shape as the Array prototype family but
    /// element snapshots come from the TypedArray's backing buffer
    /// and `map` / `filter` allocate a fresh TypedArray of the
    /// receiver's kind.
    ///
    /// <https://tc39.es/ecma262/#sec-typedarray.prototype-objects>
    #[allow(clippy::too_many_arguments)]
    fn typed_array_callback_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        t: &crate::binary::typed_array::JsTypedArray,
        name: &str,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<bool, VmError> {
        let ta_value = Value::typed_array(*t);
        let len = t.length(&self.gc_heap);
        let elements: Vec<Value> = {
            let mut tmp = Vec::with_capacity(len);
            for i in 0..len {
                tmp.push(t.get(&mut self.gc_heap, i).map_err(crate::oom_to_vm)?);
            }
            tmp
        };

        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc(self.current_byte_len)?;

        let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

        let result = match name {
            "forEach" => {
                let callee = require_callable(args.first())?;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                }
                Value::undefined()
            }
            "map" => {
                // §23.2.3.20 — `A = ? TypedArraySpeciesCreate(O, « len »)`
                // (step 5) runs before any callback, so a throwing /
                // overriding species constructor is observed first. `A`
                // is parked in `dst` immediately so it is GC-rooted (via
                // the frame register scan) across each callback re-entry.
                let callee = require_callable(args.first())?;
                let a = self.typed_array_species_create(context, t, len)?;
                let a_value = Value::typed_array(a);
                let target_kind = a.kind();
                {
                    let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame_top, dst, a_value)?;
                }
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let mapped = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    let coerced = crate::binary::dispatch::coerce_element_for_store(
                        &mut self.gc_heap,
                        target_kind,
                        &mapped,
                    )?;
                    a.set(&mut self.gc_heap, i, &coerced);
                }
                return Ok(true);
            }
            "filter" => {
                // §23.2.3.10 — run the predicate over every element,
                // collecting kept values, *then* call
                // `TypedArraySpeciesCreate(O, « captured »)` (step 9)
                // with the kept count and copy the survivors in.
                let callee = require_callable(args.first())?;
                let mut kept: Vec<Value> = Vec::new();
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let selected = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if selected.to_boolean(&self.gc_heap) {
                        kept.push(value);
                    }
                }
                let a = self.typed_array_species_create(context, t, kept.len())?;
                let a_value = Value::typed_array(a);
                let target_kind = a.kind();
                {
                    let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame_top, dst, a_value)?;
                }
                for (i, value) in kept.iter().enumerate() {
                    let coerced = crate::binary::dispatch::coerce_element_for_store(
                        &mut self.gc_heap,
                        target_kind,
                        value,
                    )?;
                    a.set(&mut self.gc_heap, i, &coerced);
                }
                return Ok(true);
            }
            "find" => {
                let callee = require_callable(args.first())?;
                let mut found = Value::undefined();
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let hit = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if hit.to_boolean(&self.gc_heap) {
                        found = value;
                        break;
                    }
                }
                found
            }
            "findIndex" => {
                let callee = require_callable(args.first())?;
                let mut idx: i32 = -1;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let hit = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if hit.to_boolean(&self.gc_heap) {
                        idx = i as i32;
                        break;
                    }
                }
                Value::number_i32(idx)
            }
            "findLast" => {
                let callee = require_callable(args.first())?;
                let mut found = Value::undefined();
                for i in (0..len).rev() {
                    let value = elements[i];
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let hit = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if hit.to_boolean(&self.gc_heap) {
                        found = value;
                        break;
                    }
                }
                found
            }
            "findLastIndex" => {
                let callee = require_callable(args.first())?;
                let mut idx: i32 = -1;
                for i in (0..len).rev() {
                    let value = elements[i];
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let hit = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if hit.to_boolean(&self.gc_heap) {
                        idx = i as i32;
                        break;
                    }
                }
                Value::number_i32(idx)
            }
            "every" => {
                let callee = require_callable(args.first())?;
                let mut all = true;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let hit = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if !hit.to_boolean(&self.gc_heap) {
                        all = false;
                        break;
                    }
                }
                Value::boolean(all)
            }
            "some" => {
                let callee = require_callable(args.first())?;
                let mut any = false;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &ta_value);
                    let hit = self.run_callable_sync(context, &callee, this_arg, cb_args)?;
                    if hit.to_boolean(&self.gc_heap) {
                        any = true;
                        break;
                    }
                }
                Value::boolean(any)
            }
            "reduce" | "reduceRight" => {
                let callee = require_callable(args.first())?;
                let has_init = args.len() >= 2;
                let reverse = name == "reduceRight";
                if len == 0 && !has_init {
                    return Err(VmError::TypeMismatch);
                }
                let step: i64 = if reverse { -1 } else { 1 };
                let (mut acc, start_idx) = if has_init {
                    (args[1], if reverse { len as i64 - 1 } else { 0 })
                } else {
                    let seed = if reverse { len - 1 } else { 0 };
                    (elements[seed], seed as i64 + step)
                };
                let mut i = start_idx;
                while i >= 0 && (i as usize) < len {
                    let value = elements[i as usize];
                    let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                    cb_args.push(acc);
                    cb_args.push(value);
                    cb_args.push(Value::number(NumberValue::from_i32(i as i32)));
                    cb_args.push(ta_value);
                    acc = self.run_callable_sync(context, &callee, Value::undefined(), cb_args)?;
                    i += step;
                }
                acc
            }
            _ => return Ok(false),
        };

        let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame_top, dst, result)?;
        Ok(true)
    }

    /// §23.2.4.1 `TypedArraySpeciesCreate(exemplar, « length »)`.
    /// Resolves `SpeciesConstructor(exemplar, %DefaultConstructor%)`
    /// (§7.3.22) — observing a user `constructor` / `@@species`
    /// override — then performs `TypedArrayCreate(constructor,
    /// « length »)` (§23.2.4.2) and validates the result is a
    /// non-detached TypedArray of at least `length` elements.
    fn typed_array_species_create(
        &mut self,
        context: &ExecutionContext,
        exemplar: &crate::binary::typed_array::JsTypedArray,
        length: usize,
    ) -> Result<crate::binary::typed_array::JsTypedArray, VmError> {
        let mut argv: SmallVec<[Value; 8]> = SmallVec::new();
        argv.push(Value::number(NumberValue::from_f64(length as f64)));
        self.typed_array_create_via_species(context, exemplar, argv, Some(length))
    }

    /// §23.2.4.2 `TypedArrayCreate(SpeciesConstructor(exemplar), argv)`.
    /// Shared core for the length form (`map` / `filter` / `slice`,
    /// `min_length = Some`) and the `« buffer, byteOffset, length »`
    /// form (`subarray`, `min_length = None`): resolves the species
    /// constructor, constructs the result, and validates it is a
    /// non-detached TypedArray (plus the `[[ArrayLength]] >= length`
    /// check that only applies when the argument list is a single
    /// Number).
    fn typed_array_create_via_species(
        &mut self,
        context: &ExecutionContext,
        exemplar: &crate::binary::typed_array::JsTypedArray,
        argv: SmallVec<[Value; 8]>,
        min_length: Option<usize>,
    ) -> Result<crate::binary::typed_array::JsTypedArray, VmError> {
        let exemplar_value = Value::typed_array(*exemplar);
        let default_name = exemplar.kind().name();
        let default_ctor = crate::object::get(self.global_this, &self.gc_heap, default_name)
            .ok_or_else(|| VmError::TypeError {
                message: format!("%{default_name}% intrinsic is missing"),
            })?;
        let constructor =
            self.species_constructor_value(context, &exemplar_value, &default_ctor)?;
        let result = self.run_construct_sync(context, &constructor, constructor, argv)?;
        let Some(new_ta) = result.as_typed_array(&self.gc_heap) else {
            return Err(VmError::TypeError {
                message: "Species constructor did not return a TypedArray".to_string(),
            });
        };
        if new_ta.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
            return Err(VmError::TypeError {
                message: "Species constructor returned a TypedArray with a detached buffer"
                    .to_string(),
            });
        }
        if let Some(min) = min_length
            && new_ta.length(&self.gc_heap) < min
        {
            return Err(VmError::TypeError {
                message: "Species constructor returned a TypedArray smaller than required"
                    .to_string(),
            });
        }
        Ok(new_ta)
    }

    /// §23.2.3.27 `%TypedArray%.prototype.subarray(begin, end)`. Builds
    /// a new view over the *same* buffer: `begin` / `end` coerce
    /// through `ToIntegerOrInfinity`, then
    /// `TypedArraySpeciesCreate(O, « buffer, beginByteOffset, length »)`
    /// (the buffer form, so no result-length check) allocates the view.
    fn typed_array_subarray_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        t: &crate::binary::typed_array::JsTypedArray,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let buffer = t.buffer(&self.gc_heap);
        // §23.2.3.27 step 4 — `[[ArrayLength]]` is `0` for a detached
        // buffer; `subarray` does not itself throw on detachment.
        let src_len = t.length(&self.gc_heap) as i64;
        let begin = self.integer_or_infinity_for_arg(context, args.first())?;
        let begin_index = relative_index_clamp(begin, src_len);
        let relative_end = match args.get(1) {
            None => src_len as f64,
            Some(v) if v.is_undefined() => src_len as f64,
            Some(_) => self.integer_or_infinity_for_arg(context, args.get(1))?,
        };
        let end_index = relative_index_clamp(relative_end, src_len);
        let new_length = (end_index - begin_index).max(0) as usize;
        let bpe = t.kind().bytes_per_element();
        let begin_byte_offset = t.byte_offset(&self.gc_heap) + begin_index as usize * bpe;

        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc(self.current_byte_len)?;

        let mut argv: SmallVec<[Value; 8]> = SmallVec::new();
        argv.push(Value::array_buffer(buffer));
        argv.push(Value::number(NumberValue::from_f64(
            begin_byte_offset as f64,
        )));
        argv.push(Value::number(NumberValue::from_f64(new_length as f64)));
        let a = self.typed_array_create_via_species(context, t, argv, None)?;
        let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame_top, dst, Value::typed_array(a))?;
        Ok(())
    }

    /// §23.2.3.26 `%TypedArray%.prototype.slice(start, end)`. Coerces
    /// both operands through `ToIntegerOrInfinity` (observing user
    /// `@@toPrimitive` / `valueOf`), allocates the result via
    /// `TypedArraySpeciesCreate(O, « count »)`, then copies the
    /// in-range elements. The result is parked in `dst` before the
    /// copy so it stays GC-rooted, and the source buffer is re-checked
    /// for detachment after the (potentially re-entrant) species
    /// constructor runs.
    fn typed_array_slice_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        t: &crate::binary::typed_array::JsTypedArray,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        if t.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
            return Err(VmError::TypeError {
                message: "Cannot slice a TypedArray backed by a detached buffer".to_string(),
            });
        }
        let len = t.length(&self.gc_heap) as i64;
        let start = self.integer_or_infinity_for_arg(context, args.first())?;
        let k = relative_index_clamp(start, len);
        let relative_end = match args.get(1) {
            None => len as f64,
            Some(v) if v.is_undefined() => len as f64,
            Some(_) => self.integer_or_infinity_for_arg(context, args.get(1))?,
        };
        let final_index = relative_index_clamp(relative_end, len);
        let count = (final_index - k).max(0) as usize;

        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc(self.current_byte_len)?;

        let a = self.typed_array_species_create(context, t, count)?;
        let a_value = Value::typed_array(a);
        {
            let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
            write_register(frame_top, dst, a_value)?;
        }
        if count > 0 {
            if t.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "TypedArray buffer was detached during slice".to_string(),
                });
            }
            let target_kind = a.kind();
            let base = k as usize;
            for n in 0..count {
                let value = t
                    .get(&mut self.gc_heap, base + n)
                    .map_err(crate::oom_to_vm)?;
                let coerced = crate::binary::dispatch::coerce_element_for_store(
                    &mut self.gc_heap,
                    target_kind,
                    &value,
                )?;
                a.set(&mut self.gc_heap, n, &coerced);
            }
        }
        Ok(())
    }

    /// §7.1.5 `ToIntegerOrInfinity` applied to an optional argument
    /// (missing / `undefined` → `0`). Re-enters user `@@toPrimitive`
    /// / `valueOf` via `coerce_to_number` and raises TypeError for
    /// Symbol / BigInt operands.
    fn integer_or_infinity_for_arg(
        &mut self,
        context: &ExecutionContext,
        arg: Option<&Value>,
    ) -> Result<f64, VmError> {
        let n = match arg {
            None => return Ok(0.0),
            Some(v) if v.is_undefined() => return Ok(0.0),
            Some(v) => self.coerce_to_number(context, v)?.as_f64(),
        };
        if n.is_nan() {
            Ok(0.0)
        } else if n.is_infinite() {
            Ok(n)
        } else {
            Ok(n.trunc())
        }
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
                let this_value = iter.next().unwrap_or(Value::undefined());
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                stack[top_idx].advance_pc(self.current_byte_len)?;
                self.invoke(stack, context, callee, this_value, forwarded, dst)
            }
            "apply" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::undefined());
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None => SmallVec::new(),
                    Some(v) if v.is_nullish() => SmallVec::new(),
                    Some(arg_array) => self.create_list_from_array_like(context, arg_array)?,
                };
                stack[top_idx].advance_pc(self.current_byte_len)?;
                self.invoke(stack, context, callee, this_value, forwarded, dst)
            }
            "bind" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::undefined());
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let mut ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &mut self.gc_heap,
                    &self.function_user_props,
                    &self.function_deleted_metadata,
                );
                let metadata =
                    function_metadata::bound_create_metadata(&mut ctx, callee, bound_args.len())?;
                let callee_root = *callee;
                let this_root = this_value;
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
                    *callee,
                    this_value,
                    bound_args,
                    metadata,
                    &mut external_visit,
                )?;
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::bound_function(bound))?;
                frame.advance_pc(self.current_byte_len)?;
                Ok(())
            }
            // §20.2.3.5 Function.prototype.toString — foundation
            // returns the canonical `function <name>() { [native
            // code] }` placeholder. Spec mandates a source-faithful
            // representation when source is available; the
            // foundation defers source preservation to a follow-up.
            // <https://tc39.es/ecma262/#sec-function.prototype.tostring>
            "toString" => {
                let display = {
                    let mut ctx = function_metadata::FunctionMetadataContext::new(
                        context,
                        &mut self.gc_heap,
                        &self.function_user_props,
                        &self.function_deleted_metadata,
                    );
                    function_metadata::callable_to_string(&mut ctx, callee)
                };
                let s = JsString::from_str(&display, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::string(s))?;
                frame.advance_pc(self.current_byte_len)?;
                Ok(())
            }
            _ => Err(VmError::UnknownIntrinsic {
                name: name.to_string(),
            }),
        }
    }
}

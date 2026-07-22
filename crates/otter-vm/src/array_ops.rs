//! Array static opcode helpers.
//!
//! Array constructor/static bytecodes are variadic, so their argument registers
//! still live in the executable side-operand slice. This module keeps their
//! decode and call glue out of the main interpreter loop.
//!
//! # Contents
//! - `Array(...)` / `new Array(...)` construction.
//! - `Array.from(...)` and `Array.of(...)` static calls.
//!
//! # Invariants
//! - The current frame PC is advanced before running `Array.from` so any
//!   synchronous iterator/property callbacks observe the post-call PC.
//! - Arguments are read from executable operands, not cloned bytecode DTOs.
//! - `Array.from` roots its copied arguments for the complete observable
//!   iterator/property/callback sequence and reloads them after GC safepoints.
//!
//! # See also
//! - [`crate::array_statics`]
//! - [`crate::executable`]

use crate::activation_stack::ActivationStack;
use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, array,
    operand_decode::register_operand, read_register, symbol, to_length, write_register,
};

const MAX_DENSE_ARRAY_CONSTRUCT_HOLES: u32 = 1_048_576;

impl Interpreter {
    pub(crate) fn run_array_static_operands(
        &mut self,
        op: Op,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let args = collect_array_args(&stack[top_idx], operands)?;

        stack[top_idx].advance_pc()?;
        let result = match op {
            Op::ArrayConstruct => self.array_construct_stack_rooted(stack, &args)?,
            Op::ArrayFrom => self.array_from_sync(stack, context, Value::undefined(), &args)?,
            Op::ArrayOf => self.array_of_stack_rooted(stack, &args)?,
            _ => return Err(VmError::InvalidOperand),
        };

        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)
    }

    /// §23.1.1.1 `Array(...values)`.
    fn array_construct_stack_rooted(
        &mut self,
        stack: &ActivationStack,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if args.len() == 1
            && let Some(n) = args[0].as_number()
        {
            let raw = n.as_f64();
            let len = raw as u32;
            // §23.1.1.1 step 8.b — a single Number argument whose
            // ToUint32 round-trip differs from the value is not a valid
            // array length and raises a RangeError (not a TypeError).
            if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
                return Err(self.err_range(("Invalid array length".to_string()).into()));
            }
            let arr = self.alloc_stack_rooted_array(stack, &[], &[args])?;
            if len > 0 {
                let roots = self.collect_allocation_roots(stack);
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    for value in args {
                        value.trace_value_slots(visitor);
                    }
                };
                if len <= MAX_DENSE_ARRAY_CONSTRUCT_HOLES {
                    array::fill_dense_range_with_roots(
                        arr,
                        &mut self.gc_heap,
                        0,
                        len as usize,
                        Value::hole(),
                        &mut external_visit,
                    )?;
                } else {
                    array::set_with_roots(
                        arr,
                        &mut self.gc_heap,
                        (len - 1) as usize,
                        Value::hole(),
                        &mut external_visit,
                    )?;
                }
            }
            return Ok(Value::array(arr));
        }
        self.array_of_stack_rooted(stack, args)
    }

    /// §23.1.2.3 `Array.of(...items)`.
    fn array_of_stack_rooted(
        &mut self,
        stack: &ActivationStack,
        args: &[Value],
    ) -> Result<Value, VmError> {
        Ok(Value::array(
            self.alloc_stack_rooted_array_from_values_with_root_slices(
                stack,
                args.iter().cloned(),
                &[],
                &[args],
            )?,
        ))
    }

    /// §23.1.2.1 `Array.from(items, mapFn?, thisArg?)`.
    ///
    /// `constructor` is the `this` value of the call (`C`). When it is
    /// a constructor the result `A` is `Construct(C)` (iterator path)
    /// or `Construct(C, «len»)` (array-like path); otherwise `A` is a
    /// fresh ordinary Array. Elements are installed with
    /// `CreateDataPropertyOrThrow` and the final length is written via
    /// the observable `Set(A, "length", …, true)`. A thrown `mapfn` or
    /// `CreateDataPropertyOrThrow` closes an open iterator (§7.4.11).
    ///
    /// Splits on `items`:
    /// - `@@iterator` present → drive the sync iterator protocol live
    ///   so observable mutations during `mapfn` are honoured (§7.4).
    /// - Otherwise → array-like read of `length` + indexed properties.
    ///
    /// When `mapFn` is supplied it must be callable; each value passes
    /// through `mapFn(value, index)` with `this` = `thisArg`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.from>
    pub(crate) fn array_from_sync(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        constructor: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let items = args.first().copied().unwrap_or_else(Value::undefined);
        let map_fn = args.get(1).copied().unwrap_or_else(Value::undefined);
        let this_arg = args.get(2).copied().unwrap_or_else(Value::undefined);
        self.with_handle_scope(|interp, scope| {
            let constructor_handle = interp.scoped_value(scope, constructor);
            let items_handle = interp.scoped_value(scope, items);
            let map_fn_handle = interp.scoped_value(scope, map_fn);
            let this_arg_handle = interp.scoped_value(scope, this_arg);

            let map_fn = interp.escape_scoped(map_fn_handle);
            let has_map = !map_fn.is_undefined();
            if has_map && !interp.is_callable_runtime(&map_fn) {
                return Err(
                    interp.err_type(("Array.from mapFn must be callable".to_string()).into())
                );
            }
            let constructor = interp.escape_scoped(constructor_handle);
            let use_ctor = !constructor.is_undefined()
                && crate::abstract_ops::is_constructor(&constructor, context, &interp.gc_heap);

            let items = interp.escape_scoped(items_handle);
            if !has_map
                && !use_ctor
                && let Some(arr) = items.as_array()
            {
                let len = crate::array::len(arr, &interp.gc_heap);
                let result = interp.scoped_array(scope, len)?;
                let value_handle = interp.scoped_value(scope, Value::undefined());
                for index in 0..len {
                    let items = interp.escape_scoped(items_handle);
                    let arr = items.as_array().ok_or(VmError::InvalidOperand)?;
                    let value = crate::array::get(arr, &interp.gc_heap, index);
                    interp.set_scoped(value_handle, value);
                    interp.scoped_set_index(scope, result, index, value_handle)?;
                }
                return Ok(interp.escape_scoped(result));
            }

            if !has_map && let Some(arr) = items.as_array() {
                let len = crate::array::len(arr, &interp.gc_heap);
                let anchor_base = interp.push_iteration_anchor(items) - 1;
                let result = (|interp: &mut Self| -> Result<Value, VmError> {
                    let constructor = interp.escape_scoped(constructor_handle);
                    let target = interp.array_from_make_target(
                        stack,
                        context,
                        use_ctor,
                        &constructor,
                        None,
                    )?;
                    let target_anchor = interp.push_iteration_anchor(target) - 1;
                    for index in 0..len {
                        let items = interp.escape_scoped(items_handle);
                        let arr = items.as_array().ok_or(VmError::InvalidOperand)?;
                        let value = crate::array::get(arr, &interp.gc_heap, index);
                        let target = interp.iteration_anchor(target_anchor);
                        interp.create_data_property_or_throw(
                            stack,
                            context,
                            target,
                            &index.to_string(),
                            value,
                        )?;
                    }
                    let target = interp.iteration_anchor(target_anchor);
                    interp.array_set_property_throwing(
                        stack,
                        context,
                        target,
                        "length",
                        Value::number_f64(len as f64),
                    )?;
                    Ok(target)
                })(interp);
                interp.pop_iteration_anchors_to(anchor_base);
                return result;
            }

            // §23.1.2.1 step 4 — `usingIterator = GetMethod(items,
            // @@iterator)`. Resolve `@@iterator` through the ordinary
            // property ladder so a user-deleted or user-overridden iterator
            // is honored: deleting `String.prototype[@@iterator]` makes a
            // string array-like rather than forcing the (now absent)
            // iterator path.
            let items = interp.escape_scoped(items_handle);
            let iterator_method = if items.is_undefined() || items.is_null() {
                Value::undefined()
            } else {
                let iterator_sym = interp.well_known_symbols.get(symbol::WellKnown::Iterator);
                match interp.ordinary_get_value(
                    stack,
                    context,
                    items,
                    items,
                    &VmPropertyKey::Symbol(iterator_sym),
                    0,
                )? {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let items = interp.escape_scoped(items_handle);
                        interp.run_callable_sync_rooted(
                            stack,
                            context,
                            &getter,
                            items,
                            SmallVec::new(),
                        )?
                    }
                }
            };

            if !iterator_method.is_undefined() && !iterator_method.is_null() {
                if !interp.is_callable_runtime(&iterator_method) {
                    return Err(
                        interp.err_type(("iterator method is not callable".to_string()).into())
                    );
                }
                // Step 6 — iterator path. `A = Construct(C)` (no length
                // forwarded; the count is unknown up front) or a fresh
                // Array.
                let anchor_base =
                    interp.push_iteration_anchor(interp.escape_scoped(items_handle)) - 1;
                interp.push_iteration_anchor(interp.escape_scoped(map_fn_handle));
                interp.push_iteration_anchor(interp.escape_scoped(this_arg_handle));
                let result = (|interp: &mut Self| -> Result<Value, VmError> {
                    let constructor = interp.escape_scoped(constructor_handle);
                    let target = interp.array_from_make_target(
                        stack,
                        context,
                        use_ctor,
                        &constructor,
                        None,
                    )?;
                    let target_anchor = interp.push_iteration_anchor(target) - 1;
                    let items = interp.iteration_anchor(anchor_base);
                    let (iterator, next_method) =
                        interp.get_iterator_sync(stack, context, &items)?;
                    let iterator_anchor = interp.push_iteration_anchor(iterator) - 1;
                    let next_method_anchor = interp.push_iteration_anchor(next_method) - 1;
                    let mut k = 0usize;
                    let result = loop {
                        let iterator = interp.iteration_anchor(iterator_anchor);
                        let next_method = interp.iteration_anchor(next_method_anchor);
                        let value = match interp.iterator_step_sync(
                            stack,
                            context,
                            &iterator,
                            &next_method,
                        ) {
                            Ok(Some(value)) => value,
                            Ok(None) => break Ok(()),
                            // `next` threw: the iterator is already done, no close.
                            Err(err) => break Err(err),
                        };
                        let mapped = if has_map {
                            let map_fn = interp.iteration_anchor(anchor_base + 1);
                            let this_arg = interp.iteration_anchor(anchor_base + 2);
                            let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                            cb_args.push(value);
                            cb_args.push(Value::number_f64(k as f64));
                            match interp.run_callable_sync_rooted(
                                stack, context, &map_fn, this_arg, cb_args,
                            ) {
                                Ok(mapped) => mapped,
                                Err(err) => {
                                    let iterator = interp.iteration_anchor(iterator_anchor);
                                    let _ = interp.iterator_close_sync(stack, context, &iterator);
                                    break Err(err);
                                }
                            }
                        } else {
                            value
                        };
                        let iterator = interp.iteration_anchor(iterator_anchor);
                        let target = interp.iteration_anchor(target_anchor);
                        if let Err(err) = interp.create_data_property_or_throw(
                            stack,
                            context,
                            target,
                            &k.to_string(),
                            mapped,
                        ) {
                            let _ = interp.iterator_close_sync(stack, context, &iterator);
                            break Err(err);
                        }
                        k = k.saturating_add(1);
                    };
                    result?;
                    let target = interp.iteration_anchor(target_anchor);
                    interp.array_set_property_throwing(
                        stack,
                        context,
                        target,
                        "length",
                        Value::number_f64(k as f64),
                    )?;
                    Ok(target)
                })(interp);
                interp.pop_iteration_anchors_to(anchor_base);
                return result;
            }

            // Step 4 — array-like path.
            let items = interp.escape_scoped(items_handle);
            if items.is_undefined() || items.is_null() {
                return Err(interp.err_type(
                    ("Array.from requires an iterable or array-like".to_string()).into(),
                ));
            }
            let length_value = match interp.ordinary_get_value(
                stack,
                context,
                items,
                items,
                &VmPropertyKey::String("length"),
                0,
            )? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    let items = interp.escape_scoped(items_handle);
                    interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        items,
                        SmallVec::new(),
                    )?
                }
            };
            let len = to_length(&length_value, &interp.gc_heap)?;
            let constructor = interp.escape_scoped(constructor_handle);
            let target =
                interp.array_from_make_target(stack, context, use_ctor, &constructor, Some(len))?;
            const TARGET: usize = 0;
            const ITEMS: usize = 1;
            const MAP_FN: usize = 2;
            const THIS_ARG: usize = 3;
            let anchor_base = interp.push_iteration_anchor(target) - 1;
            interp.push_iteration_anchor(interp.escape_scoped(items_handle));
            interp.push_iteration_anchor(interp.escape_scoped(map_fn_handle));
            interp.push_iteration_anchor(interp.escape_scoped(this_arg_handle));
            let result = (|interp: &mut Self| -> Result<Value, VmError> {
                for index in 0..len {
                    let key = VmPropertyKey::OwnedString(index.to_string());
                    let items = interp.iteration_anchor(anchor_base + ITEMS);
                    let value =
                        match interp.ordinary_get_value(stack, context, items, items, &key, 0)? {
                            VmGetOutcome::Value(value) => value,
                            VmGetOutcome::InvokeGetter { getter } => {
                                let items = interp.iteration_anchor(anchor_base + ITEMS);
                                interp.run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    items,
                                    SmallVec::new(),
                                )?
                            }
                        };
                    let mapped = if has_map {
                        let map_fn = interp.iteration_anchor(anchor_base + MAP_FN);
                        let this_arg = interp.iteration_anchor(anchor_base + THIS_ARG);
                        let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                        cb_args.push(value);
                        cb_args.push(Value::number_f64(index as f64));
                        interp
                            .run_callable_sync_rooted(stack, context, &map_fn, this_arg, cb_args)?
                    } else {
                        value
                    };
                    let target = interp.iteration_anchor(anchor_base + TARGET);
                    interp.create_data_property_or_throw(
                        stack,
                        context,
                        target,
                        &index.to_string(),
                        mapped,
                    )?;
                }
                let target = interp.iteration_anchor(anchor_base + TARGET);
                interp.array_set_property_throwing(
                    stack,
                    context,
                    target,
                    "length",
                    Value::number_f64(len as f64),
                )?;
                Ok(target)
            })(interp);
            interp.pop_iteration_anchors_to(anchor_base);
            result
        })
    }

    /// Allocate the Array.from result object: `Construct(C)` (optionally
    /// with a forwarded `len`) when `C` is a constructor, else a fresh
    /// ordinary Array.
    fn array_from_make_target(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        use_ctor: bool,
        constructor: &Value,
        len: Option<usize>,
    ) -> Result<Value, VmError> {
        if use_ctor {
            let mut ctor_args: SmallVec<[Value; 8]> = SmallVec::new();
            if let Some(len) = len {
                ctor_args.push(Value::number_f64(len as f64));
            }
            self.run_construct_sync_rooted(stack, context, constructor, *constructor, ctor_args)
        } else {
            Ok(Value::array(self.alloc_runtime_rooted_array_from_values(
                Vec::new(),
                &[],
                &[],
            )?))
        }
    }

    /// §23.1.2.2 `Array.of(...items)` honouring the `this` constructor
    /// `C`: when `C` is a constructor the result is `Construct(C,
    /// «len»)`, else a fresh ordinary Array. Each item is installed via
    /// `CreateDataPropertyOrThrow` and the length written through the
    /// observable `Set(A, "length", len, true)`.
    ///
    /// The `Op::ArrayOf` fast path keeps building a plain Array for
    /// direct `Array.of(...)` callsites; this is the reflective
    /// (`Array.of.call(C, …)`) entry that must observe `C`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.of>
    pub(crate) fn array_of_sync(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        constructor: Value,
        items: &[Value],
    ) -> Result<Value, VmError> {
        let use_ctor = !constructor.is_undefined()
            && crate::abstract_ops::is_constructor(&constructor, context, &self.gc_heap);
        let len = items.len();
        let target =
            self.array_from_make_target(stack, context, use_ctor, &constructor, Some(len))?;
        let anchor_base = self.push_iteration_anchor(target) - 1;
        let result = (|interp: &mut Self| -> Result<(), VmError> {
            for (k, value) in items.iter().enumerate() {
                interp.create_data_property_or_throw(
                    stack,
                    context,
                    target,
                    &k.to_string(),
                    *value,
                )?;
            }
            Ok(())
        })(self);
        self.pop_iteration_anchors_to(anchor_base);
        result?;
        self.array_set_property_throwing(
            stack,
            context,
            target,
            "length",
            Value::number_f64(len as f64),
        )?;
        Ok(target)
    }
}

fn collect_array_args(
    frame: &Frame,
    operands: impl crate::executable::OperandSource,
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(1) {
        Some(Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(2 + i))?;
        args.push(*read_register(frame, r)?);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{
        BytecodeModule, Function, Instruction, SourceKind as BcSourceKind, SpanEntry,
    };

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "array-ops-test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 0,
                param_count: 0,
                length: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                uses_arguments_callee: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new().into(),
                spans: Vec::<SpanEntry>::new(),
                number_hint_sites: Vec::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    fn empty_module() -> BytecodeModule {
        BytecodeModule {
            module: "array-ops-test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 1,
                param_count: 0,
                length: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                uses_arguments_callee: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new().into(),
                spans: Vec::<SpanEntry>::new(),
                number_hint_sites: Vec::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    #[test]
    fn array_from_sync_uses_runtime_rooted_result_allocation() {
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::number_i32(7)],
        )
        .expect("source");
        let context = empty_context();
        let module = empty_module();
        let mut stack = ActivationStack::new();
        stack.push(
            interp
                .test_frame_for_function(&module.functions[0])
                .expect("frame"),
        );
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = interp
            .array_from_sync(
                &mut stack,
                &context,
                Value::undefined(),
                &[Value::array(source)],
            )
            .expect("Array.from");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Array.from should allocate its result array in young space"
        );
        assert!(result.is_array());
    }

    #[test]
    fn array_of_uses_stack_rooted_result_allocation() {
        let mut interp = Interpreter::new();
        let module = empty_module();
        let mut stack: ActivationStack = ActivationStack::new();
        stack.push(
            interp
                .test_frame_for_function(&module.functions[0])
                .unwrap(),
        );
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = interp
            .array_of_stack_rooted(&stack, &[Value::number_i32(1), Value::number_i32(2)])
            .expect("Array.of");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Array.of should allocate its result array in young space"
        );
        let Some(array) = result.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(array, interp.gc_heap()), 2);
    }

    #[test]
    fn array_construct_length_uses_stack_rooted_shell_and_growth() {
        let mut interp = Interpreter::new();
        let module = empty_module();
        let mut stack: ActivationStack = ActivationStack::new();
        stack.push(
            interp
                .test_frame_for_function(&module.functions[0])
                .unwrap(),
        );
        let before_alloc = interp.gc_heap().stats().new_allocated_bytes;
        let before_reserved = interp.gc_heap().stats().reserved_bytes;

        let result = interp
            .array_construct_stack_rooted(&stack, &[Value::number_i32(8)])
            .expect("Array constructor");

        let after_alloc = interp.gc_heap().stats().new_allocated_bytes;
        let after_reserved = interp.gc_heap().stats().reserved_bytes;
        assert!(
            after_alloc > before_alloc,
            "Array(length) should allocate the array shell in young space"
        );
        assert!(
            after_reserved > before_reserved,
            "Array(length) should grow backing storage through root-aware reservation"
        );
        let Some(array) = result.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(array, interp.gc_heap()), 8);
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 0));
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 7));
    }

    #[test]
    fn array_construct_moderate_length_materializes_dense_holes() {
        let mut interp = Interpreter::new();
        let module = empty_module();
        let mut stack: ActivationStack = ActivationStack::new();
        stack.push(
            interp
                .test_frame_for_function(&module.functions[0])
                .unwrap(),
        );
        let before_reserved = interp.gc_heap().stats().reserved_bytes;

        let result = interp
            .array_construct_stack_rooted(&stack, &[Value::number_i32(20_000)])
            .expect("Array constructor");

        let after_reserved = interp.gc_heap().stats().reserved_bytes;
        assert!(
            after_reserved > before_reserved,
            "moderate Array(length) should reserve dense hole storage"
        );
        let Some(array) = result.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(array, interp.gc_heap()), 20_000);
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 0));
        assert!(!crate::array::has_own_element(
            array,
            interp.gc_heap(),
            19_999
        ));
    }
}

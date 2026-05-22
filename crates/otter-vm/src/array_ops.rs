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
//!
//! # See also
//! - [`crate::array_statics`]
//! - [`crate::executable`]

use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, array,
    number, operand_decode::register_operand, read_register, symbol, to_length, write_register,
};

impl Interpreter {
    pub(crate) fn run_array_static_operands(
        &mut self,
        op: Op,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let args = collect_array_args(&stack[top_idx], operands)?;

        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let result = match op {
            Op::ArrayConstruct => self.array_construct_stack_rooted(stack, &args)?,
            Op::ArrayFrom => self.array_from_sync(context, &args)?,
            Op::ArrayOf => self.array_of_stack_rooted(stack, &args)?,
            _ => return Err(VmError::InvalidOperand),
        };

        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)
    }

    /// §23.1.1.1 `Array(...values)`.
    fn array_construct_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if args.len() == 1
            && let Value::Number(n) = &args[0]
        {
            let raw = n.as_f64();
            let len = raw as u32;
            if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
                return Err(VmError::TypeError {
                    message: "Invalid array length".to_string(),
                });
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
                array::set_with_roots(
                    arr,
                    &mut self.gc_heap,
                    (len - 1) as usize,
                    Value::Hole,
                    &mut external_visit,
                )?;
            }
            return Ok(Value::Array(arr));
        }
        self.array_of_stack_rooted(stack, args)
    }

    /// §23.1.2.3 `Array.of(...items)`.
    fn array_of_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        args: &[Value],
    ) -> Result<Value, VmError> {
        Ok(Value::Array(
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
    /// Splits on `items`:
    /// - Has `@@iterator` → walk via [`Self::iterator_to_list_sync`]
    ///   (sync iterator protocol, §7.4).
    /// - Otherwise → array-like read of `length` + indexed
    ///   properties (§7.3.18 CreateListFromArrayLike with no element
    ///   type filter).
    ///
    /// When `mapFn` is supplied (must be callable), each value is
    /// passed through `mapFn(value, index)` with `this` = `thisArg`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.from>
    pub(crate) fn array_from_sync(
        &mut self,
        context: &ExecutionContext,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let items = args.first().cloned().unwrap_or(Value::undefined());
        let map_fn = args.get(1).cloned().unwrap_or(Value::undefined());
        let this_arg = args.get(2).cloned().unwrap_or(Value::undefined());
        let has_map = !matches!(map_fn, Value::Undefined);
        if has_map && !self.is_callable_runtime(&map_fn) {
            return Err(VmError::TypeError {
                message: "Array.from mapFn must be callable".to_string(),
            });
        }

        // Step 1 — built-in iterable fast paths short-circuit the
        // `@@iterator` round-trip; for everything else look up
        // `@@iterator` to decide between iterable and array-like.
        let is_builtin_iterable = matches!(
            items,
            Value::Array(_)
                | Value::String(_)
                | Value::Set(_)
                | Value::Map(_)
                | Value::Generator(_)
                | Value::Iterator(_)
        );
        let iterator_method = if matches!(items, Value::Undefined | Value::Null) {
            Value::Undefined
        } else if is_builtin_iterable {
            // Sentinel: any non-undefined value picks the iterator
            // path below; `iterator_to_list_sync` handles built-ins
            // via its fast-path branches.
            Value::Boolean(true)
        } else {
            let iterator_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
            match self.ordinary_get_value(
                context,
                items,
                items,
                &VmPropertyKey::Symbol(iterator_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, items, SmallVec::new())?
                }
            }
        };

        let raw_values: Vec<Value> = if matches!(iterator_method, Value::Undefined | Value::Null) {
            // Step 4 — ArrayLike path.
            if matches!(items, Value::Undefined | Value::Null) {
                return Err(VmError::TypeError {
                    message: "Array.from requires an iterable or array-like".to_string(),
                });
            }
            let length_value = match self.ordinary_get_value(
                context,
                items,
                items,
                &VmPropertyKey::String("length"),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, items, SmallVec::new())?
                }
            };
            let len = to_length(&length_value, &self.gc_heap)?;
            let mut out = Vec::with_capacity(len);
            for index in 0..len {
                let key = VmPropertyKey::OwnedString(index.to_string());
                let value = match self.ordinary_get_value(context, items, items, &key, 0)? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        self.run_callable_sync(context, &getter, items, SmallVec::new())?
                    }
                };
                out.push(value);
            }
            out
        } else {
            if !is_builtin_iterable && !self.is_callable_runtime(&iterator_method) {
                return Err(VmError::TypeError {
                    message: "iterator method is not callable".to_string(),
                });
            }
            // `iterator_to_list_sync` short-circuits built-ins and
            // routes everything else through `GetIterator` /
            // `IteratorStep`.
            self.iterator_to_list_sync(context, &items)?
        };

        let mut mapped: Vec<Value> = Vec::with_capacity(raw_values.len());
        for (index, value) in raw_values.into_iter().enumerate() {
            if has_map {
                let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                cb_args.push(value);
                cb_args.push(Value::number(number::NumberValue::from_i32(index as i32)));
                let mapped_value = self.run_callable_sync(context, &map_fn, this_arg, cb_args)?;
                mapped.push(mapped_value);
            } else {
                mapped.push(value);
            }
        }
        Ok(Value::Array(self.alloc_runtime_rooted_array_from_values(
            mapped,
            &[&items, &map_fn, &this_arg],
            &[],
        )?))
    }
}

fn collect_array_args(
    frame: &Frame,
    operands: &[Operand],
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(1) {
        Some(&Operand::ConstIndex(n)) => n as usize,
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
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 0,
                param_count: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                module_url: String::new(),
                code: Vec::<Instruction>::new(),
                spans: Vec::<SpanEntry>::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    fn empty_module() -> BytecodeModule {
        BytecodeModule {
            module: "array-ops-test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 1,
                param_count: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                module_url: String::new(),
                code: Vec::<Instruction>::new(),
                spans: Vec::<SpanEntry>::new(),
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
            [Value::Number(crate::NumberValue::from_i32(7))],
        )
        .expect("source");
        let context = empty_context();
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = interp
            .array_from_sync(&context, &[Value::Array(source)])
            .expect("Array.from");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Array.from should allocate its result array in young space"
        );
        assert!(matches!(result, Value::Array(_)));
    }

    #[test]
    fn array_of_uses_stack_rooted_result_allocation() {
        let mut interp = Interpreter::new();
        let module = empty_module();
        let mut stack: smallvec::SmallVec<[Frame; 8]> = smallvec::SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = interp
            .array_of_stack_rooted(
                &stack,
                &[
                    Value::Number(crate::NumberValue::from_i32(1)),
                    Value::Number(crate::NumberValue::from_i32(2)),
                ],
            )
            .expect("Array.of");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Array.of should allocate its result array in young space"
        );
        let Value::Array(array) = result else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(array, interp.gc_heap()), 2);
    }

    #[test]
    fn array_construct_length_uses_stack_rooted_shell_and_growth() {
        let mut interp = Interpreter::new();
        let module = empty_module();
        let mut stack: smallvec::SmallVec<[Frame; 8]> = smallvec::SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let before_alloc = interp.gc_heap().stats().new_allocated_bytes;
        let before_reserved = interp.gc_heap().stats().reserved_bytes;

        let result = interp
            .array_construct_stack_rooted(&stack, &[Value::Number(crate::NumberValue::from_i32(8))])
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
        let Value::Array(array) = result else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(array, interp.gc_heap()), 8);
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 0));
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 7));
    }
}

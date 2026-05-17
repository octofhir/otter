//! Static namespace call opcode helpers.
//!
//! These opcodes are variadic, so their argument registers live in the
//! executable side-operand slice. Keeping their dispatch here removes a large
//! repeated decode block from the main interpreter loop.
//!
//! # Contents
//! - Built-in static calls for Math, JSON, Date, BigInt, binary buffers,
//!   iterators, Proxy, Object, globals, Symbol, and Temporal.
//!
//! # Invariants
//! - The opcode passed to `run_static_call_operands` must be one of the
//!   supported static-call opcodes.
//! - Variadic argument operands are executable operands, not bytecode DTO
//!   vectors cloned per dispatch.
//!
//! # See also
//! - [`crate::executable`]

use otter_bytecode::{Op, Operand, method_id};
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, IteratorState, Value, VmError, VmPropertyKey,
    abstract_ops, bigint, binary, collections, constructor_return_is_object, date,
    global_functions, json, json_to_vm_error, math, math_to_vm_error, native_function, object,
    object_statics,
    operand_decode::{const_operand, register_operand},
    read_register,
    string::JsString,
    symbol_dispatch, symbol_to_vm_error, temporal, temporal_to_vm_error, write_register,
};

impl Interpreter {
    pub(crate) fn run_static_call_operands(
        &mut self,
        op: Op,
        _context: &ExecutionContext,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        match op {
            Op::MathCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::MathMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result = math::call(method, &args).map_err(math_to_vm_error)?;
                finish_static_call(frame, dst, result)
            }
            Op::JsonCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::JsonMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result = json::call(method, &args, &self.string_heap, &mut self.gc_heap)
                    .map_err(json_to_vm_error)?;
                finish_static_call(frame, dst, result)
            }
            Op::DateCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::DateMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result = date::dispatch::call(method, &args)?;
                finish_static_call(frame, dst, result)
            }
            Op::BigIntCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::BigIntMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result = bigint::dispatch::call(method, &args)?;
                finish_static_call(frame, dst, result)
            }
            Op::ArrayBufferCall => unreachable!("ArrayBufferCall requires stack-rooted dispatch"),
            Op::DataViewCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method = method_id::DataViewMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result = binary::dispatch::data_view_call(method, &args)?;
                finish_static_call(frame, dst, result)
            }
            Op::TypedArrayCall => unreachable!("TypedArrayCall requires stack-rooted dispatch"),
            Op::SharedArrayBufferCall => {
                unreachable!("SharedArrayBufferCall requires stack-rooted dispatch")
            }
            Op::GlobalCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::GlobalMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result = global_functions::call(method, &args, &self.string_heap)?;
                finish_static_call(frame, dst, result)
            }
            Op::SymbolCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::SymbolMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result =
                    symbol_dispatch::call(self, method, &args).map_err(symbol_to_vm_error)?;
                finish_static_call(frame, dst, result)
            }
            Op::TemporalCall => {
                let dst = register_operand(operands.first())?;
                let class_idx = const_operand(operands.get(1))?;
                let method_idx = const_operand(operands.get(2))?;
                let args = collect_call_args(frame, operands, 3, 4)?;
                let class = method_id::TemporalClassId::from_u32(class_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let method = method_id::TemporalMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result =
                    temporal::call_static(&self.string_heap, &self.gc_heap, class, method, &args)
                        .map_err(temporal_to_vm_error)?;
                finish_static_call(frame, dst, result)
            }
            _ => Err(VmError::InvalidOperand),
        }
    }

    pub(crate) fn run_json_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method = method_id::JsonMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for arg in &args {
                arg.trace_value_slots(visitor);
            }
        };
        let result = json::call_with_roots(
            method,
            &args,
            &self.string_heap,
            &mut self.gc_heap,
            &mut external_visit,
        )
        .map_err(json_to_vm_error)?;
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_array_buffer_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method =
            method_id::ArrayBufferMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for arg in &args {
                arg.trace_value_slots(visitor);
            }
        };
        let result = binary::dispatch::array_buffer_call_with_roots(
            method,
            &args,
            &mut self.gc_heap,
            &mut external_visit,
        )?;
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_typed_array_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, kind_idx, method_idx, args) = {
            let frame = &stack[top_idx];
            let dst = register_operand(operands.first())?;
            let kind_idx = const_operand(operands.get(1))?;
            let method_idx = const_operand(operands.get(2))?;
            let args = collect_call_args(frame, operands, 3, 4)?;
            (dst, kind_idx, method_idx, args)
        };
        let kind = binary::TypedArrayKind::from_u32(kind_idx).ok_or(VmError::InvalidOperand)?;
        let method =
            method_id::TypedArrayMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for arg in &args {
                arg.trace_value_slots(visitor);
            }
        };
        let result = binary::dispatch::typed_array_call_with_roots(
            kind,
            method,
            &args,
            &mut self.gc_heap,
            &mut external_visit,
        )?;
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_shared_array_buffer_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method = method_id::SharedArrayBufferMethod::from_u32(method_idx)
            .ok_or(VmError::InvalidOperand)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for arg in &args {
                arg.trace_value_slots(visitor);
            }
        };
        let result = binary::dispatch::shared_array_buffer_call_with_roots(
            method,
            &args,
            &mut self.gc_heap,
            &mut external_visit,
        )?;
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_object_static_call_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method =
            method_id::ObjectMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        // §20.1.2.2 Object.create / §20.1.2.3 Object.defineProperties
        // run ToPropertyDescriptor (§6.2.5.5) on the descriptor
        // source, which must invoke accessor getters and walk the
        // prototype chain. Route them through context-aware helpers
        // before the rooted/free fallbacks. Object.defineProperty is
        // already handled in `try_proxy_object_static_call` (which is
        // not actually Proxy-specific — it runs the full spec
        // descriptor coercion for any Object-typed target).
        match method {
            method_id::ObjectMethod::Create => {
                let result = self.do_object_create_with_descriptors(context, stack, &args)?;
                return finish_static_call(&mut stack[top_idx], dst, result);
            }
            method_id::ObjectMethod::DefineProperties => {
                let result = self.do_object_define_properties(context, stack, &args)?;
                return finish_static_call(&mut stack[top_idx], dst, result);
            }
            _ => {}
        }
        let result = if let Some(result) =
            self.try_function_object_static_call(Some(context), Some(stack), method, &args)?
        {
            result
        } else if let Some(result) =
            self.try_proxy_object_static_call(context, Some(stack), method, &args)?
        {
            result
        } else if let Some(result) = self.object_static_call_stack_rooted(stack, method, &args)? {
            result
        } else {
            object_statics::call(method, &args, &self.string_heap, &mut self.gc_heap)?
        };
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    /// §20.1.2.2 Object.create(O, Properties).
    ///
    /// Implements the spec algorithm including accessor-aware
    /// ToPropertyDescriptor (§6.2.5.5) on each value drawn from the
    /// `Properties` source — which is itself read via
    /// `EnumerableOwnPropertyNames` plus full `[[Get]]`, so getter
    /// invocation on `Properties` (and on each descriptor value) is
    /// observable as required.
    ///
    /// Descriptor values are accepted whenever they are of type
    /// Object — any callable / array / class-constructor / map / set /
    /// regexp form qualifies, matching `evaluate_to_property_descriptor`
    /// step 1.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-object.create>
    /// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
    fn do_object_create_with_descriptors(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let proto = args.first().cloned().unwrap_or(Value::Undefined);
        let proto_obj = match proto {
            Value::Object(object) => Some(object),
            Value::Null => None,
            _ => return Err(VmError::TypeMismatch),
        };
        let obj = self.alloc_stack_rooted_object_with_value_roots(stack, &[&proto], args)?;
        object::set_prototype(obj, &mut self.gc_heap, proto_obj);
        if let Some(props_arg) = args.get(1)
            && !matches!(props_arg, Value::Undefined)
        {
            let props = match props_arg {
                Value::Object(object) => *object,
                _ => return Err(VmError::TypeMismatch),
            };
            let entries: Vec<(String, Value)> =
                object::with_properties(props, self.gc_heap(), |p| {
                    p.enumerable_data_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect()
                });
            for (key, desc_value) in entries {
                let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
                if !object::define_own_property_partial(
                    obj,
                    &mut self.gc_heap,
                    &key,
                    descriptor,
                ) {
                    return Err(VmError::TypeError {
                        message: format!("Cannot define property '{key}'"),
                    });
                }
            }
        }
        Ok(Value::Object(obj))
    }

    /// §20.1.2.3 Object.defineProperties(O, Properties).
    ///
    /// Routes through `evaluate_to_property_descriptor` (§6.2.5.5) so
    /// each descriptor source is read with full `[[Get]]` semantics
    /// — accessor getters observe, prototype chain walks. Accepts
    /// arbitrary object-typed descriptor sources (functions, arrays,
    /// regexps, …) since `Type(desc)` is the only spec restriction.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-object.defineproperties>
    fn do_object_define_properties(
        &mut self,
        context: &ExecutionContext,
        _stack: &SmallVec<[Frame; 8]>,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let target = match args.first() {
            Some(Value::Object(target)) => *target,
            Some(Value::ClassConstructor(class)) => class.statics(self.gc_heap()),
            _ => {
                return Err(VmError::TypeError {
                    message: "Object.defineProperties target must be an object".to_string(),
                });
            }
        };
        let props = match args.get(1) {
            Some(Value::Object(o)) => *o,
            Some(Value::ClassConstructor(class)) => class.statics(self.gc_heap()),
            _ => {
                return Err(VmError::TypeError {
                    message: "Object.defineProperties properties must be an object".to_string(),
                });
            }
        };
        let entries: Vec<(String, Value)> =
            object::with_properties(props, self.gc_heap(), |p| {
                p.enumerable_data_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect()
            });
        for (key, desc_value) in entries {
            let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
            if !object::define_own_property_partial(target, &mut self.gc_heap, &key, descriptor) {
                return Err(VmError::TypeError {
                    message: format!("Cannot define property '{key}'"),
                });
            }
        }
        Ok(Value::Object(target))
    }

    fn object_static_call_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        method: method_id::ObjectMethod,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        // M::Create needs an `ExecutionContext` to run accessor-aware
        // ToPropertyDescriptor in `run_object_static_call_operands`;
        // signal "not handled here" so the caller routes to the
        // context-aware path.
        if matches!(method, method_id::ObjectMethod::Create) {
            return Ok(None);
        }
        use method_id::ObjectMethod as M;
        match method {
            M::Keys => {
                let owned: Vec<String> = match args.first() {
                    Some(Value::Object(target)) => {
                        object::with_properties(*target, self.gc_heap(), |p| {
                            p.enumerable_keys().map(|k| k.to_string()).collect()
                        })
                    }
                    Some(Value::NativeFunction(native)) => native
                        .enumerable_own_property_keys(self.gc_heap())
                        .into_iter()
                        .collect(),
                    Some(Value::BoundFunction(bound)) => {
                        crate::function_metadata::bound_enumerable_own_property_keys(
                            bound,
                            self.gc_heap(),
                        )
                        .into_iter()
                        .collect()
                    }
                    _ => return Err(VmError::TypeMismatch),
                };
                let mut names = Vec::with_capacity(owned.len());
                for key in owned {
                    names.push(stack_static_string_value(&key, self)?);
                }
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    names,
                    &[],
                    &[args],
                )?;
                Ok(Some(Value::Array(array)))
            }
            M::Values => {
                let target = match args.first() {
                    Some(Value::Object(target)) => *target,
                    _ => return Err(VmError::TypeMismatch),
                };
                let values: Vec<Value> = object::with_properties(target, self.gc_heap(), |p| {
                    p.enumerable_data_iter().map(|(_, value)| value).collect()
                });
                let target_root = Value::Object(target);
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    values,
                    &[&target_root],
                    &[args],
                )?;
                Ok(Some(Value::Array(array)))
            }
            M::Entries => {
                let target = match args.first() {
                    Some(Value::Object(target)) => *target,
                    _ => return Err(VmError::TypeMismatch),
                };
                let raw: Vec<(String, Value)> =
                    object::with_properties(target, self.gc_heap(), |p| {
                        p.enumerable_data_iter()
                            .map(|(key, value)| (key.to_string(), value))
                            .collect()
                    });
                let target_root = Value::Object(target);
                let mut pairs = Vec::with_capacity(raw.len());
                for (key, value) in raw {
                    let key_value = stack_static_string_value(&key, self)?;
                    let pair = self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        [key_value, value],
                        &[&target_root],
                        &[args, pairs.as_slice()],
                    )?;
                    pairs.push(Value::Array(pair));
                }
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    pairs,
                    &[&target_root],
                    &[args],
                )?;
                Ok(Some(Value::Array(array)))
            }
            M::FromEntries => {
                let iter = args.first().cloned().unwrap_or(Value::Undefined);
                let result = self.alloc_stack_rooted_object_with_value_roots(stack, &[], args)?;
                match iter {
                    Value::Array(arr) => {
                        let snapshot: Vec<Value> =
                            crate::array::with_elements(arr, self.gc_heap(), |elements| {
                                elements.to_vec()
                            });
                        for entry in snapshot {
                            match entry {
                                Value::Array(pair) => {
                                    let key = crate::array::get(pair, self.gc_heap(), 0);
                                    let value = crate::array::get(pair, self.gc_heap(), 1);
                                    let key_str = object_static_property_key_from_value(&key)?;
                                    object::set(result, &mut self.gc_heap, &key_str, value);
                                }
                                _ => return Err(VmError::TypeMismatch),
                            }
                        }
                    }
                    Value::Map(map) => {
                        for (key, value) in collections::map_entries(map, self.gc_heap()) {
                            let key_str = object_static_property_key_from_value(&key)?;
                            object::set(result, &mut self.gc_heap, &key_str, value);
                        }
                    }
                    _ => return Err(VmError::TypeMismatch),
                }
                Ok(Some(Value::Object(result)))
            }
            M::GetOwnPropertyDescriptor => {
                let key = Self::coerce_vm_property_key(args.get(1))?;
                let desc = match args.first() {
                    Some(Value::Object(target)) => match &key {
                        VmPropertyKey::Symbol(sym) => {
                            object::get_own_symbol_descriptor(*target, self.gc_heap(), sym)
                        }
                        _ => object::get_own_descriptor(
                            *target,
                            self.gc_heap(),
                            key.string_name()
                                .expect("non-symbol property key has string spelling"),
                        ),
                    },
                    Some(Value::ClassConstructor(class)) => match &key {
                        VmPropertyKey::Symbol(sym) => object::get_own_symbol_descriptor(
                            class.statics(self.gc_heap()),
                            self.gc_heap(),
                            sym,
                        ),
                        _ => object::get_own_descriptor(
                            class.statics(self.gc_heap()),
                            self.gc_heap(),
                            key.string_name()
                                .expect("non-symbol property key has string spelling"),
                        ),
                    },
                    Some(Value::NativeFunction(native)) => {
                        let Some(key) = key.string_name() else {
                            return Ok(Some(Value::Undefined));
                        };
                        native.own_property_descriptor(self.gc_heap(), &self.string_heap, key)?
                    }
                    Some(Value::Boolean(_))
                    | Some(Value::Number(_))
                    | Some(Value::String(_))
                    | Some(Value::Symbol(_))
                    | Some(Value::BigInt(_)) => None,
                    Some(Value::Null) | Some(Value::Undefined) | None => {
                        return Err(VmError::TypeError {
                            message:
                                "Object.getOwnPropertyDescriptor called on null or undefined"
                                    .to_string(),
                        });
                    }
                    _ => {
                        return Err(VmError::TypeError {
                            message: "Object.getOwnPropertyDescriptor target must be an object"
                                .to_string(),
                        });
                    }
                };
                match desc {
                    Some(desc) => {
                        let obj =
                            self.descriptor_to_object_stack_rooted(stack, &desc, &[], args)?;
                        Ok(Some(Value::Object(obj)))
                    }
                    None => Ok(Some(Value::Undefined)),
                }
            }
            M::GetOwnPropertyDescriptors => {
                let target = match args.first() {
                    Some(Value::Object(target)) => *target,
                    _ => return Err(VmError::TypeMismatch),
                };
                let target_root = Value::Object(target);
                let result =
                    self.alloc_stack_rooted_object_with_value_roots(stack, &[&target_root], args)?;
                let result_root = Value::Object(result);
                let (keys, symbols): (Vec<String>, Vec<crate::symbol::JsSymbol>) =
                    object::with_properties(target, self.gc_heap(), |p| {
                        (
                            p.keys().map(|s| s.to_string()).collect(),
                            p.symbol_keys().collect(),
                        )
                    });
                for key in keys {
                    if let Some(desc) = object::get_own_descriptor(target, self.gc_heap(), &key) {
                        let desc_obj = self.descriptor_to_object_stack_rooted(
                            stack,
                            &desc,
                            &[&target_root, &result_root],
                            args,
                        )?;
                        object::set(result, &mut self.gc_heap, &key, Value::Object(desc_obj));
                    }
                }
                for sym in symbols {
                    if let Some(desc) =
                        object::get_own_symbol_descriptor(target, self.gc_heap(), &sym)
                    {
                        let desc_obj = self.descriptor_to_object_stack_rooted(
                            stack,
                            &desc,
                            &[&target_root, &result_root],
                            args,
                        )?;
                        if !object::set_symbol(
                            result,
                            &mut self.gc_heap,
                            sym,
                            Value::Object(desc_obj),
                        ) {
                            return Err(VmError::TypeMismatch);
                        }
                    }
                }
                Ok(Some(Value::Object(result)))
            }
            M::GetOwnPropertyNames => {
                let owned: Vec<String> = match args.first() {
                    Some(Value::Object(target)) => {
                        object::with_properties(*target, self.gc_heap(), |p| {
                            p.keys().map(|k| k.to_string()).collect()
                        })
                    }
                    Some(Value::NativeFunction(native)) => native
                        .own_property_keys(self.gc_heap())
                        .into_iter()
                        .collect(),
                    Some(Value::BoundFunction(bound)) => {
                        crate::function_metadata::bound_own_property_keys(bound, self.gc_heap())
                            .into_iter()
                            .collect()
                    }
                    Some(Value::Boolean(_) | Value::Number(_) | Value::Symbol(_)) => Vec::new(),
                    Some(Value::String(s)) => {
                        let mut keys: Vec<String> =
                            (0..s.len()).map(|idx| idx.to_string()).collect();
                        keys.push("length".to_string());
                        keys
                    }
                    _ => return Err(VmError::TypeMismatch),
                };
                let mut names = Vec::with_capacity(owned.len());
                for key in owned {
                    names.push(stack_static_string_value(&key, self)?);
                }
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    names,
                    &[],
                    &[args],
                )?;
                Ok(Some(Value::Array(array)))
            }
            M::GetOwnPropertySymbols => {
                let target = match args.first() {
                    Some(Value::Object(target)) => *target,
                    _ => return Err(VmError::TypeMismatch),
                };
                let syms: Vec<Value> = object::with_properties(target, self.gc_heap(), |p| {
                    p.symbol_keys().map(Value::Symbol).collect()
                });
                let target_root = Value::Object(target);
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    syms,
                    &[&target_root],
                    &[args],
                )?;
                Ok(Some(Value::Array(array)))
            }
            _ => Ok(None),
        }
    }

    fn descriptor_to_object_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        desc: &object::PropertyDescriptor,
        value_roots: &[&Value],
        slice_roots: &[Value],
    ) -> Result<object::JsObject, VmError> {
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.extend_from_slice(value_roots);
        match &desc.kind {
            object::DescriptorKind::Data { value } => roots.push(value),
            object::DescriptorKind::Accessor { getter, setter } => {
                if let Some(getter) = getter {
                    roots.push(getter);
                }
                if let Some(setter) = setter {
                    roots.push(setter);
                }
            }
        }
        let result =
            self.alloc_stack_rooted_object_with_value_roots(stack, roots.as_slice(), slice_roots)?;
        match &desc.kind {
            object::DescriptorKind::Data { value } => {
                object::set(result, &mut self.gc_heap, "value", value.clone());
                object::set(
                    result,
                    &mut self.gc_heap,
                    "writable",
                    Value::Boolean(desc.writable()),
                );
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                object::set(
                    result,
                    &mut self.gc_heap,
                    "get",
                    getter.clone().unwrap_or(Value::Undefined),
                );
                object::set(
                    result,
                    &mut self.gc_heap,
                    "set",
                    setter.clone().unwrap_or(Value::Undefined),
                );
            }
        }
        object::set(
            result,
            &mut self.gc_heap,
            "enumerable",
            Value::Boolean(desc.enumerable()),
        );
        object::set(
            result,
            &mut self.gc_heap,
            "configurable",
            Value::Boolean(desc.configurable()),
        );
        Ok(result)
    }

    pub(crate) fn run_iterator_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method =
            method_id::IteratorMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        let result = self.iterator_static_call_stack_rooted(stack, method, &args)?;
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_proxy_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method = method_id::ProxyMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        let result = self.proxy_static_call_stack_rooted(stack, method, &args)?;
        finish_static_call(&mut stack[top_idx], dst, result)
    }

    fn proxy_static_call_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        method: method_id::ProxyMethod,
        args: &[Value],
    ) -> Result<Value, VmError> {
        use method_id::ProxyMethod as M;
        match method {
            M::Construct => {
                let target = coerce_proxy_target(args.first())?;
                let handler = match args.get(1) {
                    Some(Value::Object(o)) => *o,
                    _ => return Err(VmError::TypeMismatch),
                };
                Ok(Value::Proxy(crate::proxy::JsProxy::new(target, handler)))
            }
            M::Revocable => {
                let target = coerce_proxy_target(args.first())?;
                let handler = match args.get(1) {
                    Some(Value::Object(o)) => *o,
                    _ => return Err(VmError::TypeMismatch),
                };
                let proxy = crate::proxy::JsProxy::new(target.clone(), handler);
                let proxy_value = Value::Proxy(proxy.clone());
                let target_root = target;
                let handler_root = Value::Object(handler);
                let roots = self.collect_allocation_roots(stack);
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    target_root.trace_value_slots(visitor);
                    handler_root.trace_value_slots(visitor);
                    for value in args {
                        value.trace_value_slots(visitor);
                    }
                };
                let revoke = native_function::native_value_with_captures_unchecked_with_roots(
                    &mut self.gc_heap,
                    "revoke",
                    smallvec::smallvec![proxy_value.clone()],
                    &mut external_visit,
                    move |_, _, captures| {
                        if let Some(Value::Proxy(proxy)) = captures.first() {
                            proxy.revoke();
                        }
                        Ok(Value::Undefined)
                    },
                )?;
                let obj = object::alloc_object_with_roots(&mut self.gc_heap, &mut external_visit)?;
                object::set(obj, &mut self.gc_heap, "proxy", Value::Proxy(proxy));
                object::set(obj, &mut self.gc_heap, "revoke", revoke);
                Ok(Value::Object(obj))
            }
        }
    }

    fn iterator_static_call_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        method: method_id::IteratorMethod,
        args: &[Value],
    ) -> Result<Value, VmError> {
        use method_id::IteratorMethod as M;
        match method {
            M::Construct => Err(VmError::TypeMismatch),
            M::From => {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                let state = match value {
                    Value::Iterator(rc) => return Ok(Value::Iterator(rc)),
                    Value::Generator(handle) => IteratorState::Generator { handle },
                    Value::Array(arr) => IteratorState::Array {
                        array: arr,
                        index: 0,
                    },
                    Value::String(s) => IteratorState::String {
                        string: s,
                        index: 0,
                    },
                    Value::Set(s) => {
                        let value_root = Value::Set(s);
                        let snap: SmallVec<[Value; 4]> = collections::set_values(s, self.gc_heap())
                            .into_iter()
                            .collect();
                        let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                            stack,
                            snap,
                            &[&value_root],
                            &[args],
                        )?;
                        IteratorState::Array { array, index: 0 }
                    }
                    Value::Map(m) => {
                        let value_root = Value::Map(m);
                        let mut entries: Vec<Value> = Vec::new();
                        for (k, v) in collections::map_entries(m, self.gc_heap()) {
                            let pair = self.alloc_stack_rooted_array_from_values_with_root_slices(
                                stack,
                                [k, v],
                                &[&value_root],
                                &[args, entries.as_slice()],
                            )?;
                            entries.push(Value::Array(pair));
                        }
                        let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                            stack,
                            entries,
                            &[&value_root],
                            &[args],
                        )?;
                        IteratorState::Array { array, index: 0 }
                    }
                    Value::Object(_) => IteratorState::User { iterator: value },
                    _ => return Err(VmError::TypeMismatch),
                };
                let iter = self.alloc_stack_rooted_iterator_state(stack, state, &[], &[args])?;
                Ok(Value::Iterator(iter))
            }
        }
    }
}

fn coerce_proxy_target(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v) if constructor_return_is_object(v) || abstract_ops::is_callable(v) => Ok(v.clone()),
        _ => Err(VmError::TypeMismatch),
    }
}

fn stack_static_string_value(s: &str, interp: &Interpreter) -> Result<Value, VmError> {
    Ok(Value::String(
        JsString::from_str(s, &interp.string_heap).map_err(|_| VmError::TypeMismatch)?,
    ))
}

fn object_static_property_key_from_value(value: &Value) -> Result<String, VmError> {
    match value {
        Value::String(s) => Ok(s.to_lossy_string()),
        Value::Number(n) => Ok(n.to_display_string()),
        Value::Boolean(b) => Ok((if *b { "true" } else { "false" }).to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Undefined => Ok("undefined".to_string()),
        _ => Err(VmError::TypeMismatch),
    }
}

fn decode_static_call(
    frame: &Frame,
    operands: &[Operand],
    method_pos: usize,
    argc_pos: usize,
    args_start: usize,
) -> Result<(u16, u32, SmallVec<[Value; 4]>), VmError> {
    let dst = register_operand(operands.first())?;
    let method_idx = const_operand(operands.get(method_pos))?;
    let args = collect_call_args(frame, operands, argc_pos, args_start)?;
    Ok((dst, method_idx, args))
}

fn collect_call_args(
    frame: &Frame,
    operands: &[Operand],
    argc_pos: usize,
    args_start: usize,
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(argc_pos) {
        Some(&Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(args_start + i))?;
        args.push(read_register(frame, r)?.clone());
    }
    Ok(args)
}

fn finish_static_call(frame: &mut Frame, dst: u16, result: Value) -> Result<(), VmError> {
    write_register(frame, dst, result)?;
    frame.pc += 1;
    Ok(())
}

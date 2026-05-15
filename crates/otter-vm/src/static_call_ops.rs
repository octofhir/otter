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
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, IteratorState, Value, VmError, abstract_ops, bigint,
    binary, collections, constructor_return_is_object, date, global_functions, json,
    json_to_vm_error, math, math_to_vm_error, native_function, object, object_statics,
    operand_decode::{const_operand, register_operand},
    read_register, symbol_dispatch, symbol_to_vm_error, temporal, temporal_to_vm_error,
    write_register,
};

impl Interpreter {
    pub(crate) fn run_static_call_operands(
        &mut self,
        op: Op,
        context: &ExecutionContext,
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
            Op::ArrayBufferCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method = method_id::ArrayBufferMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result = binary::dispatch::array_buffer_call(method, &args, &self.gc_heap)?;
                finish_static_call(frame, dst, result)
            }
            Op::DataViewCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method = method_id::DataViewMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result = binary::dispatch::data_view_call(method, &args)?;
                finish_static_call(frame, dst, result)
            }
            Op::TypedArrayCall => {
                let dst = register_operand(operands.first())?;
                let kind_idx = const_operand(operands.get(1))?;
                let method_idx = const_operand(operands.get(2))?;
                let args = collect_call_args(frame, operands, 3, 4)?;
                let kind =
                    binary::TypedArrayKind::from_u32(kind_idx).ok_or(VmError::InvalidOperand)?;
                let method = method_id::TypedArrayMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result =
                    binary::dispatch::typed_array_call(kind, method, &args, &self.gc_heap)?;
                finish_static_call(frame, dst, result)
            }
            Op::SharedArrayBufferCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method = method_id::SharedArrayBufferMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result =
                    binary::dispatch::shared_array_buffer_call(method, &args, &self.gc_heap)?;
                finish_static_call(frame, dst, result)
            }
            Op::ObjectCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::ObjectMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                let result = if let Some(result) =
                    self.try_function_object_static_call(Some(context), method, &args)?
                {
                    result
                } else if let Some(result) =
                    self.try_proxy_object_static_call(context, method, &args)?
                {
                    result
                } else {
                    object_statics::call(method, &args, &self.string_heap, &mut self.gc_heap)?
                };
                finish_static_call(frame, dst, result)
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

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
    abstract_ops, array, bigint, binary, collections, constructor_return_is_object, date,
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
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        match op {
            Op::MathCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::MathMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                // §21.3.2.{24,25} — `Math.max` / `Math.min` and every
                // other unary / binary Math method call `ToNumber` on
                // each arg, which runs `ToPrimitive(arg, "number")`
                // for non-primitives. Pre-coerce here so the
                // `coerce_all` table below sees primitives and the
                // user-installed `@@toPrimitive` / `valueOf` /
                // `toString` ladder fires per spec.
                let coerced = self.math_coerce_args(context, args)?;
                let result = math::call(method, &coerced).map_err(math_to_vm_error)?;
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
            Op::DateCall => unreachable!("DateCall requires stack-rooted dispatch"),
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

    /// Run `ToNumber` on each Math arg by routing non-primitives
    /// through `evaluate_to_primitive(arg, Number)`. Primitives pass
    /// through untouched so the spec's BigInt / Symbol error arms
    /// surface from inside `coerce_all` rather than this prelude.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-math.max>
    /// - <https://tc39.es/ecma262/#sec-math.min>
    fn math_coerce_args(
        &mut self,
        context: &ExecutionContext,
        args: SmallVec<[Value; 4]>,
    ) -> Result<SmallVec<[Value; 4]>, VmError> {
        let mut out: SmallVec<[Value; 4]> = SmallVec::with_capacity(args.len());
        for arg in args {
            if matches!(
                arg,
                Value::Object(_)
                    | Value::Array(_)
                    | Value::Function { .. }
                    | Value::Closure { .. }
                    | Value::NativeFunction(_)
                    | Value::BoundFunction(_)
                    | Value::ClassConstructor(_)
                    | Value::Proxy(_)
                    | Value::RegExp(_)
            ) {
                out.push(self.evaluate_to_primitive(
                    context,
                    &arg,
                    crate::abstract_ops::ToPrimitiveHint::Number,
                )?);
            } else {
                out.push(arg);
            }
        }
        Ok(out)
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

    /// Stack-rooted dispatcher for `Op::DateCall`. Construct
    /// allocates a fresh ordinary object with the
    /// `[[DateValue]]` internal slot wired through
    /// [`crate::object::set_date_data`]; the other static methods
    /// (Now / Parse / UTC) just return Numbers but route through
    /// here for uniformity.
    ///
    /// # Spec
    /// - <https://tc39.es/ecma262/#sec-date-constructor>
    pub(crate) fn run_date_static_call_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method = method_id::DateMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        // Resolve `%Date.prototype%` so the freshly allocated
        // instance inherits the right method bag. Cheap lookup —
        // two property reads on the realm globals.
        let date_prototype = match self.constructor_prototype_value("Date").ok() {
            Some(Value::Object(o)) => Some(o),
            _ => None,
        };
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for arg in &args {
                arg.trace_value_slots(visitor);
            }
        };
        let result = date::dispatch::call(
            method,
            &args,
            &mut self.gc_heap,
            date_prototype,
            &mut external_visit,
        )?;
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
            method_id::ObjectMethod::Assign => {
                let result = self.do_object_assign(context, stack, &args)?;
                return finish_static_call(&mut stack[top_idx], dst, result);
            }
            method_id::ObjectMethod::GetOwnPropertyDescriptor
            | method_id::ObjectMethod::HasOwn => {
                // §20.1.2.10 / §20.1.2.13 step 2: `key = ? ToPropertyKey(P)`.
                // The ToPrimitive ladder may invoke user
                // `Symbol.toPrimitive` / `toString` / `valueOf`, so
                // we route through the context-aware path *only*
                // when the arg isn't already a String / Symbol /
                // Number / Boolean / Null / Undefined primitive
                // that the free coercion handles directly.
                let key_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
                let needs_coercion = !matches!(
                    &key_arg,
                    Value::String(_)
                        | Value::Number(_)
                        | Value::Boolean(_)
                        | Value::Null
                        | Value::Undefined
                        | Value::Symbol(_)
                );
                if needs_coercion {
                    let coerced_key = self.evaluate_to_property_key(context, &key_arg)?;
                    let coerced_value = match &coerced_key {
                        crate::VmPropertyKey::Symbol(sym) => Value::Symbol(sym.clone()),
                        other => Value::String(crate::string::JsString::from_str(
                            other
                                .string_name()
                                .expect("non-symbol key has string spelling"),
                            &self.string_heap,
                        )?),
                    };
                    let mut rewritten: SmallVec<[Value; 4]> = args.iter().cloned().collect();
                    if rewritten.len() >= 2 {
                        rewritten[1] = coerced_value;
                    } else {
                        rewritten.push(coerced_value);
                    }
                    let result = if let Some(result) = self.try_function_object_static_call(
                        Some(context),
                        Some(stack),
                        method,
                        &rewritten,
                    )? {
                        result
                    } else if let Some(result) =
                        self.try_proxy_object_static_call(context, Some(stack), method, &rewritten)?
                    {
                        result
                    } else if let Some(result) =
                        self.object_static_call_stack_rooted(context, stack, method, &rewritten)?
                    {
                        result
                    } else {
                        object_statics::call(
                            method,
                            &rewritten,
                            &self.string_heap,
                            &mut self.gc_heap,
                        )?
                    };
                    return finish_static_call(&mut stack[top_idx], dst, result);
                }
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
        } else if let Some(result) = self.object_static_call_stack_rooted(context, stack, method, &args)? {
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
            // §20.1.2.2 step 4 — `properties = ToObject(Properties)`.
            // Plain `Value::Object` uses `with_properties` directly;
            // ClassConstructor / Array / Function-like sources route
            // through their respective enumerable-own-key probes so
            // user code can pass `Object.create({}, function() { ...
            // })` with own data properties installed on the
            // function.
            let entries: Vec<(String, Value)> = match props_arg {
                Value::Object(o) => object::with_properties(*o, self.gc_heap(), |p| {
                    p.enumerable_data_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect()
                }),
                Value::ClassConstructor(class) => {
                    object::with_properties(class.statics(self.gc_heap()), self.gc_heap(), |p| {
                        p.enumerable_data_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect()
                    })
                }
                Value::Array(arr) => {
                    let mut out: Vec<(String, Value)> = Vec::new();
                    let dense: Vec<Value> = array::with_elements(*arr, self.gc_heap(), |els| {
                        els.iter().cloned().collect()
                    });
                    for (idx, v) in dense.into_iter().enumerate() {
                        out.push((idx.to_string(), v));
                    }
                    let named: Vec<(String, Value)> = self.gc_heap().read_payload(*arr, |body| {
                        body.named_properties.as_ref().map_or_else(Vec::new, |m| {
                            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                        })
                    });
                    out.extend(named);
                    out
                }
                Value::Function { function_id } | Value::Closure { function_id, .. } => {
                    // Function objects carry a user-properties bag —
                    // an ordinary JsObject — accessed via the
                    // function-id keyed side table on the runtime.
                    // Iterate its enumerable own data entries.
                    let fid = *function_id;
                    self.function_user_props
                        .get(&fid)
                        .copied()
                        .map_or_else(Vec::new, |bag| {
                            object::with_properties(bag, self.gc_heap(), |p| {
                                p.enumerable_data_iter()
                                    .map(|(k, v)| (k.to_string(), v))
                                    .collect()
                            })
                        })
                }
                Value::NativeFunction(native) => native
                    .enumerable_own_property_keys(self.gc_heap())
                    .into_iter()
                    .filter_map(|key| {
                        match native.own_property_descriptor(
                            self.gc_heap(),
                            &self.string_heap,
                            &key,
                        ) {
                            Ok(Some(desc)) => Some((
                                key,
                                match &desc.kind {
                                    object::DescriptorKind::Data { value } => value.clone(),
                                    _ => Value::Undefined,
                                },
                            )),
                            _ => None,
                        }
                    })
                    .collect(),
                // §20.1.2.2 / §20.1.2.3 step 2 — `ToObject(Properties)`
                // boxes primitives into their wrapper. Wrappers carry
                // no observable enumerable own keys (String chars +
                // length are non-enumerable on the wrapper object),
                // so the spec walk yields an empty descriptor list.
                // Return `Ok(())` rather than `TypeMismatch` so
                // `Object.create(proto, 1n)` etc. round-trip per
                // `properties-arg-to-object*.js`.
                Value::Boolean(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Symbol(_)
                | Value::BigInt(_) => Vec::new(),
                // §22.2.6 — RegExp instances walk via the generic
                // enumerable-key probe so user-installed own
                // properties surface, then each value is read with
                // observable `[[Get]]` semantics (accessors fire).
                Value::RegExp(_) => {
                    let keys = self
                        .enumerable_own_string_keys_for_value(context, props_arg.clone(), 0)?;
                    let mut out = Vec::with_capacity(keys.len());
                    for key in keys {
                        let value = self.get_property_value_for_call(
                            context,
                            props_arg.clone(),
                            &key,
                        )?;
                        out.push((key, value));
                    }
                    out
                }
                Value::BoundFunction(bound) => {
                    let names = crate::function_metadata::bound_enumerable_own_property_keys(
                        bound,
                        self.gc_heap(),
                    );
                    let mut out = Vec::with_capacity(names.len());
                    for key in names {
                        let value = self.get_property_value_for_call(
                            context,
                            props_arg.clone(),
                            &key,
                        )?;
                        out.push((key, value));
                    }
                    out
                }
                _ => return Err(VmError::TypeMismatch),
            };
            for (key, desc_value) in entries {
                let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
                if !object::define_own_property_partial(obj, &mut self.gc_heap, &key, descriptor) {
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
        let target_value = args.first().cloned().unwrap_or(Value::Undefined);
        // §20.1.2.3 step 1 — `Type(O)` must be Object.
        if !matches!(
            &target_value,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Array(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::BoundFunction(_)
                | Value::NativeFunction(_)
                | Value::ClassConstructor(_)
        ) {
            return Err(VmError::TypeError {
                message: "Object.defineProperties target must be an object".to_string(),
            });
        }
        // §20.1.2.3 step 2 — `props = ToObject(Properties)`; the
        // resulting object is then enumerated for own enumerable
        // string-keyed names. We accept any Object-typed source and
        // route the key probe through the unified
        // `own_enumerable_string_keyed_property_entries` helper so
        // arrays / functions / class constructors / native functions
        // behave like ordinary objects here.
        let props_value = args.get(1).cloned().unwrap_or(Value::Undefined);
        let entries: Vec<(String, Value)> = match &props_value {
            Value::Object(o) => object::with_properties(*o, self.gc_heap(), |p| {
                p.enumerable_data_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect()
            }),
            Value::ClassConstructor(class) => {
                object::with_properties(class.statics(self.gc_heap()), self.gc_heap(), |p| {
                    p.enumerable_data_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect()
                })
            }
            Value::Array(arr) => {
                // §22.1.3.3 EnumerableOwnPropertyNames for Array —
                // indices in storage order, then any named props.
                let mut out: Vec<(String, Value)> = Vec::new();
                let dense: Vec<Value> =
                    array::with_elements(*arr, self.gc_heap(), |els| els.iter().cloned().collect());
                for (idx, v) in dense.into_iter().enumerate() {
                    out.push((idx.to_string(), v));
                }
                let named: Vec<(String, Value)> = self.gc_heap().read_payload(*arr, |body| {
                    body.named_properties.as_ref().map_or_else(Vec::new, |m| {
                        m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    })
                });
                out.extend(named);
                out
            }
            _ => {
                return Err(VmError::TypeError {
                    message: "Object.defineProperties properties must be an object".to_string(),
                });
            }
        };
        for (key, desc_value) in entries {
            let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
            let vm_key = crate::VmPropertyKey::OwnedString(key.clone());
            let ok = self.define_own_property_value(context, &target_value, &vm_key, descriptor)?;
            if !ok {
                return Err(VmError::TypeError {
                    message: format!("Object.defineProperties: cannot define '{key}'"),
                });
            }
        }
        Ok(target_value)
    }

    /// §20.1.2.1 Object.assign(target, ...sources).
    ///
    /// 1. `target = ? ToObject(target)` — primitive targets coerce to
    ///    their wrapper objects per §7.1.18.
    /// 2. For each source: ignore `undefined` / `null`; otherwise
    ///    enumerate the source's own enumerable string-keyed
    ///    properties (foundation walks `with_properties(...)
    ///    .enumerable_data_iter`) and `Set(target, key, value)`.
    /// 3. Return `target`.
    ///
    /// Sources of any object-typed kind are accepted (Array,
    /// Function, ClassConstructor, NativeFunction, etc.). Symbol
    /// sources are accepted but their symbol-keyed properties aren't
    /// copied yet — filed as a follow-up.
    fn do_object_assign(
        &mut self,
        _context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let target_input = args.first().cloned().unwrap_or(Value::Undefined);
        let target = match &target_input {
            Value::Object(o) => *o,
            Value::Null | Value::Undefined => {
                return Err(VmError::TypeError {
                    message: "Object.assign called on null or undefined".to_string(),
                });
            }
            _ => {
                // §7.1.18 ToObject — wrap primitives. We thread the
                // arg slice + a temporary target root through the
                // boxing helper so a mid-allocation GC sees both the
                // primitive value and the in-progress sources.
                let arg_slice = args;
                let boxed = self.box_sloppy_this_primitive_stack_rooted(
                    stack,
                    target_input.clone(),
                    &[arg_slice],
                )?;
                match boxed {
                    Value::Object(o) => o,
                    _ => {
                        return Err(VmError::TypeError {
                            message: "Object.assign: ToObject failed".to_string(),
                        });
                    }
                }
            }
        };
        for src in args.iter().skip(1) {
            match src {
                Value::Undefined | Value::Null => continue,
                Value::Object(o) => {
                    let entries: Vec<(String, Value)> =
                        object::with_properties(*o, self.gc_heap(), |p| {
                            p.enumerable_data_iter()
                                .map(|(k, v)| (k.to_string(), v))
                                .collect()
                        });
                    for (k, v) in entries {
                        object::set(target, &mut self.gc_heap, &k, v);
                    }
                }
                Value::Array(arr) => {
                    // §22.1.3.3 — Array EnumerableOwnPropertyNames:
                    // dense indices then named extra slots.
                    let dense: Vec<Value> = array::with_elements(*arr, self.gc_heap(), |els| {
                        els.iter().cloned().collect()
                    });
                    for (idx, v) in dense.into_iter().enumerate() {
                        object::set(target, &mut self.gc_heap, &idx.to_string(), v);
                    }
                    let named: Vec<(String, Value)> = self.gc_heap().read_payload(*arr, |body| {
                        body.named_properties.as_ref().map_or_else(Vec::new, |m| {
                            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                        })
                    });
                    for (k, v) in named {
                        object::set(target, &mut self.gc_heap, &k, v);
                    }
                }
                Value::String(s) => {
                    // §22.1.4 — String exotic exposes its code units
                    // as own indexed properties plus a `length`. The
                    // latter is read-only on the wrapper, so we copy
                    // only the indexed slots.
                    let lossy = s.to_lossy_string();
                    for (idx, ch) in lossy.chars().enumerate() {
                        let mut buf = [0u16; 2];
                        let units = ch.encode_utf16(&mut buf);
                        let unit_string =
                            crate::string::JsString::from_utf16_units(units, &self.string_heap)
                                .map_err(|_| VmError::TypeMismatch)?;
                        object::set(
                            target,
                            &mut self.gc_heap,
                            &idx.to_string(),
                            Value::String(unit_string),
                        );
                    }
                }
                _ => {
                    // Other object-typed sources: skip the spread for
                    // now (no observable own enumerable string keys
                    // exposed through the legacy `with_properties`
                    // probe). Sym sources are intentionally skipped.
                    continue;
                }
            }
        }
        Ok(Value::Object(target))
    }

    fn object_static_call_stack_rooted(
        &mut self,
        context: &ExecutionContext,
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
                    // §7.1.18 ToObject — Boolean / Number / Symbol /
                    // BigInt wrappers expose no own enumerable
                    // string keys; String wrappers carry indexed
                    // code-unit slots.
                    Some(Value::Boolean(_))
                    | Some(Value::Number(_))
                    | Some(Value::Symbol(_))
                    | Some(Value::BigInt(_)) => Vec::new(),
                    Some(Value::String(s)) => {
                        let len = s.len() as usize;
                        (0..len).map(|i| i.to_string()).collect()
                    }
                    Some(Value::Array(arr)) => {
                        let len = crate::array::len(*arr, self.gc_heap());
                        let mut keys: Vec<String> = (0..len)
                            .filter(|&i| crate::array::has_own_element(*arr, self.gc_heap(), i))
                            .map(|i| i.to_string())
                            .collect();
                        let named: Vec<String> = self.gc_heap().read_payload(*arr, |body| {
                            body.named_properties
                                .as_ref()
                                .map_or_else(Vec::new, |m| m.keys().cloned().collect())
                        });
                        keys.extend(named);
                        keys
                    }
                    Some(Value::Null) | Some(Value::Undefined) | None => {
                        return Err(VmError::TypeError {
                            message: "Object.keys called on null or undefined".to_string(),
                        });
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
                let values: Vec<Value> = match args.first() {
                    Some(Value::Object(target)) => {
                        object::with_properties(*target, self.gc_heap(), |p| {
                            p.enumerable_data_iter().map(|(_, value)| value).collect()
                        })
                    }
                    Some(Value::Boolean(_))
                    | Some(Value::Number(_))
                    | Some(Value::Symbol(_))
                    | Some(Value::BigInt(_)) => Vec::new(),
                    Some(Value::String(s)) => {
                        let units = s.to_utf16_vec();
                        units
                            .into_iter()
                            .map(|u| {
                                crate::string::JsString::from_utf16_units(&[u], &self.string_heap)
                                    .map(Value::String)
                                    .unwrap_or(Value::Undefined)
                            })
                            .collect()
                    }
                    Some(Value::Array(arr)) => {
                        let len = crate::array::len(*arr, self.gc_heap());
                        (0..len)
                            .filter(|&i| crate::array::has_own_element(*arr, self.gc_heap(), i))
                            .map(|i| crate::array::get(*arr, self.gc_heap(), i))
                            .collect()
                    }
                    Some(Value::Null) | Some(Value::Undefined) | None => {
                        return Err(VmError::TypeError {
                            message: "Object.values called on null or undefined".to_string(),
                        });
                    }
                    _ => return Err(VmError::TypeMismatch),
                };
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    values,
                    &[],
                    &[args],
                )?;
                Ok(Some(Value::Array(array)))
            }
            M::Entries => {
                let raw: Vec<(String, Value)> = match args.first() {
                    Some(Value::Object(target)) => {
                        object::with_properties(*target, self.gc_heap(), |p| {
                            p.enumerable_data_iter()
                                .map(|(key, value)| (key.to_string(), value))
                                .collect()
                        })
                    }
                    Some(Value::Boolean(_))
                    | Some(Value::Number(_))
                    | Some(Value::Symbol(_))
                    | Some(Value::BigInt(_)) => Vec::new(),
                    Some(Value::String(s)) => {
                        let units = s.to_utf16_vec();
                        units
                            .into_iter()
                            .enumerate()
                            .map(|(i, u)| {
                                let v = crate::string::JsString::from_utf16_units(
                                    &[u],
                                    &self.string_heap,
                                )
                                .map(Value::String)
                                .unwrap_or(Value::Undefined);
                                (i.to_string(), v)
                            })
                            .collect()
                    }
                    Some(Value::Array(arr)) => {
                        let len = crate::array::len(*arr, self.gc_heap());
                        (0..len)
                            .filter(|&i| crate::array::has_own_element(*arr, self.gc_heap(), i))
                            .map(|i| (i.to_string(), crate::array::get(*arr, self.gc_heap(), i)))
                            .collect()
                    }
                    // §20.1.2.5 — `Object.entries` walks enumerable
                    // own string keys per `EnumerableOwnPropertyNames`
                    // and reads each value via the spec `[[Get]]`.
                    // Callable shapes expose `name` / `length` /
                    // `prototype` plus a user-properties bag —
                    // enumerable keys come from
                    // [`Interpreter::enumerable_own_string_keys_for_value`]
                    // and per-key values from `get_property_value_for_call`.
                    Some(target @ (Value::Function { .. }
                    | Value::Closure { .. }
                    | Value::NativeFunction(_)
                    | Value::BoundFunction(_)
                    | Value::ClassConstructor(_))) => {
                        let target_value = target.clone();
                        let keys = self.enumerable_own_string_keys_for_value(
                            context,
                            target_value.clone(),
                            0,
                        )?;
                        let mut entries = Vec::with_capacity(keys.len());
                        for key in keys {
                            let value = self.get_property_value_for_call(
                                context,
                                target_value.clone(),
                                &key,
                            )?;
                            entries.push((key, value));
                        }
                        entries
                    }
                    Some(Value::Null) | Some(Value::Undefined) | None => {
                        return Err(VmError::TypeError {
                            message: "Object.entries called on null or undefined".to_string(),
                        });
                    }
                    _ => return Err(VmError::TypeMismatch),
                };
                let mut pairs = Vec::with_capacity(raw.len());
                for (key, value) in raw {
                    let key_value = stack_static_string_value(&key, self)?;
                    let pair = self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        [key_value, value],
                        &[],
                        &[args, pairs.as_slice()],
                    )?;
                    pairs.push(Value::Array(pair));
                }
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    pairs,
                    &[],
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
                            // §20.1.2.7 step 5.b — read `[0]` / `[1]`
                            // via spec `[[Get]]`. Accepts Array,
                            // wrapper String, ordinary Object with
                            // indexed keys, and String primitive.
                            let (key, value) = match entry {
                                Value::Array(pair) => (
                                    crate::array::get(pair, self.gc_heap(), 0),
                                    crate::array::get(pair, self.gc_heap(), 1),
                                ),
                                Value::String(s) => {
                                    let units = s.to_utf16_vec();
                                    let zero =
                                        units.first().copied().map_or(Value::Undefined, |u| {
                                            crate::string::JsString::from_utf16_units(
                                                &[u],
                                                &self.string_heap,
                                            )
                                            .map(Value::String)
                                            .unwrap_or(Value::Undefined)
                                        });
                                    let one = units.get(1).copied().map_or(Value::Undefined, |u| {
                                        crate::string::JsString::from_utf16_units(
                                            &[u],
                                            &self.string_heap,
                                        )
                                        .map(Value::String)
                                        .unwrap_or(Value::Undefined)
                                    });
                                    (zero, one)
                                }
                                Value::Object(obj) => {
                                    if let Some(s) = crate::object::string_data(obj, self.gc_heap())
                                    {
                                        let units = s.to_utf16_vec();
                                        let zero =
                                            units.first().copied().map_or(Value::Undefined, |u| {
                                                crate::string::JsString::from_utf16_units(
                                                    &[u],
                                                    &self.string_heap,
                                                )
                                                .map(Value::String)
                                                .unwrap_or(Value::Undefined)
                                            });
                                        let one =
                                            units.get(1).copied().map_or(Value::Undefined, |u| {
                                                crate::string::JsString::from_utf16_units(
                                                    &[u],
                                                    &self.string_heap,
                                                )
                                                .map(Value::String)
                                                .unwrap_or(Value::Undefined)
                                            });
                                        (zero, one)
                                    } else {
                                        let k = crate::object::get(obj, self.gc_heap(), "0")
                                            .unwrap_or(Value::Undefined);
                                        let v = crate::object::get(obj, self.gc_heap(), "1")
                                            .unwrap_or(Value::Undefined);
                                        (k, v)
                                    }
                                }
                                _ => return Err(VmError::TypeMismatch),
                            };
                            match &key {
                                Value::Symbol(sym) => {
                                    object::set_symbol(
                                        result,
                                        &mut self.gc_heap,
                                        sym.clone(),
                                        value,
                                    );
                                }
                                _ => {
                                    let key_str = object_static_property_key_from_value(&key)?;
                                    object::set(result, &mut self.gc_heap, &key_str, value);
                                }
                            }
                        }
                    }
                    Value::Map(map) => {
                        for (key, value) in collections::map_entries(map, self.gc_heap()) {
                            match &key {
                                Value::Symbol(sym) => {
                                    object::set_symbol(
                                        result,
                                        &mut self.gc_heap,
                                        sym.clone(),
                                        value,
                                    );
                                }
                                _ => {
                                    let key_str = object_static_property_key_from_value(&key)?;
                                    object::set(result, &mut self.gc_heap, &key_str, value);
                                }
                            }
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
                            message: "Object.getOwnPropertyDescriptor called on null or undefined"
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
                // §20.1.2.10.1 — ToObject(target) then enumerate
                // every own key. Primitive ToObject:
                // - Boolean / Number / Symbol / BigInt wrappers
                //   have no own keys → empty result object.
                // - String wrapper exposes indexed code-unit slots +
                //   `length`; emit data descriptors directly.
                // - Null / Undefined throw TypeError per spec.
                let result = self.alloc_stack_rooted_object_with_value_roots(stack, &[], args)?;
                match args.first() {
                    Some(Value::Null) | Some(Value::Undefined) | None => {
                        return Err(VmError::TypeError {
                            message: "Object.getOwnPropertyDescriptors called on null or undefined"
                                .to_string(),
                        });
                    }
                    Some(Value::Boolean(_))
                    | Some(Value::Number(_))
                    | Some(Value::Symbol(_))
                    | Some(Value::BigInt(_)) => {
                        // Empty result; primitive wrapper carries no
                        // own keys reachable through the foundation
                        // surface.
                    }
                    Some(Value::String(s)) => {
                        let units = s.to_utf16_vec();
                        let result_root = Value::Object(result);
                        for (i, u) in units.iter().enumerate() {
                            let key = i.to_string();
                            let unit =
                                crate::string::JsString::from_utf16_units(&[*u], &self.string_heap)
                                    .map_err(|_| VmError::TypeMismatch)?;
                            let desc = crate::object::PropertyDescriptor::data(
                                Value::String(unit),
                                false,
                                true,
                                false,
                            );
                            let desc_obj = self.descriptor_to_object_stack_rooted(
                                stack,
                                &desc,
                                &[&result_root],
                                args,
                            )?;
                            object::set(result, &mut self.gc_heap, &key, Value::Object(desc_obj));
                        }
                        let length_desc = crate::object::PropertyDescriptor::data(
                            Value::Number(crate::number::NumberValue::from_f64(units.len() as f64)),
                            false,
                            false,
                            false,
                        );
                        let length_obj = self.descriptor_to_object_stack_rooted(
                            stack,
                            &length_desc,
                            &[&result_root],
                            args,
                        )?;
                        object::set(
                            result,
                            &mut self.gc_heap,
                            "length",
                            Value::Object(length_obj),
                        );
                    }
                    Some(Value::Proxy(proxy)) => {
                        // §20.1.2.10.1 step 3 — drive the spec
                        // ladder via `own_property_keys_value`
                        // (full §10.5.11 trap + invariant validation),
                        // read each descriptor through the
                        // `getOwnPropertyDescriptor` trap, and skip
                        // any key whose descriptor is `undefined`.
                        let proxy_value = Value::Proxy(proxy.clone());
                        let result_root = Value::Object(result);
                        let string_heap = self.string_heap.clone();
                        let trap_keys = self
                            .own_property_keys_value(context, &proxy_value, &string_heap)?;
                        for key in trap_keys {
                            let vm_key = match &key {
                                Value::String(s) => {
                                    crate::VmPropertyKey::OwnedString(s.to_lossy_string())
                                }
                                Value::Symbol(sym) => crate::VmPropertyKey::Symbol(sym.clone()),
                                _ => continue,
                            };
                            let desc = self
                                .ordinary_get_own_property_descriptor_value_stack_rooted(
                                    context,
                                    stack,
                                    proxy_value.clone(),
                                    &vm_key,
                                    0,
                                )?;
                                let Some(desc) = desc else {
                                continue;
                            };
                            let desc_obj = self.descriptor_to_object_stack_rooted(
                                stack,
                                &desc,
                                &[&proxy_value, &result_root],
                                args,
                            )?;
                            match &key {
                                Value::String(s) => {
                                    object::set(
                                        result,
                                        &mut self.gc_heap,
                                        &s.to_lossy_string(),
                                        Value::Object(desc_obj),
                                    );
                                }
                                Value::Symbol(sym) => {
                                    if !object::set_symbol(
                                        result,
                                        &mut self.gc_heap,
                                        sym.clone(),
                                        Value::Object(desc_obj),
                                    ) {
                                        return Err(VmError::TypeMismatch);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Value::Object(target)) => {
                        let target = *target;
                        let target_root = Value::Object(target);
                        let result_root = Value::Object(result);
                        let (keys, symbols): (Vec<String>, Vec<crate::symbol::JsSymbol>) =
                            object::with_properties(target, self.gc_heap(), |p| {
                                (
                                    p.keys().map(|s| s.to_string()).collect(),
                                    p.symbol_keys().collect(),
                                )
                            });
                        for key in keys {
                            if let Some(desc) =
                                object::get_own_descriptor(target, self.gc_heap(), &key)
                            {
                                let desc_obj = self.descriptor_to_object_stack_rooted(
                                    stack,
                                    &desc,
                                    &[&target_root, &result_root],
                                    args,
                                )?;
                                object::set(
                                    result,
                                    &mut self.gc_heap,
                                    &key,
                                    Value::Object(desc_obj),
                                );
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
                    }
                    _ => return Err(VmError::TypeMismatch),
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
                    Some(
                        Value::Boolean(_) | Value::Number(_) | Value::Symbol(_) | Value::BigInt(_),
                    ) => Vec::new(),
                    Some(Value::String(s)) => {
                        let mut keys: Vec<String> =
                            (0..s.len()).map(|idx| idx.to_string()).collect();
                        keys.push("length".to_string());
                        keys
                    }
                    Some(Value::Array(arr)) => {
                        let len = crate::array::len(*arr, self.gc_heap());
                        let mut keys: Vec<String> = (0..len)
                            .filter(|&i| crate::array::has_own_element(*arr, self.gc_heap(), i))
                            .map(|i| i.to_string())
                            .collect();
                        let named: Vec<String> = self.gc_heap().read_payload(*arr, |body| {
                            body.named_properties
                                .as_ref()
                                .map_or_else(Vec::new, |m| m.keys().cloned().collect())
                        });
                        keys.extend(named);
                        keys.push("length".to_string());
                        keys
                    }
                    Some(Value::Null) | Some(Value::Undefined) | None => {
                        return Err(VmError::TypeError {
                            message: "Object.getOwnPropertyNames called on null or undefined"
                                .to_string(),
                        });
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

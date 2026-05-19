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
                // §25.5.1 step 1 — `JText = ? ToString(text)`. Run
                // the ToPrimitive(hint: string) ladder on a
                // non-string parse operand so user
                // `Symbol.toPrimitive` / `toString` / `valueOf`
                // hooks fire before the SyntaxError check.
                let mut coerced: SmallVec<[Value; 4]> = args.iter().cloned().collect();
                if matches!(method, method_id::JsonMethod::Parse)
                    && let Some(slot) = coerced.first_mut()
                    && !matches!(slot, Value::String(_))
                {
                    let primitive = if crate::abstract_ops::is_primitive(slot) {
                        slot.clone()
                    } else {
                        self.evaluate_to_primitive(
                            context,
                            slot,
                            crate::abstract_ops::ToPrimitiveHint::String,
                        )?
                    };
                    let s = match primitive {
                        Value::String(s) => s,
                        Value::Symbol(_) => {
                            return Err(VmError::TypeError {
                                message: "JSON.parse: cannot convert a Symbol to a string"
                                    .to_string(),
                            });
                        }
                        other => crate::string::JsString::from_str(
                            &other.display_string(),
                            &self.string_heap,
                        )?,
                    };
                    *slot = Value::String(s);
                }
                let result = json::call(method, &coerced, &self.string_heap, &mut self.gc_heap)
                    .map_err(json_to_vm_error)?;
                finish_static_call(frame, dst, result)
            }
            Op::DateCall => unreachable!("DateCall requires stack-rooted dispatch"),
            Op::BigIntCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::BigIntMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                // §7.1.13 ToBigInt step 4 — Array operands flow
                // through `ToPrimitive(hint: number)`, which routes
                // through `Array.prototype.toString` = `.join(",")`.
                // The free dispatcher can't reach the heap, so we
                // pre-coerce Array args to their joined-string form
                // here. Empty arrays surface as `""` (→ 0n).
                let mut coerced: SmallVec<[Value; 4]> = args.iter().cloned().collect();
                for slot in coerced.iter_mut() {
                    if let Value::Array(arr) = slot {
                        let parts: Vec<String> =
                            crate::array::with_elements(*arr, &self.gc_heap, |elements| {
                                elements
                                    .iter()
                                    .map(|v| match v {
                                        Value::Undefined | Value::Null | Value::Hole => {
                                            String::new()
                                        }
                                        other => other.display_string(),
                                    })
                                    .collect()
                            });
                        let joined = parts.join(",");
                        *slot = Value::String(crate::string::JsString::from_str(
                            &joined,
                            &self.string_heap,
                        )?);
                    }
                }
                let result = bigint::dispatch::call(method, &coerced)?;
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
                // §20.4.1.1 / §20.4.2.4 / §20.4.2.6 — the description /
                // key argument flows through `ToString`. Object operands
                // require `evaluate_to_primitive("string")` so user
                // `@@toPrimitive` / `toString` / `valueOf` fires. The
                // intrinsic-table dispatcher has no execution context;
                // pre-coerce the first argument here.
                let coerced = self.symbol_coerce_first_arg(context, args)?;
                let result =
                    symbol_dispatch::call(self, method, &coerced).map_err(symbol_to_vm_error)?;
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

    /// Coerce the first positional argument of a `Symbol(...)` /
    /// `Symbol.for(...)` invocation through `ToPrimitive(arg,
    /// "string")` so user-defined `@@toPrimitive` / `valueOf` /
    /// `toString` overrides fire per §7.1.1. The remaining args (none
    /// today; `Symbol.keyFor` takes a Symbol that must not be coerced)
    /// pass through untouched. Delegates to the shared
    /// `Interpreter::coerce_to_primitive` ladder.
    fn symbol_coerce_first_arg(
        &mut self,
        context: &ExecutionContext,
        mut args: SmallVec<[Value; 4]>,
    ) -> Result<SmallVec<[Value; 4]>, VmError> {
        let Some(first) = args.first_mut() else {
            return Ok(args);
        };
        let coerced = self.coerce_to_primitive(context, first, abstract_ops::ToPrimitiveHint::String)?;
        *first = coerced;
        Ok(args)
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
        context: Option<&ExecutionContext>,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method = method_id::JsonMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        // §25.5.1 step 1 — `JText = ? ToString(text)`. Run the
        // ToPrimitive(hint: string) ladder on a non-string parse
        // operand so user `Symbol.toPrimitive` / `toString` /
        // `valueOf` hooks fire before the SyntaxError check.
        let args: SmallVec<[Value; 4]> = if matches!(method, method_id::JsonMethod::Parse) {
            let mut coerced: SmallVec<[Value; 4]> = args.iter().cloned().collect();
            if let Some(slot) = coerced.first_mut()
                && !matches!(slot, Value::String(_))
            {
                let primitive = if crate::abstract_ops::is_primitive(slot) {
                    slot.clone()
                } else if let Some(context) = context {
                    self.evaluate_to_primitive(
                        context,
                        slot,
                        crate::abstract_ops::ToPrimitiveHint::String,
                    )?
                } else {
                    return Err(VmError::TypeError {
                        message: "JSON.parse argument 0 must be a string".to_string(),
                    });
                };
                let s = match primitive {
                    Value::String(s) => s,
                    Value::Symbol(_) => {
                        return Err(VmError::TypeError {
                            message: "JSON.parse: cannot convert a Symbol to a string".to_string(),
                        });
                    }
                    other => crate::string::JsString::from_str(
                        &other.display_string(),
                        &self.string_heap,
                    )?,
                };
                *slot = Value::String(s);
            }
            coerced
        } else {
            args.iter().cloned().collect()
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
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let (dst, method_idx, args) = {
            let frame = &stack[top_idx];
            decode_static_call(frame, operands, 1, 2, 3)?
        };
        let method = method_id::DateMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
        // §21.4.2.1 step 4 / §21.4.3.4 step 1 — `new Date(year, month,
        // ...)` and `Date.UTC` walk their arguments through
        // `ToNumber` in declaration order **before** assembling the
        // resulting time value. Pre-coerce here so user
        // `@@toPrimitive` / `valueOf` / `toString` overrides fire per
        // spec and abrupt completions halt subsequent coercions.
        // Single-arg `new Date(value)` (§21.4.2.2 step 3) follows
        // its own ToPrimitive(`number`) ladder, handled inside
        // `date::dispatch::construct_time_value`.
        let mut args = args;
        let needs_to_number = matches!(method, method_id::DateMethod::UTC)
            || (matches!(method, method_id::DateMethod::Construct) && args.len() >= 2);
        if needs_to_number {
            for slot in args.iter_mut() {
                let coerced = self.coerce_to_number(context, slot)?;
                *slot = Value::Number(coerced);
            }
        } else if matches!(method, method_id::DateMethod::Construct) && args.len() == 1 {
            // §21.4.2.2 step 3.b — single-arg `new Date(value)` runs
            // `ToPrimitive(value)` (hint "default") when `value` is
            // not already a Date instance. `String` results then
            // drive Date.parse; everything else flows through
            // ToNumber. Objects with `[[DateValue]]` skip ToPrimitive
            // entirely (§21.4.2.2 step 3.a) so subclass instances
            // copy the underlying time value verbatim.
            let slot = &mut args[0];
            let is_date_instance = matches!(slot, Value::Object(o) if crate::object::date_data(*o, &self.gc_heap).is_some());
            if !is_date_instance {
                let primitive = self.coerce_to_primitive(
                    context,
                    slot,
                    crate::abstract_ops::ToPrimitiveHint::Default,
                )?;
                *slot = primitive;
            }
        }
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
            method_id::ObjectMethod::GetOwnPropertyDescriptor | method_id::ObjectMethod::HasOwn => {
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
        } else if let Some(result) =
            self.object_static_call_stack_rooted(context, stack, method, &args)?
        {
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
            // §20.1.2.2 step 5 — enumerate own enumerable string keys
            // of `properties`, then `Get` each through the
            // accessor-aware path so user-defined `valueOf` /
            // `toString` / accessor getters fire per §6.2.5.5
            // ToPropertyDescriptor.
            let props_owned = props_arg.clone();
            let keys = own_enumerable_keys_for_define(self, &props_owned)?;
            for key in keys {
                let vm_key = crate::VmPropertyKey::OwnedString(key.clone());
                let outcome = self.ordinary_get_value(
                    context,
                    props_owned.clone(),
                    props_owned.clone(),
                    &vm_key,
                    0,
                )?;
                let desc_value = match outcome {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, props_owned.clone(), args)?
                    }
                };
                let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
                if !self.define_own_property_partial(obj, &key, descriptor)? {
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
        // §7.1.18 ToObject — `null` / `undefined` throw a
        // TypeError. Other primitives wrap into their boxed
        // form which has no own enumerable string-keyed
        // properties (except `String`, where the code units
        // surface as indexed slots).
        if matches!(props_value, Value::Null | Value::Undefined) {
            return Err(VmError::TypeError {
                message: "Object.defineProperties properties must be an object".to_string(),
            });
        }
        let keys = own_enumerable_keys_for_define(self, &props_value)?;
        for key in keys {
            // §6.2.5.5 step 4 — `Get(props, key)` is accessor-aware,
            // and step 5 — `ToPropertyDescriptor(descObj)` reads the
            // accessor / data fields off the resolved value. Thread
            // both through the interpreter so user getters fire and
            // any abrupt completion propagates.
            let vm_key = crate::VmPropertyKey::OwnedString(key.clone());
            let outcome =
                self.ordinary_get_value(context, props_value.clone(), props_value.clone(), &vm_key, 0)?;
            let desc_value = match outcome {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                    self.run_callable_sync(context, &getter, props_value.clone(), args)?
                }
            };
            let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
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
        context: &ExecutionContext,
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
                        // §20.1.2.1 step 4.c.iii.2.b — `Set(to, nextKey,
                        // propValue, true)` runs OrdinarySet with the
                        // *strict* flag, so frozen / sealed / non-
                        // writable / non-extensible targets surface
                        // TypeError instead of silently dropping.
                        self.ordinary_set_with_callable_setter(context, target, &k, v, true)?;
                    }
                    // §20.1.2.1 step 4.c.ii — `OwnPropertyKeys(O)`
                    // returns string keys followed by symbol keys.
                    // Copy enumerable own symbol data slots too.
                    let sym_entries: Vec<(crate::symbol::JsSymbol, Value)> =
                        object::with_properties(*o, self.gc_heap(), |p| {
                            p.enumerable_symbol_data_iter().collect()
                        });
                    for (sym, v) in sym_entries {
                        self.ordinary_set_symbol_with_callable_setter(
                            context, target, &sym, v, true,
                        )?;
                    }
                }
                Value::Array(arr) => {
                    // §22.1.3.3 — Array EnumerableOwnPropertyNames:
                    // dense indices then named extra slots.
                    let dense: Vec<Value> = array::with_elements(*arr, self.gc_heap(), |els| {
                        els.iter().cloned().collect()
                    });
                    for (idx, v) in dense.into_iter().enumerate() {
                        self.ordinary_set_with_callable_setter(
                            context,
                            target,
                            &idx.to_string(),
                            v,
                            true,
                        )?;
                    }
                    let named: Vec<(String, Value)> = self.gc_heap().read_payload(*arr, |body| {
                        body.named_properties.as_ref().map_or_else(Vec::new, |m| {
                            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                        })
                    });
                    for (k, v) in named {
                        self.ordinary_set_with_callable_setter(context, target, &k, v, true)?;
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
                        self.ordinary_set_with_callable_setter(
                            context,
                            target,
                            &idx.to_string(),
                            Value::String(unit_string),
                            true,
                        )?;
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
                    Some(
                        target @ (Value::Function { .. }
                        | Value::Closure { .. }
                        | Value::NativeFunction(_)
                        | Value::BoundFunction(_)
                        | Value::ClassConstructor(_)),
                    ) => {
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
                // §20.1.2.7 Object.fromEntries(iterable). Spec iterator
                // protocol with IteratorClose on abrupt completion per
                // the AddEntriesFromIterable analogue used in step 4.
                // <https://tc39.es/ecma262/#sec-object.fromentries>
                let iter = args.first().cloned().unwrap_or(Value::Undefined);
                if matches!(iter, Value::Undefined | Value::Null) {
                    return Err(VmError::TypeError {
                        message: "Object.fromEntries: iterable must not be null or undefined"
                            .to_string(),
                    });
                }
                // §20.1.2.7 step 2 — `obj = OrdinaryObjectCreate(%Object.prototype%)`.
                let object_proto = self.constructor_prototype_value("Object").ok();
                let result = self.alloc_stack_rooted_object_with_value_roots(stack, &[], args)?;
                if let Some(Value::Object(proto_obj)) = object_proto {
                    object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
                }

                let (iterator, next_method) = self.get_iterator_sync(context, &iter)?;

                loop {
                    let stepped =
                        self.iterator_step_sync(context, &iterator, &next_method)?;
                    let Some(entry) = stepped else {
                        break;
                    };

                    if !value_is_object_like_for_entry(&entry) {
                        let _ = self.iterator_close_sync(context, &iterator);
                        return Err(VmError::TypeError {
                            message:
                                "Object.fromEntries: iterator value is not an entry object"
                                    .to_string(),
                        });
                    }

                    let key = match read_indexed_entry(self, context, &entry, "0") {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = self.iterator_close_sync(context, &iterator);
                            return Err(e);
                        }
                    };
                    let value = match read_indexed_entry(self, context, &entry, "1") {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = self.iterator_close_sync(context, &iterator);
                            return Err(e);
                        }
                    };

                    let key_pk = match self.to_property_key_sync(context, key) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = self.iterator_close_sync(context, &iterator);
                            return Err(e);
                        }
                    };
                    let set_result = match &key_pk {
                        VmPropertyKey::Symbol(sym) => {
                            object::set_symbol(result, &mut self.gc_heap, sym.clone(), value);
                            Ok(())
                        }
                        _ => {
                            let k = key_pk
                                .string_name()
                                .expect("non-symbol property key has string spelling")
                                .to_owned();
                            self.set_property(result, &k, value)
                        }
                    };
                    if let Err(e) = set_result {
                        let _ = self.iterator_close_sync(context, &iterator);
                        return Err(e);
                    }
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
                    Some(Value::NativeFunction(native)) => match &key {
                        VmPropertyKey::Symbol(sym) => {
                            native.own_symbol_property_descriptor(self.gc_heap(), sym)
                        }
                        _ => {
                            let key = key
                                .string_name()
                                .expect("non-symbol property key has string spelling");
                            native.own_property_descriptor(
                                self.gc_heap(),
                                &self.string_heap,
                                key,
                            )?
                        }
                    },
                    // §10.4.5.1 IntegerIndexedExoticObject
                    // [[GetOwnProperty]] — canonical-numeric-index
                    // strings produce a data descriptor for in-range
                    // elements; everything else returns no descriptor.
                    Some(Value::TypedArray(t)) => match &key {
                        VmPropertyKey::Symbol(sym) => t.expando().and_then(|bag| {
                            crate::object::get_own_symbol_descriptor(bag, self.gc_heap(), sym)
                        }),
                        _ => {
                            let k = key
                                .string_name()
                                .expect("non-symbol property key has string spelling");
                            if let Some(n) =
                                crate::property_dispatch::canonical_numeric_index_string(k)
                            {
                                if t.buffer().is_detached()
                                    || !n.is_finite()
                                    || n.fract() != 0.0
                                    || n < 0.0
                                    || (n as usize) >= t.length()
                                {
                                    None
                                } else {
                                    Some(crate::object::PropertyDescriptor::data(
                                        t.get(n as usize),
                                        true,
                                        true,
                                        true,
                                    ))
                                }
                            } else if let Some(bag) = t.expando() {
                                crate::object::get_own_descriptor(bag, self.gc_heap(), k)
                            } else {
                                None
                            }
                        }
                    },
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
                // The result inherits from `%Object.prototype%` per
                // step 2 (`OrdinaryObjectCreate(%Object.prototype%)`).
                let object_proto = self.constructor_prototype_value("Object").ok();
                let result = self.alloc_stack_rooted_object_with_value_roots(stack, &[], args)?;
                if let Some(Value::Object(proto_obj)) = object_proto {
                    object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
                }
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
                            self.set_property(result, &key, Value::Object(desc_obj))?;
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
                        self.set_property(result, "length", Value::Object(length_obj))?;
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
                        let trap_keys =
                            self.own_property_keys_value(context, &proxy_value, &string_heap)?;
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
                                    self.set_property(
                                        result,
                                        &s.to_lossy_string(),
                                        Value::Object(desc_obj),
                                    )?;
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
                                self.set_property(result, &key, Value::Object(desc_obj))?;
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
            // §20.1.2.7 `Object.groupBy(items, callbackfn)` — groups
            // an iterable into a null-prototype object keyed by the
            // callback's return value. The foundation drives Array
            // / array-like items directly; the spec's full
            // `GetIterator` ladder is reachable but not exercised by
            // the test262 buckets covered today.
            //
            // <https://tc39.es/ecma262/#sec-object.groupby>
            M::GroupBy => Ok(Some(self.do_object_group_by(context, stack, args)?)),
            _ => Ok(None),
        }
    }

    fn do_object_group_by(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let items = args.first().cloned().unwrap_or(Value::Undefined);
        let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
        if matches!(items, Value::Undefined | Value::Null) {
            return Err(VmError::TypeError {
                message: "Object.groupBy: items must be iterable".to_string(),
            });
        }
        if !self.is_callable_runtime(&callback) {
            return Err(VmError::TypeError {
                message: "Object.groupBy: callback must be a function".to_string(),
            });
        }
        let result =
            self.alloc_stack_rooted_object_with_extra_roots(stack, &[&items, &callback])?;
        object::set_prototype(result, &mut self.gc_heap, None);

        let items_snapshot: Vec<Value> = match &items {
            Value::Array(arr) => {
                crate::array::with_elements(*arr, &self.gc_heap, |elements| elements.to_vec())
            }
            Value::Object(obj) => {
                let length =
                    crate::object::get(*obj, &self.gc_heap, "length").unwrap_or(Value::Undefined);
                let length_n = crate::number::to_number_value(&length);
                let length_usize = if length_n.is_nan() || length_n <= 0.0 {
                    0
                } else {
                    length_n.min(9_007_199_254_740_991.0) as usize
                };
                let mut out: Vec<Value> = Vec::with_capacity(length_usize);
                for i in 0..length_usize {
                    let key = i.to_string();
                    out.push(
                        crate::object::get(*obj, &self.gc_heap, &key).unwrap_or(Value::Undefined),
                    );
                }
                out
            }
            Value::String(s) => {
                let mut out: Vec<Value> = Vec::new();
                let units = s.to_utf16_vec();
                for &u in &units {
                    let unit_str = crate::JsString::from_utf16_units(&[u], &self.string_heap)
                        .map_err(|_| VmError::TypeMismatch)?;
                    out.push(Value::String(unit_str));
                }
                out
            }
            _ => {
                return Err(VmError::TypeError {
                    message: "Object.groupBy: items is not iterable".to_string(),
                });
            }
        };

        for (idx, item) in items_snapshot.iter().enumerate() {
            let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
            cb_args.push(item.clone());
            cb_args.push(Value::Number(crate::number::NumberValue::from_f64(
                idx as f64,
            )));
            let key = self.run_callable_sync(context, &callback, Value::Undefined, cb_args)?;
            let key_pk = self.to_property_key_sync(context, key)?;
            let key_str = match key_pk {
                crate::VmPropertyKey::Symbol(sym) => {
                    let existing = crate::object::get_symbol(result, &self.gc_heap, &sym);
                    let group = match existing {
                        Some(Value::Array(arr)) => arr,
                        _ => {
                            let arr = self.alloc_stack_rooted_array_from_values_with_root_slices(
                                stack,
                                Vec::new(),
                                &[&Value::Object(result), item],
                                &[args],
                            )?;
                            crate::object::set_symbol(
                                result,
                                &mut self.gc_heap,
                                sym.clone(),
                                Value::Array(arr),
                            );
                            arr
                        }
                    };
                    let value_root = item.clone();
                    let arr_value = Value::Array(group);
                    let res_root = Value::Object(result);
                    let roots = [&value_root, &arr_value, &res_root];
                    let mut external_visit =
                        |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                            for v in &roots {
                                v.trace_value_slots(visitor);
                            }
                        };
                    crate::array::push_with_roots(
                        group,
                        &mut self.gc_heap,
                        item.clone(),
                        &mut external_visit,
                    )?;
                    continue;
                }
                crate::VmPropertyKey::Atom(a) => a.name().to_string(),
                crate::VmPropertyKey::String(s) => s.to_string(),
                crate::VmPropertyKey::OwnedString(s) => s,
            };
            let existing = crate::object::get(result, &self.gc_heap, &key_str);
            let group = match existing {
                Some(Value::Array(arr)) => arr,
                _ => {
                    let arr = self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        Vec::new(),
                        &[&Value::Object(result), item],
                        &[args],
                    )?;
                    self.set_property(result, &key_str, Value::Array(arr))?;
                    arr
                }
            };
            let value_root = item.clone();
            let arr_value = Value::Array(group);
            let res_root = Value::Object(result);
            let roots = [&value_root, &arr_value, &res_root];
            let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for v in &roots {
                    v.trace_value_slots(visitor);
                }
            };
            crate::array::push_with_roots(
                group,
                &mut self.gc_heap,
                item.clone(),
                &mut external_visit,
            )?;
        }
        Ok(Value::Object(result))
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
        // §6.2.5.4 FromPropertyDescriptor step 2 — descriptor objects
        // inherit `%Object.prototype%`.
        let object_proto = self.constructor_prototype_value("Object").ok();
        let result =
            self.alloc_stack_rooted_object_with_value_roots(stack, roots.as_slice(), slice_roots)?;
        if let Some(Value::Object(proto_obj)) = object_proto {
            object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
        }
        match &desc.kind {
            object::DescriptorKind::Data { value } => {
                self.set_property(result, "value", value.clone())?;
                self.set_property(result, "writable", Value::Boolean(desc.writable()))?;
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                self.set_property(result, "get", getter.clone().unwrap_or(Value::Undefined))?;
                self.set_property(result, "set", setter.clone().unwrap_or(Value::Undefined))?;
            }
        }
        self.set_property(result, "enumerable", Value::Boolean(desc.enumerable()))?;
        self.set_property(result, "configurable", Value::Boolean(desc.configurable()))?;
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
                let obj = self.alloc_stack_rooted_object_with_value_roots(
                    stack,
                    &[&proxy_value, &revoke],
                    args,
                )?;
                self.set_property(obj, "proxy", Value::Proxy(proxy))?;
                self.set_property(obj, "revoke", revoke)?;
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
                        origin: crate::BuiltinIteratorOrigin::Array,
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
                        IteratorState::Array {
                            array,
                            index: 0,
                            origin: crate::BuiltinIteratorOrigin::Set,
                        }
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
                        IteratorState::Array {
                            array,
                            index: 0,
                            origin: crate::BuiltinIteratorOrigin::Map,
                        }
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

/// §6.2.5.5 + §20.1.2.3 — enumerate the own enumerable string keys
/// of a `properties` argument supplied to `Object.defineProperties`
/// / `Object.create`. Includes accessor-shaped own keys so the
/// caller can `Get` the descriptor value through the spec's
/// accessor-aware path.
fn own_enumerable_keys_for_define(
    interp: &Interpreter,
    props: &Value,
) -> Result<Vec<String>, VmError> {
    match props {
        Value::Object(o) => Ok(object::with_properties(*o, interp.gc_heap(), |p| {
            p.enumerable_keys().map(|k| k.to_string()).collect()
        })),
        Value::ClassConstructor(class) => Ok(object::with_properties(
            class.statics(interp.gc_heap()),
            interp.gc_heap(),
            |p| p.enumerable_keys().map(|k| k.to_string()).collect(),
        )),
        Value::Array(arr) => {
            // §22.1.3.3 EnumerableOwnPropertyNames for Array — indices
            // in storage order, then any named props that were
            // installed enumerable on the array exotic (including
            // accessor-shaped own properties hung via
            // `Object.defineProperty`).
            let mut out: Vec<String> = Vec::new();
            let dense_len = array::with_elements(*arr, interp.gc_heap(), |els| els.len());
            for idx in 0..dense_len {
                out.push(idx.to_string());
            }
            let (named, accessor_keys): (Vec<String>, Vec<String>) =
                interp.gc_heap().read_payload(*arr, |body| {
                    let named = body
                        .named_properties
                        .as_ref()
                        .map_or_else(Vec::new, |m| m.keys().cloned().collect());
                    let accessors = body.accessors.as_ref().map_or_else(Vec::new, |m| {
                        m.keys()
                            .filter(|k| k.parse::<usize>().is_err())
                            .cloned()
                            .collect()
                    });
                    (named, accessors)
                });
            out.extend(named);
            for key in accessor_keys {
                if !out.contains(&key) {
                    out.push(key);
                }
            }
            Ok(out)
        }
        Value::String(s) => {
            let units = s.to_utf16_vec();
            Ok((0..units.len()).map(|i| i.to_string()).collect())
        }
        _ => Ok(Vec::new()),
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

/// §6.2.4.5 IsObject classifier used by `Object.fromEntries` to gate
/// the entry validation step. Mirrors the broad set of Object-like
/// runtime kinds the rest of the VM treats as objects (every heap
/// allocation but the primitive variants).
fn value_is_object_like_for_entry(v: &Value) -> bool {
    matches!(
        v,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Promise(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_)
    )
}

/// §7.3.2 `Get(target, name)` for indexed-string entry probing in
/// `Object.fromEntries`. Honours installed accessors via the
/// `ordinary_get_value` outcome ladder.
fn read_indexed_entry(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    target: &Value,
    name: &str,
) -> Result<Value, VmError> {
    let outcome = interp.ordinary_get_value(
        context,
        target.clone(),
        target.clone(),
        &VmPropertyKey::String(name),
        0,
    )?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync(context, &getter, target.clone(), SmallVec::new())
        }
    }
}

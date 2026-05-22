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
    abstract_ops, array, bigint, binary, collections, date, global_functions, json,
    json_to_vm_error, math, math_to_vm_error, native_function, object, object_statics,
    operand_decode::{const_operand, register_operand},
    read_register,
    string::JsString,
    symbol_dispatch, symbol_to_vm_error, temporal, temporal_to_vm_error, write_register,
};

/// Object-shaped values that need §7.1.1 `ToPrimitive` coercion before
/// numeric / string arithmetic. Mirrors the matches! variant list
/// repeated through static-call arg preambles: every callable object
/// shape plus `RegExp`.
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

/// Full property-bearing object-like family used by Object.* and
/// Reflect.* static dispatchers. Excludes Iterator / Generator /
/// Temporal / Intl which the existing dispatchers route through
/// separate intrinsic tables.
fn is_property_bearing_object(v: &Value) -> bool {
    v.is_object()
        || v.is_array()
        || v.is_proxy()
        || v.is_function()
        || v.is_closure()
        || v.is_native_function()
        || v.is_bound_function()
        || v.is_class_constructor()
        || v.is_regexp()
        || v.is_map()
        || v.is_set()
        || v.is_weak_map()
        || v.is_weak_set()
        || v.is_weak_ref()
        || v.is_finalization_registry()
        || v.is_array_buffer()
        || v.is_data_view()
        || v.is_typed_array()
        || v.is_promise()
}

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
                let result =
                    math::call(method, &coerced, &self.gc_heap).map_err(math_to_vm_error)?;
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
                    && !slot.is_string()
                {
                    let primitive = if crate::abstract_ops::is_primitive(slot) {
                        *slot
                    } else {
                        self.evaluate_to_primitive(
                            context,
                            slot,
                            crate::abstract_ops::ToPrimitiveHint::String,
                        )?
                    };
                    let s = if let Some(s) = primitive.as_string(&self.gc_heap) {
                        s
                    } else if primitive.is_symbol() {
                        return Err(VmError::TypeError {
                            message: "JSON.parse: cannot convert a Symbol to a string".to_string(),
                        });
                    } else {
                        crate::string::JsString::from_str(
                            &primitive.display_string(&self.gc_heap),
                            self.gc_heap_mut(),
                        )?
                    };
                    *slot = Value::string(s);
                }
                let result =
                    json::call(method, &coerced, &mut self.gc_heap).map_err(json_to_vm_error)?;
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
                    if let Some(arr) = slot.as_array() {
                        let parts: Vec<String> =
                            crate::array::with_elements(arr, &self.gc_heap, |elements| {
                                elements
                                    .iter()
                                    .map(|v| {
                                        if v.is_undefined() || v.is_null() || v.is_hole() {
                                            String::new()
                                        } else {
                                            v.display_string(&self.gc_heap)
                                        }
                                    })
                                    .collect()
                            });
                        let joined = parts.join(",");
                        *slot = Value::string(crate::string::JsString::from_str(
                            &joined,
                            self.gc_heap_mut(),
                        )?);
                    }
                }
                let result = bigint::dispatch::call(&mut self.gc_heap, method, &coerced)?;
                finish_static_call(frame, dst, result)
            }
            Op::ArrayBufferCall => unreachable!("ArrayBufferCall requires stack-rooted dispatch"),
            Op::DataViewCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method = method_id::DataViewMethod::from_u32(method_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let result = binary::dispatch::data_view_call(method, &args, &mut self.gc_heap)?;
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
                let result = global_functions::call(method, &args, &mut self.gc_heap)?;
                finish_static_call(frame, dst, result)
            }
            Op::SymbolCall => {
                let (dst, method_idx, args) = decode_static_call(frame, operands, 1, 2, 3)?;
                let method =
                    method_id::SymbolMethod::from_u32(method_idx).ok_or(VmError::InvalidOperand)?;
                // §20.4.1.1 / §20.4.2.4 — `Symbol(desc)` and
                // `Symbol.for(key)` flow through `ToString`, so object
                // operands need the observable `ToPrimitive("string")`
                // ladder here. §20.4.2.6 `Symbol.keyFor(sym)` instead
                // requires an actual Symbol argument and must not unwrap
                // Symbol wrapper objects before the type check.
                let coerced = match method {
                    method_id::SymbolMethod::Construct | method_id::SymbolMethod::For => {
                        self.symbol_coerce_first_arg(context, args)?
                    }
                    method_id::SymbolMethod::KeyFor => args,
                };
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
                let result = temporal::call_static(&mut self.gc_heap, class, method, &args)
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
        let coerced =
            self.coerce_to_primitive(context, first, abstract_ops::ToPrimitiveHint::String)?;
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
            if needs_to_primitive(&arg) {
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
                && !slot.is_string()
            {
                let primitive = if crate::abstract_ops::is_primitive(slot) {
                    *slot
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
                let s = if let Some(s) = primitive.as_string(&self.gc_heap) {
                    s
                } else if primitive.is_symbol() {
                    return Err(VmError::TypeError {
                        message: "JSON.parse: cannot convert a Symbol to a string".to_string(),
                    });
                } else {
                    crate::string::JsString::from_str(
                        &primitive.display_string(&self.gc_heap),
                        self.gc_heap_mut(),
                    )?
                };
                *slot = Value::string(s);
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
        let result = json::call_with_roots(method, &args, &mut self.gc_heap, &mut external_visit)
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
                *slot = Value::number(coerced);
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
            let is_date_instance = slot
                .as_object()
                .is_some_and(|o| crate::object::date_data(o, &self.gc_heap).is_some());
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
        let date_prototype = self
            .constructor_prototype_value("Date")
            .ok()
            .and_then(|v| v.as_object());
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
                // §20.1.2.10 / §20.1.2.13 step 1: `obj = ? ToObject(O)`.
                // Preserve the observable ordering before the
                // context-aware `ToPropertyKey(P)` path below: null /
                // undefined receivers must throw before key coercion
                // can invoke user code.
                if args.first().is_none_or(|v| v.is_nullish()) {
                    return Err(VmError::TypeError {
                        message: "Object static method called on null or undefined".to_string(),
                    });
                }
                // §20.1.2.10 / §20.1.2.13 step 2: `key = ? ToPropertyKey(P)`.
                // The ToPrimitive ladder may invoke user
                // `Symbol.toPrimitive` / `toString` / `valueOf`, so
                // we route through the context-aware path *only*
                // when the arg isn't already a String / Symbol /
                // Number / Boolean / Null / Undefined primitive
                // that the free coercion handles directly.
                let key_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let needs_coercion = !(key_arg.is_string()
                    || key_arg.is_number()
                    || key_arg.is_boolean()
                    || key_arg.is_null()
                    || key_arg.is_undefined()
                    || key_arg.is_symbol());
                if needs_coercion {
                    let coerced_key = self.evaluate_to_property_key(context, &key_arg)?;
                    let coerced_value = match &coerced_key {
                        crate::VmPropertyKey::Symbol(sym) => Value::symbol(*sym),
                        other => Value::string(crate::string::JsString::from_str(
                            other
                                .string_name()
                                .expect("non-symbol key has string spelling"),
                            self.gc_heap_mut(),
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
                        object_statics::call(method, &rewritten, &mut self.gc_heap)?
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
            object_statics::call(method, &args, &mut self.gc_heap)?
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
        let proto = args.first().cloned().unwrap_or(Value::undefined());
        let proto_value = if proto.is_object() || proto.is_iterator() {
            Some(proto)
        } else if proto.is_null() {
            None
        } else {
            return Err(VmError::TypeMismatch);
        };
        let obj = self.alloc_stack_rooted_object_with_value_roots(stack, &[&proto], args)?;
        if !object::set_prototype_value(obj, &mut self.gc_heap, proto_value) {
            return Err(VmError::TypeError {
                message: "Object.create failed".to_string(),
            });
        }
        if let Some(props_arg) = args.get(1)
            && !props_arg.is_undefined()
        {
            // §20.1.2.2 step 5 — enumerate own enumerable property
            // keys of `properties`, then `Get` each through the
            // accessor-aware path so user-defined `valueOf` /
            // `toString` / accessor getters fire per §6.2.5.5
            // ToPropertyDescriptor.
            let props_owned = *props_arg;
            let keys = own_enumerable_keys_for_define(self, context, &props_owned)?;
            for key in keys {
                let outcome =
                    self.ordinary_get_value(context, props_owned, props_owned, &key, 0)?;
                let desc_value = match outcome {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, props_owned, args)?
                    }
                };
                let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
                if !self.define_own_property_value(
                    context,
                    &Value::object(obj),
                    &key,
                    descriptor,
                )? {
                    return Err(VmError::TypeError {
                        message: format!(
                            "Cannot define property '{}'",
                            property_key_label(&key, &self.gc_heap)
                        ),
                    });
                }
            }
        }
        Ok(Value::object(obj))
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
        let target_value = args.first().cloned().unwrap_or(Value::undefined());
        // §20.1.2.3 step 1 — `Type(O)` must be Object.
        if !target_value.is_object_type() {
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
        let props_value = args.get(1).cloned().unwrap_or(Value::undefined());
        // §7.1.18 ToObject — `null` / `undefined` throw a
        // TypeError. Other primitives wrap into their boxed
        // form which has no own enumerable string-keyed
        // properties (except `String`, where the code units
        // surface as indexed slots).
        if props_value.is_nullish() {
            return Err(VmError::TypeError {
                message: "Object.defineProperties properties must be an object".to_string(),
            });
        }
        let keys = own_enumerable_keys_for_define(self, context, &props_value)?;
        for key in keys {
            // §6.2.5.5 step 4 — `Get(props, key)` is accessor-aware,
            // and step 5 — `ToPropertyDescriptor(descObj)` reads the
            // accessor / data fields off the resolved value. Thread
            // both through the interpreter so user getters fire and
            // any abrupt completion propagates.
            let outcome = self.ordinary_get_value(context, props_value, props_value, &key, 0)?;
            let desc_value = match outcome {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                    self.run_callable_sync(context, &getter, props_value, args)?
                }
            };
            let descriptor = self.evaluate_to_property_descriptor(context, &desc_value)?;
            let ok = self.define_own_property_value(context, &target_value, &key, descriptor)?;
            if !ok {
                return Err(VmError::TypeError {
                    message: format!(
                        "Object.defineProperties: cannot define '{}'",
                        property_key_label(&key, &self.gc_heap)
                    ),
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
        let target_input = args.first().cloned().unwrap_or(Value::undefined());
        // §20.1.2.1 step 2 — `ToObject(target)`. The spec returns the
        // resulting object as `target`, so Array / RegExp / Map / etc.
        // exotics pass straight through; only Null / Undefined throw
        // and only primitives go through the wrapper-boxing path.
        let target_value: Value = if is_property_bearing_object(&target_input) {
            target_input
        } else if target_input.is_nullish() {
            return Err(VmError::TypeError {
                message: "Object.assign called on null or undefined".to_string(),
            });
        } else {
            let arg_slice = args;
            self.box_sloppy_this_primitive_stack_rooted(stack, target_input, &[arg_slice])?
        };
        // Cache the object form when applicable so the existing
        // `ordinary_set_with_callable_setter` fast path keeps working
        // unchanged for plain-object targets. Exotic targets fall
        // through the value-level `[[Set]]` helper below.
        let target_object: Option<crate::object::JsObject> = target_value.as_object();
        for src in args.iter().skip(1) {
            if src.is_nullish() {
                continue;
            }
            if let Some(s) = src.as_string(&self.gc_heap) {
                // §22.1.4 — String exotic exposes its code units
                // as own indexed properties plus a `length`. The
                // latter is read-only on the wrapper, so we copy
                // only the indexed slots.
                let lossy = s.to_lossy_string(&self.gc_heap);
                for (idx, ch) in lossy.chars().enumerate() {
                    let mut buf = [0u16; 2];
                    let units = ch.encode_utf16(&mut buf);
                    let unit_string =
                        crate::string::JsString::from_utf16_units(units, self.gc_heap_mut())
                            .map_err(|_| VmError::TypeMismatch)?;
                    assign_set_string(
                        self,
                        context,
                        &target_value,
                        target_object,
                        &idx.to_string(),
                        Value::string(unit_string),
                    )?;
                }
            } else if assign_source_uses_own_property_keys(src) {
                assign_copy_source_keys(self, context, &target_value, target_object, src, args)?;
            } else {
                // Primitive Boolean / Number / Symbol / BigInt
                // wrappers have no enumerable own properties in
                // this VM slice, so ToObject(source) contributes
                // an empty key list.
                continue;
            }
        }
        Ok(target_value)
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
            M::ForInKeys => {
                let target = args.first().cloned().unwrap_or(Value::undefined());
                let keys = self.enumerable_for_in_string_keys_for_value(context, target)?;
                let mut names = Vec::with_capacity(keys.len());
                for key in keys {
                    names.push(stack_static_string_value(&key, self)?);
                }
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    names,
                    &[],
                    &[args],
                )?;
                Ok(Some(Value::array(array)))
            }
            M::Keys => {
                let owned: Vec<String> = match args.first() {
                    // §7.1.18 ToObject — Boolean / Number / Symbol /
                    // BigInt wrappers expose no own enumerable
                    // string keys; String wrappers carry indexed
                    // code-unit slots.
                    None => {
                        return Err(VmError::TypeError {
                            message: "Object.keys called on null or undefined".to_string(),
                        });
                    }
                    Some(target) if target.is_nullish() => {
                        return Err(VmError::TypeError {
                            message: "Object.keys called on null or undefined".to_string(),
                        });
                    }
                    Some(target)
                        if target.is_boolean()
                            || target.is_number()
                            || target.is_symbol()
                            || target.is_big_int() =>
                    {
                        Vec::new()
                    }
                    Some(target) if target.is_string() => {
                        let s = target.as_string(&self.gc_heap).expect("guarded");
                        let len = s.len() as usize;
                        (0..len).map(|i| i.to_string()).collect()
                    }
                    Some(target) if enumerable_own_names_uses_internal_methods(target) => {
                        self.enumerable_own_string_keys_for_value(context, *target, 0)?
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
                Ok(Some(Value::array(array)))
            }
            M::Values => {
                let values: Vec<Value> = match args.first() {
                    None => {
                        return Err(VmError::TypeError {
                            message: "Object.values called on null or undefined".to_string(),
                        });
                    }
                    Some(target) if target.is_nullish() => {
                        return Err(VmError::TypeError {
                            message: "Object.values called on null or undefined".to_string(),
                        });
                    }
                    Some(target)
                        if target.is_boolean()
                            || target.is_number()
                            || target.is_symbol()
                            || target.is_big_int() =>
                    {
                        Vec::new()
                    }
                    Some(target) if target.is_string() => {
                        let s = target.as_string(&self.gc_heap).expect("guarded");
                        let units = s.to_utf16_vec(&self.gc_heap);
                        units
                            .into_iter()
                            .map(|u| {
                                crate::string::JsString::from_utf16_units(&[u], self.gc_heap_mut())
                                    .map(Value::string)
                                    .unwrap_or(Value::undefined())
                            })
                            .collect()
                    }
                    Some(target) if enumerable_own_names_uses_internal_methods(target) => {
                        enumerable_own_string_entries(self, context, target, args)?
                            .into_iter()
                            .map(|(_, value)| value)
                            .collect()
                    }
                    _ => return Err(VmError::TypeMismatch),
                };
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    values,
                    &[],
                    &[args],
                )?;
                Ok(Some(Value::array(array)))
            }
            M::Entries => {
                let raw: Vec<(String, Value)> = match args.first() {
                    None => {
                        return Err(VmError::TypeError {
                            message: "Object.entries called on null or undefined".to_string(),
                        });
                    }
                    Some(target) if target.is_nullish() => {
                        return Err(VmError::TypeError {
                            message: "Object.entries called on null or undefined".to_string(),
                        });
                    }
                    Some(target)
                        if target.is_boolean()
                            || target.is_number()
                            || target.is_symbol()
                            || target.is_big_int() =>
                    {
                        Vec::new()
                    }
                    Some(target) if target.is_string() => {
                        let s = target.as_string(&self.gc_heap).expect("guarded");
                        let units = s.to_utf16_vec(&self.gc_heap);
                        units
                            .into_iter()
                            .enumerate()
                            .map(|(i, u)| {
                                let v = crate::string::JsString::from_utf16_units(
                                    &[u],
                                    self.gc_heap_mut(),
                                )
                                .map(Value::string)
                                .unwrap_or(Value::undefined());
                                (i.to_string(), v)
                            })
                            .collect()
                    }
                    Some(target) if enumerable_own_names_uses_internal_methods(target) => {
                        enumerable_own_string_entries(self, context, target, args)?
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
                    pairs.push(Value::array(pair));
                }
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    pairs,
                    &[],
                    &[args],
                )?;
                Ok(Some(Value::array(array)))
            }
            M::FromEntries => {
                // §20.1.2.7 Object.fromEntries(iterable). Spec iterator
                // protocol with IteratorClose on abrupt completion per
                // the AddEntriesFromIterable analogue used in step 4.
                // <https://tc39.es/ecma262/#sec-object.fromentries>
                let iter = args.first().cloned().unwrap_or(Value::undefined());
                if iter.is_nullish() {
                    return Err(VmError::TypeError {
                        message: "Object.fromEntries: iterable must not be null or undefined"
                            .to_string(),
                    });
                }
                // §20.1.2.7 step 2 — `obj = OrdinaryObjectCreate(%Object.prototype%)`.
                let object_proto = self.constructor_prototype_value("Object").ok();
                let result = self.alloc_stack_rooted_object_with_value_roots(stack, &[], args)?;
                if let Some(proto_obj) = object_proto.and_then(|v| v.as_object()) {
                    object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
                }

                let (iterator, next_method) = self.get_iterator_sync(context, &iter)?;

                loop {
                    let stepped = self.iterator_step_sync(context, &iterator, &next_method)?;
                    let Some(entry) = stepped else {
                        break;
                    };

                    if !entry.is_object_type() {
                        let _ = self.iterator_close_sync(context, &iterator);
                        return Err(VmError::TypeError {
                            message: "Object.fromEntries: iterator value is not an entry object"
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
                            object::set_symbol(result, &mut self.gc_heap, *sym, value);
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

                Ok(Some(Value::object(result)))
            }
            M::GetOwnPropertyDescriptor => {
                let key = Self::coerce_vm_property_key(args.get(1), &self.gc_heap)?;
                let Some(target) = args.first() else {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyDescriptor called on null or undefined"
                            .to_string(),
                    });
                };
                if target.is_nullish() {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyDescriptor called on null or undefined"
                            .to_string(),
                    });
                }
                let desc = if target.is_object() || target.is_string() {
                    self.ordinary_get_own_property_descriptor_value_stack_rooted(
                        context, stack, *target, &key, 0,
                    )?
                } else if let Some(class) = target.as_class_constructor() {
                    match &key {
                        VmPropertyKey::Symbol(sym) => object::get_own_symbol_descriptor(
                            class.statics(self.gc_heap()),
                            self.gc_heap(),
                            *sym,
                        ),
                        _ => object::get_own_descriptor(
                            class.statics(self.gc_heap()),
                            self.gc_heap(),
                            key.string_name()
                                .expect("non-symbol property key has string spelling"),
                        ),
                    }
                } else if let Some(native) = target.as_native_function() {
                    match &key {
                        VmPropertyKey::Symbol(sym) => {
                            native.own_symbol_property_descriptor(self.gc_heap(), *sym)
                        }
                        _ => {
                            let key = key
                                .string_name()
                                .expect("non-symbol property key has string spelling");
                            native.own_property_descriptor(self.gc_heap_mut(), key)?
                        }
                    }
                } else if let Some(t) = target.as_typed_array(&self.gc_heap) {
                    // §10.4.5.1 IntegerIndexedExoticObject
                    // [[GetOwnProperty]] — canonical-numeric-index
                    // strings produce a data descriptor for in-range
                    // elements; everything else returns no descriptor.
                    match &key {
                        VmPropertyKey::Symbol(sym) => t.expando(&self.gc_heap).and_then(|bag| {
                            crate::object::get_own_symbol_descriptor(bag, self.gc_heap(), *sym)
                        }),
                        _ => {
                            let k = key
                                .string_name()
                                .expect("non-symbol property key has string spelling");
                            if let Some(n) =
                                crate::property_dispatch::canonical_numeric_index_string(k)
                            {
                                if t.buffer(&self.gc_heap).is_detached(&self.gc_heap)
                                    || !n.is_finite()
                                    || n.fract() != 0.0
                                    || n < 0.0
                                    || (n as usize) >= t.length(&self.gc_heap)
                                {
                                    None
                                } else {
                                    Some(crate::object::PropertyDescriptor::data(
                                        t.get(&mut self.gc_heap, n as usize)
                                            .map_err(crate::oom_to_vm)?,
                                        true,
                                        true,
                                        true,
                                    ))
                                }
                            } else if let Some(bag) = t.expando(&self.gc_heap) {
                                crate::object::get_own_descriptor(bag, self.gc_heap(), k)
                            } else {
                                None
                            }
                        }
                    }
                } else if target.is_boolean()
                    || target.is_number()
                    || target.is_symbol()
                    || target.is_big_int()
                {
                    None
                } else {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyDescriptor target must be an object"
                            .to_string(),
                    });
                };
                match desc {
                    Some(desc) => {
                        let obj =
                            self.descriptor_to_object_stack_rooted(stack, &desc, &[], args)?;
                        Ok(Some(Value::object(obj)))
                    }
                    None => Ok(Some(Value::undefined())),
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
                if let Some(proto_obj) = object_proto.and_then(|v| v.as_object()) {
                    object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
                }
                let Some(target) = args.first() else {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyDescriptors called on null or undefined"
                            .to_string(),
                    });
                };
                if target.is_nullish() {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyDescriptors called on null or undefined"
                            .to_string(),
                    });
                }
                if target.is_boolean()
                    || target.is_number()
                    || target.is_symbol()
                    || target.is_big_int()
                {
                    // Empty result; primitive wrapper carries no
                    // own keys reachable through the foundation surface.
                } else if let Some(s) = target.as_string(&self.gc_heap) {
                    let units = s.to_utf16_vec(&self.gc_heap);
                    let result_root = Value::object(result);
                    for (i, u) in units.iter().enumerate() {
                        let key = i.to_string();
                        let unit =
                            crate::string::JsString::from_utf16_units(&[*u], self.gc_heap_mut())
                                .map_err(|_| VmError::TypeMismatch)?;
                        let desc = crate::object::PropertyDescriptor::data(
                            Value::string(unit),
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
                        self.set_property(result, &key, Value::object(desc_obj))?;
                    }
                    let length_desc = crate::object::PropertyDescriptor::data(
                        Value::number_f64(units.len() as f64),
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
                    self.set_property(result, "length", Value::object(length_obj))?;
                } else if own_property_descriptors_uses_internal_methods(target) {
                    // §20.1.2.10.1 step 3 — drive the spec
                    // ladder via `own_property_keys_value`, then
                    // read each descriptor through the target's
                    // `[[GetOwnProperty]]`.
                    let target_value = *target;
                    let result_root = Value::object(result);
                    let keys = self.own_property_keys_value(context, &target_value)?;
                    for key in keys {
                        let vm_key = if let Some(s) = key.as_string(&self.gc_heap) {
                            crate::VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
                        } else if let Some(sym) = key.as_symbol(&self.gc_heap) {
                            crate::VmPropertyKey::Symbol(sym)
                        } else {
                            continue;
                        };
                        let desc = self.ordinary_get_own_property_descriptor_value_stack_rooted(
                            context,
                            stack,
                            target_value,
                            &vm_key,
                            0,
                        )?;
                        let Some(desc) = desc else {
                            continue;
                        };
                        let desc_obj = self.descriptor_to_object_stack_rooted(
                            stack,
                            &desc,
                            &[&target_value, &result_root],
                            args,
                        )?;
                        if let Some(s) = key.as_string(&self.gc_heap) {
                            self.set_property(
                                result,
                                &s.to_lossy_string(&self.gc_heap),
                                Value::object(desc_obj),
                            )?;
                        } else if let Some(sym) = key.as_symbol(&self.gc_heap)
                            && !object::set_symbol(
                                result,
                                &mut self.gc_heap,
                                sym,
                                Value::object(desc_obj),
                            )
                        {
                            return Err(VmError::TypeMismatch);
                        }
                    }
                } else {
                    return Err(VmError::TypeMismatch);
                }
                Ok(Some(Value::object(result)))
            }
            M::GetOwnPropertyNames => {
                let Some(target) = args.first() else {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyNames called on null or undefined"
                            .to_string(),
                    });
                };
                if target.is_nullish() {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertyNames called on null or undefined"
                            .to_string(),
                    });
                }
                let owned: Vec<String> = if target.is_boolean()
                    || target.is_number()
                    || target.is_symbol()
                    || target.is_big_int()
                {
                    Vec::new()
                } else if let Some(s) = target.as_string(&self.gc_heap) {
                    let mut keys: Vec<String> = (0..s.len()).map(|idx| idx.to_string()).collect();
                    keys.push("length".to_string());
                    keys
                } else if own_property_names_uses_internal_methods(target) {
                    self.own_property_keys_value(context, target)?
                        .into_iter()
                        .filter_map(|key| key.as_string(&self.gc_heap).map(|s| s.to_lossy_string(&self.gc_heap)))
                        .collect()
                } else {
                    return Err(VmError::TypeMismatch);
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
                Ok(Some(Value::array(array)))
            }
            M::GetOwnPropertySymbols => {
                let Some(target) = args.first() else {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertySymbols called on null or undefined"
                            .to_string(),
                    });
                };
                if target.is_nullish() {
                    return Err(VmError::TypeError {
                        message: "Object.getOwnPropertySymbols called on null or undefined"
                            .to_string(),
                    });
                }
                let syms: Vec<Value> = if target.is_boolean()
                    || target.is_number()
                    || target.is_symbol()
                    || target.is_big_int()
                    || target.is_string()
                {
                    Vec::new()
                } else if own_property_names_uses_internal_methods(target) {
                    self.own_property_keys_value(context, target)?
                        .into_iter()
                        .filter(|key| key.is_symbol())
                        .collect()
                } else {
                    return Err(VmError::TypeMismatch);
                };
                let target_root = args.first().cloned().unwrap_or(Value::undefined());
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    syms,
                    &[&target_root],
                    &[args],
                )?;
                Ok(Some(Value::array(array)))
            }
            // §20.1.2.7 `Object.groupBy(items, callbackfn)` — groups
            // an iterable into a null-prototype object keyed by the
            // callback's return value.
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
        let items = args.first().cloned().unwrap_or(Value::undefined());
        let callback = args.get(1).cloned().unwrap_or(Value::undefined());
        if items.is_nullish() {
            return Err(VmError::TypeError {
                message: "Object.groupBy: items must be iterable".to_string(),
            });
        }
        if !self.is_callable_runtime(&callback) {
            return Err(VmError::TypeError {
                message: "Object.groupBy: callback must be a function".to_string(),
            });
        }
        let items_snapshot = self.iterator_to_list_sync(context, &items)?;
        let result =
            self.alloc_stack_rooted_object_with_extra_roots(stack, &[&items, &callback])?;
        object::set_prototype(result, &mut self.gc_heap, None);

        for (idx, item) in items_snapshot.iter().enumerate() {
            let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
            cb_args.push(*item);
            cb_args.push(Value::number(crate::number::NumberValue::from_f64(
                idx as f64,
            )));
            let key = self.run_callable_sync(context, &callback, Value::undefined(), cb_args)?;
            let key_pk = self.to_property_key_sync(context, key)?;
            let key_str = match key_pk {
                crate::VmPropertyKey::Symbol(sym) => {
                    let existing = crate::object::get_symbol(result, &self.gc_heap, sym);
                    let group = match existing {
                        Some(v) if v.is_array() => v.as_array().expect("guarded"),
                        _ => {
                            let arr = self.alloc_stack_rooted_array_from_values_with_root_slices(
                                stack,
                                Vec::new(),
                                &[&Value::object(result), item],
                                &[args],
                            )?;
                            crate::object::set_symbol(
                                result,
                                &mut self.gc_heap,
                                sym,
                                Value::array(arr),
                            );
                            arr
                        }
                    };
                    let value_root = *item;
                    let arr_value = Value::array(group);
                    let res_root = Value::object(result);
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
                        *item,
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
                Some(v) if v.is_array() => v.as_array().expect("guarded"),
                _ => {
                    let arr = self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        Vec::new(),
                        &[&Value::object(result), item],
                        &[args],
                    )?;
                    self.set_property(result, &key_str, Value::array(arr))?;
                    arr
                }
            };
            let value_root = *item;
            let arr_value = Value::array(group);
            let res_root = Value::object(result);
            let roots = [&value_root, &arr_value, &res_root];
            let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for v in &roots {
                    v.trace_value_slots(visitor);
                }
            };
            crate::array::push_with_roots(group, &mut self.gc_heap, *item, &mut external_visit)?;
        }
        Ok(Value::object(result))
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
        if let Some(proto_obj) = object_proto.and_then(|v| v.as_object()) {
            object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
        }
        match &desc.kind {
            object::DescriptorKind::Data { value } => {
                self.set_property(result, "value", *value)?;
                self.set_property(result, "writable", Value::boolean(desc.writable()))?;
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                self.set_property(result, "get", (*getter).unwrap_or(Value::undefined()))?;
                self.set_property(result, "set", (*setter).unwrap_or(Value::undefined()))?;
            }
        }
        self.set_property(result, "enumerable", Value::boolean(desc.enumerable()))?;
        self.set_property(result, "configurable", Value::boolean(desc.configurable()))?;
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
                let handler = coerce_proxy_target(args.get(1))?;
                let proxy = crate::proxy::JsProxy::new(&mut self.gc_heap, target, handler)
                    .map_err(crate::oom_to_vm)?;
                Ok(Value::proxy(proxy))
            }
            M::Revocable => {
                let target = coerce_proxy_target(args.first())?;
                let handler = coerce_proxy_target(args.get(1))?;
                let proxy = crate::proxy::JsProxy::new(&mut self.gc_heap, target, handler)
                    .map_err(crate::oom_to_vm)?;
                let proxy_value = Value::proxy(proxy);
                let target_root = target;
                let handler_root = handler;
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
                    smallvec::smallvec![proxy_value],
                    &mut external_visit,
                    move |ctx, _, captures| {
                        if let Some(proxy) = captures.first().and_then(|v| v.as_proxy()) {
                            proxy.revoke(ctx.heap_mut());
                        }
                        Ok(Value::undefined())
                    },
                )?;
                let obj = self.alloc_stack_rooted_object_with_value_roots(
                    stack,
                    &[&proxy_value, &revoke],
                    args,
                )?;
                self.set_property(obj, "proxy", proxy_value)?;
                self.set_property(obj, "revoke", revoke)?;
                Ok(Value::object(obj))
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
                let value = args.first().cloned().unwrap_or(Value::undefined());
                let state = if let Some(rc) = value.as_iterator() {
                    return Ok(Value::iterator(rc));
                } else if let Some(handle) = value.as_generator() {
                    IteratorState::Generator { handle }
                } else if let Some(arr) = value.as_array() {
                    IteratorState::Array {
                        array: arr,
                        index: 0,
                        origin: crate::BuiltinIteratorOrigin::Array,
                    }
                } else if let Some(s) = value.as_string(&self.gc_heap) {
                    IteratorState::String {
                        string: s,
                        index: 0,
                    }
                } else if let Some(s) = value.as_set() {
                    let value_root = Value::set(s);
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
                } else if let Some(m) = value.as_map() {
                    let value_root = Value::map(m);
                    let mut entries: Vec<Value> = Vec::new();
                    for (k, v) in collections::map_entries(m, self.gc_heap()) {
                        let pair = self.alloc_stack_rooted_array_from_values_with_root_slices(
                            stack,
                            [k, v],
                            &[&value_root],
                            &[args, entries.as_slice()],
                        )?;
                        entries.push(Value::array(pair));
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
                } else if value.is_object() {
                    IteratorState::User { iterator: value }
                } else {
                    return Err(VmError::TypeMismatch);
                };
                let iter = self.alloc_stack_rooted_iterator_state(stack, state, &[], &[args])?;
                Ok(Value::iterator(iter))
            }
        }
    }
}

/// §6.2.5.5 + §20.1.2.3 — enumerate the own enumerable property keys
/// of a `properties` argument supplied to `Object.defineProperties`
/// / `Object.create`. Includes accessor-shaped own keys so the
/// caller can `Get` the descriptor value through the spec's
/// accessor-aware path.
fn own_enumerable_keys_for_define(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    props: &Value,
) -> Result<Vec<VmPropertyKey<'static>>, VmError> {
    if props.is_nullish() {
        return Err(VmError::TypeMismatch);
    }
    if props.is_object()
        || props.is_class_constructor()
        || props.is_function()
        || props.is_closure()
        || props.is_native_function()
        || props.is_bound_function()
        || props.is_regexp()
        || props.is_proxy()
    {
        let keys = interp.own_property_keys_value(context, props)?;
        let mut out = Vec::new();
        for key in keys {
            let vm_key = value_to_static_property_key(&key, interp.gc_heap())?;
            let desc = interp.get_own_property_descriptor_for_value(context, *props, Some(&key))?;
            if desc.is_some_and(|desc| desc.enumerable()) {
                out.push(vm_key);
            }
        }
        return Ok(out);
    }
    if let Some(arr) = props.as_array() {
        // §22.1.3.3 EnumerableOwnPropertyNames for Array.
        let mut out: Vec<String> = Vec::new();
        let dense_len = array::with_elements(arr, interp.gc_heap(), |els| els.len());
        for idx in 0..dense_len {
            out.push(idx.to_string());
        }
        let (named, accessor_keys): (Vec<String>, Vec<String>) =
            interp.gc_heap().read_payload(arr, |body| {
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
        return Ok(out.into_iter().map(VmPropertyKey::OwnedString).collect());
    }
    if let Some(s) = props.as_string(interp.gc_heap()) {
        let units = s.to_utf16_vec(interp.gc_heap());
        return Ok((0..units.len())
            .map(|i| VmPropertyKey::OwnedString(i.to_string()))
            .collect());
    }
    Ok(Vec::new())
}

fn value_to_static_property_key(
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<VmPropertyKey<'static>, VmError> {
    if let Some(s) = value.as_string(heap) {
        return Ok(VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
    }
    if let Some(sym) = value.as_symbol(heap) {
        return Ok(VmPropertyKey::Symbol(sym));
    }
    Err(VmError::TypeError {
        message: "property key must be a string or symbol".to_string(),
    })
}

fn property_key_label(key: &VmPropertyKey<'_>, heap: &otter_gc::GcHeap) -> String {
    match key {
        VmPropertyKey::Symbol(sym) => sym.descriptive_string(heap),
        _ => key
            .string_name()
            .expect("non-symbol key has string spelling")
            .to_string(),
    }
}

fn coerce_proxy_target(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v) if v.is_object_type() || abstract_ops::is_callable(v) => Ok(*v),
        _ => Err(VmError::TypeMismatch),
    }
}

fn stack_static_string_value(s: &str, interp: &mut Interpreter) -> Result<Value, VmError> {
    Ok(Value::string(
        JsString::from_str(s, interp.gc_heap_mut()).map_err(|_| VmError::TypeMismatch)?,
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
        args.push(*read_register(frame, r)?);
    }
    Ok(args)
}

fn finish_static_call(frame: &mut Frame, dst: u16, result: Value) -> Result<(), VmError> {
    write_register(frame, dst, result)?;
    frame.pc += 1;
    Ok(())
}

fn enumerable_own_names_uses_internal_methods(target: &Value) -> bool {
    is_property_bearing_object(target)
}

fn own_property_names_uses_internal_methods(target: &Value) -> bool {
    target.is_object()
        || target.is_array()
        || target.is_proxy()
        || target.is_function()
        || target.is_closure()
        || target.is_native_function()
        || target.is_bound_function()
        || target.is_class_constructor()
}

fn own_property_descriptors_uses_internal_methods(target: &Value) -> bool {
    target.is_object()
        || target.is_array()
        || target.is_proxy()
        || target.is_function()
        || target.is_closure()
        || target.is_native_function()
        || target.is_bound_function()
        || target.is_class_constructor()
        || target.is_regexp()
}

fn enumerable_own_string_entries(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    target: &Value,
    args: &[Value],
) -> Result<Vec<(String, Value)>, VmError> {
    let keys = interp.own_property_keys_value(context, target)?;
    let mut entries = Vec::new();
    for key_value in &keys {
        let Some(name) = key_value.as_string(interp.gc_heap()) else {
            continue;
        };
        let key_name = name.to_lossy_string(interp.gc_heap());
        let key = VmPropertyKey::OwnedString(key_name.clone());
        let desc = interp.ordinary_get_own_property_descriptor_value_runtime_rooted(
            context,
            *target,
            &key,
            0,
            &[target],
            &[args, keys.as_slice()],
        )?;
        let Some(desc) = desc else {
            continue;
        };
        if !desc.enumerable() {
            continue;
        }
        let value = match interp.ordinary_get_value(context, *target, *target, &key, 0)? {
            crate::VmGetOutcome::Value(value) => value,
            crate::VmGetOutcome::InvokeGetter { getter } => {
                interp.run_callable_sync(context, &getter, *target, SmallVec::new())?
            }
        };
        entries.push((key_name, value));
    }
    Ok(entries)
}

fn assign_source_uses_own_property_keys(source: &Value) -> bool {
    is_property_bearing_object(source)
}

fn assign_copy_source_keys(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    target_value: &Value,
    target_object: Option<crate::object::JsObject>,
    source: &Value,
    args: &[Value],
) -> Result<(), VmError> {
    let keys = interp.own_property_keys_value(context, source)?;
    for key_value in &keys {
        let key = if let Some(s) = key_value.as_string(interp.gc_heap()) {
            VmPropertyKey::OwnedString(s.to_lossy_string(interp.gc_heap()))
        } else if let Some(sym) = key_value.as_symbol(interp.gc_heap()) {
            VmPropertyKey::Symbol(sym)
        } else {
            return Err(VmError::TypeError {
                message: "Object.assign source ownKeys returned non-property key".to_string(),
            });
        };
        let desc = interp.ordinary_get_own_property_descriptor_value_runtime_rooted(
            context,
            *source,
            &key,
            0,
            &[target_value, source],
            &[args, keys.as_slice()],
        )?;
        let Some(desc) = desc else {
            continue;
        };
        if !desc.enumerable() {
            continue;
        }
        let value = match interp.ordinary_get_value(context, *source, *source, &key, 0)? {
            crate::VmGetOutcome::Value(value) => value,
            crate::VmGetOutcome::InvokeGetter { getter } => {
                interp.run_callable_sync(context, &getter, *source, SmallVec::new())?
            }
        };
        match &key {
            VmPropertyKey::Symbol(sym) => {
                assign_set_symbol(interp, context, target_value, target_object, *sym, value)?;
            }
            _ => {
                assign_set_string(
                    interp,
                    context,
                    target_value,
                    target_object,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                    value,
                )?;
            }
        }
    }
    Ok(())
}

/// `Object.assign` value-level write helper. Routes string-keyed
/// writes through the matching exotic [[Set]]:
///
/// - Plain objects use `ordinary_set_with_callable_setter` for
///   accessor dispatch and the strict-mode TypeError surface.
/// - Array exotics route writes through ArraySetLength (`length`),
///   the dense element store (canonical-numeric-index strings), or
///   the named-property side table (everything else).
/// - Every other exotic (RegExp, Promise, …) falls back to the
///   ordinary path against its lazy expando bag if installed.
///
/// `strict` is implicit-`true` per §20.1.2.1 step 4.c.iii.2.b.
fn assign_set_string(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    target_value: &Value,
    target_object: Option<crate::object::JsObject>,
    key: &str,
    value: Value,
) -> Result<(), VmError> {
    if let Some(obj) = target_object {
        if let Some(desc) =
            interp.string_object_exotic_descriptor(obj, &VmPropertyKey::String(key))?
            && !desc.writable()
        {
            return Err(VmError::TypeError {
                message: format!("Cannot assign to read-only property '{key}'"),
            });
        }
        return interp.ordinary_set_with_callable_setter(context, obj, key, value, true);
    }
    if let Some(arr) = target_value.as_array() {
        if key == "length" {
            let number_len = crate::coerce::to_number_or_throw(interp, context, &value)?;
            let new_len = crate::number::bitwise::to_uint32(number_len);
            if (new_len as f64) != number_len.as_f64() {
                return Err(VmError::RangeError {
                    message: "Invalid array length".to_string(),
                });
            }
            crate::array::set_length(arr, &mut interp.gc_heap, new_len as usize)
                .map_err(|_| VmError::TypeMismatch)?;
            return Ok(());
        }
        if let Some(idx) = crate::object::array_index_property_name(key) {
            crate::array::set(arr, &mut interp.gc_heap, idx as usize, value)
                .map_err(|_| VmError::TypeMismatch)?;
            return Ok(());
        }
        crate::array::set_named_property(arr, &mut interp.gc_heap, key, value)
            .map_err(|_| VmError::TypeMismatch)?;
        return Ok(());
    }
    // For other exotic value kinds, surface failure rather than
    // silently dropping the assign step.
    Err(VmError::TypeError {
        message: format!(
            "Object.assign: cannot set '{key}' on {}",
            crate::value_kind_name(target_value)
        ),
    })
}

fn assign_set_symbol(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    target_value: &Value,
    target_object: Option<crate::object::JsObject>,
    sym: crate::symbol::JsSymbol,
    value: Value,
) -> Result<(), VmError> {
    if let Some(obj) = target_object {
        return interp.ordinary_set_symbol_with_callable_setter(context, obj, sym, value, true);
    }
    if let Some(arr) = target_value.as_array() {
        crate::array::set_symbol_property(arr, &mut interp.gc_heap, sym, value);
        return Ok(());
    }
    Err(VmError::TypeError {
        message: format!(
            "Object.assign: cannot set symbol on {}",
            crate::value_kind_name(target_value)
        ),
    })
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
    let outcome =
        interp.ordinary_get_value(context, *target, *target, &VmPropertyKey::String(name), 0)?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync(context, &getter, *target, SmallVec::new())
        }
    }
}

//! ECMAScript primitive conversion helpers.
//!
//! Keep VM conversion semantics in one place instead of scattering local
//! `ToNumber` / `ToString` fragments through opcode dispatch, builtins, and
//! arithmetic helpers.
//!
//! # Contents
//! - Primitive `ToNumber` for opcode and builtin tails.
//! - Primitive `ToString` helpers for string concatenation and `String(...)`.
//! - `ToPrimitive` / `[Symbol.toPrimitive]` dispatch helpers.
//! - Register entrypoints used by dense VM dispatch.
//!
//! # Invariants
//! - Object `ToPrimitive` dispatch is driven here before primitive tails.
//! - `ToString(Symbol)` is an error, while bare `String(symbol)` returns the
//!   symbol descriptive form per §22.1.1.1.
//!
//! # See also
//! - [`crate::abstract_ops`]
//! - [`crate::number`]
//! - [`crate::string::dispatch`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, JsObject, JsString, NumberValue, PendingToPrimitive,
    ToPrimitiveStage, Value, VmError, VmGetOutcome, VmPropertyKey, abstract_ops, number, object,
    object_prototype_intercept,
    operand_decode::{const_operand, register_operand},
    ordinary_method_for, read_register, symbol, write_register,
};

pub(crate) fn to_number_primitive(
    value: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> Result<NumberValue, VmError> {
    if let Some(n) = value.as_number() {
        return Ok(n);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(NumberValue::Smi(if b { 1 } else { 0 }));
    }
    if value.is_null() {
        return Ok(NumberValue::Smi(0));
    }
    if value.is_big_int() || value.is_symbol() {
        return Err(VmError::TypeMismatch);
    }
    if let Some(s) = value.as_string() {
        return Ok(number::to_number_from_string(&s.to_lossy_string(gc_heap)));
    }
    Ok(NumberValue::Double(f64::NAN))
}

pub(crate) fn to_string_primitive(
    value: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> Result<String, VmError> {
    if let Some(s) = value.as_string() {
        return Ok(s.to_lossy_string(gc_heap));
    }
    if let Some(n) = value.as_number() {
        return Ok(n.to_display_string());
    }
    if let Some(b) = value.as_big_int() {
        return Ok(b.to_decimal_string(gc_heap));
    }
    if let Some(b) = value.as_boolean() {
        return Ok(if b { "true" } else { "false" }.to_string());
    }
    if value.is_null() {
        return Ok("null".to_string());
    }
    if value.is_undefined() || value.is_hole() {
        return Ok("undefined".to_string());
    }
    Err(VmError::TypeMismatch)
}

pub(crate) fn to_js_string_primitive(
    value: &Value,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<JsString, VmError> {
    if let Some(s) = value.as_string() {
        return Ok(*s);
    }
    if let Some(n) = value.as_number() {
        return number::ecma::number_to_string(n.as_f64(), gc_heap)
            .map_err(|_| VmError::TypeMismatch);
    }
    // BigInt arm cannot fire here without a GcHeap; the caller is
    // expected to coerce BigInt through
    // `to_string_primitive(..., heap)` upstream of this helper.
    Err(VmError::TypeMismatch)
}

pub(crate) fn string_constructor_js_string(
    value: Option<&Value>,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<JsString, VmError> {
    let Some(value) = value else {
        return JsString::empty(gc_heap).map_err(|_| VmError::TypeMismatch);
    };
    if let Some(s) = value.as_symbol() {
        return JsString::from_str(&s.descriptive_string(gc_heap), gc_heap)
            .map_err(|_| VmError::TypeMismatch);
    }
    match to_js_string_primitive(value, gc_heap) {
        Ok(v) => Ok(v),
        Err(VmError::TypeMismatch) => {
            let rendered = to_string_primitive(value, gc_heap)
                .unwrap_or_else(|_| value.display_string(gc_heap));
            JsString::from_str(&rendered, gc_heap).map_err(|_| VmError::TypeMismatch)
        }
        Err(err) => Err(err),
    }
}

impl Interpreter {
    pub(crate) fn run_to_number_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = to_number_primitive(read_register(frame, src)?, &self.gc_heap)?;
        write_register(frame, dst, Value::number(value))?;
        frame.pc += 1;
        Ok(())
    }

    /// Pre-dispatch hook for [`Op::ToNumber`] that consults
    /// `[Symbol.toPrimitive]` on object operands.
    ///
    /// # Algorithm
    /// 1. If the source register holds a [`Value::Object`] whose
    ///    `[Symbol.toPrimitive]` symbol-keyed property is callable,
    ///    advance pc past the `ToNumber` instruction and invoke
    ///    the hook with `this = obj` and `args = ["number"]`.
    /// 2. The hook's return value lands in the `ToNumber`'s
    ///    destination register on frame pop. The foundation does
    ///    not re-coerce; tests targeting this slice return a
    ///    Number directly.
    /// 3. Return `Ok(Some(()))` when the hook fired (caller
    ///    `continue`s the dispatch loop), `Ok(None)` otherwise so
    ///    the in-frame fast path runs.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    pub(crate) fn try_to_primitive_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<Option<()>, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let recv = *read_register(&stack[top_idx], src)?;
        let Some(obj) = recv.as_object() else {
            return Ok(None);
        };
        let to_primitive_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
        let Some(callee) = crate::object::get_symbol(obj, &self.gc_heap, &to_primitive_sym) else {
            return Ok(None);
        };
        if !self.is_callable_runtime(&callee) {
            return Ok(None);
        }
        let hint = JsString::from_str("number", self.gc_heap_mut())?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(Value::string(hint));
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, context, &callee, recv, args, dst)?;
        Ok(Some(()))
    }

    /// Drive one tick of the [`Op::ToPrimitive`] ladder.
    ///
    /// # Algorithm
    /// Implements ECMA-262 §7.1.1 `ToPrimitive` plus §7.1.1.1
    /// `OrdinaryToPrimitive`:
    ///
    /// 1. **Already primitive** — write `src` to `dst`, advance pc.
    /// 2. **Resume from prior stage** — read the result the called
    ///    function wrote into `dst`. If primitive, advance pc and
    ///    clear the parked state. Otherwise advance the stage.
    /// 3. **`SymbolToPrim`** — look up `[Symbol.toPrimitive]`. If
    ///    callable, push a frame with `[hint]` and `this = obj`,
    ///    park state with `stage = OrdinaryFirst` (set so a
    ///    non-primitive result falls through to the ordinary
    ///    chain). Otherwise fall through to `OrdinaryFirst`
    ///    immediately.
    /// 4. **`OrdinaryFirst` / `OrdinarySecond`** — pick `valueOf`
    ///    (default / number) or `toString` (string) for the first
    ///    slot; the other method for the second. If callable, push
    ///    a frame with no arguments. If neither slot returns a
    ///    primitive, raise `VmError::TypeMismatch` (task 25 will
    ///    upgrade this to a real `TypeError` Error object).
    ///
    /// Returns `Ok(true)` when the ladder pushed a frame (the
    /// dispatch loop must `continue` to the new top frame),
    /// `Ok(false)` when the ladder finished synchronously and pc
    /// advanced.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    pub(crate) fn drive_to_primitive(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let hint_idx = const_operand(operands.get(2))?;
        let hint_token = context
            .string_constant_str(hint_idx)
            .ok_or(VmError::InvalidOperand)?;
        let hint =
            abstract_ops::ToPrimitiveHint::from_token(hint_token).ok_or(VmError::InvalidOperand)?;

        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path — only when the parked state matches this
        //    instruction. Read the result the called function wrote
        //    to `dst`; if primitive, finish.
        let resume = stack[top_idx]
            .pending_to_primitive
            .as_ref()
            .filter(|s| s.pc == pc && s.dst == dst)
            .cloned();
        if let Some(state) = resume {
            let produced = *read_register(&stack[top_idx], dst)?;
            if abstract_ops::is_primitive(&produced) {
                stack[top_idx].pending_to_primitive = None;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                return Ok(false);
            }
            // Non-primitive — advance to the next stage.
            return self.drive_to_primitive_stage(
                stack,
                context,
                dst,
                state.obj,
                hint,
                state.stage,
            );
        }

        // 2. Fresh entry — primitive fast path.
        let recv = *read_register(&stack[top_idx], src)?;
        if abstract_ops::is_primitive(&recv) {
            write_register(&mut stack[top_idx], dst, recv)?;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(false);
        }

        // 3. Object operand — start the ladder at SymbolToPrim.
        self.drive_to_primitive_stage(
            stack,
            context,
            dst,
            recv,
            hint,
            ToPrimitiveStage::SymbolToPrim,
        )
    }

    /// If `value` (the data-path result of a callable property
    /// lookup) is `Undefined`, probe `%Function.prototype%` for an
    /// inherited accessor descriptor under `key`. Returns
    /// `Some(VmGetOutcome::InvokeGetter)` only when the chain hosts
    /// a callable getter (e.g. the §10.2.4
    /// `AddRestrictedFunctionProperties` poison pills for `caller`
    /// and `arguments`). All other outcomes — data hit, accessor
    /// without getter, no chain entry — return `None` so the caller
    /// keeps the original `value`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryget>
    /// - <https://tc39.es/ecma262/#sec-addrestrictedfunctionproperties>
    pub(crate) fn callable_realm_prototype_accessor_outcome(
        &self,
        value: &Value,
        key: &VmPropertyKey,
    ) -> Result<Option<VmGetOutcome>, VmError> {
        if !value.is_undefined() {
            return Ok(None);
        }
        let Ok(proto) = self.function_prototype_object() else {
            return Ok(None);
        };
        let lookup = match key {
            VmPropertyKey::Symbol(sym) => object::lookup_symbol(proto, &self.gc_heap, sym),
            _ => object::lookup(
                proto,
                &self.gc_heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        };
        if let object::PropertyLookup::Accessor {
            getter: Some(getter),
            ..
        } = lookup
            && abstract_ops::is_callable(&getter)
        {
            return Ok(Some(VmGetOutcome::InvokeGetter { getter }));
        }
        Ok(None)
    }

    /// Resolve the realm prototype Object that `[[Get]]` walks for a
    /// non-`Value::Object` heap-shape value. Mirrors §7.1.1 step 1's
    /// requirement that any object — Function, Array, Map, etc. —
    /// participate in `ToPrimitive` lookup through its own prototype
    /// chain. `Value::Object` is handled directly by callers; this
    /// helper only resolves the exotic shapes whose prototype lives
    /// on the realm's intrinsic constructor object.
    ///
    /// Returns `None` when:
    /// - the value is a primitive (callers short-circuit before
    ///   reaching this helper),
    /// - the value is `Value::Object` (already an ordinary object),
    /// - the value is `Value::Proxy` (proxy lookups must invoke the
    ///   `get` trap; §7.1.1 callers fall back to the trap dispatcher
    ///   rather than direct proto walking), or
    /// - the realm has no installed constructor for that shape.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinaryget>
    pub(crate) fn intrinsic_prototype_object_for(&mut self, value: &Value) -> Option<JsObject> {
        if value.is_function()
            || value.is_closure()
            || value.is_native_function()
            || value.is_bound_function()
            || value.is_class_constructor()
        {
            return self.function_prototype_object().ok();
        }
        if value.is_object() || value.is_proxy() {
            return None;
        }
        let constructor_name = if let Some(arr) = value.as_array() {
            if let Some(proto) =
                crate::array::prototype_override(arr, &self.gc_heap).and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "Array"
        } else if let Some(regexp) = value.as_regexp() {
            if let Some(proto) = regexp
                .prototype_override(&self.gc_heap)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "RegExp"
        } else if let Some(map) = value.as_map() {
            if let Some(proto) = crate::collections::map_prototype_override(map, &self.gc_heap)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "Map"
        } else if let Some(set) = value.as_set() {
            if let Some(proto) = crate::collections::set_prototype_override(set, &self.gc_heap)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "Set"
        } else if let Some(map) = value.as_weak_map() {
            if let Some(proto) = crate::collections::weak_map_prototype_override(map, &self.gc_heap)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "WeakMap"
        } else if let Some(set) = value.as_weak_set() {
            if let Some(proto) = crate::collections::weak_set_prototype_override(set, &self.gc_heap)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "WeakSet"
        } else if let Some(weak_ref) = value.as_weak_ref() {
            if let Some(proto) =
                crate::weak_refs::weak_ref_prototype_override(weak_ref, &self.gc_heap)
                    .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "WeakRef"
        } else if let Some(registry) = value.as_finalization_registry() {
            if let Some(proto) =
                crate::weak_refs::finalization_registry_prototype_override(registry, &self.gc_heap)
                    .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "FinalizationRegistry"
        } else if let Some(promise) = value.as_promise() {
            if let Some(proto) = promise
                .prototype_override(&self.gc_heap)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "Promise"
        } else if let Some(b) = value.as_array_buffer() {
            if let Some(proto) = self
                .non_gc_exotic_prototype_override(value)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            if b.is_shared() {
                "SharedArrayBuffer"
            } else {
                "ArrayBuffer"
            }
        } else if value.as_data_view().is_some() {
            if let Some(proto) = self
                .non_gc_exotic_prototype_override(value)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            "DataView"
        } else if let Some(t) = value.as_typed_array() {
            if let Some(proto) = self
                .non_gc_exotic_prototype_override(value)
                .and_then(|v| v.as_object())
            {
                return Some(proto);
            }
            t.kind().name()
        } else if let Some(handle) = value.as_iterator() {
            // §22.1.5 / §23.1.5 / §24.1.5 / §24.2.5 — per-kind
            // iterator prototypes inherit from `%IteratorPrototype%`
            // and override `@@toStringTag`. Route through the cached
            // realm prototypes before falling back to the generic
            // `%IteratorPrototype%`.
            let origin = self.gc_heap.read_payload(handle, |s| s.builtin_origin());
            if let Some(origin) = origin
                && let Some(proto) = self.builtin_iterator_prototype_for(origin)
            {
                return Some(proto);
            }
            "Iterator"
        } else if value.is_generator() {
            // §27.5 generators expose `%GeneratorPrototype%`'s
            // intrinsic ancestor, which is `%IteratorPrototype%`.
            // The Otter foundation collapses both into the same
            // realm prototype today.
            "Iterator"
        } else if value.is_symbol() {
            "Symbol"
        } else if value.is_string() {
            "String"
        } else if value.is_number() {
            "Number"
        } else if value.is_boolean() {
            "Boolean"
        } else if value.is_big_int() {
            "BigInt"
        } else {
            return None;
        };
        self.constructor_prototype_value(constructor_name)
            .ok()?
            .as_object()
    }

    /// §7.1.1.1 step 4.a — `func = ? Get(O, name)`.
    ///
    /// Accessor getters are invoked before callability is tested, and
    /// a present non-callable result is observable as "skip this
    /// method" rather than as a missing property.
    fn get_string_for_to_primitive(
        &mut self,
        context: &ExecutionContext,
        base: &Value,
        name: &str,
    ) -> Result<Option<Value>, VmError> {
        match self.ordinary_get_value(context, *base, *base, &VmPropertyKey::String(name), 0)? {
            VmGetOutcome::Value(value) if value.is_undefined() => Ok(None),
            VmGetOutcome::Value(value) => Ok(Some(value)),
            VmGetOutcome::InvokeGetter { getter } => {
                let value = self.run_callable_sync(context, &getter, *base, SmallVec::new())?;
                Ok(Some(value))
            }
        }
    }

    /// §7.1.1 step 2.a — `exoticToPrim = ? GetMethod(input, @@toPrimitive)`.
    fn get_symbol_for_to_primitive(
        &mut self,
        context: &ExecutionContext,
        base: &Value,
        sym: symbol::JsSymbol,
    ) -> Result<Option<Value>, VmError> {
        match self.ordinary_get_value(context, *base, *base, &VmPropertyKey::Symbol(sym), 0)? {
            VmGetOutcome::Value(value) if value.is_undefined() => Ok(None),
            VmGetOutcome::Value(value) => Ok(Some(value)),
            VmGetOutcome::InvokeGetter { getter } => {
                let value = self.run_callable_sync(context, &getter, *base, SmallVec::new())?;
                Ok(Some(value))
            }
        }
    }

    /// Run a single stage of the §7.1.1 / §7.1.1.1 ladder, falling
    /// through synchronously when the chosen method is missing or
    /// non-callable until we either push a frame, throw, or write
    /// a primitive result.
    fn drive_to_primitive_stage(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        dst: u16,
        obj: Value,
        hint: abstract_ops::ToPrimitiveHint,
        mut stage: ToPrimitiveStage,
    ) -> Result<bool, VmError> {
        loop {
            match stage {
                ToPrimitiveStage::SymbolToPrim => {
                    let to_prim_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
                    match self.get_symbol_for_to_primitive(context, &obj, to_prim_sym)? {
                        Some(v) if v.is_nullish() => {
                            stage = ToPrimitiveStage::OrdinaryFirst;
                        }
                        None => {
                            stage = ToPrimitiveStage::OrdinaryFirst;
                        }
                        Some(callee) if self.is_callable_runtime(&callee) => {
                            let hint_str = JsString::from_str(hint.as_token(), &mut self.gc_heap)?;
                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                            args.push(Value::string(hint_str));
                            // §7.1.1 step 5.d. The resume guard
                            // upstream validates the result is a
                            // primitive — if not, that branch lands
                            // on `OrdinaryFirst` which is **wrong**
                            // per spec (a non-primitive return from
                            // `[Symbol.toPrimitive]` is supposed to
                            // throw TypeError directly). The runtime
                            // currently routes that case through the
                            // ordinary chain rather than throwing, to
                            // mirror the existing `Op::ToNumber` hook
                            // behaviour. Task 25 + a follow-up will
                            // tighten this branch to spec.
                            return self.push_to_primitive_call(
                                stack,
                                context,
                                dst,
                                obj,
                                hint,
                                ToPrimitiveStage::OrdinaryFirst,
                                &callee,
                                obj,
                                args,
                            );
                        }
                        Some(_) => {
                            return Err(VmError::TypeError {
                                message: "Symbol.toPrimitive method is not callable".to_string(),
                            });
                        }
                    }
                }
                ToPrimitiveStage::OrdinaryFirst => {
                    let method = ordinary_method_for(hint, stage);
                    let callee = self.get_string_for_to_primitive(context, &obj, method)?;
                    if let Some(callee) = &callee
                        && self.is_callable_runtime(callee)
                    {
                        // OrdinaryToPrimitive calls valueOf /
                        // toString with `this = obj` and no args.
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        return self.push_to_primitive_call(
                            stack,
                            context,
                            dst,
                            obj,
                            hint,
                            ToPrimitiveStage::OrdinarySecond,
                            callee,
                            obj,
                            args,
                        );
                    }
                    // Fallback: when the prototype chain has no
                    // own / inherited callable for `method`, fall
                    // back to the synthetic Object.prototype
                    // intercept (the same one the call dispatcher
                    // routes plain `obj.valueOf()` / `obj.toString()`
                    // through). This keeps behaviour consistent
                    // for plain object literals which never receive
                    // a real Object.prototype linkage.
                    if callee.is_none()
                        && let Some(o) = obj.as_object()
                    {
                        let no_args: SmallVec<[Value; 8]> = SmallVec::new();
                        let fn_proto = self.function_prototype_object().ok();
                        if let Some(v) = object_prototype_intercept(
                            &o,
                            method,
                            &no_args,
                            &mut self.gc_heap,
                            fn_proto,
                        )? && abstract_ops::is_primitive(&v)
                        {
                            let top_idx = stack.len() - 1;
                            stack[top_idx].pending_to_primitive = None;
                            write_register(&mut stack[top_idx], dst, v)?;
                            stack[top_idx].pc = stack[top_idx]
                                .pc
                                .checked_add(1)
                                .ok_or(VmError::InvalidOperand)?;
                            return Ok(false);
                        }
                    }
                    stage = ToPrimitiveStage::OrdinarySecond;
                }
                ToPrimitiveStage::OrdinarySecond => {
                    let method = ordinary_method_for(hint, stage);
                    let callee = self.get_string_for_to_primitive(context, &obj, method)?;
                    if let Some(callee) = &callee
                        && self.is_callable_runtime(callee)
                    {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        // After OrdinarySecond the only spec-legal
                        // outcomes are: primitive result (resume
                        // path writes it) or non-primitive →
                        // throw. Park the stage as `Exhausted` so
                        // the resume re-entry can't loop back into
                        // this slot.
                        return self.push_to_primitive_call(
                            stack,
                            context,
                            dst,
                            obj,
                            hint,
                            ToPrimitiveStage::Exhausted,
                            callee,
                            obj,
                            args,
                        );
                    }
                    // Same prototype-intercept fallback as
                    // OrdinaryFirst above — runs the second method
                    // (`toString` for hint=number, `valueOf` for
                    // hint=string) when the chain has nothing
                    // callable.
                    if callee.is_none()
                        && let Some(o) = obj.as_object()
                    {
                        let no_args: SmallVec<[Value; 8]> = SmallVec::new();
                        let fn_proto = self.function_prototype_object().ok();
                        if let Some(v) = object_prototype_intercept(
                            &o,
                            method,
                            &no_args,
                            &mut self.gc_heap,
                            fn_proto,
                        )? && abstract_ops::is_primitive(&v)
                        {
                            let top_idx = stack.len() - 1;
                            stack[top_idx].pending_to_primitive = None;
                            write_register(&mut stack[top_idx], dst, v)?;
                            stack[top_idx].pc = stack[top_idx]
                                .pc
                                .checked_add(1)
                                .ok_or(VmError::InvalidOperand)?;
                            return Ok(false);
                        }
                    }
                    stage = ToPrimitiveStage::Exhausted;
                }
                ToPrimitiveStage::Exhausted => {
                    // §7.1.1.1 step 6 — TypeError. Task 25 will
                    // upgrade `VmError::TypeMismatch` to a real
                    // `TypeError` Error object.
                    let top_idx = stack.len() - 1;
                    stack[top_idx].pending_to_primitive = None;
                    return Err(VmError::TypeMismatch);
                }
            }
        }
    }

    /// Park `Op::ToPrimitive` ladder state on the running frame and
    /// invoke `callee`. The dispatcher re-enters the same opcode
    /// after the call returns; the resume path validates the
    /// result.
    #[allow(clippy::too_many_arguments)]
    fn push_to_primitive_call(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        dst: u16,
        obj: Value,
        hint: abstract_ops::ToPrimitiveHint,
        next_stage: ToPrimitiveStage,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<bool, VmError> {
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        stack[top_idx].pending_to_primitive = Some(PendingToPrimitive {
            pc,
            dst,
            obj,
            hint,
            stage: next_stage,
        });
        // pc stays on the Op::ToPrimitive instruction so the
        // dispatcher re-enters the resume path after the called
        // function returns.
        self.invoke(stack, context, callee, this_value, args, dst)?;
        Ok(true)
    }
}

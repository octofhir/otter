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
    ordinary_method_for, read_register,
    string::StringHeap,
    symbol, write_register,
};

pub(crate) fn to_number_primitive(value: &Value) -> Result<NumberValue, VmError> {
    let number = match value {
        Value::Number(n) => *n,
        Value::Boolean(true) => NumberValue::Smi(1),
        Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
        Value::BigInt(_) | Value::Symbol(_) => return Err(VmError::TypeMismatch),
        Value::Undefined
        | Value::Hole
        | Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::Object(_)
        | Value::Array(_)
        | Value::Iterator(_)
        | Value::RegExp(_)
        | Value::Promise(_)
        | Value::ClassConstructor(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::WeakMap(_)
        | Value::WeakSet(_)
        | Value::WeakRef(_)
        | Value::FinalizationRegistry(_)
        | Value::Temporal(_)
        | Value::Intl(_)
        | Value::ArrayBuffer(_)
        | Value::DataView(_)
        | Value::TypedArray(_)
        | Value::Generator(_)
        | Value::Proxy(_) => NumberValue::Double(f64::NAN),
        Value::Date(d) => NumberValue::from_f64(d.time()),
        Value::String(s) => number::to_number_from_string(&s.to_lossy_string()),
    };
    Ok(number)
}

pub(crate) fn to_string_primitive(value: &Value) -> Result<String, VmError> {
    match value {
        Value::String(s) => Ok(s.to_lossy_string()),
        Value::Number(n) => Ok(n.to_display_string()),
        Value::BigInt(b) => Ok(b.to_decimal_string()),
        Value::Boolean(true) => Ok("true".to_string()),
        Value::Boolean(false) => Ok("false".to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Undefined | Value::Hole => Ok("undefined".to_string()),
        Value::Symbol(_) => Err(VmError::TypeMismatch),
        _ => Err(VmError::TypeMismatch),
    }
}

pub(crate) fn to_js_string_primitive(
    value: &Value,
    heap: &StringHeap,
) -> Result<JsString, VmError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => {
            number::ecma::number_to_string(n.as_f64(), heap).map_err(|_| VmError::TypeMismatch)
        }
        _ => JsString::from_str(&to_string_primitive(value)?, heap)
            .map_err(|_| VmError::TypeMismatch),
    }
}

pub(crate) fn string_constructor_js_string(
    value: Option<&Value>,
    heap: &StringHeap,
) -> Result<JsString, VmError> {
    match value {
        Some(Value::Symbol(s)) => {
            JsString::from_str(&s.descriptive_string(), heap).map_err(|_| VmError::TypeMismatch)
        }
        Some(value) => match to_js_string_primitive(value, heap) {
            Ok(value) => Ok(value),
            Err(VmError::TypeMismatch) => {
                JsString::from_str(&value.display_string(), heap).map_err(|_| VmError::TypeMismatch)
            }
            Err(err) => Err(err),
        },
        None => JsString::empty(heap).map_err(|_| VmError::TypeMismatch),
    }
}

impl Interpreter {
    pub(crate) fn run_to_number_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = to_number_primitive(read_register(frame, src)?)?;
        write_register(frame, dst, Value::Number(value))?;
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
        let recv = read_register(&stack[top_idx], src)?.clone();
        let Value::Object(obj) = &recv else {
            return Ok(None);
        };
        let to_primitive_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
        let Some(callee) = crate::object::get_symbol(*obj, &self.gc_heap, &to_primitive_sym) else {
            return Ok(None);
        };
        if !self.is_callable_runtime(&callee) {
            return Ok(None);
        }
        let hint = JsString::from_str("number", &self.string_heap)?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(Value::String(hint));
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, context, &callee, recv.clone(), args, dst)?;
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
        let hint = abstract_ops::ToPrimitiveHint::from_token(&hint_token)
            .ok_or(VmError::InvalidOperand)?;

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
            let produced = read_register(&stack[top_idx], dst)?.clone();
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
        let recv = read_register(&stack[top_idx], src)?.clone();
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
        if !matches!(value, Value::Undefined) {
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
    pub(crate) fn intrinsic_prototype_object_for(&self, value: &Value) -> Option<JsObject> {
        let constructor_name = match value {
            Value::Function { .. }
            | Value::Closure { .. }
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_) => return self.function_prototype_object().ok(),
            Value::Array(_) => "Array",
            Value::RegExp(_) => "RegExp",
            Value::Map(_) => "Map",
            Value::Set(_) => "Set",
            Value::WeakMap(_) => "WeakMap",
            Value::WeakSet(_) => "WeakSet",
            Value::WeakRef(_) => "WeakRef",
            Value::FinalizationRegistry(_) => "FinalizationRegistry",
            Value::Promise(_) => "Promise",
            Value::ArrayBuffer(b) => {
                if b.is_shared() {
                    "SharedArrayBuffer"
                } else {
                    "ArrayBuffer"
                }
            }
            Value::DataView(_) => "DataView",
            Value::TypedArray(t) => t.kind().name(),
            // §10.4 Date is an exotic object; §20.1.2.10
            // Object.getPrototypeOf also accepts primitives by
            // routing through ToObject (§7.1.18), so we hand
            // primitive values their own constructor's
            // `%X.prototype%` here. Callers that only deal with
            // exotic-object shapes already filter out primitives.
            Value::Date(_) => "Date",
            Value::Symbol(_) => "Symbol",
            Value::String(_) => "String",
            Value::Number(_) => "Number",
            Value::Boolean(_) => "Boolean",
            Value::BigInt(_) => "BigInt",
            Value::Object(_) | Value::Proxy(_) => return None,
            _ => return None,
        };
        match self.constructor_prototype_value(constructor_name).ok()? {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Look up a string-keyed property over a non-primitive value's
    /// `[[Prototype]]` chain. Returns `None` when the chain has no
    /// inherited definition. This is the §7.1.1.1 `OrdinaryToPrimitive`
    /// fast path for `valueOf` / `toString` and intentionally does
    /// not invoke accessor getters: callers want the raw `[[Value]]`
    /// of an inherited data property (typically the realm's installed
    /// `valueOf` / `toString` callables) and treat accessor hits as
    /// "no callable found" so the next stage runs.
    fn get_proto_string_for_to_primitive(&self, base: &Value, name: &str) -> Option<Value> {
        let proto = match base {
            Value::Object(o) => Some(*o),
            _ => self.intrinsic_prototype_object_for(base),
        };
        proto.and_then(|o| object::get(o, &self.gc_heap, name))
    }

    /// Look up a Symbol-keyed property over a non-primitive value's
    /// `[[Prototype]]` chain. Used by the §7.1.1 step 2 lookup of
    /// `[Symbol.toPrimitive]`. Same accessor policy as
    /// [`Self::get_proto_string_for_to_primitive`].
    fn get_proto_symbol_for_to_primitive(
        &self,
        base: &Value,
        sym: &symbol::JsSymbol,
    ) -> Option<Value> {
        let proto = match base {
            Value::Object(o) => Some(*o),
            _ => self.intrinsic_prototype_object_for(base),
        };
        proto.and_then(|o| object::get_symbol(o, &self.gc_heap, sym))
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
                    let callee = self.get_proto_symbol_for_to_primitive(&obj, &to_prim_sym);
                    if let Some(callee) = callee
                        && self.is_callable_runtime(&callee)
                    {
                        let hint_str = JsString::from_str(hint.as_token(), &self.string_heap)?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(Value::String(hint_str));
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
                            obj.clone(),
                            hint,
                            ToPrimitiveStage::OrdinaryFirst,
                            &callee,
                            obj.clone(),
                            args,
                        );
                    }
                    stage = ToPrimitiveStage::OrdinaryFirst;
                }
                ToPrimitiveStage::OrdinaryFirst => {
                    let method = ordinary_method_for(hint, stage);
                    let callee = self.get_proto_string_for_to_primitive(&obj, method);
                    if let Some(callee) = callee
                        && self.is_callable_runtime(&callee)
                    {
                        // OrdinaryToPrimitive calls valueOf /
                        // toString with `this = obj` and no args.
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        return self.push_to_primitive_call(
                            stack,
                            context,
                            dst,
                            obj.clone(),
                            hint,
                            ToPrimitiveStage::OrdinarySecond,
                            &callee,
                            obj.clone(),
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
                    if let Value::Object(o) = &obj {
                        let no_args: SmallVec<[Value; 8]> = SmallVec::new();
                        if let Some(v) = object_prototype_intercept(
                            o,
                            method,
                            &no_args,
                            &self.string_heap,
                            &self.gc_heap,
                            self.function_prototype_object().ok(),
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
                    let callee = self.get_proto_string_for_to_primitive(&obj, method);
                    if let Some(callee) = callee
                        && self.is_callable_runtime(&callee)
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
                            obj.clone(),
                            hint,
                            ToPrimitiveStage::Exhausted,
                            &callee,
                            obj.clone(),
                            args,
                        );
                    }
                    // Same prototype-intercept fallback as
                    // OrdinaryFirst above — runs the second method
                    // (`toString` for hint=number, `valueOf` for
                    // hint=string) when the chain has nothing
                    // callable.
                    if let Value::Object(o) = &obj {
                        let no_args: SmallVec<[Value; 8]> = SmallVec::new();
                        if let Some(v) = object_prototype_intercept(
                            o,
                            method,
                            &no_args,
                            &self.string_heap,
                            &self.gc_heap,
                            self.function_prototype_object().ok(),
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

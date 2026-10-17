//! Committed semantic completions and boxed-value kernels.
//!
//! # Contents
//! - Existing source/terminal error disposition shared by direct semantic
//!   opcodes and compiled callers.
//! - Typed operation families for object-protocol and scalar bytecodes,
//!   including error allocation, throw origins and derived-`this` binding.
//! - Generic binary arithmetic and relational operators for compiled sites
//!   whose feedback is not numeric ([`BinaryOperator`]).
//! - The complete `Array.prototype.push`/`pop` builtins for a compiled method
//!   site whose inline append or removal does not cover the receiver.
//! - Rooted value kernels shared by interpreter and compiled callers.
//! - [`ObjectProtocolValueOp`] owns the checked compiled operation word.
//! - [`RuntimeCall`] typed object operations and source-decoded scalar sites.
//! - Side-channel-free JavaScript exception materialization for the committed
//!   `NativeResultPair` domain.
//!
//! # Invariants
//! - `value0` and `value1` are rooted as VM locals before any allocation,
//!   collection, Proxy trap, getter, coercion, or nested JavaScript call. The
//!   binary-operator kernels are the interpreter's and root both operands in
//!   their own handle scope before the first coercion.
//! - A compiled object operation is explicit; its source/root record owns
//!   tracing and reconstruction. Scalar operations retain source decoding.
//!   Neither boxed-value family receives a destination or register index.
//! - Prototype changes preserve the actual object identity of both operands;
//!   the canonical internal-method owner handles every object family.
//! - Once a kernel begins it returns only a normal value or an error. Compiled
//!   entries turn source errors into rooted JavaScript exception values and
//!   retain an already completed terminal native/allocator/control failure;
//!   there is no miss, deopt, materialization fallback, or replay outcome.
//! - No native-frame or register-window borrow survives JavaScript reentry.
//!
//! # See also
//! - `crate::native_abi::RuntimeStubSignature::ContextWords`
//! - `crate::native_abi::RuntimeStubSignature::CommittedValue2`
//! - `crate::function_ops::Interpreter::instanceof_operator`
//! - `crate::object_internal_ops::Interpreter::ordinary_has_property_value`

use otter_bytecode::Op;

use crate::{
    Interpreter, Value, VmError, VmPropertyKey, abstract_ops, number::NumberValue,
    rooting::RootScopeExt,
};

use super::RuntimeCall;

/// Failure domain for a fixed committed value entry.
///
/// ABI/source decoding and activation validation cannot be caught by JavaScript.
/// Source semantic failures retain ordinary throwable materialization. A
/// completed native execution failure, or refusal while materializing its
/// exception, is terminal even after operation entry and is never projected
/// again. The existing pending detail/frames remain owned by the interpreter.
#[derive(Debug)]
pub enum CommittedValueError {
    /// Catchable failure produced after entering the typed JS operation.
    JavaScript(VmError),
    /// Invalid structure, or an actual terminal native/control/allocator
    /// completion after entry. Its pending detail/frames remain authoritative.
    Fatal(VmError),
}

impl CommittedValueError {
    /// Preserve the disposition of a child callable that has completed its
    /// trampoline/error projection. This uses the existing completed-run
    /// policy; direct errors of the current source operation do not enter here.
    pub(crate) fn completed_call(error: VmError) -> Self {
        if crate::RunError::bare(error).is_fatal() {
            Self::Fatal(error)
        } else {
            Self::JavaScript(error)
        }
    }

    /// Transfer an actual async-job completion into the existing owned run
    /// result before its published activation extent is released. This is the
    /// final job boundary: an escaping allocation failure has completed the job
    /// and follows `RunError` policy, without another JavaScript projection.
    pub(crate) fn into_run_error(self, interp: &mut Interpreter) -> crate::RunError {
        let error = match self {
            Self::JavaScript(error) | Self::Fatal(error) => error,
        };
        crate::RunError {
            error,
            frames: interp.take_uncaught_frames(),
            detail: interp.take_error_detail(),
        }
    }

    /// Complete at the native boundary while the canonical error detail and
    /// source frames are still owned by the interpreter. A local JavaScript
    /// allocation refusal enters the existing authored native OOM domain and
    /// may materialize RangeError once. A terminal completion keeps the owned
    /// execution-failure mapper. Source and compiled dispatch instead consume
    /// the variants without another projection.
    pub(crate) fn into_native(
        self,
        interp: &mut Interpreter,
        name: &'static str,
    ) -> crate::NativeError {
        match self {
            Self::JavaScript(VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            }) => crate::NativeError::OutOfMemory {
                name,
                requested_bytes,
                heap_limit_bytes,
            },
            Self::JavaScript(error) | Self::Fatal(error) => {
                crate::native_function::vm_to_native_error(interp, error, name)
            }
        }
    }
}

/// Object-protocol semantics shared by interpreter and compiled callers.
///
/// Compiled entries pass the checked operation word explicitly. Published
/// source/root records retain tracing and reconstruction ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectProtocolValueOp {
    /// ECMAScript `InstanceofOperator(value, target)`.
    Instanceof,
    /// ECMAScript `HasProperty(object, key)` (`key in object`).
    HasProperty,
    /// `[[GetPrototypeOf]]`.
    GetPrototype,
    /// `[[SetPrototypeOf]]`.
    SetPrototype,
    /// ECMAScript `IsLooselyEqual(x, y)`, including object-to-primitive
    /// coercion and the HTMLDDA equivalence class.
    LooseEqual,
    /// The negation of [`Self::LooseEqual`].
    LooseNotEqual,
    /// A binary arithmetic, relational or bitwise operator (`+ - * / % **`,
    /// `< <= > >= & | ^ << >> >>>`) over operands whose feedback is not
    /// numeric, including every observable `ToPrimitive` / `ToNumeric`
    /// coercion and `+` string concatenation.
    Binary(BinaryOperator),
    /// `Array.prototype.push` of the second value onto the first, at a site
    /// whose generated code proved the builtin is the callee.
    ArrayPush,
    /// `Array.prototype.pop` of the first value, at a site whose generated
    /// code proved the builtin is the callee.
    ArrayPop,
}

/// The generic binary operator a committed site completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    /// `+` — string concatenation or numeric addition.
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `**`
    Pow,
    /// `<`
    LessThan,
    /// `<=`
    LessEq,
    /// `>`
    GreaterThan,
    /// `>=`
    GreaterEq,
    /// `&`
    BitwiseAnd,
    /// `|`
    BitwiseOr,
    /// `^`
    BitwiseXor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `>>>`
    Ushr,
}

impl BinaryOperator {
    /// The operator a bytecode binary opcode denotes, if it is generic.
    #[must_use]
    pub fn from_opcode(op: Op) -> Option<Self> {
        Some(match op {
            Op::Add => Self::Add,
            Op::Sub => Self::Sub,
            Op::Mul => Self::Mul,
            Op::Div => Self::Div,
            Op::Rem => Self::Rem,
            Op::Pow => Self::Pow,
            Op::LessThan => Self::LessThan,
            Op::LessEq => Self::LessEq,
            Op::GreaterThan => Self::GreaterThan,
            Op::GreaterEq => Self::GreaterEq,
            Op::BitwiseAnd | Op::BitwiseAndImm => Self::BitwiseAnd,
            Op::BitwiseOr => Self::BitwiseOr,
            Op::BitwiseXor => Self::BitwiseXor,
            Op::Shl => Self::Shl,
            Op::Shr => Self::Shr,
            Op::Ushr => Self::Ushr,
            _ => return None,
        })
    }
}

impl ObjectProtocolValueOp {
    /// The scalar operation word passed after the two boxed operands.
    #[must_use]
    pub const fn encode_abi(self) -> u64 {
        match self {
            Self::Instanceof => 0,
            Self::HasProperty => 1,
            Self::GetPrototype => 2,
            Self::SetPrototype => 3,
            Self::LooseEqual => 4,
            Self::LooseNotEqual => 5,
            Self::Binary(BinaryOperator::Add) => 6,
            Self::Binary(BinaryOperator::Sub) => 7,
            Self::Binary(BinaryOperator::Mul) => 8,
            Self::Binary(BinaryOperator::Div) => 9,
            Self::Binary(BinaryOperator::Rem) => 10,
            Self::Binary(BinaryOperator::Pow) => 11,
            Self::Binary(BinaryOperator::LessThan) => 12,
            Self::Binary(BinaryOperator::LessEq) => 13,
            Self::Binary(BinaryOperator::GreaterThan) => 14,
            Self::Binary(BinaryOperator::GreaterEq) => 15,
            Self::Binary(BinaryOperator::BitwiseAnd) => 16,
            Self::Binary(BinaryOperator::BitwiseOr) => 17,
            Self::Binary(BinaryOperator::BitwiseXor) => 18,
            Self::Binary(BinaryOperator::Shl) => 19,
            Self::Binary(BinaryOperator::Shr) => 20,
            Self::Binary(BinaryOperator::Ushr) => 21,
            Self::ArrayPush => 22,
            Self::ArrayPop => 23,
        }
    }

    /// Validate the complete operation word at the compiled ABI boundary.
    /// No bytecode lookup or low-byte truncation selects the operation.
    pub const fn decode_abi(word: u64) -> Result<Self, VmError> {
        Ok(match word {
            0 => Self::Instanceof,
            1 => Self::HasProperty,
            2 => Self::GetPrototype,
            3 => Self::SetPrototype,
            4 => Self::LooseEqual,
            5 => Self::LooseNotEqual,
            6 => Self::Binary(BinaryOperator::Add),
            7 => Self::Binary(BinaryOperator::Sub),
            8 => Self::Binary(BinaryOperator::Mul),
            9 => Self::Binary(BinaryOperator::Div),
            10 => Self::Binary(BinaryOperator::Rem),
            11 => Self::Binary(BinaryOperator::Pow),
            12 => Self::Binary(BinaryOperator::LessThan),
            13 => Self::Binary(BinaryOperator::LessEq),
            14 => Self::Binary(BinaryOperator::GreaterThan),
            15 => Self::Binary(BinaryOperator::GreaterEq),
            16 => Self::Binary(BinaryOperator::BitwiseAnd),
            17 => Self::Binary(BinaryOperator::BitwiseOr),
            18 => Self::Binary(BinaryOperator::BitwiseXor),
            19 => Self::Binary(BinaryOperator::Shl),
            20 => Self::Binary(BinaryOperator::Shr),
            21 => Self::Binary(BinaryOperator::Ushr),
            22 => Self::ArrayPush,
            23 => Self::ArrayPop,
            _ => return Err(VmError::InvalidOperand),
        })
    }
}

/// Scalar semantics selected by one published bytecode site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarValueOp {
    /// ECMAScript `ToObject`.
    ToObject,
    /// ECMAScript `ToPropertyKey`.
    ToPropertyKey,
    /// Materialize the `typeof` result string.
    TypeOf,
    /// Read the activation's `new.target` binding.
    LoadNewTarget,
    /// ECMAScript `SameValue`.
    SameValue,
    /// Read one dense array's length.
    ArrayLength,
    /// Read one string's UTF-16 code-unit length.
    LoadLength,
    /// Read the activation's implicit arguments length.
    LoadArgumentsLength,
    /// Read an actual index or materialize for observable property semantics.
    LoadArgumentsElement,
    /// Capture an explicit throw origin before native frames unwind.
    PrepareThrow,
    /// Intrinsic error allocation selected by the published kind constant.
    NewError,
    /// Bind the completed `super()` result as derived-constructor `this`.
    BindThisValue,
}

impl ScalarValueOp {
    fn from_opcode(op: Op) -> Result<Self, VmError> {
        match op {
            Op::ToObject => Ok(Self::ToObject),
            Op::ToPropertyKey => Ok(Self::ToPropertyKey),
            Op::TypeOf => Ok(Self::TypeOf),
            Op::LoadNewTarget => Ok(Self::LoadNewTarget),
            Op::SameValue => Ok(Self::SameValue),
            Op::ArrayLength => Ok(Self::ArrayLength),
            Op::LoadLength => Ok(Self::LoadLength),
            Op::LoadArgumentsLength => Ok(Self::LoadArgumentsLength),
            Op::LoadArgumentsElement => Ok(Self::LoadArgumentsElement),
            Op::Throw => Ok(Self::PrepareThrow),
            Op::NewError => Ok(Self::NewError),
            Op::BindThisValue => Ok(Self::BindThisValue),
            _ => Err(VmError::InvalidOperand),
        }
    }
}

impl Interpreter {
    /// Complete one object-protocol operation over rooted boxed values.
    pub(crate) fn object_protocol_value(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &crate::ExecutionContext,
        operation: ObjectProtocolValueOp,
        mut value0: Value,
        mut value1: Value,
    ) -> Result<Value, CommittedValueError> {
        // The binary kernels are the interpreter's own: they root both
        // operands in a handle scope before their first coercion, so they need
        // no second root frame here.
        if let ObjectProtocolValueOp::Binary(operator) = operation {
            return self.binary_operator_value(stack, context, operator, value0, value1);
        }
        // `index in array` on a plain array is its own storage's answer: no
        // key to spell, no prototype walk.
        if let ObjectProtocolValueOp::HasProperty = operation
            && let Some(arr) = value1.as_array()
            && let Some(index) = value0.as_i32().and_then(|i| usize::try_from(i).ok())
            && let Some(has) = crate::array::plain_dense_has(
                arr,
                &self.gc_heap,
                index,
                !self.array_index_accessor_protector,
            )
        {
            return Ok(Value::boolean(has));
        }
        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: all three locals precede `roots` and remain stationary until
        // the complete semantic operation returns.
        unsafe {
            roots.add_value(&mut value0);
            roots.add_value(&mut value1);
            roots.add_value(&mut result);
        }

        result = match operation {
            ObjectProtocolValueOp::Instanceof => {
                Value::boolean(self.instanceof_operator(stack, context, &value0, &value1)?)
            }
            ObjectProtocolValueOp::LooseEqual => {
                Value::boolean(self.loose_equal_with_context(stack, context, &value0, &value1)?)
            }
            ObjectProtocolValueOp::LooseNotEqual => {
                Value::boolean(!self.loose_equal_with_context(stack, context, &value0, &value1)?)
            }
            ObjectProtocolValueOp::Binary(_) => unreachable!("binary operators return above"),
            ObjectProtocolValueOp::ArrayPush => {
                self.array_push(stack, context, value0, &[value1], &[])?
            }
            ObjectProtocolValueOp::ArrayPop => self.array_pop(stack, context, value0, &[])?,
            ObjectProtocolValueOp::HasProperty => {
                if !value1.is_object_type() {
                    return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                }
                result = self.coerce_property_key_value(stack, context, value0)?;
                if let (Some(proxy), Some(symbol)) =
                    (value1.as_proxy(), result.as_symbol(&self.gc_heap))
                    && symbol.is_private_name()
                {
                    Value::boolean(self.proxy_private_find(&proxy, symbol).is_some())
                } else {
                    let name: std::sync::Arc<str>;
                    let key = if let Some(symbol) = result.as_symbol(&self.gc_heap) {
                        VmPropertyKey::Symbol(symbol)
                    } else if let Some(string) = result.as_string(&self.gc_heap) {
                        // An interned key spells its atom: no decoding, and
                        // shape walks compare ids.
                        if let Some((atom, spelling)) = self.interned_string_key(string) {
                            name = spelling;
                            VmPropertyKey::Atom(crate::property_atom::AtomizedPropertyKey::new(
                                crate::property_atom::PropertyAtom::new(atom),
                                &name,
                            ))
                        } else {
                            VmPropertyKey::OwnedString(string.to_key_spelling(&self.gc_heap))
                        }
                    } else if let Some(number) = result.as_number() {
                        VmPropertyKey::OwnedString(number.to_display_string())
                    } else {
                        return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
                    };
                    Value::boolean(self.ordinary_has_property_value(
                        stack,
                        Some(context),
                        value1,
                        &key,
                        0,
                    )?)
                }
            }
            ObjectProtocolValueOp::GetPrototype => {
                if value0.is_proxy() {
                    self.ordinary_get_prototype_value(stack, context, value0, 0)?
                } else {
                    self.get_prototype_for_op(&value0)
                        .map_err(CommittedValueError::JavaScript)?
                }
            }
            ObjectProtocolValueOp::SetPrototype => {
                if !value0.is_object_type() || (!value1.is_object_type() && !value1.is_null()) {
                    return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                }
                if !self.set_prototype_value_proxy_aware(stack, context, &value0, &value1)? {
                    return Err(CommittedValueError::JavaScript(
                        self.err_type(("Object.setPrototypeOf failed".to_string()).into()),
                    ));
                }
                Value::undefined()
            }
        };
        Ok(result)
    }

    /// Complete one scalar operation over rooted boxed values.
    pub(crate) fn scalar_value(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &crate::ExecutionContext,
        operation: ScalarValueOp,
        mut value0: Value,
        mut value1: Value,
        mut new_target: Value,
    ) -> Result<Value, CommittedValueError> {
        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the input, activation binding, and result locals all precede
        // `roots` and remain stationary for the entire operation.
        unsafe {
            roots.add_value(&mut value0);
            roots.add_value(&mut value1);
            roots.add_value(&mut new_target);
            roots.add_value(&mut result);
        }

        result = match operation {
            ScalarValueOp::BindThisValue
            | ScalarValueOp::LoadArgumentsLength
            | ScalarValueOp::LoadArgumentsElement
            | ScalarValueOp::NewError
            | ScalarValueOp::PrepareThrow => {
                // This operation consumes activation metadata and is completed
                // by `RuntimeCall::scalar_values`.
                return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
            }
            ScalarValueOp::ToObject => {
                if value0.is_nullish() {
                    return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                }
                self.box_sloppy_this_primitive_runtime_rooted(value0, &[])
                    .map_err(CommittedValueError::JavaScript)?
            }
            ScalarValueOp::ToPropertyKey => {
                self.coerce_property_key_value(stack, context, value0)?
            }
            ScalarValueOp::TypeOf => {
                let kind = value0.typeof_kind_with_heap(&self.gc_heap);
                self.typeof_string_value(kind)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            }
            ScalarValueOp::LoadNewTarget => new_target,
            ScalarValueOp::SameValue => {
                Value::boolean(abstract_ops::same_value(&value0, &value1, &self.gc_heap))
            }
            ScalarValueOp::ArrayLength => {
                let array = value0
                    .as_array()
                    .ok_or(VmError::TypeMismatch)
                    .map_err(CommittedValueError::JavaScript)?;
                Value::number(NumberValue::from_f64(
                    crate::array::len(array, &self.gc_heap) as f64,
                ))
            }
            ScalarValueOp::LoadLength => {
                let string = value0
                    .as_string(&self.gc_heap)
                    .ok_or(VmError::TypeMismatch)
                    .map_err(CommittedValueError::JavaScript)?;
                Value::number_u32(string.len())
            }
        };
        Ok(result)
    }
}

impl RuntimeCall<'_> {
    /// Complete the supplied object operation over boxed values.
    ///
    /// The published source/root record remains available to tracing and recovery.
    pub fn object_protocol_values(
        &mut self,
        operation: ObjectProtocolValueOp,
        value0: Value,
        value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        vm.object_protocol_value(
            unsafe { &mut *self.stack.as_ptr() },
            &self.context,
            operation,
            value0,
            value1,
        )
    }

    /// Whether the static handler table of the throwing function covers the
    /// published throw instruction.
    fn throw_lands_in_local_handler(&self) -> Result<bool, CommittedValueError> {
        let (function_id, pc) = self.semantic_source().map_err(CommittedValueError::Fatal)?;
        let owner = self
            .context
            .for_function(function_id)
            .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let function = owner
            .exec_function(function_id)
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        Ok(function.control_flow().handler_at(pc).is_some())
    }

    /// Complete the exact published scalar site over boxed values.
    pub fn scalar_values(
        &mut self,
        value0: Value,
        value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let opcode = self
            .published_opcode()
            .map_err(CommittedValueError::Fatal)?;
        let operation = ScalarValueOp::from_opcode(opcode).map_err(CommittedValueError::Fatal)?;
        if matches!(
            operation,
            ScalarValueOp::LoadArgumentsLength | ScalarValueOp::LoadArgumentsElement
        ) {
            return self.arguments_value(
                (operation == ScalarValueOp::LoadArgumentsElement).then_some(value0),
            );
        }
        if operation == ScalarValueOp::BindThisValue {
            self.bind_derived_this_value(value0)?;
            return Ok(value0);
        }
        if operation == ScalarValueOp::PrepareThrow {
            // A handler of the throwing function that covers this instruction
            // absorbs the throw inside this activation, which acknowledges the
            // catch; only a throw leaving the activation needs its site.
            let handled_here = self.throw_lands_in_local_handler()?;
            let vm = unsafe { &mut *self.vm.as_ptr() };
            vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
            if !handled_here {
                vm.record_throw_site();
            }
            return Ok(value0);
        }
        if operation == ScalarValueOp::NewError {
            let (function_id, _) = self.semantic_source().map_err(CommittedValueError::Fatal)?;
            let context = self
                .context
                .for_function(function_id)
                .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
            let constant = self
                .published_const_index(1)
                .map_err(CommittedValueError::Fatal)?;
            let kind = context
                .string_constant_str_for_function(function_id, constant)
                .and_then(crate::ErrorKind::from_class_name)
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
            let vm = unsafe { &mut *self.vm.as_ptr() };
            vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
            return vm.new_error_value(
                &context,
                unsafe { &mut *self.stack.as_ptr() },
                function_id,
                kind,
                value0,
            );
        }
        let new_target = if operation == ScalarValueOp::LoadNewTarget {
            self.with_frame(|frame| Ok(frame.new_target_value()))
                .map_err(CommittedValueError::Fatal)?
        } else {
            Value::undefined()
        };
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        vm.scalar_value(
            unsafe { &mut *self.stack.as_ptr() },
            &self.context,
            operation,
            value0,
            value1,
            new_target,
        )
    }

    /// Convert one catchable VM failure into a JavaScript exception value.
    ///
    /// The exception-value side channel is consumed on return. Diagnostic
    /// uncaught-frame provenance is deliberately preserved until a local catch
    /// acknowledges that it absorbed the throw; propagating compiled frames
    /// therefore cannot erase the original nested getter/Proxy/call stack.
    /// A nested JavaScript throw is consumed from `pending_uncaught_throw`; an
    /// engine error is materialized directly. The caller owns the returned
    /// value and must either take a compiled landing or invoke the typed throw
    /// router. Structural host failures remain `Err`.
    pub fn take_js_throw(&mut self, err: VmError) -> Result<Value, VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        if matches!(err, VmError::Uncaught) {
            let exception = vm
                .take_pending_uncaught_throw()
                .ok_or(VmError::InvalidOperand)?;
            let _ = vm.take_error_detail();
            return Ok(exception);
        }
        let exception = vm.vm_error_to_throwable_with_stack_roots(
            Some(&self.context),
            unsafe { self.stack.as_ref() },
            &err,
        )?;
        let _ = vm.take_error_detail();
        Ok(exception)
    }
}

#[cfg(test)]
mod tests {
    use super::{BinaryOperator, ObjectProtocolValueOp, VmError};

    #[test]
    fn prototype_operation_preserves_actual_array_and_class_values() {
        use crate::{ActivationStack, Interpreter, Value, class_constructor::ClassConstructor};

        let mut vm = Interpreter::new().expect("prototype operation fixture");
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("prototype-operation.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("valid prototype source");
        let mut stack = ActivationStack::new();
        vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            vm.with_handle_scope(|vm, scope| {
                let object = vm.scoped_object(scope).unwrap();
                let array = vm.scoped_array(scope, 0).unwrap();
                let prototype = vm.scoped_object(scope).unwrap();
                let statics = vm.scoped_object(scope).unwrap();
                let _runtime_roots = vm.scope_runtime_roots_guard();
                let prototype_object = vm.escape_scoped(prototype).as_object().unwrap();
                let statics_object = vm.escape_scoped(statics).as_object().unwrap();
                let class = ClassConstructor::new_with_roots(
                    &mut vm.gc_heap,
                    Value::function(0),
                    prototype_object,
                    statics_object,
                    &mut |_| {},
                )
                .unwrap();
                let class = vm.scoped_value(scope, Value::class_constructor(class));
                let function = vm.scoped_value(scope, Value::function(0));
                let null = vm.scoped_value(scope, Value::null());
                for (target, requested) in [
                    (object, array),
                    (object, class),
                    (array, class),
                    (function, array),
                    (object, null),
                ] {
                    let result = vm
                        .object_protocol_value(
                            stack,
                            &context,
                            ObjectProtocolValueOp::SetPrototype,
                            vm.escape_scoped(target),
                            vm.escape_scoped(requested),
                        )
                        .expect("object-family prototype completion");
                    assert_eq!(result, Value::undefined());
                    assert_eq!(
                        vm.get_prototype_for_op(&vm.escape_scoped(target)).unwrap(),
                        vm.escape_scoped(requested),
                        "the requested object is the published prototype"
                    );
                }
                assert!(matches!(
                    vm.object_protocol_value(
                        stack,
                        &context,
                        ObjectProtocolValueOp::SetPrototype,
                        vm.escape_scoped(object),
                        Value::number_i32(17),
                    ),
                    Err(super::CommittedValueError::JavaScript(
                        VmError::TypeMismatch
                    ))
                ));
                assert_eq!(
                    vm.get_prototype_for_op(&vm.escape_scoped(object)).unwrap(),
                    Value::null(),
                    "invalid prototypes do not mutate the receiver"
                );
            });
        });
    }

    #[test]
    fn object_operation_words_roundtrip_without_truncation_or_aliases() {
        use BinaryOperator as B;
        use ObjectProtocolValueOp as O;

        let operations = [
            O::Instanceof,
            O::HasProperty,
            O::GetPrototype,
            O::SetPrototype,
            O::LooseEqual,
            O::LooseNotEqual,
            O::Binary(B::Add),
            O::Binary(B::Sub),
            O::Binary(B::Mul),
            O::Binary(B::Div),
            O::Binary(B::Rem),
            O::Binary(B::Pow),
            O::Binary(B::LessThan),
            O::Binary(B::LessEq),
            O::Binary(B::GreaterThan),
            O::Binary(B::GreaterEq),
            O::Binary(B::BitwiseAnd),
            O::Binary(B::BitwiseOr),
            O::Binary(B::BitwiseXor),
            O::Binary(B::Shl),
            O::Binary(B::Shr),
            O::Binary(B::Ushr),
            O::ArrayPush,
            O::ArrayPop,
        ];
        let mut words = std::collections::BTreeSet::new();
        for operation in operations {
            let word = operation.encode_abi();
            assert!(words.insert(word), "operation words must be unique");
            assert_eq!(O::decode_abi(word).unwrap(), operation);
            for high_bit in [1 << 8, 1 << 32, 1 << 63] {
                assert!(matches!(
                    O::decode_abi(word | high_bit),
                    Err(VmError::InvalidOperand)
                ));
            }
        }
        assert_eq!(words.len(), 24);
        for word in [24, 255, u64::MAX] {
            assert!(matches!(O::decode_abi(word), Err(VmError::InvalidOperand)));
        }
    }
}

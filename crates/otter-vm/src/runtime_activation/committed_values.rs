//! Fixed boxed-value semantic completions for compiled activations.
//!
//! # Contents
//! - Typed operation families for object-protocol and scalar bytecodes,
//!   including error allocation, throw origins and derived-`this` binding.
//! - Rooted value kernels shared by interpreter and compiled callers.
//! - [`RuntimeCall`] site decoding with no opcode/register ABI.
//! - Side-channel-free JavaScript exception materialization for the committed
//!   `NativeResultPair` domain.
//!
//! # Invariants
//! - `value0` and `value1` are rooted as VM locals before any allocation,
//!   collection, Proxy trap, getter, coercion, or nested JavaScript call.
//! - Function/PC identity is authoritative for the semantic operation; the
//!   native ABI never receives an opcode, destination, or register index.
//! - Once a kernel begins it returns only a normal value or an error. Compiled
//!   entries turn catchable errors into rooted JavaScript exception values;
//!   there is no miss, deopt, materialization fallback, or replay outcome.
//! - No native-frame or register-window borrow survives JavaScript reentry.
//!
//! # See also
//! - `crate::native_abi::RuntimeStubSignature::CommittedValue2`
//! - `crate::function_ops::Interpreter::instanceof_operator`
//! - `crate::object_internal_ops::Interpreter::ordinary_has_property_value`

use otter_bytecode::Op;

use crate::{
    Interpreter, JsString, Value, VmError, VmPropertyKey, abstract_ops, number::NumberValue,
    rooting::RootScopeExt,
};

use super::RuntimeCall;

/// Failure domain for a fixed committed value entry.
///
/// Site decoding and activation validation happen before JavaScript semantics
/// begin and therefore cannot be caught by JavaScript. Once the typed operation
/// has been selected, semantic failures may be materialized as a pure throw
/// value by the native entry.
#[derive(Debug)]
pub enum CommittedValueError {
    /// Catchable failure produced after entering the typed JS operation.
    JavaScript(VmError),
    /// Invalid activation, PC, opcode family, or other pre-entry structure.
    Fatal(VmError),
}

/// Object-protocol semantics selected by one published bytecode site.
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
}

impl ObjectProtocolValueOp {
    fn from_opcode(op: Op) -> Result<Self, VmError> {
        match op {
            Op::Instanceof => Ok(Self::Instanceof),
            Op::HasProperty => Ok(Self::HasProperty),
            Op::GetPrototype => Ok(Self::GetPrototype),
            Op::SetPrototype => Ok(Self::SetPrototype),
            Op::LooseEqual => Ok(Self::LooseEqual),
            Op::LooseNotEqual => Ok(Self::LooseNotEqual),
            _ => Err(VmError::InvalidOperand),
        }
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
    /// ECMAScript `IsArray`.
    IsArray,
    /// Read one dense array's length.
    ArrayLength,
    /// Read one string's UTF-16 code-unit length.
    LoadLength,
    /// Capture an explicit throw origin before native frames unwind.
    PrepareThrow,
    /// Intrinsic Error allocation with observable message coercion.
    NewError,
    /// Intrinsic native-error allocation selected by the published constant.
    NewBuiltinError,
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
            Op::IsArray => Ok(Self::IsArray),
            Op::ArrayLength => Ok(Self::ArrayLength),
            Op::LoadLength => Ok(Self::LoadLength),
            Op::Throw => Ok(Self::PrepareThrow),
            Op::NewError => Ok(Self::NewError),
            Op::NewBuiltinError => Ok(Self::NewBuiltinError),
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
    ) -> Result<Value, VmError> {
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
            ObjectProtocolValueOp::HasProperty => {
                if !value1.is_object_type() {
                    return Err(VmError::TypeMismatch);
                }
                result = self.coerce_property_key_value(stack, context, value0)?;
                if let (Some(proxy), Some(symbol)) =
                    (value1.as_proxy(), result.as_symbol(&self.gc_heap))
                    && symbol.is_private_name()
                {
                    Value::boolean(self.proxy_private_find(&proxy, symbol).is_some())
                } else {
                    let key = if let Some(symbol) = result.as_symbol(&self.gc_heap) {
                        VmPropertyKey::Symbol(symbol)
                    } else if let Some(string) = result.as_string(&self.gc_heap) {
                        VmPropertyKey::OwnedString(string.to_lossy_string(&self.gc_heap))
                    } else if let Some(number) = result.as_number() {
                        VmPropertyKey::OwnedString(number.to_display_string())
                    } else {
                        return Err(VmError::InvalidOperand);
                    };
                    Value::boolean(
                        self.ordinary_has_property_value(stack, context, value1, &key, 0)?,
                    )
                }
            }
            ObjectProtocolValueOp::GetPrototype => {
                if value0.is_proxy() {
                    self.ordinary_get_prototype_value(stack, context, value0, 0)?
                } else {
                    self.get_prototype_for_op(&value0)?
                }
            }
            ObjectProtocolValueOp::SetPrototype => {
                // Preserve the bytecode's current receiver/prototype matrix.
                // A Proxy receiver historically takes the trap-aware driver,
                // whose accepted prototype kinds are deliberately narrower
                // than the class-linking path used for ordinary objects.
                value1 = if value0.is_proxy() {
                    if value1.is_object() || value1.is_proxy() || value1.is_null() {
                        value1
                    } else if let Some(class) = value1.as_class_constructor() {
                        Value::object(class.statics(&self.gc_heap))
                    } else {
                        return Err(VmError::TypeMismatch);
                    }
                } else if value1.is_object()
                    || value1.is_proxy()
                    || value1.is_iterator()
                    || value1.is_null()
                    || value1.is_native_function()
                    || value1.is_function()
                    || value1.is_closure()
                    || value1.is_bound_function()
                {
                    value1
                } else if let Some(class) = value1.as_class_constructor() {
                    Value::object(class.statics(&self.gc_heap))
                } else {
                    return Err(VmError::TypeMismatch);
                };

                if value0.is_proxy() || value0.is_object() {
                    if !self.set_prototype_value_proxy_aware(stack, context, &value0, &value1)? {
                        return Err(
                            self.err_type(("Object.setPrototypeOf failed".to_string()).into())
                        );
                    }
                } else if value0.is_function()
                    || value0.is_closure()
                    || value0.is_bound_function()
                    || value0.is_native_function()
                    || value0.is_boolean()
                    || value0.is_number()
                    || value0.is_string()
                    || value0.is_symbol()
                    || value0.is_big_int()
                {
                    // Callable receiver metadata and primitive wrapper
                    // prototypes are intentionally unaffected by this
                    // internal bytecode operation.
                } else {
                    return Err(VmError::TypeMismatch);
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
    ) -> Result<Value, VmError> {
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
            | ScalarValueOp::NewError
            | ScalarValueOp::NewBuiltinError
            | ScalarValueOp::PrepareThrow => {
                // This operation consumes activation metadata and is completed
                // by `RuntimeCall::scalar_values`.
                return Err(VmError::InvalidOperand);
            }
            ScalarValueOp::ToObject => {
                if value0.is_nullish() {
                    return Err(VmError::TypeMismatch);
                }
                self.box_sloppy_this_primitive_runtime_rooted(value0, &[])?
            }
            ScalarValueOp::ToPropertyKey => {
                self.coerce_property_key_value(stack, context, value0)?
            }
            ScalarValueOp::TypeOf => {
                let tag = value0.typeof_string_with_heap(&self.gc_heap);
                Value::string(JsString::from_str(tag, &mut self.gc_heap)?)
            }
            ScalarValueOp::LoadNewTarget => new_target,
            ScalarValueOp::SameValue => {
                Value::boolean(abstract_ops::same_value(&value0, &value1, &self.gc_heap))
            }
            ScalarValueOp::IsArray => {
                let mut is_array = abstract_ops::is_array(&self.gc_heap, &value0)?;
                if !is_array
                    && let Some(object) = value0.as_object()
                    && self
                        .current_array_prototype_override()
                        .and_then(Value::as_object)
                        == Some(object)
                {
                    is_array = true;
                }
                Value::boolean(is_array)
            }
            ScalarValueOp::ArrayLength => {
                let array = value0.as_array().ok_or(VmError::TypeMismatch)?;
                Value::number(NumberValue::from_f64(
                    crate::array::len(array, &self.gc_heap) as f64,
                ))
            }
            ScalarValueOp::LoadLength => {
                let string = value0
                    .as_string(&self.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                Value::number_u32(string.len())
            }
        };
        Ok(result)
    }
}

impl RuntimeCall<'_> {
    pub(super) fn published_opcode(&self) -> Result<Op, VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let context = unsafe { self.context.as_ref() };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != instruction_pc {
            return Err(VmError::InvalidOperand);
        }
        Ok(function.op(instruction))
    }

    /// The published instruction's string-constant operand at `operand`.
    pub(super) fn published_const_index(&self, operand: u8) -> Result<u32, VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let context = unsafe { self.context.as_ref() };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != instruction_pc {
            return Err(VmError::InvalidOperand);
        }
        // Binding opcodes may carry more than the inline operand words (the
        // snapshot forms have five), so resolve through the owning CodeBlock,
        // which consults the overflow table.
        function
            .const_index(instruction, usize::from(operand))
            .ok_or(VmError::InvalidOperand)
    }

    /// The published instruction's signed immediate operand at `operand`.
    pub(super) fn published_imm32(&self, operand: u8) -> Result<i32, VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let context = unsafe { self.context.as_ref() };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != instruction_pc {
            return Err(VmError::InvalidOperand);
        }
        function
            .imm32(instruction, usize::from(operand))
            .ok_or(VmError::InvalidOperand)
    }

    /// Complete the exact published object-protocol site over boxed values.
    pub fn object_protocol_values(
        &mut self,
        value0: Value,
        value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let operation = self
            .published_opcode()
            .and_then(ObjectProtocolValueOp::from_opcode)
            .map_err(CommittedValueError::Fatal)?;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        vm.object_protocol_value(
            unsafe { &mut *self.stack.as_ptr() },
            unsafe { self.context.as_ref() },
            operation,
            value0,
            value1,
        )
        .map_err(CommittedValueError::JavaScript)
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
        if operation == ScalarValueOp::BindThisValue {
            self.bind_derived_this_value(value0)?;
            return Ok(value0);
        }
        if operation == ScalarValueOp::PrepareThrow {
            let vm = unsafe { &mut *self.vm.as_ptr() };
            vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
            if vm.pending_uncaught_frames.is_none() {
                vm.pending_uncaught_frames = Some(vm.snapshot_active_frames(
                    unsafe { self.context.as_ref() },
                    unsafe { self.stack.as_ref() },
                    usize::MAX,
                ));
            }
            return Ok(value0);
        }
        if matches!(
            operation,
            ScalarValueOp::NewError | ScalarValueOp::NewBuiltinError
        ) {
            let context = unsafe { self.context.as_ref() };
            let kind = if operation == ScalarValueOp::NewError {
                crate::ErrorKind::Error
            } else {
                let constant = self
                    .published_const_index(1)
                    .map_err(CommittedValueError::Fatal)?;
                context
                    .string_constant_str_for_function(self.function_id(), constant)
                    .and_then(crate::ErrorKind::from_class_name)
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?
            };
            let vm = unsafe { &mut *self.vm.as_ptr() };
            vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
            return vm
                .new_error_value(context, unsafe { &mut *self.stack.as_ptr() }, kind, value0)
                .map_err(CommittedValueError::JavaScript);
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
            unsafe { self.context.as_ref() },
            operation,
            value0,
            value1,
            new_target,
        )
        .map_err(CommittedValueError::JavaScript)
    }

    /// Convert one catchable VM failure into a JavaScript exception value.
    ///
    /// The exception-value side channel is consumed on return. Diagnostic
    /// uncaught-frame provenance is deliberately preserved until a local catch
    /// acknowledges that it absorbed the throw; propagating compiled frames
    /// therefore cannot erase the original nested getter/Proxy/call stack.
    /// A nested JavaScript throw is consumed from `pending_uncaught_throw`; an
    /// engine error is materialized directly. The caller owns the returned
    /// value and must either take a Machine landing or invoke the typed throw
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
            Some(unsafe { self.context.as_ref() }),
            unsafe { self.stack.as_ref() },
            &err,
        );
        if exception.is_some() {
            let _ = vm.take_error_detail();
        }
        exception.ok_or(err)
    }
}

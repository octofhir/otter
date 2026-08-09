//! Stack-owned scalar query and coercion operations.
//!
//! # Contents
//! - [`ScalarRuntimeOp`] is the decoded descriptor shared by every frame owner.
//! - [`RuntimeCall::scalar_op`] completes scalar bytecodes against the published
//!   activation without recovering an interpreter-frame index.
//!
//! # Invariants
//! - Inputs are copied from checked rooted registers before VM work begins.
//! - The destination is committed only after the complete operation succeeds.
//! - Observable coercion runs exactly once; errors are returned after the
//!   operation has committed no destination write.
//! - The published native frame remains the moving-GC root for every source.
//!
//! # See also
//! - [`crate::scalar_ops`] for interpreter-owned dispatch helpers.

use crate::{JsString, Value, VmError, abstract_ops, number::NumberValue};

use super::RuntimeCall;

/// Decoded scalar operation at the compiled-runtime boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarRuntimeOp {
    /// ECMAScript `ToObject` conversion.
    ToObject {
        /// Destination register.
        dst: u16,
        /// Source register.
        src: u16,
    },
    /// ECMAScript `ToPropertyKey` conversion.
    ToPropertyKey {
        /// Destination register.
        dst: u16,
        /// Source register.
        src: u16,
    },
    /// Materialize the `typeof` result string.
    TypeOf {
        /// Destination register.
        dst: u16,
        /// Source register.
        src: u16,
    },
    /// Read the activation's immutable `new.target` binding.
    LoadNewTarget {
        /// Destination register.
        dst: u16,
    },
    /// ECMAScript `SameValue` comparison.
    SameValue {
        /// Destination register.
        dst: u16,
        /// Left operand register.
        lhs: u16,
        /// Right operand register.
        rhs: u16,
    },
    /// ECMAScript `IsArray` query.
    IsArray {
        /// Destination register.
        dst: u16,
        /// Source register.
        src: u16,
    },
    /// Read one dense array's length.
    ArrayLength {
        /// Destination register.
        dst: u16,
        /// Array register.
        src: u16,
    },
    /// Read one string's UTF-16 code-unit length.
    LoadLength {
        /// Destination register.
        dst: u16,
        /// String register.
        src: u16,
    },
}

impl RuntimeCall<'_> {
    /// Complete one decoded scalar operation on either physical frame shape.
    pub fn scalar_op(&mut self, operation: ScalarRuntimeOp) -> Result<(), VmError> {
        // SAFETY: RuntimeCall brands exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let value = match operation {
            ScalarRuntimeOp::ToObject { src, .. } => {
                let value = self.read(src)?;
                if value.is_nullish() {
                    return Err(VmError::TypeMismatch);
                }
                // The source register stays published while wrapper allocation
                // may collect and relocate its primitive payload.
                vm.box_sloppy_this_primitive_stack_rooted(
                    unsafe { &mut *self.stack.as_ptr() },
                    value,
                    &[],
                )?
            }
            ScalarRuntimeOp::ToPropertyKey { src, .. } => {
                let value = self.read(src)?;
                let primitive = vm.evaluate_to_primitive(
                    unsafe { &mut *self.stack.as_ptr() },
                    unsafe { self.context.as_ref() },
                    &value,
                    abstract_ops::ToPrimitiveHint::String,
                )?;
                if primitive.as_symbol(&vm.gc_heap).is_some()
                    || primitive.as_string(&vm.gc_heap).is_some()
                {
                    primitive
                } else {
                    let text = primitive.display_string(&vm.gc_heap);
                    Value::string(JsString::from_str(&text, &mut vm.gc_heap)?)
                }
            }
            ScalarRuntimeOp::TypeOf { src, .. } => {
                let tag = self.read(src)?.typeof_string_with_heap(&vm.gc_heap);
                Value::string(JsString::from_str(tag, &mut vm.gc_heap)?)
            }
            ScalarRuntimeOp::LoadNewTarget { .. } => {
                self.with_frame(|frame| Ok(frame.new_target_value()))?
            }
            ScalarRuntimeOp::SameValue { lhs, rhs, .. } => Value::boolean(
                abstract_ops::same_value(&self.read(lhs)?, &self.read(rhs)?, &vm.gc_heap),
            ),
            ScalarRuntimeOp::IsArray { src, .. } => {
                let value = self.read(src)?;
                let mut result = abstract_ops::is_array(&vm.gc_heap, &value)?;
                if !result
                    && let Some(object) = value.as_object()
                    && vm
                        .current_array_prototype_override()
                        .and_then(Value::as_object)
                        == Some(object)
                {
                    result = true;
                }
                Value::boolean(result)
            }
            ScalarRuntimeOp::ArrayLength { src, .. } => {
                let array = self.read(src)?.as_array().ok_or(VmError::TypeMismatch)?;
                Value::number(NumberValue::from_f64(
                    crate::array::len(array, &vm.gc_heap) as f64
                ))
            }
            ScalarRuntimeOp::LoadLength { src, .. } => {
                let string = self
                    .read(src)?
                    .as_string(&vm.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                Value::number(NumberValue::from_i32(string.len() as i32))
            }
        };
        let dst = match operation {
            ScalarRuntimeOp::ToObject { dst, .. }
            | ScalarRuntimeOp::ToPropertyKey { dst, .. }
            | ScalarRuntimeOp::TypeOf { dst, .. }
            | ScalarRuntimeOp::LoadNewTarget { dst }
            | ScalarRuntimeOp::SameValue { dst, .. }
            | ScalarRuntimeOp::IsArray { dst, .. }
            | ScalarRuntimeOp::ArrayLength { dst, .. }
            | ScalarRuntimeOp::LoadLength { dst, .. } => dst,
        };
        self.write(dst, value)
    }
}

//! Array length and canonical Array prototype indexed definitions.
//!
//! # Contents
//! - One `ArraySetLength` algorithm for dense bodies and branded prototypes.
//! - Storage publication and descending deletion at each physical layout.
//! - Prototype index definitions with the same length preconditions.
//!
//! # Invariants
//! - Length coercion runs twice before descriptor validation, with both the
//!   target and candidate rooted across each observable conversion.
//! - A failed shrink restores length above the highest undeletable index and
//!   still applies a requested transition to non-writable length.
//! - Indexed publication checks length writability before adding a property.
//! - The intrinsic brand owns classification; existing prototype shape role
//!   excludes generated stores that would bypass Array definition semantics.
//!
//! # See also
//! - `super::define` owns value-level definition and Proxy fallthrough.
//! - `crate::array` owns dense and sparse Array storage.
//! - `crate::object::intrinsic_brand` owns canonical prototype identity.

use crate::activation_stack::ActivationStack;
use crate::native_abi::CommittedValueError;
use crate::{ExecutionContext, Interpreter, Value, VmError, array, object};

impl Interpreter {
    pub(super) fn define_array_length_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, target);
            let candidate = descriptor
                .value
                .map(|value| interp.scoped_value(scope, value));
            let new_length = if let Some(candidate) = candidate {
                let value = interp.escape_scoped(candidate);
                let number = crate::coerce::to_number_or_throw(interp, stack, context, &value)?;
                let length = crate::number::bitwise::to_uint32(number);
                let value = interp.escape_scoped(candidate);
                let number = crate::coerce::to_number_or_throw(interp, stack, context, &value)?;
                if f64::from(length) != number.as_f64() {
                    return Err(CommittedValueError::JavaScript(
                        interp.err_range("Invalid array length".into()),
                    ));
                }
                Some(length as usize)
            } else {
                None
            };
            if descriptor.is_accessor()
                || descriptor.configurable == Some(true)
                || descriptor.enumerable == Some(true)
            {
                return Ok(false);
            }
            let value = interp.escape_scoped(target);
            let old_length = interp
                .exact_array_length(value)
                .expect("Array length is canonical before a definition");
            let writable = interp.array_length_writable_value(value);
            let want_readonly = descriptor.writable == Some(false);
            let want_writable = descriptor.writable == Some(true);
            let Some(new_length) = new_length else {
                if !writable {
                    return Ok(!want_writable);
                }
                if want_readonly {
                    interp
                        .publish_array_length_writable(value, false)
                        .map_err(CommittedValueError::JavaScript)?;
                }
                return Ok(true);
            };
            if !writable {
                return Ok(new_length == old_length && !want_writable);
            }
            let deleted = interp
                .publish_array_length(value, new_length)
                .map_err(CommittedValueError::JavaScript)?;
            if want_readonly {
                interp
                    .publish_array_length_writable(interp.escape_scoped(target), false)
                    .map_err(CommittedValueError::JavaScript)?;
            }
            Ok(deleted)
        })
    }

    fn array_length_writable_value(&self, target: Value) -> bool {
        if let Some(array) = target.as_array() {
            return array::length_writable(array, &self.gc_heap);
        }
        let object = target.as_object().expect("canonical Array prototype");
        object::get_own_descriptor(object, &self.gc_heap, "length")
            .expect("canonical Array prototype length")
            .writable()
    }

    fn publish_array_length_writable(
        &mut self,
        target: Value,
        writable: bool,
    ) -> Result<(), VmError> {
        if let Some(array) = target.as_array() {
            array::set_length_writable(array, &mut self.gc_heap, writable);
            return Ok(());
        }
        let mut object = target.as_object().expect("canonical Array prototype");
        let accepted = self.define_own_property_partial(
            &mut object,
            "length",
            object::PartialPropertyDescriptor {
                writable: Some(writable),
                ..Default::default()
            },
        )?;
        assert!(accepted, "canonical Array length writable transition");
        Ok(())
    }

    fn publish_array_length(&mut self, target: Value, length: usize) -> Result<bool, VmError> {
        if let Some(array) = target.as_array() {
            return array::set_length_checked(array, &mut self.gc_heap, length)
                .map_err(VmError::from);
        }
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, target);
            let mut object = interp
                .escape_scoped(target)
                .as_object()
                .expect("canonical Array prototype");
            let old_length = interp.exact_array_length(Value::object(object)).unwrap();
            let accepted = interp.define_own_property_partial(
                &mut object,
                "length",
                object::PartialPropertyDescriptor {
                    value: Some(Value::number(crate::NumberValue::from_f64(length as f64))),
                    ..Default::default()
                },
            )?;
            assert!(accepted, "writable canonical Array length publication");
            interp.set_scoped(target, Value::object(object));
            if length >= old_length {
                return Ok(true);
            }
            let mut indices = object::with_properties(object, &interp.gc_heap, |properties| {
                properties
                    .keys()
                    .filter_map(object::array_index_property_name)
                    .filter(|index| (*index as usize) >= length)
                    .collect::<Vec<_>>()
            });
            indices.sort_unstable_by(|left, right| right.cmp(left));
            for index in indices {
                let mut object = interp.escape_scoped(target).as_object().unwrap();
                let deleted = object::delete(&mut object, &mut interp.gc_heap, &index.to_string())?;
                interp.set_scoped(target, Value::object(object));
                if !deleted {
                    let accepted = interp.define_own_property_partial(
                        &mut object,
                        "length",
                        object::PartialPropertyDescriptor {
                            value: Some(Value::number(crate::NumberValue::from_f64(
                                f64::from(index) + 1.0,
                            ))),
                            ..Default::default()
                        },
                    )?;
                    assert!(accepted, "failed Array shrink restores length");
                    interp.set_scoped(target, Value::object(object));
                    return Ok(false);
                }
            }
            Ok(true)
        })
    }

    pub(super) fn define_array_prototype_index(
        &mut self,
        target: Value,
        key: &str,
        index: u32,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, target);
            let value = descriptor
                .value
                .map(|value| interp.scoped_value(scope, value));
            let getter = descriptor
                .get
                .map(|value| interp.scoped_value(scope, value));
            let setter = descriptor
                .set
                .map(|value| interp.scoped_value(scope, value));
            let old_length = interp
                .exact_array_length(interp.escape_scoped(target))
                .unwrap();
            if index as usize >= old_length
                && !interp.array_length_writable_value(interp.escape_scoped(target))
            {
                return Ok(false);
            }
            let descriptor = object::PartialPropertyDescriptor {
                value: value.map(|value| interp.escape_scoped(value)),
                get: getter.map(|value| interp.escape_scoped(value)),
                set: setter.map(|value| interp.escape_scoped(value)),
                ..descriptor
            };
            let mut object = interp.escape_scoped(target).as_object().unwrap();
            let accepted = interp.define_own_property_partial(&mut object, key, descriptor)?;
            interp.set_scoped(target, Value::object(object));
            if accepted && index as usize >= old_length {
                let updated = interp
                    .publish_array_length(interp.escape_scoped(target), index as usize + 1)?;
                assert!(updated, "growing Array length cannot delete elements");
            }
            Ok(accepted)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Interpreter, Value};
    use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};

    #[test]
    fn named_prototype_length_store_normalizes_cold_and_shaped_receivers() {
        for shaped in [false, true] {
            let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
            let operands = [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(1),
                Operand::Register(2),
            ];
            let mut module =
                crate::test_support::minimal_bytecode_module("prototype-length-store.js");
            module.constants.push(Constant::String {
                utf16: "length".encode_utf16().collect(),
            });
            module.functions[0].locals = 3;
            let mut code = FunctionCodeBuilder::new();
            code.push(Op::StoreProperty, &operands);
            code.push(Op::ReturnUndefined, &[]);
            module.functions[0].code = code.finish();
            let context = interp
                .link_module(module, crate::source_registry::SourceRegistry::default())
                .expect("valid named store source");
            let mut prototype = interp.default_array_prototype().unwrap();
            if shaped {
                interp.migrate_slow_to_fast(&mut prototype);
                assert!(!crate::object::is_dictionary(prototype, &interp.gc_heap));
            }
            let source = crate::JsString::from_str("3", &mut interp.gc_heap).unwrap();
            let mut frame = interp
                .test_frame_for_function(context.function(context.function_base()).unwrap())
                .unwrap();
            frame.registers[0] = Value::object(prototype);
            frame.registers[1] = Value::string(source);
            let mut stack = crate::test_support::FrameChainFixture::new();
            stack.push(frame);
            assert!(
                interp
                    .drive_store_property(&mut stack, &context, operands.as_slice(), false)
                    .unwrap()
            );
            let prototype = stack[0].registers[0];
            assert_eq!(interp.exact_array_length(prototype), Some(3));
            assert_eq!(
                crate::object::get(prototype.as_object().unwrap(), &interp.gc_heap, "length")
                    .unwrap()
                    .as_number()
                    .unwrap()
                    .as_f64(),
                3.0
            );
            assert_eq!(
                stack[0].registers[1]
                    .as_string(&interp.gc_heap)
                    .unwrap()
                    .to_lossy_string(&interp.gc_heap),
                "3"
            );
            assert_eq!(stack[0].pc, 1);
        }
    }
}

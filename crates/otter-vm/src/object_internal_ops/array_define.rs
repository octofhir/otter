//! Array own descriptors, expando publication and canonical length definitions.
//!
//! # Contents
//! - One `ArraySetLength` algorithm for dense bodies and branded prototypes.
//! - One own-descriptor reader for indexed storage and shaped expandos.
//! - Scoped expando definitions through the ordinary Object descriptor owner.
//! - Integrity publication over indexed authority and ordinary bag attributes.
//! - Storage publication and descending deletion at each physical layout.
//! - Prototype index definitions with the same length preconditions.
//!
//! # Invariants
//! - Length coercion runs twice before descriptor validation, with both the
//!   target and candidate rooted across each observable conversion.
//! - A failed shrink restores length above the highest undeletable index and
//!   still applies a requested transition to non-writable length.
//! - Indexed publication checks length writability before adding a property.
//! - The Array owns extensibility and the original receiver; its expando owns
//!   only ordinary property storage and field write barriers.
//! - The intrinsic brand owns classification; existing prototype shape role
//!   excludes generated stores that would bypass Array definition semantics.
//!
//! # See also
//! - `super::define` owns value-level definition and Proxy fallthrough.
//! - `crate::array` owns dense and sparse Array storage.
//! - `crate::object::intrinsic_brand` owns canonical prototype identity.

use crate::activation_stack::ActivationStack;
use crate::native_abi::CommittedValueError;
use crate::{ExecutionContext, Interpreter, Value, VmError, VmPropertyKey, array, object};

impl Interpreter {
    pub(super) fn array_own_index_descriptor(
        &self,
        target: array::JsArray,
        index: u32,
    ) -> Option<object::PropertyDescriptor> {
        if let Some((getter, setter)) = array::get_index_accessor(target, &self.gc_heap, index) {
            return Some(object::PropertyDescriptor {
                kind: object::DescriptorKind::Accessor { getter, setter },
                flags: array::get_index_flags(target, &self.gc_heap, index)
                    .unwrap_or_else(|| object::PropertyFlags::new(false, true, true)),
            });
        }
        array::has_own_element(target, &self.gc_heap, index as usize).then(|| {
            object::PropertyDescriptor {
                kind: object::DescriptorKind::Data {
                    value: array::get(target, &self.gc_heap, index as usize),
                },
                flags: array::get_index_flags(target, &self.gc_heap, index)
                    .unwrap_or_else(object::PropertyFlags::data_default),
            }
        })
    }

    pub(crate) fn array_own_property_descriptor(
        &self,
        target: array::JsArray,
        key: &VmPropertyKey<'_>,
    ) -> Option<object::PropertyDescriptor> {
        match array::classify_property_key(key) {
            array::ArrayPropertyKey::Length => Some(object::PropertyDescriptor {
                kind: object::DescriptorKind::Data {
                    value: Value::number_u32(array::len(target, &self.gc_heap) as u32),
                },
                flags: array::length_flags(target, &self.gc_heap),
            }),
            array::ArrayPropertyKey::Index(index) => self.array_own_index_descriptor(target, index),
            array::ArrayPropertyKey::Expando => {
                let bag = array::expando_object(target, &self.gc_heap)?;
                match key {
                    VmPropertyKey::Symbol(symbol) => {
                        object::get_own_symbol_descriptor(bag, &self.gc_heap, *symbol)
                    }
                    _ => object::get_own_descriptor(bag, &self.gc_heap, key.string_name()?),
                }
            }
        }
    }

    /// [[Set]] of a named key on an Array with no named storage yet, when
    /// nothing on its default prototype chain intercepts the key (it is
    /// absent there, or a writable data property): the key becomes an own
    /// data property in a new expando bag, as OrdinarySet's
    /// CreateDataProperty step would. `false` leaves every other case to the
    /// full [[Set]].
    pub(crate) fn try_add_array_expando(
        &mut self,
        receiver: Value,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
        value: Value,
    ) -> Result<bool, VmError> {
        let Some(array) = receiver.as_array() else {
            return Ok(false);
        };
        if self.active_realm_id != 0
            || !array::is_ordinary_dense(array, &self.gc_heap)
            || !array::is_extensible(array, &self.gc_heap)
        {
            return Ok(false);
        }
        let vm_key = match key.symbol_handle() {
            Some(symbol) => {
                VmPropertyKey::Symbol(crate::symbol::JsSymbol::from_handle(&self.gc_heap, symbol))
            }
            None => VmPropertyKey::String(key.name()),
        };
        if !matches!(
            array::classify_property_key(&vm_key),
            array::ArrayPropertyKey::Expando
        ) {
            return Ok(false);
        }
        let Some(prototype) = self.default_array_prototype() else {
            return Ok(false);
        };
        match self.resolve_property_load(prototype, key) {
            crate::property_cache::PropertyLoad::Absent(_) => {}
            crate::property_cache::PropertyLoad::Data(resolved) if resolved.is_writable => {}
            _ => return Ok(false),
        }
        let descriptor = object::PartialPropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..object::PartialPropertyDescriptor::default()
        };
        self.define_array_expando_property(array, &vm_key, descriptor)
    }

    pub(crate) fn define_array_expando_property(
        &mut self,
        target: array::JsArray,
        key: &VmPropertyKey<'_>,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        debug_assert!(matches!(
            array::classify_property_key(key),
            array::ArrayPropertyKey::Expando
        ));
        if self.array_own_property_descriptor(target, key).is_none()
            && !array::is_extensible(target, &self.gc_heap)
        {
            return Ok(false);
        }
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, Value::array(target));
            let value = descriptor
                .value
                .map(|value| interp.scoped_value(scope, value));
            let getter = descriptor
                .get
                .map(|value| interp.scoped_value(scope, value));
            let setter = descriptor
                .set
                .map(|value| interp.scoped_value(scope, value));
            let symbol = match key {
                VmPropertyKey::Symbol(symbol) => {
                    Some(interp.scoped_value(scope, Value::symbol(*symbol)))
                }
                _ => None,
            };
            let bag = interp.ensure_array_expando(scope, target)?;
            let descriptor = object::PartialPropertyDescriptor {
                value: value.map(|value| interp.escape_scoped(value)),
                get: getter.map(|value| interp.escape_scoped(value)),
                set: setter.map(|value| interp.escape_scoped(value)),
                ..descriptor
            };
            let mut bag = interp
                .escape_scoped(bag)
                .as_object()
                .ok_or(VmError::InvalidOperand)?;
            let accepted = if let Some(symbol) = symbol {
                let symbol = interp
                    .escape_scoped(symbol)
                    .as_symbol(&interp.gc_heap)
                    .ok_or(VmError::InvalidOperand)?;
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut interp.gc_heap,
                    symbol,
                    descriptor,
                )
                .map_err(VmError::from)?
            } else {
                interp.define_own_property_partial(
                    &mut bag,
                    key.string_name().ok_or(VmError::InvalidOperand)?,
                    descriptor,
                )?
            };

            Ok(accepted)
        })
    }

    pub(crate) fn delete_array_own_property(
        &mut self,
        target: array::JsArray,
        key: &VmPropertyKey<'_>,
    ) -> Result<bool, VmError> {
        match array::classify_property_key(key) {
            array::ArrayPropertyKey::Length => Ok(false),
            array::ArrayPropertyKey::Index(index) => Ok(array::delete_index_property(
                target,
                &mut self.gc_heap,
                index,
            )),
            array::ArrayPropertyKey::Expando => {
                if self.array_own_property_descriptor(target, key).is_none() {
                    return Ok(true);
                }
                self.with_handle_scope(|interp, scope| {
                    let target = interp.scoped_value(scope, Value::array(target));
                    let array = interp
                        .escape_scoped(target)
                        .as_array()
                        .ok_or(VmError::InvalidOperand)?;
                    let mut bag = array::expando_object(array, &interp.gc_heap)
                        .ok_or(VmError::InvalidOperand)?;
                    let deleted = match key {
                        VmPropertyKey::Symbol(symbol) => {
                            object::delete_symbol(&mut bag, &mut interp.gc_heap, *symbol)?
                        }
                        _ => object::delete(
                            &mut bag,
                            &mut interp.gc_heap,
                            key.string_name().ok_or(VmError::InvalidOperand)?,
                        )?,
                    };

                    Ok(deleted)
                })
            }
        }
    }

    pub(crate) fn set_array_integrity_level(
        &mut self,
        target: array::JsArray,
        frozen: bool,
    ) -> Result<array::JsArray, VmError> {
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, Value::array(target));
            let array = interp
                .escape_scoped(target)
                .as_array()
                .ok_or(VmError::InvalidOperand)?;
            array::set_indexed_integrity_level(array, &mut interp.gc_heap, frozen)?;
            let array = interp
                .escape_scoped(target)
                .as_array()
                .ok_or(VmError::InvalidOperand)?;
            if let Some(bag) = array::expando_object(array, &interp.gc_heap) {
                if frozen {
                    interp.freeze_object(bag)?;
                } else {
                    interp.seal_object(bag)?;
                }
            }
            interp
                .escape_scoped(target)
                .as_array()
                .ok_or(VmError::InvalidOperand)
        })
    }

    pub(crate) fn test_array_integrity_level(&self, target: array::JsArray, frozen: bool) -> bool {
        if !array::test_indexed_integrity_level(target, &self.gc_heap, frozen) {
            return false;
        }
        let Some(bag) = array::expando_object(target, &self.gc_heap) else {
            return true;
        };
        let valid = |descriptor: object::PropertyDescriptor| {
            !descriptor.configurable()
                && (!frozen || !descriptor.is_data() || !descriptor.writable())
        };
        object::with_properties(bag, &self.gc_heap, |properties| {
            properties
                .keys()
                .all(|key| object::get_own_descriptor(bag, &self.gc_heap, key).is_some_and(valid))
                && properties
                    .symbol_keys()
                    .filter(|key| !key.is_private_name())
                    .all(|key| {
                        object::get_own_symbol_descriptor(bag, &self.gc_heap, key)
                            .is_some_and(valid)
                    })
        })
    }

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
            array::set_length_writable(array, &mut self.gc_heap, writable)?;
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

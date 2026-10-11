//! Scoped Object descriptor batches over live own-property keys.
//!
//! # Contents
//! - Own-key collection for Object and primitive descriptor sources.
//! - Descriptor preparation followed by the separate definition phase.
//!
//! # Invariants
//! - Every key and descriptor payload remains in the canonical handle arena
//!   through coercion, getters, Proxy traps, and later definitions.
//! - Only present own enumerable properties contribute descriptors, including
//!   Symbols; inherited properties and Array holes never contribute.
//! - All source reads and descriptor conversions finish before target writes.
//!
//! # See also
//! - [super]: Object.create and Object.defineProperties dispatch.
//! - [crate::object_internal_ops]: canonical own-key and descriptor semantics.

use super::property_key_label;
use crate::handles::HandleScope;
use crate::native_abi::CommittedValueError;
use crate::{
    ActivationStack, ExecutionContext, Interpreter, Local, VmError, VmGetOutcome, VmPropertyKey,
    object,
};
use smallvec::SmallVec;

struct ScopedDescriptor<'s> {
    key: Local<'s>,
    descriptor: object::PartialPropertyDescriptor,
    value: Option<Local<'s>>,
    getter: Option<Local<'s>>,
    setter: Option<Local<'s>>,
}

fn property_key(interp: &Interpreter, key: Local<'_>) -> Result<VmPropertyKey<'static>, VmError> {
    let key = interp.escape_scoped(key);
    if let Some(string) = key.as_string(interp.gc_heap()) {
        return Ok(VmPropertyKey::OwnedString(
            string.to_key_spelling(interp.gc_heap()),
        ));
    }
    if let Some(symbol) = key.as_symbol(interp.gc_heap()) {
        return Ok(VmPropertyKey::Symbol(symbol));
    }
    Err(VmError::InvalidOperand)
}

impl Interpreter {
    pub(super) fn define_properties_from_source<'s>(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        scope: &'s HandleScope,
        target: Local<'s>,
        source: Local<'s>,
    ) -> Result<(), CommittedValueError> {
        let source_value = self.escape_scoped(source);
        if source_value.is_nullish() {
            return Err(CommittedValueError::JavaScript(
                self.err_type("Cannot convert null or undefined to object".into()),
            ));
        }
        let keys = if source_value.is_object_type() {
            self.own_property_keys_value(stack, context, &source_value)?
                .into_iter()
                .map(|key| self.scoped_value(scope, key))
                .collect::<Vec<_>>()
        } else if let Some(string) = source_value.as_string(self.gc_heap()) {
            let length = string.len();
            let mut keys = Vec::with_capacity(length as usize);
            for index in 0..length {
                keys.push(
                    self.scoped_string(scope, &index.to_string())
                        .map_err(CommittedValueError::JavaScript)?,
                );
            }
            keys
        } else {
            Vec::new()
        };
        let mut prepared = Vec::with_capacity(keys.len());
        for key_handle in keys {
            let key = property_key(self, key_handle).map_err(CommittedValueError::Fatal)?;
            let source_value = self.escape_scoped(source);
            let own = self.ordinary_get_own_property_descriptor_value(
                stack,
                Some(context),
                source_value,
                &key,
                0,
            )?;
            if !own.is_some_and(|descriptor| descriptor.enumerable()) {
                continue;
            }
            let key = property_key(self, key_handle).map_err(CommittedValueError::Fatal)?;
            let source_value = self.escape_scoped(source);
            let outcome =
                self.ordinary_get_value(stack, Some(context), source_value, source_value, &key, 0)?;
            let descriptor_value = match outcome {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => self
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &getter,
                        self.escape_scoped(source),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call)?,
            };
            let mut descriptor =
                self.evaluate_to_property_descriptor(stack, Some(context), &descriptor_value)?;
            let value = descriptor
                .value
                .take()
                .map(|value| self.scoped_value(scope, value));
            let getter = descriptor
                .get
                .take()
                .map(|value| self.scoped_value(scope, value));
            let setter = descriptor
                .set
                .take()
                .map(|value| self.scoped_value(scope, value));
            prepared.push(ScopedDescriptor {
                key: key_handle,
                descriptor,
                value,
                getter,
                setter,
            });
        }
        for prepared in prepared {
            let key = property_key(self, prepared.key).map_err(CommittedValueError::Fatal)?;
            let descriptor = object::PartialPropertyDescriptor {
                value: prepared.value.map(|value| self.escape_scoped(value)),
                get: prepared.getter.map(|value| self.escape_scoped(value)),
                set: prepared.setter.map(|value| self.escape_scoped(value)),
                ..prepared.descriptor
            };
            if !self.define_own_property_value(
                stack,
                context,
                &self.escape_scoped(target),
                &key,
                descriptor,
            )? {
                let key = property_key(self, prepared.key).map_err(CommittedValueError::Fatal)?;
                return Err(CommittedValueError::JavaScript(
                    self.err_type(
                        format!(
                            "Cannot define property '{}'",
                            property_key_label(&key, self.gc_heap())
                        )
                        .into(),
                    ),
                ));
            }
        }
        Ok(())
    }
}

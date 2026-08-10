//! Stack-owned ordinary object-protocol operations.
//!
//! # Contents
//! - [`ObjectProtocolRuntimeOp`] describes decoded protocol operands.
//! - [`RuntimeCall::object_protocol_op`] completes non-reentrant ordinary
//!   `[[SetPrototypeOf]]` without materializing a generated frame.
//!
//! # Invariants
//! - Only an ordinary object receiver commits here; Proxy and exotic receivers
//!   refuse before effects and retain the canonical reentrant implementation.
//! - Operand values stay in the published native register window across the
//!   operation and no raw opcode reaches VM semantics.
//! - A rejected ordinary prototype mutation throws once and is never replayed.
//!
//! # See also
//! - [`crate::Interpreter::set_prototype_value_proxy_aware`]

use crate::{Value, VmError};

use super::RuntimeCall;

/// Decoded object-protocol operation accepted by a stack-owned activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectProtocolRuntimeOp {
    /// Set `object.[[Prototype]]` to `prototype`.
    SetPrototype {
        /// Ordinary object receiver register.
        object: u16,
        /// Prototype value register.
        prototype: u16,
    },
}

impl RuntimeCall<'_> {
    /// Complete one ordinary object-protocol operation. `false` refuses an
    /// exotic receiver before any mutation so the caller can side-exit.
    pub fn object_protocol_op(
        &mut self,
        operation: ObjectProtocolRuntimeOp,
    ) -> Result<bool, VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        match operation {
            ObjectProtocolRuntimeOp::SetPrototype { object, prototype } => {
                let receiver = self.read(object)?;
                if receiver.as_object().is_none() {
                    return Ok(false);
                }
                let raw_proto = self.read(prototype)?;
                let proto = if raw_proto.is_object()
                    || raw_proto.is_proxy()
                    || raw_proto.is_iterator()
                    || raw_proto.is_null()
                    || raw_proto.is_native_function()
                    || raw_proto.is_function()
                    || raw_proto.is_closure()
                    || raw_proto.is_bound_function()
                {
                    raw_proto
                } else if let Some(class) = raw_proto.as_class_constructor() {
                    Value::object(class.statics(&vm.gc_heap))
                } else {
                    return Err(VmError::TypeMismatch);
                };
                let ok = vm.set_prototype_value_proxy_aware(
                    unsafe { &mut *self.stack.as_ptr() },
                    unsafe { self.context.as_ref() },
                    &receiver,
                    &proto,
                )?;
                if !ok {
                    return Err(vm.err_type("Object.setPrototypeOf failed".to_string().into()));
                }
                Ok(true)
            }
        }
    }
}

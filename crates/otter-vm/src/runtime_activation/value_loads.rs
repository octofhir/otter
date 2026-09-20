//! Stack-owned static value loads.
//!
//! # Contents
//! - [`ValueLoadRuntimeOp`] describes namespace, literal, and string-index loads.
//! - [`RuntimeCall::value_load_op`] resolves and commits them through one rooted
//!   activation boundary.
//!
//! # Invariants
//! - Constant indexes are resolved against the active function context.
//! - Allocating results are committed only after successful construction.
//! - Receiver and index inputs remain rooted in the published register window
//!   across BigInt and string allocation.
//!
//! # See also
//! - [`crate::static_load_ops`]
//! - [`crate::constant_ops`]

use crate::{JsString, Value, VmError, math, symbol_dispatch, symbol_to_vm_error};

use super::RuntimeCall;

/// Decoded static value-load operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueLoadRuntimeOp {
    /// Load one `Math` namespace constant.
    Math {
        /// Destination register.
        dst: u16,
        /// Context string-constant index naming the property.
        name_index: u32,
    },
    /// Load one `Symbol` namespace static value.
    Symbol {
        /// Destination register.
        dst: u16,
        /// Context string-constant index naming the property.
        name_index: u32,
    },
    /// Reject the parked legacy `TemporalLoad` opcode.
    Temporal {
        /// Destination register from the legacy encoding.
        dst: u16,
        /// Context string-constant index from the legacy encoding.
        name_index: u32,
    },
    /// Materialize one BigInt literal.
    BigInt {
        /// Destination register.
        dst: u16,
        /// BigInt constant-pool index.
        constant_index: u32,
    },
    /// Load one UTF-16 code unit from a string.
    StringIndex {
        /// Destination register.
        dst: u16,
        /// String receiver register.
        receiver: u16,
        /// Numeric index register.
        index: u16,
    },
}

impl RuntimeCall<'_> {
    /// Complete one decoded static load without requiring a materialized frame.
    pub fn value_load_op(&mut self, operation: ValueLoadRuntimeOp) -> Result<(), VmError> {
        // SAFETY: RuntimeCall brands exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        // Constant indexes are chunk-local. `RuntimeCall::bind` has already
        // resolved this frame's owning chunk even when a generated direct call
        // entered it under a sibling chunk's activation.
        let context = &self.context;
        let (dst, value) = match operation {
            ValueLoadRuntimeOp::Math { dst, name_index } => {
                let name = context
                    .string_constant_str(name_index)
                    .ok_or(VmError::InvalidOperand)?;
                let value = math::load_constant(name)
                    .ok_or_else(|| vm.err_unknown_intrinsic(format!("Math.{name}").into()))?;
                (dst, value)
            }
            ValueLoadRuntimeOp::Symbol { dst, name_index } => {
                let name = context
                    .string_constant_str(name_index)
                    .ok_or(VmError::InvalidOperand)?;
                let value = symbol_dispatch::load_static(vm, name)
                    .map_err(|error| symbol_to_vm_error(vm, error))?;
                (dst, value)
            }
            ValueLoadRuntimeOp::Temporal { .. } => return Err(VmError::InvalidOperand),
            ValueLoadRuntimeOp::BigInt {
                dst,
                constant_index,
            } => (dst, vm.load_bigint_constant_value(context, constant_index)?),
            ValueLoadRuntimeOp::StringIndex {
                dst,
                receiver,
                index,
            } => {
                let receiver = self
                    .read(receiver)?
                    .as_string(&vm.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                let index = self.read(index)?;
                let index = if let Some(number) = index.as_number() {
                    match number.as_smi() {
                        Some(value) if value >= 0 => value as u32,
                        _ => receiver.len(),
                    }
                } else {
                    return Err(VmError::TypeMismatch);
                };
                let string = match receiver.char_code_at(index, &vm.gc_heap) {
                    Some(unit) => JsString::from_utf16_units(&[unit], &mut vm.gc_heap)?,
                    None => JsString::empty(&mut vm.gc_heap)?,
                };
                (dst, Value::string(string))
            }
        };
        self.write(dst, value)
    }
}

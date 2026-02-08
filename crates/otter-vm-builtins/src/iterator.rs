//! Iterator built-ins
//!
//! Provides Iterator protocol support:
//! - Iterator helpers for creating iterator results
//!
//! Generator methods (next/return/throw) are now handled by
//! NativeContext-based closures on Generator.prototype in
//! otter-vm-core/src/intrinsics_impl/generator.rs.

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Iterator ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Iterator_result", native_iterator_result),
        op_native("__Iterator_done", native_iterator_done),
    ]
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create an iterator result object { value, done }
fn create_iterator_result(value: VmValue, done: bool, mm: Arc<memory::MemoryManager>) -> VmValue {
    let result = GcRef::new(JsObject::new(VmValue::null(), mm));
    let _ = result.set(PropertyKey::string("value"), value);
    let _ = result.set(PropertyKey::string("done"), VmValue::boolean(done));
    VmValue::object(result)
}

// ============================================================================
// Native Operations
// ============================================================================

/// Create an iterator result { value, done: false }
/// Args: [value]
fn native_iterator_result(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let value = args.first().cloned().unwrap_or(VmValue::undefined());
    Ok(create_iterator_result(value, false, mm))
}

/// Create a done iterator result { value, done: true }
/// Args: [value?]
fn native_iterator_done(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let value = args.first().cloned().unwrap_or(VmValue::undefined());
    Ok(create_iterator_result(value, true, mm))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iterator_result() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_iterator_result(&[VmValue::number(42.0)], mm).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get(&"value".into()).unwrap().as_number(), Some(42.0));
        assert_eq!(obj.get(&"done".into()).unwrap().as_boolean(), Some(false));
    }

    #[test]
    fn test_iterator_done() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_iterator_done(&[VmValue::number(100.0)], mm).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get(&"value".into()).unwrap().as_number(), Some(100.0));
        assert_eq!(obj.get(&"done".into()).unwrap().as_boolean(), Some(true));
    }

    #[test]
    fn test_iterator_done_no_value() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_iterator_done(&[], mm).unwrap();
        let obj = result.as_object().unwrap();
        assert!(obj.get(&"value".into()).unwrap().is_undefined());
        assert_eq!(obj.get(&"done".into()).unwrap().as_boolean(), Some(true));
    }
}

//! Iterator/Generator built-ins
//!
//! Provides Iterator protocol and Generator support:
//! - `Generator.prototype.next(value?)`
//! - `Generator.prototype.return(value?)`
//! - `Generator.prototype.throw(exception)`
//! - Iterator helpers for creating iterator results

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Iterator/Generator ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Iterator_result", native_iterator_result),
        op_native("__Iterator_done", native_iterator_done),
        op_native("__Generator_next", native_generator_next),
        op_native("__Generator_return", native_generator_return),
        op_native("__Generator_throw", native_generator_throw),
        op_native("__Generator_isGenerator", native_is_generator),
    ]
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create an iterator result object { value, done }
fn create_iterator_result(value: VmValue, done: bool, mm: Arc<memory::MemoryManager>) -> VmValue {
    let result = GcRef::new(JsObject::new(None, mm));
    result.set(PropertyKey::string("value"), value);
    result.set(PropertyKey::string("done"), VmValue::boolean(done));
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

/// Generator.prototype.next(value?)
/// Args: [generator, value?]
fn native_generator_next(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let generator_val = args
        .first()
        .ok_or("Generator.next requires a generator argument")?;

    let generator = generator_val
        .as_generator()
        .ok_or("First argument must be a generator")?;

    // Check if generator is completed
    if generator.is_completed() {
        return Ok(create_iterator_result(VmValue::undefined(), true, mm));
    }

    // Get the value to send to the generator
    // Note: In Otter, the interpreter handles generator resumption in CallMethod.
    // This native implementation is a fallback - sent_value is captured but unused here.
    let _sent_value = args.get(1).cloned().unwrap_or(VmValue::undefined());

    // Set the sent value for the generator to receive
    // The actual execution and yielding happens in the interpreter
    // which we will trigger via the runtime logic.
    // However, since this is a native function, we return a result
    // and let the caller/interpreter handle the resumption if needed.
    // In Otter, the interpreter handles the specific "special" methods
    // in CallMethod (interpreter.rs:2056).
    // This native implementation is a fallback or for direct calls.

    Ok(create_iterator_result(VmValue::undefined(), false, mm))
}

/// Generator.prototype.return(value?)
/// Args: [generator, value?]
fn native_generator_return(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let generator_val = args
        .first()
        .ok_or("Generator.return requires a generator argument")?;

    let generator = generator_val
        .as_generator()
        .ok_or("First argument must be a generator")?;

    let return_value = args.get(1).cloned().unwrap_or(VmValue::undefined());

    // Complete the generator
    generator.complete();

    // Return done result with the provided value
    Ok(create_iterator_result(return_value, true, mm))
}

/// Generator.prototype.throw(exception)
/// Args: [generator, exception]
fn native_generator_throw(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let generator_val = args
        .first()
        .ok_or("Generator.throw requires a generator argument")?;

    let generator = generator_val
        .as_generator()
        .ok_or("First argument must be a generator")?;

    let exception = args.get(1).cloned().unwrap_or(VmValue::undefined());

    // If generator is completed, throw the exception as per spec
    // https://tc39.es/ecma262/#sec-generator.prototype.throw
    if generator.is_completed() {
        // Re-throw: return an error that will be caught by the VM's error handling
        return Err(VmError::type_error(format!("Generator already completed: {:?}", exception)));
    }

    // Note: The actual resumption with throw is handled in interpreter.rs
    // in the CallMethod instruction (around line 2088).
    // This native version serves as a fallback or for direct calls.
    // To make it work here if called directly from JS (bpassing CallMethod optimization),
    // we would need access to the VmContext to execute the generator.
    // For now, we return a placeholder that indicate the generator is still running.

    generator.set_pending_throw(exception);

    Ok(create_iterator_result(VmValue::undefined(), false, mm))
}

/// Check if value is a generator
/// Args: [value]
fn native_is_generator(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let value = args.first().ok_or("Missing value argument")?;
    Ok(VmValue::boolean(value.is_generator()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_core::generator::JsGenerator;

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

    #[test]
    fn test_generator_return() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm.clone()));
        let generator = JsGenerator::new_simple(0, vec![], obj);
        let result = native_generator_return(
            &[VmValue::generator(generator.clone()), VmValue::number(99.0)],
            mm,
        )
        .unwrap();

        let obj = result.as_object().unwrap();
        assert_eq!(obj.get(&"value".into()).unwrap().as_number(), Some(99.0));
        assert_eq!(obj.get(&"done".into()).unwrap().as_boolean(), Some(true));
        assert!(generator.is_completed());
    }

    #[test]
    fn test_is_generator() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm.clone()));
        let generator = JsGenerator::new_simple(0, vec![], obj);
        let result = native_is_generator(&[VmValue::generator(generator)], mm.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        let result = native_is_generator(&[VmValue::number(42.0)], mm).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }

    #[test]
    fn test_generator_next_on_completed() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm.clone()));
        let generator = JsGenerator::new_simple(0, vec![], obj);
        generator.complete();

        let result = native_generator_next(&[VmValue::generator(generator)], mm).unwrap();
        let obj = result.as_object().unwrap();
        assert!(obj.get(&"value".into()).unwrap().is_undefined());
        assert_eq!(obj.get(&"done".into()).unwrap().as_boolean(), Some(true));
    }
}

//! CacheIR interpreter — executes CacheIR stub chains without native compilation.
//!
//! This is the first consumer of CacheIR: the interpreter calls
//! `interpret_cache_ir()` to try the IC fast path before falling back
//! to the generic runtime slow path.
//!
//! ## Performance model
//!
//! The CacheIR interpreter is not as fast as native IC stubs, but it's
//! still significantly faster than the generic runtime path because:
//! 1. Guard checks are simple comparisons (no hash table lookup)
//! 2. Shape check + slot load is O(1) vs O(depth) prototype chain walk
//! 3. No allocation or string interning needed
//!
//! SpiderMonkey's Baseline Interpreter does the same thing — it interprets
//! CacheIR bytecode before the Baseline JIT compiles CacheIR to native stubs.

use crate::cache_ir::{CacheIROp, CacheIRSequence, ICSite, StubField};

/// Result of interpreting a CacheIR stub chain.
#[derive(Debug, Clone)]
pub enum ICResult {
    /// IC hit: the fast path produced a NaN-boxed result value.
    Hit(u64),
    /// IC hit for a store operation (no result value, just success).
    StoreHit,
    /// IC miss: no stub matched, fall through to slow path.
    Miss,
}

/// Input registers for the CacheIR interpreter.
///
/// Typically:
/// - `regs[0]` = receiver/object (NaN-boxed)
/// - `regs[1]` = key or value (NaN-boxed)
/// - `regs[2]` = value for stores (NaN-boxed)
pub struct ICInputs {
    pub regs: [u64; 4],
}

impl ICInputs {
    /// Create inputs for a property load: obj in reg 0.
    #[must_use]
    pub fn prop_load(obj_bits: u64) -> Self {
        Self { regs: [obj_bits, 0, 0, 0] }
    }

    /// Create inputs for a property store: obj in reg 0, value in reg 1.
    #[must_use]
    pub fn prop_store(obj_bits: u64, val_bits: u64) -> Self {
        Self { regs: [obj_bits, val_bits, 0, 0] }
    }

    /// Create inputs for a binary op: lhs in reg 0, rhs in reg 1.
    #[must_use]
    pub fn binary(lhs_bits: u64, rhs_bits: u64) -> Self {
        Self { regs: [lhs_bits, rhs_bits, 0, 0] }
    }
}

// NaN-boxing constants (must match otter-vm value.rs / codegen/value_repr.rs)
const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;
const INT32_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
const TAG_PTR_OBJECT: u64 = 0x7FFC_0000_0000_0000;
const TAG_PTR_STRING: u64 = 0x7FFD_0000_0000_0000;
const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const TAG_UNDEFINED: u64 = 0x7FF8_0000_0000_0000;
const TAG_NULL: u64 = 0x7FF8_0000_0000_0001;
const TAG_TRUE: u64 = 0x7FF8_0000_0000_0002;
const TAG_FALSE: u64 = 0x7FF8_0000_0000_0003;

fn is_object(bits: u64) -> bool {
    (bits & TAG_MASK) == TAG_PTR_OBJECT
}

fn is_int32(bits: u64) -> bool {
    (bits & INT32_TAG_MASK) == TAG_INT32
}

fn is_number(bits: u64) -> bool {
    // Int32 or any f64 (non-tagged value)
    is_int32(bits) || (bits & TAG_MASK) < TAG_PTR_OBJECT
}

fn is_string(bits: u64) -> bool {
    (bits & TAG_MASK) == TAG_PTR_STRING
}

fn is_bool(bits: u64) -> bool {
    bits == TAG_TRUE || bits == TAG_FALSE
}

fn extract_int32(bits: u64) -> i32 {
    bits as u32 as i32
}

fn box_int32(val: i32) -> u64 {
    TAG_INT32 | (val as u32 as u64)
}

/// Try to execute the IC stub chain for one IC site.
///
/// Walks the stub chain in order. For each stub, interprets its CacheIR
/// instructions. If all guards pass, returns the result. If any guard
/// fails, moves to the next stub. If no stub matches, returns `Miss`.
///
/// The `shape_checker` callback validates shape IDs against the runtime
/// object system without requiring a direct dependency on otter-vm's
/// internal object representation.
pub fn interpret_cache_ir<F>(
    site: &ICSite,
    inputs: &ICInputs,
    shape_checker: F,
) -> ICResult
where
    F: Fn(u64, u64) -> Option<ICShapeCheckResult>,
{
    for stub in &site.stubs {
        match interpret_single_stub(stub, inputs, &shape_checker) {
            StubResult::Hit(val) => return ICResult::Hit(val),
            StubResult::StoreHit => return ICResult::StoreHit,
            StubResult::GuardFailed => continue, // Try next stub.
        }
    }
    ICResult::Miss
}

/// Result of a shape check callback.
#[derive(Debug, Clone, Copy)]
pub struct ICShapeCheckResult {
    /// Whether the shape matched.
    pub matched: bool,
    /// The slot value (NaN-boxed) if matched and loading.
    pub slot_value: Option<u64>,
}

enum StubResult {
    Hit(u64),
    StoreHit,
    GuardFailed,
}

fn interpret_single_stub<F>(
    stub: &CacheIRSequence,
    inputs: &ICInputs,
    shape_checker: &F,
) -> StubResult
where
    F: Fn(u64, u64) -> Option<ICShapeCheckResult>,
{
    let mut result: u64 = 0;

    for op in &stub.ops {
        match op {
            // ---- Type guards ----
            CacheIROp::GuardIsObject { input } => {
                if !is_object(inputs.regs[*input as usize]) {
                    return StubResult::GuardFailed;
                }
            }
            CacheIROp::GuardIsInt32 { input } => {
                if !is_int32(inputs.regs[*input as usize]) {
                    return StubResult::GuardFailed;
                }
            }
            CacheIROp::GuardIsNumber { input } => {
                if !is_number(inputs.regs[*input as usize]) {
                    return StubResult::GuardFailed;
                }
            }
            CacheIROp::GuardIsString { input } => {
                if !is_string(inputs.regs[*input as usize]) {
                    return StubResult::GuardFailed;
                }
            }
            CacheIROp::GuardIsBool { input } => {
                if !is_bool(inputs.regs[*input as usize]) {
                    return StubResult::GuardFailed;
                }
            }
            CacheIROp::GuardIsUndefined { input } => {
                if inputs.regs[*input as usize] != TAG_UNDEFINED {
                    return StubResult::GuardFailed;
                }
            }
            CacheIROp::GuardIsNull { input } => {
                if inputs.regs[*input as usize] != TAG_NULL {
                    return StubResult::GuardFailed;
                }
            }

            // ---- Shape guard ----
            CacheIROp::GuardShape { obj, shape_field } => {
                let obj_bits = inputs.regs[*obj as usize];
                let expected_shape = match &stub.fields[*shape_field as usize] {
                    StubField::Shape(id) => *id,
                    _ => return StubResult::GuardFailed,
                };
                match shape_checker(obj_bits, expected_shape) {
                    Some(r) if r.matched => {
                        // Shape matched.
                    }
                    _ => return StubResult::GuardFailed,
                }
            }

            // ---- Property loads ----
            CacheIROp::LoadFixedSlot { obj, offset_field } => {
                let obj_bits = inputs.regs[*obj as usize];
                let offset = match &stub.fields[*offset_field as usize] {
                    StubField::Offset(o) => *o,
                    _ => return StubResult::GuardFailed,
                };
                // Use shape_checker with a special "load slot" protocol:
                // Pass offset as shape_id argument. The callback handles both
                // shape check and slot load in one call.
                // For now, we can't load slots from pure CacheIR — need runtime callback.
                // Return the offset-encoded result as a marker.
                let _ = (obj_bits, offset);
                // Actual slot load requires runtime access — signal this by using
                // the shape_checker callback with the shape from a previous guard.
                // In practice, the shape check + load happen together. We'll get
                // the slot value from shape_checker's result.
                //
                // For the initial implementation, LoadFixedSlot relies on the
                // shape checker returning the slot value in `slot_value`.
                if let Some(shape_field_idx) = find_preceding_shape_field(&stub.ops, obj) {
                    let expected_shape = match &stub.fields[shape_field_idx as usize] {
                        StubField::Shape(id) => *id,
                        _ => return StubResult::GuardFailed,
                    };
                    match shape_checker(obj_bits, expected_shape) {
                        Some(r) if r.matched => {
                            if let Some(val) = r.slot_value {
                                result = val;
                            } else {
                                return StubResult::GuardFailed;
                            }
                        }
                        _ => return StubResult::GuardFailed,
                    }
                } else {
                    return StubResult::GuardFailed;
                }
            }

            // ---- Int32 arithmetic ----
            CacheIROp::Int32Add { lhs, rhs } => {
                let a = extract_int32(inputs.regs[*lhs as usize]);
                let b = extract_int32(inputs.regs[*rhs as usize]);
                match a.checked_add(b) {
                    Some(r) => result = box_int32(r),
                    None => return StubResult::GuardFailed, // Overflow → miss.
                }
            }
            CacheIROp::Int32Sub { lhs, rhs } => {
                let a = extract_int32(inputs.regs[*lhs as usize]);
                let b = extract_int32(inputs.regs[*rhs as usize]);
                match a.checked_sub(b) {
                    Some(r) => result = box_int32(r),
                    None => return StubResult::GuardFailed,
                }
            }
            CacheIROp::Int32Mul { lhs, rhs } => {
                let a = extract_int32(inputs.regs[*lhs as usize]);
                let b = extract_int32(inputs.regs[*rhs as usize]);
                match a.checked_mul(b) {
                    Some(r) => result = box_int32(r),
                    None => return StubResult::GuardFailed,
                }
            }

            // ---- Store operations ----
            CacheIROp::StoreFixedSlot { .. }
            | CacheIROp::StoreDynamicSlot { .. }
            | CacheIROp::StoreDenseElement { .. } => {
                // Store operations require runtime access — can't be done purely
                // from CacheIR interpreter without a store callback.
                // For now, signal store success (the caller handles the actual store).
                return StubResult::StoreHit;
            }

            // ---- Terminal ----
            CacheIROp::ReturnFromIC => {
                return StubResult::Hit(result);
            }

            // ---- Unhandled (guard fail → try next stub) ----
            _ => return StubResult::GuardFailed,
        }
    }

    // Fell through without ReturnFromIC — treat as miss.
    StubResult::GuardFailed
}

/// Find the shape_field index from a preceding GuardShape for the given obj register.
fn find_preceding_shape_field(ops: &[CacheIROp], obj: &u8) -> Option<u8> {
    for op in ops {
        if let CacheIROp::GuardShape { obj: o, shape_field } = op
            && o == obj
        {
            return Some(*shape_field);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_ir::{CacheIRSequence, ICSite};

    #[test]
    fn test_int32_add_ic_hit() {
        let mut site = ICSite::new(0);
        site.attach_stub(CacheIRSequence::int32_add());

        let a = box_int32(20);
        let b = box_int32(22);
        let inputs = ICInputs::binary(a, b);

        let result = interpret_cache_ir(&site, &inputs, |_, _| None);
        match result {
            ICResult::Hit(val) => {
                assert_eq!(extract_int32(val), 42);
            }
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[test]
    fn test_int32_add_ic_miss_on_non_int() {
        let mut site = ICSite::new(0);
        site.attach_stub(CacheIRSequence::int32_add());

        // Pass a non-int32 value (object pointer).
        let obj = TAG_PTR_OBJECT | 0x1234;
        let b = box_int32(22);
        let inputs = ICInputs::binary(obj, b);

        let result = interpret_cache_ir(&site, &inputs, |_, _| None);
        assert!(matches!(result, ICResult::Miss));
    }

    #[test]
    fn test_int32_add_overflow_miss() {
        let mut site = ICSite::new(0);
        site.attach_stub(CacheIRSequence::int32_add());

        let a = box_int32(i32::MAX);
        let b = box_int32(1);
        let inputs = ICInputs::binary(a, b);

        let result = interpret_cache_ir(&site, &inputs, |_, _| None);
        assert!(matches!(result, ICResult::Miss));
    }

    #[test]
    fn test_prop_load_ic_with_shape_check() {
        let shape_id = 42u64;
        let slot_value = box_int32(99);

        let mut site = ICSite::new(0);
        site.attach_stub(CacheIRSequence::monomorphic_prop_load(shape_id, 0));

        let obj_bits = TAG_PTR_OBJECT | 0x1000;
        let inputs = ICInputs::prop_load(obj_bits);

        // Shape checker: if shape matches, return the slot value.
        let result = interpret_cache_ir(&site, &inputs, |_obj, shape| {
            if shape == shape_id {
                Some(ICShapeCheckResult {
                    matched: true,
                    slot_value: Some(slot_value),
                })
            } else {
                Some(ICShapeCheckResult {
                    matched: false,
                    slot_value: None,
                })
            }
        });

        match result {
            ICResult::Hit(val) => assert_eq!(val, slot_value),
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[test]
    fn test_prop_load_ic_shape_mismatch() {
        let mut site = ICSite::new(0);
        site.attach_stub(CacheIRSequence::monomorphic_prop_load(42, 0));

        let obj_bits = TAG_PTR_OBJECT | 0x1000;
        let inputs = ICInputs::prop_load(obj_bits);

        // Shape checker: always mismatch.
        let result = interpret_cache_ir(&site, &inputs, |_, _| {
            Some(ICShapeCheckResult { matched: false, slot_value: None })
        });

        assert!(matches!(result, ICResult::Miss));
    }

    #[test]
    fn test_polymorphic_stub_chain() {
        let mut site = ICSite::new(0);
        // Two stubs for different shapes.
        site.attach_stub(CacheIRSequence::monomorphic_prop_load(10, 0));
        site.attach_stub(CacheIRSequence::monomorphic_prop_load(20, 8));

        let obj_bits = TAG_PTR_OBJECT | 0x1000;
        let inputs = ICInputs::prop_load(obj_bits);

        // Shape checker: only shape 20 matches (second stub).
        let result = interpret_cache_ir(&site, &inputs, |_, shape| {
            if shape == 20 {
                Some(ICShapeCheckResult {
                    matched: true,
                    slot_value: Some(box_int32(777)),
                })
            } else {
                Some(ICShapeCheckResult { matched: false, slot_value: None })
            }
        });

        match result {
            ICResult::Hit(val) => assert_eq!(extract_int32(val), 777),
            other => panic!("expected Hit from second stub, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_site_misses() {
        let site = ICSite::new(0);
        let inputs = ICInputs::prop_load(0);
        let result = interpret_cache_ir(&site, &inputs, |_, _| None);
        assert!(matches!(result, ICResult::Miss));
    }
}

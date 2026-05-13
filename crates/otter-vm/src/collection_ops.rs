//! Collection constructor opcode helpers.
//!
//! The compiler still emits a compact `NewCollection` bytecode for collection
//! constructor fast paths. This module owns the synchronous construction tail
//! and seed decoding so the main VM loop can stay focused on dispatch.
//!
//! # Contents
//! - `Map`, `Set`, `WeakMap`, and `WeakSet` construction.
//! - Array-seed validation and entry insertion.
//!
//! # Invariants
//! - Inputs are decoded from executable operands.
//! - Helpers advance the current frame PC exactly once on success.
//! - Weak collection keys are validated by the collection backend and surface
//!   as VM type errors.
//!
//! # See also
//! - [`crate::collections`]
//! - [`crate::executable`]

use crate::{ExecutionContext, Frame, Interpreter, Value, VmError, read_register, write_register};

impl Interpreter {
    pub(crate) fn run_new_collection_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        kind_idx: u32,
        iter_reg: u16,
    ) -> Result<(), VmError> {
        let kind = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let seed = read_register(frame, iter_reg)?.clone();
        let value = build_collection(kind, &seed, &mut self.gc_heap)?;
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }
}

fn build_collection(
    kind: &str,
    seed: &Value,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    match kind {
        "Map" => {
            let m = crate::collections::alloc_map(gc_heap)?;
            if seed_is_present(seed) {
                let entries = seed_array(seed, gc_heap)?;
                for entry in entries {
                    let pair = match entry {
                        Value::Array(a) => a,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    if crate::array::len(pair, gc_heap) < 2 {
                        return Err(VmError::TypeMismatch);
                    }
                    crate::collections::map_set(
                        m,
                        gc_heap,
                        crate::array::get(pair, gc_heap, 0),
                        crate::array::get(pair, gc_heap, 1),
                    )?;
                }
            }
            Ok(Value::Map(m))
        }
        "Set" => {
            let s = crate::collections::alloc_set(gc_heap)?;
            if seed_is_present(seed) {
                for v in seed_array(seed, gc_heap)? {
                    crate::collections::set_add(s, gc_heap, v)?;
                }
            }
            Ok(Value::Set(s))
        }
        "WeakMap" => {
            let m = crate::collections::alloc_weak_map(gc_heap)?;
            if seed_is_present(seed) {
                for entry in seed_array(seed, gc_heap)? {
                    let pair = match entry {
                        Value::Array(a) => a,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    if crate::array::len(pair, gc_heap) < 2 {
                        return Err(VmError::TypeMismatch);
                    }
                    crate::collections::weak_map_set(
                        m,
                        gc_heap,
                        crate::array::get(pair, gc_heap, 0),
                        crate::array::get(pair, gc_heap, 1),
                    )
                    .map_err(|_| VmError::TypeMismatch)?;
                }
            }
            Ok(Value::WeakMap(m))
        }
        "WeakSet" => {
            let s = crate::collections::alloc_weak_set(gc_heap)?;
            if seed_is_present(seed) {
                for v in seed_array(seed, gc_heap)? {
                    crate::collections::weak_set_add(s, gc_heap, v)
                        .map_err(|_| VmError::TypeMismatch)?;
                }
            }
            Ok(Value::WeakSet(s))
        }
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("new {kind}"),
        }),
    }
}

fn seed_is_present(v: &Value) -> bool {
    !matches!(v, Value::Undefined | Value::Null)
}

fn seed_array(seed: &Value, gc_heap: &otter_gc::GcHeap) -> Result<Vec<Value>, VmError> {
    match seed {
        Value::Array(a) => Ok(crate::array::with_elements(*a, gc_heap, |elements| {
            elements.to_vec()
        })),
        _ => Err(VmError::TypeMismatch),
    }
}

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
use smallvec::SmallVec;

impl Interpreter {
    pub(crate) fn run_new_collection_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        dst: u16,
        kind_idx: u32,
        iter_reg: u16,
    ) -> Result<(), VmError> {
        let kind = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let frame = &stack[top_idx];
        let seed = *read_register(frame, iter_reg)?;
        let value = self.build_collection_with_stack_roots(kind, &seed, stack)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)?;
        frame.advance_pc(1)?;
        Ok(())
    }

    fn build_collection_with_stack_roots(
        &mut self,
        kind: &str,
        seed: &Value,
        stack: &SmallVec<[Frame; 8]>,
    ) -> Result<Value, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let seed_entries = if seed_is_present(seed) {
            seed_array(seed, &self.gc_heap)?
        } else {
            Vec::new()
        };
        match kind {
            "Map" => {
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    seed.trace_value_slots(visitor);
                    for entry in &seed_entries {
                        entry.trace_value_slots(visitor);
                    }
                };
                let mut m = crate::collections::alloc_map_with_roots(
                    &mut self.gc_heap,
                    &mut external_visit,
                )?;
                for entry in &seed_entries {
                    let pair = entry.as_array().ok_or(VmError::TypeMismatch)?;
                    if crate::array::len(pair, &self.gc_heap) < 2 {
                        return Err(VmError::TypeMismatch);
                    }
                    let key = crate::array::get(pair, &self.gc_heap, 0);
                    let value = crate::array::get(pair, &self.gc_heap, 1);
                    crate::collections::map_set_with_roots(
                        &mut m,
                        &mut self.gc_heap,
                        key,
                        value,
                        &mut external_visit,
                    )?;
                }
                Ok(Value::map(m))
            }
            "Set" => {
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    seed.trace_value_slots(visitor);
                    for entry in &seed_entries {
                        entry.trace_value_slots(visitor);
                    }
                };
                let mut s = crate::collections::alloc_set_with_roots(
                    &mut self.gc_heap,
                    &mut external_visit,
                )?;
                for v in &seed_entries {
                    crate::collections::set_add_with_roots(
                        &mut s,
                        &mut self.gc_heap,
                        *v,
                        &mut external_visit,
                    )?;
                }
                Ok(Value::set(s))
            }
            "WeakMap" => {
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    seed.trace_value_slots(visitor);
                    for entry in &seed_entries {
                        entry.trace_value_slots(visitor);
                    }
                };
                let mut m = crate::collections::alloc_weak_map_with_roots(
                    &mut self.gc_heap,
                    &mut external_visit,
                )?;
                for entry in &seed_entries {
                    let pair = entry.as_array().ok_or(VmError::TypeMismatch)?;
                    if crate::array::len(pair, &self.gc_heap) < 2 {
                        return Err(VmError::TypeMismatch);
                    }
                    let key = crate::array::get(pair, &self.gc_heap, 0);
                    let value = crate::array::get(pair, &self.gc_heap, 1);
                    crate::collections::weak_map_set_with_roots(
                        &mut m,
                        &mut self.gc_heap,
                        key,
                        value,
                        &mut external_visit,
                    )
                    .map_err(weak_collection_to_vm_error)?;
                }
                Ok(Value::weak_map(m))
            }
            "WeakSet" => {
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    seed.trace_value_slots(visitor);
                    for entry in &seed_entries {
                        entry.trace_value_slots(visitor);
                    }
                };
                let mut s = crate::collections::alloc_weak_set_with_roots(
                    &mut self.gc_heap,
                    &mut external_visit,
                )?;
                for v in &seed_entries {
                    crate::collections::weak_set_add_with_roots(
                        &mut s,
                        &mut self.gc_heap,
                        *v,
                        &mut external_visit,
                    )
                    .map_err(weak_collection_to_vm_error)?;
                }
                Ok(Value::weak_set(s))
            }
            _ => Err(VmError::UnknownIntrinsic {
                name: format!("new {kind}"),
            }),
        }
    }
}

fn seed_is_present(v: &Value) -> bool {
    !v.is_undefined() && !v.is_null()
}

fn seed_array(seed: &Value, gc_heap: &otter_gc::GcHeap) -> Result<Vec<Value>, VmError> {
    let arr = seed.as_array().ok_or(VmError::TypeMismatch)?;
    Ok(crate::array::with_elements(arr, gc_heap, |elements| {
        elements.to_vec()
    }))
}

fn weak_collection_to_vm_error(err: crate::collections::CollectionError) -> VmError {
    match err {
        crate::collections::CollectionError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        _ => VmError::TypeMismatch,
    }
}

//! Allocation opcode helpers.
//!
//! These helpers allocate VM heap objects for bytecode instructions that do not
//! alter the call stack. They sit behind the dense executable operand format so
//! the main dispatch loop can keep allocation tails out of `lib.rs`.
//!
//! # Contents
//! - Object literal allocation.
//! - Array literal allocation from variadic register operands.
//! - Array push helper used by spread/rest lowering.
//! - WeakRef and FinalizationRegistry allocation.
//!
//! # Invariants
//! - Inputs are decoded from executable operands.
//! - Helpers advance the current frame PC exactly once on success.
//!
//! # See also
//! - [`crate::array`]
//! - [`crate::object`]
//! - [`crate::executable`]

use crate::holt_stack::HoltStack;
use crate::{
    ExecutionContext, Frame, Interpreter, IteratorHandle, IteratorState, NativeCall, NativeFastFn,
    NativeFunction, Value, VmError,
    operand_decode::{const_operand, register_operand},
    read_register, regexp,
    runtime_state::RuntimeState,
    write_register,
};
use otter_bytecode::Operand;
use otter_gc::raw::RawGc;

impl Interpreter {
    pub(crate) fn collect_allocation_roots(&self, stack: &HoltStack) -> Vec<*mut RawGc> {
        // Fast path: when the dispatch loop has registered its frame-roots
        // and extra-roots providers, an allocation-triggered collection walks
        // both of them *internally* — see `GcHeap::collect_minor_internal`
        // (`visit_extra_roots_deduped` + `frame_root_providers.trace`) and the
        // full-GC `mark_phase`. Re-tracing the entire root set here, eagerly,
        // on *every* allocation (most of which never trigger a collection)
        // is pure waste — it dominated allocation-heavy workloads (e.g. the
        // RegExp property-escape harness building 100k-element strings). Hand
        // the heap an empty external set and let it pull roots lazily, only
        // when a collection actually fires.
        if self.gc_heap.has_frame_root_providers() {
            return Vec::new();
        }
        // Fallback: no frame-roots provider registered (out-of-band alloc with
        // no active dispatch loop). The GC cannot reach the live Rust frame
        // stack on its own, so snapshot the runtime + frame roots here to keep
        // a collection at this site sound.
        let mut roots = Vec::new();
        RuntimeState::new(self).trace_roots(&mut |slot| roots.push(slot));
        let pool = self.cold_frames();
        for frame in stack.iter() {
            frame.trace_frame_slots(&mut |slot| roots.push(slot));
            if let Some(idx) = frame.cold {
                pool.get(idx).trace_cold_slots(&mut |slot| roots.push(slot));
            }
        }
        roots
    }

    pub(crate) fn collect_runtime_roots(&self) -> Vec<*mut RawGc> {
        let mut roots = Vec::new();
        RuntimeState::new(self).trace_roots(&mut |slot| roots.push(slot));
        self.gc_heap
            .trace_frame_root_providers(&mut |slot| roots.push(slot));
        roots
    }

    pub(crate) fn collect_runtime_roots_without_shape_runtime(&self) -> Vec<*mut RawGc> {
        let mut roots = Vec::new();
        RuntimeState::new(self).trace_roots_without_shape_runtime(&mut |slot| roots.push(slot));
        roots
    }

    pub(crate) fn alloc_runtime_rooted_object_with_roots(
        &mut self,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::object::JsObject, VmError> {
        let roots = self.collect_runtime_roots();
        let shape_root = self.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::object::alloc_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            &mut external_visit,
        )
        .map_err(VmError::from)
    }

    /// Allocate a host-created object while exposing runtime roots and
    /// caller-owned pending values.
    ///
    /// Runtime integration code uses this instead of borrowing the raw GC heap
    /// when it creates JS objects outside a VM frame stack.
    pub fn alloc_host_object_with_roots(
        &mut self,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::object::JsObject, otter_gc::OutOfMemory> {
        let roots = self.collect_runtime_roots();
        let shape_root = self.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::object::alloc_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            &mut external_visit,
        )
    }

    /// Allocate a deferred module namespace exotic object (TC39 import
    /// defer): a null-proto object tagged with the target module URL and
    /// carrying `@@toStringTag` = "Module". It stays extensible until the
    /// module is evaluated and export properties are installed, after
    /// which it is made non-extensible.
    pub(crate) fn alloc_deferred_namespace_object(
        &mut self,
        target_url: std::sync::Arc<str>,
    ) -> Result<crate::object::JsObject, otter_gc::OutOfMemory> {
        let roots = self.collect_runtime_roots();
        let shape_root = self.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let obj = crate::object::alloc_host_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            crate::object::DeferredNamespaceData {
                target_url,
                populated: std::cell::Cell::new(false),
            },
            &mut external_visit,
        )?;
        let tag_sym = self
            .well_known_symbols
            .get(crate::symbol::WellKnown::ToStringTag);
        let module_str = crate::JsString::from_str("Deferred Module", &mut self.gc_heap)?;
        crate::object::define_own_symbol_property(
            obj,
            &mut self.gc_heap,
            tag_sym,
            crate::object::PropertyDescriptor::data(
                crate::Value::string(module_str),
                false,
                false,
                false,
            ),
        );
        Ok(obj)
    }

    /// Allocate a Module Namespace Exotic Object (ECMA-262 §10.4.6): a
    /// null-proto, non-extensible object carrying `@@toStringTag` =
    /// "Module" and a [`crate::object::ModuleNamespaceData`] pointing at
    /// the wrapped module environment `env`. Property reads resolve live
    /// through `env`; writes/defines/deletes fail (enforced by the
    /// namespace MOP forks in `object_internal_ops`).
    pub(crate) fn alloc_module_namespace_object(
        &mut self,
        env: crate::object::JsObject,
        module_url: std::sync::Arc<str>,
    ) -> Result<crate::object::JsObject, otter_gc::OutOfMemory> {
        let roots = self.collect_runtime_roots();
        let shape_root = self.shape_root();
        let env_value = Value::object(env);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            env_value.trace_value_slots(visitor);
        };
        let obj = crate::object::alloc_host_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            crate::object::ModuleNamespaceData { env, module_url },
            &mut external_visit,
        )?;
        let tag_sym = self
            .well_known_symbols
            .get(crate::symbol::WellKnown::ToStringTag);
        let module_str = crate::JsString::from_str("Module", &mut self.gc_heap)?;
        crate::object::define_own_symbol_property(
            obj,
            &mut self.gc_heap,
            tag_sym,
            crate::object::PropertyDescriptor::data(
                crate::Value::string(module_str),
                false,
                false,
                false,
            ),
        );
        // §10.4.6 namespaces are non-extensible from creation.
        crate::object::prevent_extensions(obj, &mut self.gc_heap);
        Ok(obj)
    }

    pub(crate) fn alloc_runtime_rooted_object_with_proto(
        &mut self,
        proto: crate::object::JsObject,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::object::JsObject, VmError> {
        let proto_value = Value::object(proto);
        let roots = self.collect_runtime_roots();
        let shape_root = self.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            proto_value.trace_value_slots(visitor);
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        let object = crate::object::alloc_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            &mut external_visit,
        )
        .map_err(VmError::from)?;
        crate::object::set_prototype(object, &mut self.gc_heap, Some(proto));
        Ok(object)
    }

    pub(crate) fn alloc_runtime_rooted_array_from_values<I>(
        &mut self,
        elements: I,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::array::JsArray, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::array::from_vec_with_roots(&mut self.gc_heap, elements, &mut external_visit)
            .map_err(VmError::from)
    }

    /// Allocate a host-created array while exposing runtime roots and
    /// caller-owned pending values.
    ///
    /// The array payload itself is traced by the GC allocation API; `slice_roots`
    /// covers sibling buffers and host-local values that are not part of the
    /// returned array.
    pub fn array_from_elements_host_rooted<I>(
        &mut self,
        elements: I,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::array::JsArray, otter_gc::OutOfMemory>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::array::from_vec_with_roots(&mut self.gc_heap, elements, &mut external_visit)
    }

    /// Allocate a host-created static native function while exposing
    /// runtime roots and caller-owned pending values.
    pub fn native_function_static_host_rooted(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, otter_gc::OutOfMemory> {
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        Ok(Value::native_function(
            NativeFunction::new_static_with_roots(
                &mut self.gc_heap,
                name,
                length,
                call,
                &mut external_visit,
            )?,
        ))
    }

    /// Allocate a host-created native function while exposing runtime
    /// roots and caller-owned pending values.
    pub fn native_function_from_call_host_rooted(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeCall,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, otter_gc::OutOfMemory> {
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        Ok(Value::native_function(
            NativeFunction::from_call_with_roots(
                &mut self.gc_heap,
                name,
                length,
                call,
                &mut external_visit,
            )?,
        ))
    }

    /// Allocate a host-created native constructor while exposing runtime roots
    /// and caller-owned pending values.
    pub fn native_constructor_from_call_host_rooted(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeCall,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, otter_gc::OutOfMemory> {
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        Ok(Value::native_function(
            NativeFunction::from_constructor_call_with_roots(
                &mut self.gc_heap,
                name,
                length,
                call,
                &mut external_visit,
            )?,
        ))
    }

    pub(crate) fn alloc_runtime_rooted_iterator_state(
        &mut self,
        state: IteratorState,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<IteratorHandle, VmError> {
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        // Old-space: iterator handles are copied into Rust locals across
        // GC-bearing calls (IteratorStep drains, native `next` bodies), so
        // the cell must never move under a young-space scavenge.
        self.gc_heap
            .alloc_old_with_roots(state, &mut external_visit)
            .map_err(VmError::from)
    }

    pub(crate) fn make_runtime_rooted_iter_result(
        &mut self,
        value: Value,
        done: bool,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let mut roots = Vec::with_capacity(value_roots.len() + 1);
        roots.push(&value);
        roots.extend_from_slice(value_roots);
        let obj = self.alloc_runtime_rooted_object_with_roots(&roots, slice_roots)?;
        // §7.4.12 CreateIteratorResultObject — OrdinaryObjectCreate
        // from %Object.prototype%.
        if let Some(object_proto) = self.object_prototype_object_opt() {
            crate::object::set_prototype(obj, &mut self.gc_heap, Some(object_proto));
        }
        self.set_property(obj, "value", value)?;
        self.set_property(obj, "done", Value::boolean(done))?;
        Ok(Value::object(obj))
    }

    pub(crate) fn alloc_stack_rooted_object(
        &mut self,
        stack: &HoltStack,
    ) -> Result<crate::object::JsObject, VmError> {
        self.alloc_stack_rooted_object_with_extra_roots(stack, &[])
    }

    pub(crate) fn alloc_stack_rooted_object_with_extra_roots(
        &mut self,
        stack: &HoltStack,
        extra_roots: &[&Value],
    ) -> Result<crate::object::JsObject, VmError> {
        self.alloc_stack_rooted_object_with_value_roots(stack, extra_roots, &[])
    }

    pub(crate) fn alloc_stack_rooted_object_with_value_roots(
        &mut self,
        stack: &HoltStack,
        value_roots: &[&Value],
        slice_roots: &[Value],
    ) -> Result<crate::object::JsObject, VmError> {
        self.alloc_stack_rooted_object_with_value_roots_and_slices(
            stack,
            value_roots,
            &[slice_roots],
        )
    }

    pub(crate) fn alloc_stack_rooted_object_with_value_roots_and_slices(
        &mut self,
        stack: &HoltStack,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::object::JsObject, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let shape_root = self.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::object::alloc_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            &mut external_visit,
        )
        .map_err(VmError::from)
    }

    pub(crate) fn alloc_stack_rooted_object_with_proto(
        &mut self,
        stack: &HoltStack,
        proto: crate::object::JsObject,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::object::JsObject, VmError> {
        let proto_value = Value::object(proto);
        let roots = self.collect_allocation_roots(stack);
        let shape_root = self.shape_root();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            proto_value.trace_value_slots(visitor);
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        let object = crate::object::alloc_object_with_shape_roots(
            &mut self.gc_heap,
            shape_root,
            &mut external_visit,
        )
        .map_err(VmError::from)?;
        crate::object::set_prototype(object, &mut self.gc_heap, Some(proto));
        Ok(object)
    }

    pub(crate) fn alloc_stack_rooted_array(
        &mut self,
        stack: &HoltStack,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::array::JsArray, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::array::alloc_array_with_roots(&mut self.gc_heap, &mut external_visit)
            .map_err(VmError::from)
    }

    pub(crate) fn alloc_stack_rooted_array_from_values<I>(
        &mut self,
        stack: &HoltStack,
        elements: I,
        value_roots: &[&Value],
        slice_roots: &[Value],
    ) -> Result<crate::array::JsArray, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        self.alloc_stack_rooted_array_from_values_with_root_slices(
            stack,
            elements,
            value_roots,
            &[slice_roots],
        )
    }

    pub(crate) fn alloc_stack_rooted_array_from_values_with_root_slices<I>(
        &mut self,
        stack: &HoltStack,
        elements: I,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<crate::array::JsArray, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        crate::array::from_vec_with_roots(&mut self.gc_heap, elements, &mut external_visit)
            .map_err(VmError::from)
    }

    pub(crate) fn alloc_stack_rooted_iterator_state(
        &mut self,
        stack: &HoltStack,
        state: IteratorState,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<IteratorHandle, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in value_roots {
                value.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        // Old-space for the same reason as
        // `alloc_runtime_rooted_iterator_state`: the handle outlives this
        // call inside Rust locals that a moving scavenge cannot rewrite.
        self.gc_heap
            .alloc_old_with_roots(state, &mut external_visit)
            .map_err(VmError::from)
    }

    fn alloc_stack_rooted_array_from_vec(
        &mut self,
        stack: &HoltStack,
        elements: Vec<Value>,
    ) -> Result<crate::array::JsArray, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        crate::array::from_vec_with_roots(&mut self.gc_heap, elements, &mut external_visit)
            .map_err(VmError::from)
    }

    pub(crate) fn run_new_object_reg(
        &mut self,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        let shape_root = self.shape_root();
        let obj = match crate::object::try_alloc_object_with_shape_no_collect(
            &mut self.gc_heap,
            shape_root,
        ) {
            Some(obj) => obj,
            None => self.alloc_stack_rooted_object(stack)?,
        };
        // Allocate first, THEN read `%Object.prototype%`. The slow allocation
        // can trigger a scavenge that relocates the realm prototype while
        // it is still young; reading the handle beforehand would capture a
        // stale offset and install a dangling `[[Prototype]]` on the new
        // object (corrupting the proto chain — only on the alloc that
        // happens to drive the GC). `object_prototype_object_opt` reads it
        // from the always-traced realm-intrinsic table, so post-alloc it
        // yields the relocated handle.
        if let Some(proto) = self.object_prototype_object_opt() {
            crate::object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        }
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::object(obj))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_new_array_operands(
        &mut self,
        stack: &mut HoltStack,
        top_idx: usize,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let count = const_operand(operands.get(1))? as usize;
        let mut elements: Vec<Value> = Vec::with_capacity(count);
        {
            let frame = &stack[top_idx];
            for i in 0..count {
                let r = register_operand(operands.get(2 + i))?;
                elements.push(*read_register(frame, r)?);
            }
        }
        let array = self.alloc_stack_rooted_array_from_vec(stack, elements)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::array(array))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_load_regexp_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let (pattern_utf16, flags) = context
            .regexp_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        // A regex literal evaluates to a fresh RegExp each time, but the
        // *compiled* program is a pure function of pattern + flags, so
        // resolve it through the per-isolate compile cache (a disjoint
        // field borrow with `gc_heap`) to skip re-parsing on every eval.
        let regex = regexp::JsRegExp::compile_cached(
            &mut self.gc_heap,
            &mut self.regex_compile_cache,
            pattern_utf16,
            flags,
        )
        .map_err(|e| self.err_invalid_regexp((e.to_string()).into()))?;
        write_register(frame, dst, Value::regexp(regex))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_array_push_regs(
        &mut self,
        stack: &mut HoltStack,
        top_idx: usize,
        arr_reg: u16,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let value = *read_register(frame, value_reg)?;
        let array = read_register(frame, arr_reg)?
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        crate::array::push_with_roots(array, &mut self.gc_heap, value, &mut external_visit)?;
        let frame = &mut stack[top_idx];
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_new_weak_ref_regs(
        &mut self,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        target_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let target = *read_register(frame, target_reg)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let weak_ref = crate::weak_refs::alloc_weak_ref_with_roots(
            &mut self.gc_heap,
            &target,
            &mut external_visit,
        )?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::weak_ref(weak_ref))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_new_finalization_registry_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        callback_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let callback = *read_register(frame, callback_reg)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let registry = crate::weak_refs::alloc_finalization_registry_with_context_and_roots(
            &mut self.gc_heap,
            callback,
            Some(context.clone()),
            &mut external_visit,
        )?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::finalization_registry(registry))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }
}

//! Runtime entries generated code calls when a guard misses.
//!
//! # Contents
//! - Property, element and global load/store completions.
//! - Object and array construction.
//! - Inline-cache cell fills and the write barrier.
//!
//! # Invariants
//! - Every entry completes the whole observable operation, so a miss never
//!   leaves a half-performed effect for the interpreter to finish.

use crate::activation_stack::ActivationStack;
use crate::{
    ActiveFrameMut, ActiveFrameRef, ExecutionContext, Interpreter, JsObject, Value, VmError,
    VmPropertyKey, cache_ir, object, property_atom::AtomizedPropertyKey,
    property_ic::PropertyIcKind, read_register, rooting::RootScopeExt, value_kind_name,
};

impl Interpreter {
    /// Resolve a compiled `LoadProperty` against the canonical activation.
    ///
    /// Writes the result into `frame[dst]` and returns the WhiskerIC cell-fill
    /// (`0` = no inline). Non-object, accessor, prototype, proxy, exotic, and
    /// megamorphic misses complete through the full `[[Get]]` ladder; this
    /// transition never requests an exact side exit.
    ///
    pub fn jit_runtime_load_property(
        &mut self,
        context: &ExecutionContext,
        frame: &mut crate::ActiveFrameMut<'_>,
        stack: &mut ActivationStack,
        function_id: u32,
        dst: u16,
        obj_reg: u16,
        name_idx: u32,
        site: usize,
    ) -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
        self.record_jit_runtime_property_stub();
        let atomized_key = context
            .property_atom_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = frame.read(obj_reg)?;
        // Non-object receivers (primitives, proxies) and saturated sites
        // complete through the full resolution cascade below; the IC layers
        // only serve cache-representable ordinary-object loads.
        let full_get = |vm: &mut Self,
                        frame: &mut crate::ActiveFrameMut<'_>,
                        stack: &mut ActivationStack|
         -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
            // Re-read the receiver from the traced activation: the IC probes
            // above may have allocated (cache-stub install) and moved the
            // handle read at entry.
            let receiver = frame.read(obj_reg)?;
            let value = vm.load_property_value(context, stack, receiver, atomized_key.name())?;
            frame.write(dst, value)?;
            Ok(None)
        };
        let Some(obj) = receiver.as_object() else {
            return full_get(self, frame, stack);
        };
        if self
            .feedback_directory
            .property_is_megamorphic(site, PropertyIcKind::Load)
            != Some(false)
        {
            // A saturated site still reads one slot per receiver class. The
            // shared `(shape, atom)` table answers it without the ladder; only
            // a pair nothing has resolved falls through.
            if let Some(resolved) = self.resolve_property_data_slot(obj, atomized_key) {
                self.feedback_directory
                    .record_property_hit(PropertyIcKind::Load);
                frame.write(dst, resolved.value)?;
                return Ok(None);
            }
            return full_get(self, frame, stack);
        }
        if let Some(value) =
            self.feedback_directory
                .probe_load(site, obj, &self.gc_heap, atomized_key)
        {
            self.feedback_directory
                .record_property_hit(PropertyIcKind::Load);
            // Compute feedback before committing the destination: register
            // allocation may legally alias `dst` with `obj_reg` (common in an
            // optimizing OSR transition). Reading the receiver after the write
            // would then inspect the loaded property value as an object.
            let fill = self.whisker_load_cell_fill(site, obj, atomized_key);
            frame.write(dst, value)?;
            return Ok(fill);
        }
        if self
            .feedback_directory
            .property_entry_count(site, PropertyIcKind::Load)
            .unwrap_or_default()
            > 0
        {
            self.feedback_directory
                .record_property_guard_miss(site, PropertyIcKind::Load);
        } else {
            self.feedback_directory
                .record_property_uncached_miss(site, PropertyIcKind::Load);
        }
        // A dictionary-mode receiver has no hidden class for a guard to name, so
        // no cache program can describe an access on it. Bootstrap namespace
        // objects are built that way; migrate the receiver onto the shaped path,
        // then re-read it — the migration allocates and may relocate it.
        let mut migrating = obj;
        self.migrate_slow_to_fast(&mut migrating);
        let obj = migrating;
        if let Some(resolved) = self.resolve_property_data_slot(obj, atomized_key) {
            if self
                .feedback_directory
                .property_is_megamorphic(site, PropertyIcKind::Load)
                == Some(false)
            {
                let ic = cache_ir::CacheStub::from_resolved_load(
                    object::shape_id(obj, &self.gc_heap),
                    &resolved,
                );
                self.feedback_directory
                    .install_property_stub(site, PropertyIcKind::Load, ic);
            }
            // `dst` may alias the receiver in an optimized register window;
            // capture its relocated object identity before the commit.
            let current_obj = frame
                .read(obj_reg)?
                .as_object()
                .ok_or(VmError::InvalidOperand)?;
            let fill = self.whisker_load_cell_fill(site, current_obj, atomized_key);
            frame.write(dst, resolved.value)?;
            return Ok(fill);
        }
        // Not cache-representable (accessor, deep prototype, absent):
        // complete the load in place through the full cascade.
        full_get(self, frame, stack)
    }

    /// Canonical-activation `StoreProperty` — the
    /// [`Self::jit_runtime_load_property`] counterpart. Resolves existing-own-data stores and ordinary shape
    /// transitions through the current IC/data path. Every non-cacheable miss
    /// completes through [`Self::store_property_value`], including accessors,
    /// exotics, proxies, primitive bases, megamorphic sites, and exceptions.
    ///
    pub fn jit_runtime_store_property(
        &mut self,
        context: &ExecutionContext,
        frame: &mut crate::ActiveFrameMut<'_>,
        stack: &mut ActivationStack,
        function_id: u32,
        obj_reg: u16,
        name_idx: u32,
        src: u16,
        site: usize,
    ) -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
        self.record_jit_runtime_property_stub();
        let atomized_key = context
            .property_atom_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = frame.read(obj_reg)?;
        let value = frame.read(src)?;
        let full_set = |vm: &mut Self,
                        frame: &mut crate::ActiveFrameMut<'_>,
                        stack: &mut ActivationStack|
         -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
            // Re-read both values from the published, traced window. IC setup
            // and property-key materialisation may allocate and move either
            // operand before the semantic slow path begins.
            let receiver = frame.read(obj_reg)?;
            let value = frame.read(src)?;
            let strict = context.function_is_strict(function_id);
            vm.store_property_value(context, stack, receiver, atomized_key.name(), value, strict)?;
            Ok(None)
        };
        let Some(obj) = receiver.as_object() else {
            return full_set(self, frame, stack);
        };
        if !object::supports_fast_property_ic(obj, &self.gc_heap) {
            return full_set(self, frame, stack);
        }
        if let Some(entries_len) = self
            .feedback_directory
            .property_entry_count(site, PropertyIcKind::Store)
        {
            // An installed stub's guards are the authority for its own outcome
            // — an own writable data slot or a captured add-transition — so a
            // probe hit needs no semantic resolution, exactly as on the
            // interpreter's store path.
            if self.feedback_directory.probe_store(
                site,
                obj,
                &mut self.gc_heap,
                atomized_key,
                &value,
            ) {
                self.feedback_directory
                    .record_property_hit(PropertyIcKind::Store);
                let current_obj = frame
                    .read(obj_reg)?
                    .as_object()
                    .ok_or(VmError::InvalidOperand)?;
                return Ok(self.whisker_store_cell_fill(
                    site,
                    current_obj,
                    &self.gc_heap,
                    atomized_key,
                ));
            }
            if entries_len > 0 {
                self.feedback_directory
                    .record_property_guard_miss(site, PropertyIcKind::Store);
            } else {
                self.feedback_directory
                    .record_property_uncached_miss(site, PropertyIcKind::Store);
            }
            // A store IC is not authority to bypass an inherited accessor,
            // non-writable, or exotic `[[Set]]` outcome it has no stub for.
            // Prove the miss is an ordinary data assignment before installing.
            let set_outcome = object::resolve_set_atomized(obj, &self.gc_heap, atomized_key);
            if !matches!(set_outcome, object::SetOutcome::AssignData) {
                return full_set(self, frame, stack);
            }
            if self
                .feedback_directory
                .property_is_megamorphic(site, PropertyIcKind::Store)
                == Some(false)
                && let Some(ic) =
                    cache_ir::CacheStub::install_store_existing(obj, &self.gc_heap, atomized_key)
                && ic
                    .run_store(obj, &mut self.gc_heap, atomized_key, &value)
                    .is_some()
            {
                self.feedback_directory
                    .install_property_stub(site, PropertyIcKind::Store, ic);
                let current_obj = frame
                    .read(obj_reg)?
                    .as_object()
                    .ok_or(VmError::InvalidOperand)?;
                return Ok(self.whisker_store_cell_fill(
                    site,
                    current_obj,
                    &self.gc_heap,
                    atomized_key,
                ));
            }
        }

        let current_obj = frame
            .read(obj_reg)?
            .as_object()
            .ok_or(VmError::InvalidOperand)?;
        let current_value = frame.read(src)?;
        self.set_property(current_obj, atomized_key.name(), current_value)?;
        Ok(None)
    }

    /// This load site's cache program lowered for inline execution, or `None`
    /// to leave the access on the stub.
    pub(crate) fn whisker_load_cell_fill(
        &self,
        site: usize,
        obj: JsObject,
        atomized_key: AtomizedPropertyKey<'_>,
    ) -> Option<crate::jit::JitPropertyIcWay> {
        self.feedback_directory
            .whisker_load_cell_fill(site, obj, &self.gc_heap, atomized_key)
    }

    /// Complete one computed `[[Get]]` from boxed values owned by generated
    /// SSA rather than an interpreter-compatible register window.
    ///
    /// `function_id` and `instruction_pc` identify the currently published
    /// native frame. Recording happens before key coercion or receiver
    /// dispatch, so a throwing proxy/getter still matures the same feedback
    /// cell as interpreted execution. The operation either returns its value
    /// or throws; it never requests replay at the original instruction.
    pub fn jit_runtime_load_element_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        instruction_pc: u32,
        receiver: Value,
        key: Value,
    ) -> Result<Value, VmError> {
        self.record_jit_runtime_property_stub();
        if let Some(code_block) = context.exec_function(function_id) {
            self.record_element_family_feedback(code_block, instruction_pc, function_id, receiver);
        }
        self.load_element_values(stack, context, receiver, key)
    }

    /// Complete `LoadGlobalOrThrow` from a compiled activation. Resolves a free
    /// identifier through the global object and throws `ReferenceError` when
    /// unbound without mutating the caller's PC.
    ///
    /// # Errors
    /// Propagates the `ReferenceError` for an unbound identifier (and any
    /// throwing global accessor) plus `InvalidOperand`.
    pub fn jit_runtime_load_global(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        frame: &mut crate::ActiveFrameMut<'_>,
        function_id: u32,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        let value = self.load_global_or_throw_value(stack, context, function_id, name_idx)?;
        frame.write(dst, value)
    }

    /// `Op::DefineDataProperty obj, key, value` — construction-time data-property
    /// definition for object literals. Shared by the dispatch loop and compiled
    /// runtime operation; does **not** advance the PC (the caller does).
    ///
    /// # Errors
    /// Propagates a `TypeError` when the target rejects the definition, plus any
    /// error from key coercion (`ToPropertyKey`).
    pub(crate) fn run_define_data_property_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        obj_reg: u16,
        key_reg: u16,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let frame = stack.get(top_idx).ok_or(VmError::InvalidOperand)?;
        let target = *read_register(frame, obj_reg)?;
        let key_value = *read_register(frame, key_reg)?;
        let value = *read_register(frame, value_reg)?;
        self.define_data_property_values(stack, context, target, key_value, value)
    }

    /// Representation-neutral object-literal property definition.
    ///
    /// All three source values enter the handle arena before key coercion can
    /// allocate or re-enter JavaScript. This lets a frameless compiled callee
    /// complete the opcode against its canonical register window without a
    /// temporary interpreter frame or copied window.
    pub(crate) fn run_define_data_property_active(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        obj_reg: u16,
        key_reg: u16,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let target = frame.read(obj_reg)?;
        let key_value = frame.read(key_reg)?;
        let value = frame.read(value_reg)?;
        self.define_data_property_values(stack, context, target, key_value, value)
    }

    pub(crate) fn define_data_property_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key_value: Value,
        value: Value,
    ) -> Result<(), VmError> {
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, target);
            let key_value = interp.scoped_value(scope, key_value);
            let value = interp.scoped_value(scope, value);
            let key =
                interp.to_property_key_sync(stack, context, interp.escape_scoped(key_value))?;
            let target = interp.escape_scoped(target);
            let value = interp.escape_scoped(value);
            // Fast path: a plain object receiver takes the shape-friendly
            // construction-time store (no prototype consult — define semantics).
            if let Some(obj) = target.as_object() {
                match &key {
                    VmPropertyKey::Symbol(sym) => {
                        object::set_symbol(obj, &mut interp.gc_heap, *sym, value);
                    }
                    _ => {
                        let name = key
                            .string_name()
                            .expect("non-symbol key has string spelling")
                            .to_string();
                        interp.set_property(obj, &name, value)?;
                    }
                }
            } else {
                let descriptor = object::PartialPropertyDescriptor {
                    value: Some(value),
                    writable: Some(true),
                    enumerable: Some(true),
                    configurable: Some(true),
                    ..Default::default()
                };
                if !interp.define_own_property_value(stack, context, &target, &key, descriptor)? {
                    return Err(interp.err_type(
                        ("Cannot define property on object literal".to_string()).into(),
                    ));
                }
            }
            Ok(())
        })
    }

    /// Typed JIT allocation operation for `NewObject`. It uses the shared
    /// stack-rooted allocator, so a
    /// young-generation scavenge can rewrite live frame registers before the
    /// object handle is published back into `dst`.
    ///
    /// # Errors
    /// Propagates allocation failures.
    pub fn jit_runtime_new_object(
        &mut self,
        frame: &mut crate::ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = self.allocate_object_literal_value()?;
        frame.write(dst, value)
    }

    /// Typed JIT allocation operation for `NewArray`. The compiler supplies the
    /// decoded destination and source-register list; this operation only owns
    /// stack-rooted allocation and result publication.
    ///
    /// # Errors
    /// Propagates invalid operands and allocation failures.
    pub fn jit_runtime_new_array(
        &mut self,
        frame: &mut crate::ActiveFrameMut<'_>,
        dst: u16,
        source_regs: &[u16],
    ) -> Result<(), VmError> {
        let mut elements = Vec::with_capacity(source_regs.len());
        for &register in source_regs {
            elements.push(frame.read(register)?);
        }
        let value = self.allocate_array_literal_value(elements)?;
        frame.write(dst, value)
    }

    /// Complete one computed `[[Set]]` from boxed values owned by generated
    /// SSA rather than an interpreter-compatible register window.
    ///
    /// Strictness is derived from `function_id`; feedback is recorded at the
    /// exact published logical PC before any observable key coercion, proxy
    /// trap, or setter call. A successful return means the complete store has
    /// committed exactly once.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_store_element_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        instruction_pc: u32,
        receiver: Value,
        key_value: Value,
        value: Value,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        if let Some(code_block) = context.exec_function(function_id) {
            self.record_element_family_feedback(code_block, instruction_pc, function_id, receiver);
        }
        self.store_element_values(stack, context, function_id, receiver, key_value, value)
    }

    /// Complete computed `[[Set]]` from copied, representation-independent
    /// operands. No frame or stack view survives into this method, so GC and
    /// synchronous JavaScript reentry can revisit the published activation
    /// without aliasing a retained Rust borrow.
    pub(crate) fn store_element_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        mut receiver: Value,
        mut key_value: Value,
        mut value: Value,
    ) -> Result<(), VmError> {
        // Complete the dense-array miss without allocating a property key.
        // Existing slots need only the write barrier; plain holes are also
        // safe while no indexed accessor has ever polluted the prototype
        // universe. Sidecars retain the descriptor-aware authority below.
        let allow_plain_hole = !self.array_index_accessor_protector;
        if let Some(arr) = receiver.as_array()
            && let Some(index) = key_value
                .as_i32()
                .and_then(|index| usize::try_from(index).ok())
            && crate::array::set_plain_dense_slot(
                arr,
                &mut self.gc_heap,
                index,
                value,
                allow_plain_hole,
            )
        {
            return Ok(());
        }
        let mut property_key = Value::undefined();
        let strict = context.function_is_strict(function_id);

        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: all four locals are declared before `roots` and remain live
        // and unmoved until the scope is explicitly dropped below.
        unsafe {
            roots.add_value(&mut receiver);
            roots.add_value(&mut key_value);
            roots.add_value(&mut value);
            roots.add_value(&mut property_key);
        }
        property_key = self.coerce_property_key_value(stack, context, key_value)?;

        let key = if let Some(symbol) = property_key.as_symbol(&self.gc_heap) {
            VmPropertyKey::Symbol(symbol)
        } else if let Some(string) = property_key.as_string(&self.gc_heap) {
            VmPropertyKey::OwnedString(string.to_lossy_string(&self.gc_heap))
        } else if let Some(number) = property_key.as_number() {
            VmPropertyKey::OwnedString(number.to_display_string())
        } else {
            return Err(VmError::InvalidOperand);
        };

        if receiver.is_undefined() || receiver.is_null() || receiver.is_hole() {
            return Err(self.err_type(
                (format!("Cannot set property on {}", value_kind_name(&receiver))).into(),
            ));
        }
        let wrapper_name = if receiver.is_boolean() {
            Some("Boolean")
        } else if receiver.is_number() {
            Some("Number")
        } else if receiver.is_string() {
            Some("String")
        } else if receiver.is_symbol() {
            Some("Symbol")
        } else if receiver.is_big_int() {
            Some("BigInt")
        } else {
            None
        };
        let accepted = if let Some(wrapper_name) = wrapper_name {
            let parent = self.primitive_wrapper_prototype(wrapper_name)?;
            self.ordinary_set_data_value(
                stack,
                context,
                Value::object(parent),
                &key,
                value,
                receiver,
                0,
            )?
        } else {
            self.ordinary_set_data_value(stack, context, receiver, &key, value, receiver, 0)?
        };
        if !accepted {
            let name = key.string_name().unwrap_or("symbol");
            self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))?;
        }
        drop(roots);
        Ok(())
    }

    /// This store site's cache program lowered for inline execution, or `None`
    /// to leave the access on the stub.
    ///
    /// An add-transition store mutates the shape and grows the value slab, so
    /// its program has no inline form and stays on the stub. The shape guard
    /// the emitted site keeps also guarantees the slot is the writable data
    /// slot the cache captured (a shape encodes per-slot flags and key), so the
    /// inline write is sound.
    pub(crate) fn whisker_store_cell_fill(
        &self,
        site: usize,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        atomized_key: AtomizedPropertyKey<'_>,
    ) -> Option<crate::jit::JitPropertyIcWay> {
        self.feedback_directory
            .whisker_store_cell_fill(site, obj, heap, atomized_key)
    }

    /// Run the GC write barrier after an inline pointer-valued property store.
    ///
    /// Parent and child are read through the canonical activation. The
    /// emitted fast path already performed the slot store and calls here only
    /// for heap-pointer values, so this operation merely records the old→young
    /// edge; it never needs a materialized interpreter frame or raw window.
    pub fn jit_runtime_write_barrier(
        &mut self,
        frame: &ActiveFrameRef<'_>,
        obj_reg: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let receiver = frame.read(obj_reg)?;
        let value = frame.read(src)?;
        if let Some(obj) = receiver.as_object() {
            self.gc_heap.record_write(obj, &value);
        } else if let Some(arr) = receiver.as_array() {
            self.gc_heap.record_write(arr, &value);
        }
        Ok(())
    }
}

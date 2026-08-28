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
use otter_bytecode::Op;

/// Decode one fixed named-property operation from the exact published native
/// frame identity. Generated code supplies values only; the immutable
/// CodeBlock remains the authority for property spelling and feedback site.
fn named_property_site(
    context: &ExecutionContext,
    function_id: u32,
    instruction_pc: u32,
    expected_op: Op,
) -> Result<(AtomizedPropertyKey<'_>, usize), VmError> {
    let function = context
        .exec_function(function_id)
        .ok_or(VmError::InvalidOperand)?;
    let instruction = function
        .instr_at_index(instruction_pc as usize)
        .ok_or(VmError::InvalidOperand)?;
    if instruction.instruction_pc != instruction_pc || function.op(instruction) != expected_op {
        return Err(VmError::InvalidOperand);
    }
    let name_operand = match expected_op {
        Op::LoadProperty => 2,
        Op::StoreProperty => 1,
        _ => return Err(VmError::InvalidOperand),
    };
    let name_index = function
        .const_index(instruction, name_operand)
        .ok_or(VmError::InvalidOperand)?;
    let key = context
        .property_atom_for_function(function_id, name_index)
        .ok_or(VmError::InvalidOperand)?;
    let site = instruction
        .property_ic_site()
        .ok_or(VmError::InvalidOperand)?;
    Ok((key, site))
}

impl Interpreter {
    /// Resolve and complete the exact published `LoadProperty` over one boxed
    /// receiver.
    ///
    /// The function id and logical PC select and validate the immutable
    /// instruction, property name, and feedback site. The receiver and result
    /// remain rooted across IC setup, moving collection, and synchronous
    /// accessor/proxy reentry. A successful return contains the loaded value
    /// plus an optional WhiskerIC program; no outcome requests replay.
    pub fn jit_runtime_load_property_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        instruction_pc: u32,
        mut receiver: Value,
    ) -> Result<(Value, Option<crate::jit::JitPropertyIcWay>), VmError> {
        let (atomized_key, site) =
            named_property_site(context, function_id, instruction_pc, Op::LoadProperty)?;
        self.record_jit_runtime_property_stub();
        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: both locals precede `roots` and remain live and stationary
        // until the complete property operation and optional cell program are
        // produced.
        unsafe {
            roots.add_value(&mut receiver);
            roots.add_value(&mut result);
        }
        // Non-object receivers (primitives, proxies) and saturated sites
        // complete through the full resolution cascade below; the IC layers
        // only serve cache-representable ordinary-object loads.
        let Some(obj) = receiver.as_object() else {
            result = self.load_property_value(context, stack, receiver, atomized_key.name())?;
            return Ok((result, None));
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
                result = resolved.value;
                return Ok((result, None));
            }
            result = self.load_property_value(context, stack, receiver, atomized_key.name())?;
            return Ok((result, None));
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
            result = value;
            return Ok((result, fill));
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
        receiver = Value::object(migrating);
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
            let current_obj = receiver.as_object().ok_or(VmError::InvalidOperand)?;
            let fill = self.whisker_load_cell_fill(site, current_obj, atomized_key);
            result = resolved.value;
            return Ok((result, fill));
        }
        // Not cache-representable (accessor, deep prototype, absent):
        // complete the load in place through the full cascade.
        result = self.load_property_value(context, stack, receiver, atomized_key.name())?;
        Ok((result, None))
    }

    /// Complete the exact published `StoreProperty` over boxed receiver/value
    /// operands.
    ///
    /// Instruction metadata is decoded from `function_id` and logical PC.
    /// Both operands remain rooted across shape work, moving collection, and
    /// setter/proxy reentry. Success means the store committed exactly once and
    /// optionally returns an inline WhiskerIC program.
    pub fn jit_runtime_store_property_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        instruction_pc: u32,
        mut receiver: Value,
        mut value: Value,
    ) -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
        let (atomized_key, site) =
            named_property_site(context, function_id, instruction_pc, Op::StoreProperty)?;
        self.record_jit_runtime_property_stub();
        let strict = context.function_is_strict(function_id);
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: both boxed operands outlive `roots`; the collector rewrites
        // these exact slots before execution resumes after any moving GC.
        unsafe {
            roots.add_value(&mut receiver);
            roots.add_value(&mut value);
        }
        let Some(obj) = receiver.as_object() else {
            self.store_property_value(
                context,
                stack,
                receiver,
                atomized_key.name(),
                value,
                strict,
            )?;
            return Ok(None);
        };
        if !object::supports_fast_property_ic(obj, &self.gc_heap) {
            self.store_property_value(
                context,
                stack,
                receiver,
                atomized_key.name(),
                value,
                strict,
            )?;
            return Ok(None);
        }
        // A shape token is useful only after canonical `[[Set]]` has selected
        // an ordinary data assignment. Inherited setters, non-writable data,
        // proxies/exotic parents, and non-extensible receivers retain the full
        // value-level implementation (and strict-mode throwing behavior).
        if !matches!(
            object::resolve_set_atomized(obj, &self.gc_heap, atomized_key),
            object::SetOutcome::AssignData
        ) {
            self.store_property_value(
                context,
                stack,
                receiver,
                atomized_key.name(),
                value,
                strict,
            )?;
            return Ok(None);
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
                let current_obj = receiver.as_object().ok_or(VmError::InvalidOperand)?;
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
        }

        // Canonical resolution above proved an ordinary data assignment. Only
        // now may the cache install a writable existing slot or capture the
        // first add as its authoritative transition sample; the very next peer
        // receiver can then stay entirely in generated code.
        let current_obj = receiver.as_object().ok_or(VmError::InvalidOperand)?;
        if self
            .feedback_directory
            .property_is_megamorphic(site, PropertyIcKind::Store)
            == Some(false)
        {
            if let Some(ic) = cache_ir::CacheStub::install_store_existing(
                current_obj,
                &self.gc_heap,
                atomized_key,
            ) && ic
                .run_store(current_obj, &mut self.gc_heap, atomized_key, &value)
                .is_some()
            {
                self.feedback_directory
                    .install_property_stub(site, PropertyIcKind::Store, ic);
                return Ok(self.whisker_store_cell_fill(
                    site,
                    current_obj,
                    &self.gc_heap,
                    atomized_key,
                ));
            }

            // Shape interning and slab preparation may collect. The helper
            // roots the complete stack plus receiver/value and commits the
            // property exactly once when it returns a transition.
            if let Some(transition) = self.capture_store_property_transition_with_stack_roots(
                stack,
                current_obj,
                atomized_key,
                &value,
            )? {
                self.feedback_directory.install_property_stub(
                    site,
                    PropertyIcKind::Store,
                    cache_ir::CacheStub::store_transition(transition),
                );
                let current_obj = receiver.as_object().ok_or(VmError::InvalidOperand)?;
                return Ok(self.whisker_store_cell_fill(
                    site,
                    current_obj,
                    &self.gc_heap,
                    atomized_key,
                ));
            }
        }

        // A rejected transition attempt may have collected while interning its
        // child shape, so never reuse the pre-attempt raw object handle.
        let current_obj = receiver.as_object().ok_or(VmError::InvalidOperand)?;
        if !self.ordinary_set_data_property(current_obj, atomized_key.name(), value)? {
            self.failed_set_result(
                strict,
                format!("Cannot assign to property '{}'", atomized_key.name()),
            )?;
        }
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
        self.store_element_values(
            stack,
            context,
            function_id,
            receiver,
            key_value,
            value,
            false,
        )
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
        force_strict: bool,
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
        let strict = force_strict || context.function_is_strict(function_id);

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
    /// Existing-slot programs name their guarded writable own slot. A bounded
    /// add-transition program additionally names immutable parent/child shapes
    /// and the complete null/direct-terminal-prototype proof; anything that may
    /// allocate or observe user code stays on the stub.
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

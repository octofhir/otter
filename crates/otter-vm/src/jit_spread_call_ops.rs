//! Compiled call-family, arguments, and tail-call transitions.
//!
//! # Contents
//! - `CollectArguments` construction shared by interpreter frames and
//!   compiled activations, including stack-owned frames whose generated
//!   caller published the actual arguments after the register window.
//! - Synchronous full-completion siblings for frame-pushing call/construct
//!   helpers.
//! - Canonical `GetMethod + Call` completion for compiled method-call misses.
//! - Packed-operand dispatch for the spread call/construct stub.
//!
//! # Invariants
//! - Calls and constructions append to the current rooted activation stack;
//!   JIT code owns no parallel JS call semantics.
//! - No frame borrow survives reentrant completion; the destination is
//!   re-borrowed from the published stack afterwards.
//! - A compiled generic method call records its attempt before receiver or
//!   method lookup, matching interpreter dispatch and invalidating any stale
//!   cold-exit snapshot even when lookup throws.
//! - Resolved explicit-receiver and forwarded targets use the same bounded
//!   CodeBlock feedback and invalidate stale caller generations on target growth.
//! - Spread validation order matches interpreter dispatch.
//! - Elided mapped arguments read live parameter cells; after materialization,
//!   forwarding reads the actual arguments object through CreateListFromArrayLike.
//!
//! # See also
//! - [`crate::Interpreter::do_call_spread`]
//! - [`crate::Interpreter::do_construct_spread`]

use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind, Op};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Interpreter, Value, VmError, activation_stack::ActivationStack,
    interp::helpers::is_constructor_runtime, object, read_register, write_register,
};

impl Interpreter {
    pub(crate) fn jit_runtime_method_call_values(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        call_pc: u32,
        receiver: Value,
        name_index: u32,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_call_attempt_feedback(function, call_pc, function_id);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.jit_to_rust_call_transitions = self
            .jit_runtime_stats
            .jit_to_rust_call_transitions
            .saturating_add(1);
        let receiver_anchor = self.push_iteration_anchor(receiver) - 1;
        let anchor_base = receiver_anchor;
        let args_start = receiver_anchor + 1;
        let args_len = args.len();
        for value in args {
            self.push_iteration_anchor(value);
        }
        let call = |interp: &mut Self, stack: &mut ActivationStack| {
            let mut receiver = interp.iteration_anchor(receiver_anchor);
            if receiver.is_nullish() {
                let label = if receiver.is_null() {
                    "null"
                } else {
                    "undefined"
                };
                return Err(interp.err_type((format!("Cannot read properties of {label}")).into()));
            }
            let feedback_site = context.property_ic_site(function_id, call_pc);
            let property_slot = context.property_feedback_slot(
                function_id,
                call_pc,
                crate::property_ic::PropertyIcKind::Load,
            );
            let capture_site =
                feedback_site.is_some_and(|site| !interp.method_site_feedback_saturated(site));
            let method_site = capture_site
                .then(|| {
                    interp.method_site_for_receiver(context, function_id, name_index, &mut receiver)
                })
                .flatten();
            // Shape migration may allocate and relocate the receiver. The
            // iteration anchor is the canonical root used by all resolution
            // and call steps below.
            interp.set_iteration_anchor(receiver_anchor, receiver);
            // The compiled site shares the interpreter's method-resolution
            // caches: a shape-guarded own-slot hit, then the load-IC-backed
            // resolution (which serves prototype methods), and only then the
            // per-call `[[Get]]` chain walk.
            let method_ic_site = feedback_site.unwrap_or(usize::MAX);
            let mut method = Value::undefined();
            if method_ic_site != usize::MAX
                && let Some(obj) = receiver.as_object()
            {
                if let Some(crate::method_ops::MethodCallIc::Ordinary(hit)) =
                    interp.method_feedback.method_ic(method_ic_site)
                {
                    if let Some(cached) =
                        crate::object::load_own_data_slot_by_shape(obj, &interp.gc_heap, hit)
                        && interp.is_callable_runtime(&cached)
                    {
                        method = cached;
                    } else {
                        interp.method_feedback.clear_method_ic(method_ic_site);
                    }
                }
                if method.is_undefined()
                    && let Some(atomized_key) =
                        context.property_atom_for_function(function_id, name_index)
                    && let Some(slot) = property_slot
                    && let Some(resolved) = interp.resolve_method_ic(obj, atomized_key, slot)
                    && interp.is_callable_runtime(&resolved)
                {
                    if let Some(hit) = property_slot.and_then(|slot| slot.mono_load_own_data_hit())
                    {
                        interp.method_feedback.install_method_ic(
                            method_ic_site,
                            crate::method_ops::MethodCallIc::Ordinary(hit),
                        );
                    }
                    method = resolved;
                }
            }
            if method.is_undefined() {
                let method_key = context
                    .property_atom_for_function(function_id, name_index)
                    .ok_or(VmError::InvalidOperand)?;
                receiver = interp.iteration_anchor(receiver_anchor);
                method = interp
                    .get_method_value_for_call(context, stack, receiver, method_key)?
                    .unwrap_or_else(Value::undefined);
            }
            if !interp.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }

            // Match interpreter dispatch's resolved-target publication. A
            // target is recorded only when the pre-call receiver/holder shape
            // program names the exact data slot that produced this callable.
            // Accessors, proxies, exotic receivers, and deep chains therefore
            // remain unrecorded rather than weakening the generated guard.
            if let (Some(feedback_site), Some(method_site)) = (feedback_site, method_site) {
                let method_function = method.as_function().or_else(|| {
                    method
                        .as_closure(&interp.gc_heap)
                        .map(|closure| closure.function_id())
                });
                let changed = if let Some(method_function) = method_function {
                    interp.note_method_target(feedback_site, method_function, method_site)
                } else {
                    method
                        .as_native_function()
                        .and_then(|native| {
                            crate::jit_static_native::jit_static_call_target(
                                native,
                                &interp.gc_heap,
                            )
                        })
                        .filter(|declaration| usize::from(declaration.argument_count) == args_len)
                        .is_some_and(|declaration| {
                            interp.record_method_native_leaf_feedback(
                                feedback_site,
                                declaration.leaf_stub_id,
                                method_site,
                            )
                        })
                };
                interp.commit_method_call_feedback_transition(function, function_id, changed);
            }
            let receiver = interp.iteration_anchor(receiver_anchor);
            let mut rooted_args = SmallVec::with_capacity(args_len);
            for index in args_start..args_start + args_len {
                rooted_args.push(interp.iteration_anchor(index));
            }
            interp.run_callable_sync_rooted(stack, context, &method, receiver, rooted_args)
        };
        let result = call(self, stack);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    /// Complete one explicit-receiver call for a published compiled frame:
    /// the callee value is already resolved, so only call-target feedback and
    /// the canonical rooted call remain.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn jit_runtime_call_with_this_values(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        call_pc: u32,
        callee: Value,
        receiver: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_call_attempt_feedback(function, call_pc, function_id);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.jit_to_rust_call_transitions = self
            .jit_runtime_stats
            .jit_to_rust_call_transitions
            .saturating_add(1);
        self.record_resolved_call_feedback(function, call_pc, function_id, callee);
        self.run_rooted_call_values(stack, context, callee, receiver, args)
    }

    /// Complete one `new` for a published compiled frame whose site owns no
    /// generated construct edge: record the attempt and the resolved target so
    /// the site can settle, then run the canonical rooted construct.
    pub(crate) fn jit_runtime_construct_values(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        call_pc: u32,
        callee: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_call_attempt_feedback(function, call_pc, function_id);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.jit_to_rust_call_transitions = self
            .jit_runtime_stats
            .jit_to_rust_call_transitions
            .saturating_add(1);
        let target_callable = callee
            .as_class_constructor()
            .map(|class| class.ctor(&self.gc_heap))
            .unwrap_or(callee);
        if let Some(target_function_id) = target_callable.as_function().or_else(|| {
            target_callable
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        }) {
            let transition = self.record_ordinary_call_feedback(
                function,
                call_pc,
                crate::feedback::OrdinaryCallTarget::Bytecode(target_function_id),
            );
            if transition.evict_for_reopt() {
                self.evict_compiled_for_reopt(function_id);
            }
        }
        self.observe_class_constructor_field_transitions(context, callee)?;
        self.run_rooted_construct_values(stack, context, callee, callee, args)
    }

    /// Final hidden class of an `arguments` object: index keys, `length`,
    /// and `callee`, built through the ordinary transition table so every
    /// arguments object of one arity shares a shape and stays IC-cacheable.
    /// Uncached above the arity cap, but still shaped — the transition chain
    /// itself is interned by the shape runtime.
    fn arguments_object_shape(
        &mut self,
        stack: &ActivationStack,
        argc: usize,
        mapped: bool,
    ) -> Result<crate::object::ShapeHandle, VmError> {
        const CACHED_ARGC_MAX: usize = 64;
        let key = (argc as u32, mapped);
        if argc <= CACHED_ARGC_MAX
            && let Some(shape) = self.arguments_shape_cache.get(&key)
        {
            return Ok(*shape);
        }
        let mut shape = self.shape_root();
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let mut key_buffer = itoa::Buffer::new();
        let hidden = crate::object::PropertyFlags::new(true, false, true);
        for index in 0..argc {
            let name = key_buffer.format(index);
            shape = if let Some(child) = self.shape_runtime.child_if_cached(
                &self.gc_heap,
                shape,
                name,
                crate::object::PropertyFlags::data_default(),
                false,
            ) {
                child
            } else {
                self.shape_runtime
                    .child_with_roots(
                        &mut self.gc_heap,
                        shape,
                        name,
                        crate::object::PropertyFlags::data_default(),
                        false,
                        &mut external_visit,
                    )
                    .map_err(VmError::from)?
            };
        }
        let tail: [(&str, crate::object::PropertyFlags, bool); 2] = [
            ("length", hidden, false),
            if mapped {
                ("callee", hidden, false)
            } else {
                (
                    "callee",
                    crate::object::PropertyFlags::new(false, false, false),
                    true,
                )
            },
        ];
        for (name, flags, is_accessor) in tail {
            shape = if let Some(child) =
                self.shape_runtime
                    .child_if_cached(&self.gc_heap, shape, name, flags, is_accessor)
            {
                child
            } else {
                self.shape_runtime
                    .child_with_roots(
                        &mut self.gc_heap,
                        shape,
                        name,
                        flags,
                        is_accessor,
                        &mut external_visit,
                    )
                    .map_err(VmError::from)?
            };
        }
        if argc <= CACHED_ARGC_MAX {
            self.arguments_shape_cache.insert(key, shape);
        }
        Ok(shape)
    }

    /// §10.4.4 Arguments exotic object construction for an interpreter frame.
    pub(crate) fn run_collect_arguments_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = self.materialize_frame_arguments_object(context, stack, frame_index)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// The interpreter frame's arguments exotic object, built on first use
    /// and kept in the cold record so every later use in the activation
    /// observes the same object.
    pub(crate) fn materialize_frame_arguments_object(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
    ) -> Result<Value, VmError> {
        if let Some(existing) = self
            .frame_cold(&stack[frame_index])
            .and_then(|cold| cold.arguments_object)
        {
            return Ok(existing);
        }
        let (elements, kind, mapped_entries, callee) = {
            let function_id = stack[frame_index].function_id;
            let function = context
                .exec_function(function_id)
                .ok_or(VmError::InvalidOperand)?;
            let frame = &mut stack[frame_index];
            // The cold record keeps its copy: §B.3.6.2 `fn.arguments` reads
            // the same incoming values from a frame that has already built
            // its own arguments object.
            let elements: SmallVec<[Value; 4]> = self
                .frame_cold_mut(frame)
                .map(|cold| cold.incoming_args.clone())
                .unwrap_or_default();
            let mapped_entries = Self::mapped_argument_entries(function, elements.len(), |idx| {
                frame.upvalues.get(idx as usize).copied()
            });
            (
                elements,
                function.arguments_object_kind,
                mapped_entries,
                frame.self_value,
            )
        };
        let value = self.collect_arguments_value(stack, elements, kind, mapped_entries, callee)?;
        self.frame_ensure_cold(&mut stack[frame_index])
            .arguments_object = Some(value);
        Ok(value)
    }

    /// §10.4.4 Arguments exotic object construction for a compiled activation.
    ///
    /// A stack-owned frame reads the actual arguments its generated caller
    /// published after the register window; a materialized activation reads
    /// the same list from its cold record. Mapped parameters alias through the
    /// activation's capture cells either way.
    pub(crate) fn jit_runtime_collect_arguments(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame: &mut crate::ActiveFrameMut<'_>,
        materialized_frame_index: Option<usize>,
        dst: u16,
    ) -> Result<(), VmError> {
        let function = context
            .exec_function(frame.function_id())
            .ok_or(VmError::InvalidOperand)?;
        let elements: SmallVec<[Value; 4]> =
            match (frame.incoming_argument_count(), materialized_frame_index) {
                (Some(count), _) => (0..count)
                    .map(|index| frame.incoming_argument(index))
                    .collect::<Result<_, _>>()?,
                (None, Some(frame_index)) => {
                    let materialized = stack.get_mut(frame_index).ok_or(VmError::InvalidOperand)?;
                    self.frame_cold_mut(materialized)
                        .map(|cold| cold.incoming_args.clone())
                        .unwrap_or_default()
                }
                (None, None) => return Err(VmError::InvalidOperand),
            };
        let mapped_entries = Self::mapped_argument_entries(function, elements.len(), |idx| {
            frame.upvalue(u32::from(idx)).ok()
        });
        let callee = frame.self_value();
        let kind = function.arguments_object_kind;
        let value = self.collect_arguments_value(stack, elements, kind, mapped_entries, callee)?;
        frame.write(dst, value)
    }

    /// Parameter bindings that alias the arguments object's indexed entries,
    /// resolved against the activation's capture cells.
    fn mapped_argument_entries(
        function: &crate::executable::CodeBlock,
        argument_count: usize,
        mut upvalue: impl FnMut(u16) -> Option<crate::UpvalueCell>,
    ) -> Vec<crate::object::MappedArgumentEntry> {
        if function.arguments_object_kind != ArgumentsObjectKind::Mapped {
            return Vec::new();
        }
        function
            .mapped_argument_bindings
            .iter()
            .filter_map(|binding| {
                if binding.argument_index as usize >= argument_count {
                    return None;
                }
                let ArgumentBindingStorage::Upvalue { idx } = binding.storage else {
                    return None;
                };
                let cell = upvalue(idx)?;
                Some(crate::object::MappedArgumentEntry {
                    key: binding.argument_index.to_string(),
                    cell,
                })
            })
            .collect()
    }

    /// Allocate and initialize one arguments exotic object from already
    /// collected inputs; the caller commits the result to its destination.
    fn collect_arguments_value(
        &mut self,
        stack: &mut ActivationStack,
        elements: SmallVec<[Value; 4]>,
        kind: ArgumentsObjectKind,
        mapped_entries: Vec<crate::object::MappedArgumentEntry>,
        callee: Value,
    ) -> Result<Value, VmError> {
        let elements_len = elements.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let elements_start = callee_anchor + 1;
        for value in elements {
            self.push_iteration_anchor(value);
        }

        let collect = |interp: &mut Self| {
            // The typed intrinsic slot resolves `%Array.prototype%` without
            // walking the global object; the chain below only serves embedders
            // whose bootstrap omitted Array.
            let iterator_method = interp
                .realm_intrinsics
                .array_prototype()
                .or_else(|| {
                    crate::object::get(interp.global_this, &interp.gc_heap, "Array")
                        .and_then(|value| {
                            if let Some(ctor) = value.as_object() {
                                crate::object::get(ctor, &interp.gc_heap, "prototype")
                            } else if let Some(native) = value.as_native_function() {
                                native
                                    .own_property_descriptor(&mut interp.gc_heap, "prototype")
                                    .ok()
                                    .flatten()
                                    .and_then(|descriptor| match descriptor.kind {
                                        crate::object::DescriptorKind::Data { value } => {
                                            Some(value)
                                        }
                                        _ => None,
                                    })
                            } else {
                                None
                            }
                        })
                        .and_then(|value| value.as_object())
                })
                .and_then(|prototype| crate::object::get(prototype, &interp.gc_heap, "values"));
            let iterator_symbol = interp
                .well_known_symbols
                .get(crate::symbol::WellKnown::Iterator);
            let iterator_anchor =
                interp.push_iteration_anchor(iterator_method.unwrap_or(Value::undefined())) - 1;
            let obj = if kind == ArgumentsObjectKind::Mapped {
                let shape = interp.arguments_object_shape(stack, elements_len, true)?;
                let callee = interp.iteration_anchor(callee_anchor);
                let iterator_root = interp.iteration_anchor(iterator_anchor);
                let elements: SmallVec<[Value; 4]> = (elements_start
                    ..elements_start + elements_len)
                    .map(|index| interp.iteration_anchor(index))
                    .collect();
                let iterator_descriptor = iterator_method.map(|_| (iterator_symbol, iterator_root));
                let obj = interp.alloc_stack_rooted_object_with_value_roots(
                    stack,
                    &[&callee, &iterator_root],
                    &elements,
                )?;
                if let Some(proto) = interp.object_prototype_object_opt() {
                    object::set_prototype(obj, &mut interp.gc_heap, Some(proto));
                }
                crate::arguments_object::initialize_mapped(
                    obj,
                    &mut interp.gc_heap,
                    elements,
                    callee,
                    mapped_entries,
                    iterator_descriptor,
                    shape,
                )
            } else {
                let shape = interp.arguments_object_shape(stack, elements_len, false)?;
                let thrower = interp.restricted_throw_type_error()?;
                let iterator_root = interp.iteration_anchor(iterator_anchor);
                let elements: SmallVec<[Value; 4]> = (elements_start
                    ..elements_start + elements_len)
                    .map(|index| interp.iteration_anchor(index))
                    .collect();
                let iterator_descriptor = iterator_method.map(|_| (iterator_symbol, iterator_root));
                let obj = interp.alloc_stack_rooted_object_with_value_roots(
                    stack,
                    &[&thrower, &iterator_root],
                    &elements,
                )?;
                if let Some(proto) = interp.object_prototype_object_opt() {
                    object::set_prototype(obj, &mut interp.gc_heap, Some(proto));
                }
                crate::arguments_object::initialize_unmapped(
                    obj,
                    &mut interp.gc_heap,
                    elements,
                    thrower,
                    iterator_descriptor,
                    shape,
                )
            };
            Ok(Value::object(obj))
        };
        let result = collect(self);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    fn spread_arguments(
        &self,
        stack: &ActivationStack,
        frame_index: usize,
        args_reg: u16,
    ) -> Result<SmallVec<[Value; 8]>, VmError> {
        let args_array = read_register(&stack[frame_index], args_reg)?
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        Ok(crate::array::with_elements(
            args_array,
            &self.gc_heap,
            |elements| elements.iter().copied().collect(),
        ))
    }

    pub(crate) fn run_rooted_call_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let args_len = args.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let this_anchor = self.push_iteration_anchor(this_value) - 1;
        let args_start = this_anchor + 1;
        for value in args {
            self.push_iteration_anchor(value);
        }
        let run = |interp: &mut Self, stack: &mut ActivationStack| {
            let callee = interp.iteration_anchor(callee_anchor);
            let this_value = interp.iteration_anchor(this_anchor);
            let mut rooted_args = SmallVec::with_capacity(args_len);
            for index in args_start..args_start + args_len {
                rooted_args.push(interp.iteration_anchor(index));
            }
            interp.run_callable_sync_rooted(stack, context, &callee, this_value, rooted_args)
        };
        let result = run(self, stack);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    fn run_rooted_construct_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let args_len = args.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let new_target_anchor = self.push_iteration_anchor(new_target) - 1;
        let args_start = new_target_anchor + 1;
        for value in args {
            self.push_iteration_anchor(value);
        }
        let run = |interp: &mut Self, stack: &mut ActivationStack| {
            let callee = interp.iteration_anchor(callee_anchor);
            let new_target = interp.iteration_anchor(new_target_anchor);
            let mut rooted_args = SmallVec::with_capacity(args_len);
            for index in args_start..args_start + args_len {
                rooted_args.push(interp.iteration_anchor(index));
            }
            interp.run_construct_sync_rooted(stack, context, &callee, new_target, rooted_args)
        };
        let result = run(self, stack);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_call_spread_full_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        this_reg: u16,
        args_reg: u16,
    ) -> Result<(), VmError> {
        let callee = *read_register(&stack[frame_index], callee_reg)?;
        let this_value = *read_register(&stack[frame_index], this_reg)?;
        let args = self.spread_arguments(stack, frame_index, args_reg)?;
        let result = self.run_rooted_call_values(stack, context, callee, this_value, args)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_construct_spread_full_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        args_reg: u16,
        super_construct: bool,
    ) -> Result<(), VmError> {
        let callee = *read_register(&stack[frame_index], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let new_target = if super_construct {
            self.frame_cold(&stack[frame_index])
                .and_then(|cold| cold.new_target)
                .unwrap_or(callee)
        } else {
            callee
        };
        let args = self.spread_arguments(stack, frame_index, args_reg)?;
        let result = self.run_rooted_construct_values(stack, context, callee, new_target, args)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Complete one spread/call-family opcode for a published compiled frame.
    pub fn jit_runtime_spread_call_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if matches!(
            opcode,
            value
                if value == Op::CallSpread as u8
                    || value == Op::NewSpread as u8
                    || value == Op::SuperConstructSpread as u8
        ) {
            self.jit_runtime_stats.jit_to_rust_call_transitions = self
                .jit_runtime_stats
                .jit_to_rust_call_transitions
                .saturating_add(1);
        }
        let saved_pc = stack[frame_index].pc;
        let lane = |packed: u64, index: usize| ((packed >> (index * 16)) & 0xffff) as u16;
        match opcode {
            value if value == Op::CallSpread as u8 => {
                self.run_call_spread_full_regs(
                    context,
                    stack,
                    frame_index,
                    lane(arg0, 0),
                    lane(arg0, 1),
                    lane(arg0, 2),
                    lane(arg0, 3),
                )?;
            }
            value if value == Op::NewSpread as u8 || value == Op::SuperConstructSpread as u8 => {
                self.run_construct_spread_full_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                    arg2 as u16,
                    value == Op::SuperConstructSpread as u8,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}

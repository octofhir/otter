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
//! - Elided mapped arguments read live parameter-context slots; after
//!   materialization,
//!   forwarding reads the actual arguments object through CreateListFromArrayLike.
//! - Arguments receivers start on the final shape's prototype/capacity root;
//!   initialization cannot change their persistent inline footprint.
//!
//! # See also
//! - [`crate::Interpreter::do_call_spread`]
//! - [`crate::Interpreter::do_construct_spread`]

use crate::native_abi::CommittedValueError;
use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind, Op};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Interpreter, Value, VmError, activation_stack::ActivationStack,
    interp::helpers::is_constructor_runtime, read_register, write_register,
};

impl Interpreter {
    /// Resolve the callable of one published `CallMethodValue` site through
    /// the interpreter's method caches and record its method/call feedback.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn jit_runtime_resolve_method(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        call_pc: u32,
        receiver: Value,
        name_index: u32,
        args_len: usize,
    ) -> Result<Value, CommittedValueError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        self.record_call_attempt_feedback(function, call_pc);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let receiver_anchor = self.push_iteration_anchor(receiver) - 1;
        let resolve = |interp: &mut Self, stack: &mut ActivationStack| {
            let mut receiver = interp.iteration_anchor(receiver_anchor);
            if receiver.is_nullish() {
                let label = if receiver.is_null() {
                    "null"
                } else {
                    "undefined"
                };
                return Err(CommittedValueError::JavaScript(
                    interp.err_type((format!("Cannot read properties of {label}")).into()),
                ));
            }
            let feedback_site = context.property_ic_site(function_id, call_pc);
            let property_slot = context.property_feedback_slot(
                function_id,
                call_pc,
                crate::property_ic::PropertyIcKind::Load,
            );
            let capture_site =
                feedback_site.is_some_and(|site| !interp.method_site_feedback_saturated(site));
            // The compiled site shares the interpreter's method-resolution
            // caches: a shape-guarded own-slot hit, then the load-IC-backed
            // resolution (which serves prototype methods), and only then the
            // per-call `[[Get]]` chain walk. The cached steps run no user
            // code and never allocate.
            let mut method = Value::undefined();
            // A shaped receiver whose resolution the site already records
            // changes no feedback, so the layout capture (chain migration,
            // lookup and shape registration) is skipped, as a V8 IC hit
            // leaves its feedback slot untouched.
            let mut capture = capture_site;
            if capture
                && let Some(site) = feedback_site
                && let Some(obj) = receiver.as_object()
                && !crate::object::keyed_shape(obj, &interp.gc_heap).is_null()
            {
                method = interp.cached_method_resolution(
                    context,
                    function_id,
                    name_index,
                    site,
                    property_slot,
                    obj,
                );
                let recv_shape = crate::object::shape_id(obj, &interp.gc_heap);
                if let Some(target) = interp.method_feedback_target(method, args_len)
                    && interp.method_feedback_records(site, recv_shape, target)
                {
                    capture = false;
                } else {
                    method = Value::undefined();
                }
            }
            let method_site = capture
                .then(|| {
                    interp.method_site_for_receiver(context, function_id, name_index, &mut receiver)
                })
                .flatten();
            // Shape migration may allocate and relocate the receiver. The
            // iteration anchor is the canonical root used by all resolution
            // steps below.
            interp.set_iteration_anchor(receiver_anchor, receiver);
            if method.is_undefined()
                && let Some(site) = feedback_site
                && let Some(obj) = receiver.as_object()
            {
                method = interp.cached_method_resolution(
                    context,
                    function_id,
                    name_index,
                    site,
                    property_slot,
                    obj,
                );
            }
            if method.is_undefined() {
                let method_key = context
                    .property_atom_for_function(function_id, name_index)
                    .ok_or(VmError::InvalidOperand)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                receiver = interp.iteration_anchor(receiver_anchor);
                method = interp
                    .get_method_value_for_call(context, stack, receiver, method_key)?
                    .unwrap_or_else(Value::undefined);
            }
            if !interp.is_callable_runtime(&method) {
                return Err(CommittedValueError::JavaScript(VmError::NotCallable));
            }

            // Match interpreter dispatch's resolved-target publication. A
            // target is recorded only when the pre-call receiver/holder shape
            // program names the exact data slot that produced this callable.
            // Accessors, proxies, exotic receivers, and deep chains therefore
            // remain unrecorded rather than weakening the generated guard.
            // A receiver no guard can describe gets no record, so without
            // saturation the site would repeat this capture on every call.
            if capture
                && method_site.is_none()
                && let Some(feedback_site) = feedback_site
            {
                interp.saturate_method_site_feedback(feedback_site);
            }
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
                        // Shaped method entries receive only arguments.
                        .filter(|declaration| {
                            usize::from(declaration.argument_count) == args_len
                                && !declaration.this_operand
                        })
                        .map(|declaration| {
                            interp.record_method_native_leaf_feedback(
                                feedback_site,
                                declaration.leaf_stub_id,
                                method_site,
                            )
                        })
                        .unwrap_or_else(|| {
                            // A native no leaf entry declares is never
                            // recorded; stop capturing the site.
                            interp.saturate_method_site_feedback(feedback_site);
                            false
                        })
                };
                interp.commit_method_call_feedback_transition(function, changed);
            }
            // `f.call(thisArg, ...)` with the intrinsic `call` runs `f`: the
            // site records that target for the optimizing tier.
            let receiver = interp.iteration_anchor(receiver_anchor);
            if method.as_native_function().is_some_and(|native| {
                native.is_vm_intrinsic(
                    &interp.gc_heap,
                    crate::native_function::VmIntrinsicFunction::FunctionPrototypeCall,
                )
            }) && interp.is_callable_runtime(&receiver)
                && let Some(target) = interp.function_prototype_call_target(method, receiver)
            {
                let _ = interp.record_ordinary_call_feedback(function, call_pc, target);
            }
            Ok(method)
        };
        let result = resolve(self, stack);
        self.pop_iteration_anchors_to(receiver_anchor);
        result
    }

    /// The method a shaped receiver's site caches resolve without user code:
    /// the site's own-slot method IC, then its property load IC. Undefined
    /// when neither names a callable.
    fn cached_method_resolution(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_index: u32,
        site: usize,
        property_slot: Option<crate::feedback::PropertyFeedbackSlot<'_>>,
        obj: crate::object::JsObject,
    ) -> Value {
        if let Some(crate::method_ops::MethodCallIc::Ordinary(hit)) =
            self.method_feedback.method_ic(site)
        {
            if let Some(cached) =
                crate::object::load_own_data_slot_by_shape(obj, &self.gc_heap, hit)
                && self.is_callable_runtime(&cached)
            {
                return cached;
            }
            self.method_feedback.clear_method_ic(site);
        }
        if let Some(atomized_key) = context.property_atom_for_function(function_id, name_index)
            && let Some(slot) = property_slot
            && let Some(resolved) = self.resolve_method_ic(obj, atomized_key, slot)
            && self.is_callable_runtime(&resolved)
        {
            if let Some(hit) = slot.mono_load_own_data_hit() {
                self.method_feedback
                    .install_method_ic(site, crate::method_ops::MethodCallIc::Ordinary(hit));
            }
            return resolved;
        }
        Value::undefined()
    }

    /// The identity a method-site distribution records for `method`.
    fn method_feedback_target(
        &self,
        method: Value,
        args_len: usize,
    ) -> Option<crate::interp::MethodFeedbackTarget> {
        if let Some(fid) = method.as_function().or_else(|| {
            method
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        }) {
            return Some(crate::interp::MethodFeedbackTarget::Function(fid));
        }
        let declaration = crate::jit_static_native::jit_static_call_target(
            method.as_native_function()?,
            &self.gc_heap,
        )?;
        (usize::from(declaration.argument_count) == args_len && !declaration.this_operand)
            .then_some(crate::interp::MethodFeedbackTarget::NativeLeaf(
                declaration.leaf_stub_id,
            ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn jit_runtime_method_call_values(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        call_pc: u32,
        receiver: Value,
        name_index: u32,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, CommittedValueError> {
        let base = self.push_iteration_anchor(receiver) - 1;
        for &value in &args {
            self.push_iteration_anchor(value);
        }
        let result = (|| {
            let method = self.jit_runtime_resolve_method(
                context,
                stack,
                function_id,
                call_pc,
                receiver,
                name_index,
                args.len(),
            )?;
            let receiver = self.iteration_anchor(base);
            let rooted: SmallVec<[Value; 8]> = (0..args.len())
                .map(|index| self.iteration_anchor(base + 1 + index))
                .collect();
            self.run_rooted_call_values(stack, context, method, receiver, rooted)
                .map_err(CommittedValueError::completed_call)
        })();
        self.pop_iteration_anchors_to(base);
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
        self.record_call_attempt_feedback(function, call_pc);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.jit_to_rust_call_transitions = self
            .jit_runtime_stats
            .jit_to_rust_call_transitions
            .saturating_add(1);
        self.record_resolved_call_feedback(function, call_pc, callee, receiver);
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
        self.record_call_attempt_feedback(function, call_pc);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        // §13.3.5.1.1 step 7 — `new` of a value without [[Construct]] throws,
        // exactly as the interpreter's `Op::New`; natives that are callable
        // but not constructors must never run as a construct.
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
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
            let _ = self.record_ordinary_call_feedback(
                function,
                call_pc,
                crate::feedback::OrdinaryCallTarget::Bytecode(target_function_id),
            );
        }
        let mut callee = callee;
        let mut new_target = callee;
        let mut args = args;
        self.observe_class_constructor_field_transitions_rooted(
            context,
            &mut callee,
            &mut new_target,
            &mut args,
        )?;
        self.run_rooted_construct_values(stack, context, callee, callee, args, 0)
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
        // An arguments object's prototype is the realm's `%Object.prototype%`
        // (§10.4.4.6 step 2, §10.4.4.7 step 5); its root starts the chain.
        let object_prototype = self.object_prototype_object_opt();
        let mut shape = self.object_root(
            object_prototype,
            crate::object::inline_capacity_for(argc + 2),
            crate::object::ShapeState::ORDINARY,
        )?;
        let key = (argc as u32, mapped, crate::object::shape_body::id_of(shape));
        if argc <= CACHED_ARGC_MAX
            && let Some(shape) = self.arguments_shape_cache.get(&key)
        {
            return Ok(*shape);
        }
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
    /// `CollectArguments dst, ctx`. A mapped object aliases the slots of the
    /// parameter-scope context in `ctx`, the register every context-held
    /// mapped formal names.
    pub(crate) fn run_collect_arguments_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        ctx: u16,
    ) -> Result<(), VmError> {
        debug_assert!(
            context
                .exec_function(stack[frame_index].function_id)
                .is_none_or(
                    |function| function.mapped_argument_bindings.iter().all(
                        |binding| match binding.storage {
                            ArgumentBindingStorage::Context { reg, .. } => reg == ctx,
                            ArgumentBindingStorage::Register { .. } => true,
                        }
                    )
                ),
            "CollectArguments names the mapped formals' context register"
        );
        let value = self.materialize_frame_arguments_object(context, stack, frame_index)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// The interpreter frame's arguments exotic object, built on first use
    /// and kept in the common frame so every later use in the activation
    /// observes the same object.
    pub(crate) fn materialize_frame_arguments_object(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
    ) -> Result<Value, VmError> {
        if let Some(existing) = stack[frame_index].arguments_object() {
            return Ok(Value::object(existing));
        }
        let (elements, kind, mapped_entries, callee) = {
            let function_id = stack[frame_index].function_id;
            let function = context
                .exec_function(function_id)
                .ok_or(VmError::InvalidOperand)?;
            let frame = &mut stack[frame_index];
            let view = crate::ActiveFrameRef::from_frame(frame);
            let count = view.incoming_argument_count();
            let elements: SmallVec<[Value; 4]> = (0..count)
                .map(|index| view.incoming_argument(index))
                .collect::<Result<_, _>>()?;
            let mapped_entries = Self::mapped_arguments(function, elements.len(), |reg| {
                frame.registers.get(usize::from(reg)).copied()
            });
            (
                elements,
                function.arguments_object_kind,
                mapped_entries,
                frame.self_value,
            )
        };
        let value = self.collect_arguments_value(stack, elements, kind, mapped_entries, callee)?;
        stack[frame_index].set_arguments_object(value.as_object());
        Ok(value)
    }

    /// §10.4.4 Arguments exotic object construction for a compiled activation.
    ///
    /// A stack-owned frame reads the actual arguments its generated caller
    /// published after the register window; a materialized activation reads
    /// the same list from its cold record. Mapped parameters alias slots of
    /// the activation's parameter context either way.
    pub(crate) fn jit_runtime_collect_arguments(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame: &mut crate::ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = self.jit_materialize_arguments(context, stack, frame)?;
        frame.write(dst, value)
    }

    pub(crate) fn jit_materialize_arguments(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame: &mut crate::ActiveFrameMut<'_>,
    ) -> Result<Value, VmError> {
        let existing = frame.as_ref().native_arguments_object().map(Value::object);
        if let Some(value) = existing {
            frame.set_native_arguments_object(value.as_object().ok_or(VmError::InvalidOperand)?)?;
            return Ok(value);
        }
        let function = context
            .exec_function(frame.function_id())
            .ok_or(VmError::InvalidOperand)?;
        let count = frame.incoming_argument_count();
        let elements: SmallVec<[Value; 4]> = (0..count)
            .map(|index| frame.incoming_argument(index))
            .collect::<Result<_, _>>()?;
        let mapped_entries =
            Self::mapped_arguments(function, elements.len(), |reg| frame.read(reg).ok());
        let callee = frame.self_value();
        let kind = function.arguments_object_kind;
        let value = self.collect_arguments_value(stack, elements, kind, mapped_entries, callee)?;
        frame.set_native_arguments_object(value.as_object().ok_or(VmError::InvalidOperand)?)?;
        Ok(value)
    }

    /// Parameter bindings that alias the arguments object's indexed entries:
    /// slots of the parameter-scope context held in the register the
    /// context-held mapped formals name.
    fn mapped_arguments(
        function: &crate::executable::CodeBlock,
        argument_count: usize,
        mut read_register: impl FnMut(u16) -> Option<Value>,
    ) -> Option<crate::object::MappedArguments> {
        if function.arguments_object_kind != ArgumentsObjectKind::Mapped {
            return None;
        }
        let mut context_register = None;
        let entries: Vec<_> = function
            .mapped_argument_bindings
            .iter()
            .filter_map(|binding| {
                if binding.argument_index as usize >= argument_count {
                    return None;
                }
                let ArgumentBindingStorage::Context { reg, slot } = binding.storage else {
                    return None;
                };
                context_register = Some(reg);
                Some(crate::object::MappedArgumentEntry {
                    key: binding.argument_index.to_string(),
                    slot,
                })
            })
            .collect();
        let context = read_register(context_register?)?.as_context()?;
        Some(crate::object::MappedArguments { context, entries })
    }

    /// Allocate and initialize one arguments exotic object from already
    /// collected inputs; the caller commits the result to its destination.
    fn collect_arguments_value(
        &mut self,
        stack: &mut ActivationStack,
        elements: SmallVec<[Value; 4]>,
        kind: ArgumentsObjectKind,
        mapped: Option<crate::object::MappedArguments>,
        callee: Value,
    ) -> Result<Value, VmError> {
        let elements_len = elements.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let elements_start = callee_anchor + 1;
        for value in elements {
            self.push_iteration_anchor(value);
        }
        // The parameter-scope context is young: it rides as an anchor through
        // every allocation below and is re-read before the map is installed.
        let mapped = mapped.map(|mapped| {
            let anchor = self.push_iteration_anchor(Value::context(mapped.context)) - 1;
            (anchor, mapped.entries)
        });

        let collect = |interp: &mut Self| {
            let iterator_method = interp.realm_intrinsics.array_values();
            let iterator_symbol = interp
                .well_known_symbols
                .get(crate::symbol::WellKnown::Iterator);
            let iterator_anchor =
                interp.push_iteration_anchor(iterator_method.unwrap_or(Value::undefined())) - 1;
            let obj = if kind == ArgumentsObjectKind::Mapped {
                let shape = interp.arguments_object_shape(stack, elements_len, true)?;
                let obj = interp.allocate_arguments_receiver(stack, shape)?;
                // Allocation can relocate every pending argument. Read the
                // canonical anchors only after the receiver exists.
                let callee = interp.iteration_anchor(callee_anchor);
                let iterator_root = interp.iteration_anchor(iterator_anchor);
                let elements: SmallVec<[Value; 4]> = (elements_start
                    ..elements_start + elements_len)
                    .map(|index| interp.iteration_anchor(index))
                    .collect();
                let iterator_descriptor = iterator_method.map(|_| (iterator_symbol, iterator_root));
                let mapped = match &mapped {
                    Some((anchor, entries)) => Some(crate::object::MappedArguments {
                        context: interp
                            .iteration_anchor(*anchor)
                            .as_context()
                            .ok_or(VmError::InvalidOperand)?,
                        entries: entries.clone(),
                    }),
                    None => None,
                };
                crate::arguments_object::initialize_mapped(
                    obj,
                    &mut interp.gc_heap,
                    elements,
                    callee,
                    mapped,
                    iterator_descriptor,
                    shape,
                )?
            } else {
                let shape = interp.arguments_object_shape(stack, elements_len, false)?;
                let thrower = interp.restricted_throw_type_error()?;
                let thrower_anchor = interp.push_iteration_anchor(thrower) - 1;
                let obj = interp.allocate_arguments_receiver(stack, shape)?;
                let thrower = interp.iteration_anchor(thrower_anchor);
                let iterator_root = interp.iteration_anchor(iterator_anchor);
                let elements: SmallVec<[Value; 4]> = (elements_start
                    ..elements_start + elements_len)
                    .map(|index| interp.iteration_anchor(index))
                    .collect();
                let iterator_descriptor = iterator_method.map(|_| (iterator_symbol, iterator_root));
                crate::arguments_object::initialize_unmapped(
                    obj,
                    &mut interp.gc_heap,
                    elements,
                    thrower,
                    iterator_descriptor,
                    shape,
                )?
            };
            Ok(Value::object(obj))
        };
        let result = collect(self);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    /// Allocate the slotless receiver whose footprint the arguments shape owns.
    /// Pending inputs live in iteration anchors and are reread by the caller.
    fn allocate_arguments_receiver(
        &mut self,
        stack: &ActivationStack,
        shape: crate::object::ShapeHandle,
    ) -> Result<crate::object::JsObject, VmError> {
        let root = crate::object::shape_body::lineage_root_of(&self.gc_heap, shape);
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        crate::object::alloc_object_with_shape_roots(&mut self.gc_heap, root, &mut external_visit)
            .map_err(VmError::from)
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
            interp.run_callable_sync_rooted(stack, Some(context), &callee, this_value, rooted_args)
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
        super_origin: u64,
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
            interp.run_construct_sync_rooted(
                stack,
                context,
                &callee,
                new_target,
                rooted_args,
                super_origin,
            )
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
            {
                let target = stack[frame_index].new_target();
                if target.is_undefined() {
                    callee
                } else {
                    target
                }
            }
        } else {
            callee
        };
        let args = self.spread_arguments(stack, frame_index, args_reg)?;
        let origin = if super_construct {
            std::ptr::from_mut(&mut stack[frame_index]) as u64
        } else {
            0
        };
        let result =
            self.run_rooted_construct_values(stack, context, callee, new_target, args, origin)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_receiver_allocates_arity_capacity_before_initialization() {
        for argc in 0..=3 {
            let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
            interp.gc_heap_mut().set_gc_stress(0, false);
            let mut stack = ActivationStack::new();
            interp.with_handle_scope(|interp, scope| {
                let (elements, offsets) = interp.with_handle_scope(|interp, children| {
                    let mut elements = SmallVec::<[Value; 4]>::new();
                    let mut offsets = Vec::new();
                    for index in 0..argc {
                        let child = interp.scoped_object(children).expect("argument child");
                        let marker = interp.scoped_number(children, index as f64 + 317.0);
                        interp
                            .scoped_set(children, child, "marker", marker)
                            .expect("argument marker");
                        let value = interp.escape_scoped(child);
                        offsets.push(value.as_raw_gc().unwrap().0);
                        elements.push(value);
                    }
                    (elements, offsets)
                });
                // No allocation intervenes before these values enter the
                // production constructor's iteration-anchor root window.
                let value = interp
                    .collect_arguments_value(
                        &mut stack,
                        elements,
                        ArgumentsObjectKind::Mapped,
                        None,
                        Value::undefined(),
                    )
                    .expect("arguments exact-capacity allocation");
                let arguments = interp.scoped_value(scope, value);
                let object = value.as_object().unwrap();
                let receiver_offset = value.as_raw_gc().unwrap().0;
                assert_eq!(
                    interp
                        .gc_heap()
                        .read_payload(object, crate::object::ObjectBody::inline_capacity),
                    argc + 2,
                );
                assert_eq!(
                    crate::object::get_own(object, interp.gc_heap(), "length"),
                    Some(Value::number_i32(argc as i32))
                );
                assert_eq!(
                    crate::object::get_own(object, interp.gc_heap(), "callee"),
                    Some(Value::undefined())
                );
                interp.collect_minor_tracing_runtime_roots();
                assert_ne!(
                    interp.escape_scoped(arguments).as_raw_gc().unwrap().0,
                    receiver_offset,
                    "actual arguments receiver move",
                );
                for (index, offset) in offsets.into_iter().enumerate() {
                    let child = interp
                        .scoped_get(scope, arguments, &index.to_string())
                        .expect("moved argument");
                    let value = interp.escape_scoped(child);
                    assert_ne!(
                        value.as_raw_gc().unwrap().0,
                        offset,
                        "actual argument-child move"
                    );
                    let marker = interp
                        .scoped_get(scope, child, "marker")
                        .expect("moved argument marker");
                    assert_eq!(
                        interp.escape_scoped(marker).as_f64(),
                        Some(index as f64 + 317.0)
                    );
                }
                interp.force_gc().expect("full arguments collection");
                let object = interp.escape_scoped(arguments).as_object().unwrap();
                assert!(crate::object::is_arguments_object(object, interp.gc_heap()));
                assert_eq!(
                    interp
                        .gc_heap()
                        .read_payload(object, crate::object::ObjectBody::inline_capacity),
                    argc + 2,
                );
            });
        }
    }
}

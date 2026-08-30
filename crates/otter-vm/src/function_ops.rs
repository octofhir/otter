//! Function and closure construction opcode helpers.
//!
//! Keep callable value construction out of the main interpreter file while
//! preserving the compact executable operand path used by dispatch.
//!
//! # Contents
//! - Closure-less function value construction for `MakeFunction`.
//! - Captured-upvalue closure construction for variadic `MakeClosure`.
//! - Class constructor wrapper construction for `MakeClass`.
//! - `Function.prototype.bind` metadata and bound-function construction.
//!
//! # Invariants
//! - `MakeFunction` receives already-decoded executable operands.
//! - `MakeClosure` reads the executable operand slice because its upvalue list
//!   is variadic.
//! - Arrow closures snapshot the enclosing frame's `this` value at construction.
//! - Function property-descriptor results remain in a handle-arena slot across
//!   every field write; shape allocation never leaves the builder with a stale
//!   raw object handle.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::Frame`]

use crate::activation_stack::ActivationStack;
use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    ActiveFrameMut, BoundFunction, ClassConstructor, ExecutionContext, Frame, Interpreter,
    JsObject, JsString, PendingBindFunction, PendingBindStage, UpvalueCell, Value, VmError,
    VmGetOutcome, VmIntrinsicFunction, VmPropertyKey, abstract_ops, array, function_metadata,
    object, object_statics,
    operand_decode::{const_operand, register_operand},
    read_register,
    rooting::RootScopeExt,
    symbol, to_length, write_register,
};

pub(crate) enum BindMetadataGet {
    Value(Value),
    Getter(Value),
}

// Mirrors the matches! variant list used by `OrdinaryHasInstance` and

impl Interpreter {
    pub(crate) fn run_make_function_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let mut active = ActiveFrameMut::materialized(frame);
        self.run_make_function_active_reg(context, &mut active, dst, idx)
    }

    /// Representation-neutral capture-free function construction.
    ///
    /// Materialized frames and stack-owned direct callees expose the same
    /// nullable direct-eval handle through [`ActiveFrameMut`]; construction
    /// never consults a second closure-tail field or fabricates `None`.
    pub(crate) fn run_make_function_active_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let eval_env = frame.eval_env();
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        // §9.1 — a capture-free function created in a frame that carries a
        // direct-eval variable environment still needs the env handle (its
        // free identifiers resolve dynamically), so it materializes as a
        // closure with no upvalues. Otherwise a capture-free function keeps
        // the canonical interned value.
        if function_id != frame.function_id()
            && let Some(env) = eval_env
        {
            let closure = crate::closure::alloc_closure(
                &mut self.gc_heap,
                function_id,
                Vec::new(),
                None,
                None,
                None,
                Some(env),
            )
            .map_err(crate::oom_to_vm)?;
            frame.write(dst, Value::closure(closure))?;
            frame.advance_pc()?;
            return Ok(());
        }
        // §10.2 — the named-function SELF binding (`function_id` is the
        // running function) must resolve to the EXACT instance executing
        // this frame, not a fresh interned bare value: otherwise
        // `this instanceof Self` / `Self.prototype` inside the body would
        // observe a different per-instance `.prototype` than the one the
        // constructor installed on `this`.
        if function_id == frame.function_id() {
            self.frame_load_self(frame, dst)?;
            frame.advance_pc()?;
            return Ok(());
        }
        // §10.2 OrdinaryFunctionCreate — every evaluation of a function
        // literal produces a DISTINCT function object. A shared interned
        // `Value::function(fid)` would collapse identity across siblings
        // minted from the same source template: `Class.create() !==
        // Class.create()`, and every sibling would share one
        // `.prototype` / expando bag (prototype.js-style class factories
        // break). Capture-free functions therefore still allocate a
        // per-instance closure body with an empty upvalue spine.
        let closure = crate::closure::alloc_closure(
            &mut self.gc_heap,
            function_id,
            Vec::new(),
            None,
            None,
            None,
            None,
        )
        .map_err(crate::oom_to_vm)?;
        frame.write(dst, Value::closure(closure))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_make_closure_operands(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let idx = const_operand(operands.get(1))?;
        let count = match operands.get(2) {
            Some(Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let mut parent_indices = Vec::with_capacity(count);
        for i in 0..count {
            match operands.get(3 + i) {
                Some(Operand::Imm32(n)) if n >= 0 => parent_indices.push(n as u32),
                _ => return Err(VmError::InvalidOperand),
            }
        }
        self.run_make_closure_regs(context, frame, dst, idx, &parent_indices)
    }

    pub(crate) fn run_make_closure_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
    ) -> Result<(), VmError> {
        let (new_target, bound_derived_this) =
            self.frame_cold(frame).map_or((None, None), |cold| {
                (cold.new_target, cold.derived_this_cell)
            });
        let mut active = ActiveFrameMut::materialized_with_new_target(
            frame,
            new_target.unwrap_or_else(Value::undefined),
        );
        self.run_make_closure_active_regs(
            context,
            &mut active,
            dst,
            function_index,
            parent_indices,
            new_target,
            bound_derived_this,
        )
    }

    /// Representation-neutral closure construction for a published activation.
    ///
    /// Native direct callees use this path without materializing an interpreter
    /// [`Frame`]. Materialized callers pass cold `new.target` / derived-`this`
    /// state while both representations source the eval environment from
    /// [`ActiveFrameMut`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_make_closure_active_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
        lexical_new_target: Option<Value>,
        bound_derived_this: Option<UpvalueCell>,
    ) -> Result<(), VmError> {
        let eval_env = frame.eval_env();
        let function_id = context
            .function_id_constant(function_index)
            .ok_or(VmError::InvalidOperand)?;
        // §10.2 — a named function referencing its own binding inside its
        // body (`function_id` is the running function) must resolve to the
        // EXACT instance executing this frame, not a freshly minted one:
        // a fresh instance owns a distinct per-instance `.prototype`, so
        // `this instanceof Self` / `Self.prototype` would diverge from the
        // prototype the constructor installed on `this`.
        if function_id == frame.function_id() {
            self.frame_load_self(frame, dst)?;
            frame.advance_pc()?;
            return Ok(());
        }
        let mut cells: Vec<UpvalueCell> = Vec::with_capacity(parent_indices.len());
        for &parent_idx in parent_indices {
            cells.push(frame.upvalue(parent_idx)?);
        }
        let upvalues = cells;
        // Arrow-closure receivers are bound lexically: every later invocation
        // ignores the call site and uses the enclosing frame's `this`.
        let bound_this = if context.function_is_arrow(function_id) {
            Some(frame.this_value())
        } else {
            None
        };
        let bound_new_target = if context.function_is_arrow(function_id) {
            lexical_new_target
        } else {
            None
        };
        let bound_derived_this = context
            .function_is_arrow(function_id)
            .then_some(bound_derived_this)
            .flatten();
        // §9.1 — closures made in a frame whose function chain
        // contains a direct eval capture the frame's eval variable
        // environment so eval-introduced vars stay reachable.
        let closure = crate::closure::alloc_closure(
            &mut self.gc_heap,
            function_id,
            upvalues,
            bound_this,
            bound_new_target,
            bound_derived_this,
            eval_env,
        )
        .map_err(crate::oom_to_vm)?;
        frame.write(dst, Value::closure(closure))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_make_class_regs(
        &mut self,
        stack: &mut ActivationStack,
        frame_idx: usize,
        dst: u16,
        ctor_reg: u16,
        proto_reg: u16,
        statics_reg: u16,
        parent_reg: Option<u16>,
    ) -> Result<(), VmError> {
        let frame = &stack[frame_idx];
        let ctor = *read_register(frame, ctor_reg)?;
        if !self.is_callable_runtime(&ctor) {
            return Err(VmError::NotCallable);
        }
        let prototype = read_register(frame, proto_reg)?
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let statics = read_register(frame, statics_reg)?
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let class = ClassConstructor::new_with_roots(
            &mut self.gc_heap,
            ctor,
            prototype,
            statics,
            &mut external_visit,
        )?;
        // §15.7.14 — a static member named `name`/`length` suppresses
        // the constructor's virtual metadata property entirely (the
        // class never receives the implicit own `name`): mark it
        // deleted so removing the static later does not resurrect it.
        //
        // Re-read `ctor` and `statics` from their GC-rooted registers: the
        // class construction above allocates a hidden class and may scavenge,
        // relocating both objects. The bare locals read before that allocation
        // are stale, and walking a stale object's shape here is a
        // use-after-move (the reclaimed shape cell is reused as another type).
        let ctor = *read_register(&stack[frame_idx], ctor_reg)?;
        let statics = read_register(&stack[frame_idx], statics_reg)?
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        if let Some(closure) = ctor.as_closure(&self.gc_heap) {
            for key in ["name", "length"] {
                if crate::object::get_own_descriptor(statics, &self.gc_heap, key).is_some() {
                    closure.set_metadata_deleted(&mut self.gc_heap, key, true);
                }
            }
        } else if let Some(fid) = ctor.as_function() {
            for key in ["name", "length"] {
                if crate::object::get_own_descriptor(statics, &self.gc_heap, key).is_some() {
                    self.function_deleted_metadata.insert((fid, key));
                }
            }
        }
        // §15.7.14 step 6.b — preserve the parent class IDENTITY for
        // [[GetPrototypeOf]]; the statics object's own prototype
        // keeps the parallel walk-able static-inheritance chain.
        if let Some(parent_reg) = parent_reg {
            let parent = *read_register(&stack[frame_idx], parent_reg)?;
            // §15.7.14 step 6.c — `extends null` keeps
            // constructorParent = %Function.prototype% (slot stays
            // `undefined`); only a real parent class value lands.
            if !parent.is_undefined() && !parent.is_null() {
                class.set_ctor_proto(&mut self.gc_heap, parent);
            }
        }
        // §15.7.14 step 6.b/6.d — a base class (no heritage) and an
        // `extends null` class both have constructorParent =
        // %Function.prototype%. The compiler only chains the statics
        // object to a *real* parent class; otherwise it keeps the
        // default %Object.prototype%, which would shadow
        // `Function.prototype.{call,apply,bind,toString,…}` with
        // `Object.prototype` and make `"" + Class` / `Class.call`
        // resolve the wrong inherited method. Re-seat the statics tail
        // on %Function.prototype% whenever no parent class identity was
        // recorded.
        if class.ctor_proto(&self.gc_heap).is_undefined()
            && let Some(function_prototype) = self.realm_intrinsics.function_prototype()
        {
            // Re-read `statics` from its GC-rooted register: the class
            // construction above may have scavenged and relocated it, leaving
            // the bare `statics` local read before the allocation stale.
            let statics = read_register(&stack[frame_idx], statics_reg)?
                .as_object()
                .ok_or(VmError::TypeMismatch)?;
            object::set_prototype(statics, &mut self.gc_heap, Some(function_prototype));
        }
        // Publish the constructor to its destination register first: the
        // shape-advancing `constructor` install below may scavenge while
        // allocating a hidden-class child, and the register slot is GC-rooted
        // (the bare `class` local is not), so it is forwarded across the move.
        write_register(&mut stack[frame_idx], dst, Value::class_constructor(class))?;
        // §15.7.10 ClassDefinitionEvaluation step 24 — install
        // `C.prototype.constructor = C` so reflective probes
        // (`new Sub(...).constructor === Sub`) walk to the
        // class constructor itself rather than to the inherited
        // parent class's `constructor` slot. Routed through the
        // hidden-class-advancing define (not the dictionary-mode
        // `object::define_own_property`) so the prototype keeps a fast shape
        // and instance method calls stay inline-guardable.
        let constructor_desc = object::PartialPropertyDescriptor {
            value: Some(Value::class_constructor(class)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        };
        // Re-read `prototype` from its GC-rooted register: every allocation
        // since it was first read (class construction, the static-prototype
        // re-seat) may have relocated it, and walking a stale shape here is a
        // use-after-move. The register slot is forwarded by the collector.
        let mut prototype = read_register(&stack[frame_idx], proto_reg)?
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        // §15.7.10 steps 17/20 — the compiler reserves `prototype.constructor`
        // with an `undefined` placeholder before the class elements run, so it
        // precedes the methods in own-key order. A class element whose
        // computed key evaluates to "constructor" (`['constructor']() {}`) is
        // an ordinary method and overrides that slot (step 20 runs after step
        // 17). Only fill the placeholder here: if the slot already holds a real
        // value, a class element claimed it and must not be clobbered.
        // The placeholder is a data slot holding `undefined`; an accessor a
        // class element installed is neither, and reading it as a value
        // would answer `undefined` and lose the accessor.
        let placeholder_pending =
            crate::object::get_own_descriptor(prototype, &self.gc_heap, "constructor").is_none_or(
                |descriptor| match descriptor.kind {
                    object::DescriptorKind::Data { value } => value.is_undefined(),
                    object::DescriptorKind::Accessor { .. } => false,
                },
            );
        if placeholder_pending {
            let _ =
                self.define_own_property_partial(&mut prototype, "constructor", constructor_desc)?;
        }
        let frame = &mut stack[frame_idx];
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn drive_bind_function(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        if let Some(result) = self.continue_pending_bind_function(stack, context, dst) {
            return result;
        }

        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let target = *read_register(&stack[top_idx], callee_reg)?;
        if !self.is_callable_runtime(&target) {
            return Err(VmError::NotCallable);
        }
        let bound_this = *read_register(&stack[top_idx], this_reg)?;
        let mut bound_args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            bound_args.push(*read_register(&stack[top_idx], r)?);
        }
        // §20.2.3.2 — `length` is read (HasOwnProperty + Get) BEFORE
        // `name`.
        match self.callable_bind_metadata_get(context, &target, "length")? {
            BindMetadataGet::Value(target_length) => self.continue_bind_function_after_length(
                stack,
                context,
                dst,
                target,
                bound_this,
                bound_args,
                target_length,
            ),
            BindMetadataGet::Getter(getter) => {
                self.frame_ensure_cold(&mut stack[top_idx])
                    .pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    stage: PendingBindStage::Length,
                    target_length: None,
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    pub(crate) fn continue_pending_bind_function(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst: u16,
    ) -> Option<Result<(), VmError>> {
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        let state = self
            .frame_cold(&stack[top_idx])
            .and_then(|c| c.pending_bind_function.as_ref())
            .filter(|state| state.pc == pc && state.dst == dst)
            .cloned()?;
        let produced = match read_register(&stack[top_idx], dst) {
            Ok(value) => *value,
            Err(err) => return Some(Err(err)),
        };
        Some(match state.stage {
            PendingBindStage::Length => self.continue_bind_function_after_length(
                stack,
                context,
                dst,
                state.target,
                state.bound_this,
                state.bound_args,
                produced,
            ),
            PendingBindStage::Name => {
                let target_length = match state.target_length {
                    Some(value) => value,
                    None => return Some(Err(VmError::InvalidOperand)),
                };
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_bind_function = None;
                }
                let mut target = state.target;
                let mut bound_this = state.bound_this;
                let mut bound_args = state.bound_args;
                let mut target_length = target_length;
                let mut produced = produced;
                let proto = {
                    let target_snapshot = target;
                    let mut holds: Vec<&mut Value> = vec![
                        &mut bound_this,
                        &mut target_length,
                        &mut produced,
                        &mut target,
                    ];
                    holds.extend(bound_args.iter_mut());
                    match self.bind_target_proto_anchored(
                        stack,
                        context,
                        &mut holds,
                        &target_snapshot,
                    ) {
                        Ok(p) => p,
                        Err(e) => return Some(Err(e)),
                    }
                };
                self.finish_bind_function(
                    stack,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    produced,
                    target_length,
                    proto,
                )
            }
        })
    }

    pub(crate) fn continue_bind_function_after_length(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst: u16,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        target_length: Value,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        match self.callable_bind_metadata_get(context, &target, "name")? {
            BindMetadataGet::Value(target_name) => {
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_bind_function = None;
                }
                let mut target = target;
                let mut bound_this = bound_this;
                let mut bound_args = bound_args;
                let mut target_name = target_name;
                let mut target_length = target_length;
                let proto = {
                    let target_snapshot = target;
                    let mut holds: Vec<&mut Value> = vec![
                        &mut bound_this,
                        &mut target_name,
                        &mut target_length,
                        &mut target,
                    ];
                    holds.extend(bound_args.iter_mut());
                    self.bind_target_proto_anchored(stack, context, &mut holds, &target_snapshot)?
                };
                self.finish_bind_function(
                    stack,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    target_name,
                    target_length,
                    proto,
                )
            }
            BindMetadataGet::Getter(getter) => {
                self.frame_ensure_cold(&mut stack[top_idx])
                    .pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    stage: PendingBindStage::Name,
                    target_length: Some(target_length),
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    /// §10.4.1.3 step 1 — the bind target's [[GetPrototypeOf]] result,
    /// trap-observable for a Proxy target. Every bind value the caller
    /// still holds is anchored across the trap (it can run user code),
    /// re-read via the returned anchor base afterwards.
    fn bind_target_proto_anchored(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        values: &mut [&mut Value],
        target: &Value,
    ) -> Result<Value, VmError> {
        if !target.is_proxy() {
            return self.get_prototype_for_op(target);
        }
        let base = self.push_iteration_anchor(*target) - 1;
        for v in values.iter() {
            self.push_iteration_anchor(**v);
        }
        let anchored_target = self.iteration_anchor(base);
        let proto = self.ordinary_get_prototype_value(stack, context, anchored_target, 0)?;
        for (i, v) in values.iter_mut().enumerate() {
            **v = self.iteration_anchor(base + 1 + i);
        }
        self.pop_iteration_anchors_to(base);
        Ok(proto)
    }

    fn finish_bind_function(
        &mut self,
        stack: &mut ActivationStack,
        dst: u16,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        target_name: Value,
        target_length: Value,
        target_proto: Value,
    ) -> Result<(), VmError> {
        let metadata = function_metadata::bound_create_metadata_from_values(
            &target_name,
            &target_length,
            bound_args.len(),
            &self.gc_heap,
        );
        // §10.4.1.3 BoundFunctionCreate step 1 — the bound function's
        // [[Prototype]] is the target's [[GetPrototypeOf]] result,
        // resolved by the caller. Only a non-default result needs the
        // body override.
        let default_proto = self.function_prototype_object().ok().map(Value::object);
        let proto_override = (Some(target_proto) != default_proto).then_some(target_proto);
        let target_root = target;
        let bound_this_root = bound_this;
        let bound_args_root = bound_args.clone();
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            target_root.trace_value_slots(visitor);
            bound_this_root.trace_value_slots(visitor);
            for arg in &bound_args_root {
                arg.trace_value_slots(visitor);
            }
            if let Some(p) = &proto_override {
                p.trace_value_slots(visitor);
            }
        };
        let bound = BoundFunction::new_with_metadata_and_roots(
            &mut self.gc_heap,
            target,
            bound_this,
            bound_args,
            metadata,
            &mut external_visit,
        )?;
        if let Some(proto) = proto_override {
            bound.set_prototype_override(&mut self.gc_heap, proto);
        }
        let top_idx = stack.len() - 1;
        if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
            cold.pending_bind_function = None;
        }
        write_register(&mut stack[top_idx], dst, Value::bound_function(bound))?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// Complete one `Op::BindFunction` synchronously for a compiled frame.
    ///
    /// This is the reentrant sibling of the interpreter's frame-push
    /// [`Self::drive_bind_function`]: accessor `name`/`length` getters on the
    /// bind target run through [`Self::run_callable_sync_rooted`] on the
    /// published activation stack instead of parking a
    /// [`PendingBindFunction`] continuation, so the JIT never resumes a
    /// partially observed bind. The single VM metadata reader
    /// ([`Self::callable_bind_metadata_get`]) and the single allocator
    /// ([`Self::finish_bind_function`]) are shared with the interpreter, so both
    /// tiers observe identical Proxy/accessor effects and bound-function shape.
    /// Every observable getter commits before the bound function is allocated;
    /// there is no post-effect side exit.
    pub(crate) fn bind_function_full(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        callee_reg: u16,
        this_reg: u16,
        arg_regs: &[u16],
    ) -> Result<(), VmError> {
        let target = *read_register(&stack[top_idx], callee_reg)?;
        if !self.is_callable_runtime(&target) {
            return Err(VmError::NotCallable);
        }

        // §10.4.1.3 step 1 — BoundFunctionCreate resolves the target's
        // [[GetPrototypeOf]] (trap-observable for a Proxy target) FIRST.
        let target_proto = if target.is_proxy() {
            self.ordinary_get_prototype_value(stack, context, target, 0)?
        } else {
            self.get_prototype_for_op(&target)?
        };
        let proto_anchor = self.push_iteration_anchor(target_proto) - 1;

        // §20.2.3.2 — read `length`, then `name`. Each may be an accessor whose
        // getter runs `this = target`; the frame source registers stay live
        // roots, so re-read the target after every reentrant call.
        let target = *read_register(&stack[top_idx], callee_reg)?;
        let target_length = match self.callable_bind_metadata_get(context, &target, "length")? {
            BindMetadataGet::Value(value) => value,
            BindMetadataGet::Getter(getter) => {
                let receiver = *read_register(&stack[top_idx], callee_reg)?;
                self.run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())?
            }
        };

        // Anchor the produced length across the name getter and the bound-
        // function allocation so a moving collection cannot strand it.
        let length_anchor = self.push_iteration_anchor(target_length) - 1;

        let target = *read_register(&stack[top_idx], callee_reg)?;
        let target_name = match self.callable_bind_metadata_get(context, &target, "name")? {
            BindMetadataGet::Value(value) => value,
            BindMetadataGet::Getter(getter) => {
                let receiver = *read_register(&stack[top_idx], callee_reg)?;
                self.run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())?
            }
        };
        let name_anchor = self.push_iteration_anchor(target_name) - 1;

        let target_proto = self.iteration_anchor(proto_anchor);
        let target_name = self.iteration_anchor(name_anchor);
        let target_length = self.iteration_anchor(length_anchor);
        let target = *read_register(&stack[top_idx], callee_reg)?;
        let bound_this = *read_register(&stack[top_idx], this_reg)?;
        let mut bound_args: SmallVec<[Value; 4]> = SmallVec::with_capacity(arg_regs.len());
        for &reg in arg_regs {
            bound_args.push(*read_register(&stack[top_idx], reg)?);
        }
        let result = self.finish_bind_function(
            stack,
            dst,
            target,
            bound_this,
            bound_args,
            target_name,
            target_length,
            target_proto,
        );
        self.pop_iteration_anchors_to(proto_anchor);
        result
    }

    /// Complete one `Op::BindFunction` for a published compiled frame.
    ///
    /// `packed_meta` carries `dst | callee<<16 | this<<32 | argc<<48`; the low
    /// `argc` 16-bit lanes of `packed_args` name the bound-argument registers.
    /// The lowering guarantees the fixed-operand count fits the four lanes, so
    /// they are sufficient.
    pub fn jit_runtime_bind_function(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        packed_meta: u64,
        packed_args: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        let dst = packed_meta as u16;
        let callee_reg = (packed_meta >> 16) as u16;
        let this_reg = (packed_meta >> 32) as u16;
        let argc = ((packed_meta >> 48) & 0xffff) as usize;
        let mut arg_regs: SmallVec<[u16; 4]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            arg_regs.push(((packed_args >> (16 * i)) & 0xffff) as u16);
        }
        self.bind_function_full(
            context,
            stack,
            frame_index,
            dst,
            callee_reg,
            this_reg,
            &arg_regs,
        )?;
        stack[frame_index].pc = saved_pc;
        Ok(())
    }

    pub(crate) fn run_vm_intrinsic_sync_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        intrinsic: VmIntrinsicFunction,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        // Intrinsics can allocate before forwarding to their terminal callee
        // (`apply` materializes an argument list; `bind` performs observable
        // property gets). Own and register the incoming state here instead of
        // relying on a caller whose argument slot has already been emptied by
        // a zero-copy hand-off.
        let roots = crate::call_ops::SyncJsCallRoots::call(this_value, Value::undefined(), args);
        let roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let result = self.run_vm_intrinsic_sync_with_roots(stack, context, intrinsic, &roots);
        drop(roots_guard);
        result
    }

    fn run_vm_intrinsic_sync_with_roots(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        intrinsic: VmIntrinsicFunction,
        roots: &crate::call_ops::SyncJsCallRoots,
    ) -> Result<Value, VmError> {
        match intrinsic {
            VmIntrinsicFunction::FunctionPrototypeCall => {
                if !self.is_callable_runtime(&roots.target()) {
                    return Err(VmError::NotCallable);
                }
                let mut forwarded = roots.take_args();
                let receiver = if forwarded.is_empty() {
                    Value::undefined()
                } else {
                    forwarded.remove(0)
                };
                let target = roots.target();
                self.run_callable_sync_rooted(stack, context, &target, receiver, forwarded)
            }
            VmIntrinsicFunction::FunctionPrototypeApply => {
                if !self.is_callable_runtime(&roots.target()) {
                    return Err(VmError::NotCallable);
                }
                let mut inputs = roots.take_args().into_iter();
                let receiver = inputs.next().unwrap_or(Value::undefined());
                roots.set_receiver(receiver);
                let forwarded: SmallVec<[Value; 8]> = match inputs.next() {
                    None => SmallVec::new(),
                    Some(v) if v.is_nullish() => SmallVec::new(),
                    Some(arg_array) => {
                        roots.set_scratch(0, arg_array);
                        self.create_list_from_array_like(stack, context, roots.scratch(0))?
                    }
                };
                let target = roots.target();
                let receiver = roots.receiver_value();
                self.run_callable_sync_rooted(stack, context, &target, receiver, forwarded)
            }
            VmIntrinsicFunction::FunctionPrototypeBind => {
                if !self.is_callable_runtime(&roots.target()) {
                    return Err(VmError::NotCallable);
                }
                let mut bound_args = roots.take_args();
                let receiver = if bound_args.is_empty() {
                    Value::undefined()
                } else {
                    bound_args.remove(0)
                };
                roots.set_receiver(receiver);
                roots.replace_args(bound_args);
                // §10.4.1.3 BoundFunctionCreate step 1 runs FIRST: the
                // bound function's [[Prototype]] is the target's current
                // [[GetPrototypeOf]] result (trap-observable for a
                // Proxy target), read before the length/name reads.
                let target_proto = {
                    let target = roots.target();
                    if target.is_proxy() {
                        self.ordinary_get_prototype_value(stack, context, target, 0)?
                    } else {
                        self.get_prototype_for_op(&target)?
                    }
                };
                roots.set_scratch(0, target_proto);
                // §20.2.3.2 step 3 — the length transfer keys off
                // HasOwnProperty(Target, "length") (a trap-observable
                // [[GetOwnProperty]]); an inherited `length` (mutated
                // [[Prototype]]) leaves L = 0. The read precedes `name`.
                let target_length = {
                    let target = roots.target();
                    let key = VmPropertyKey::String("length");
                    let own = self.ordinary_get_own_property_descriptor_value(
                        stack, context, target, &key, 0,
                    )?;
                    if own.is_some() {
                        self.get_property_value_for_call(stack, context, target, "length")?
                    } else {
                        Value::undefined()
                    }
                };
                roots.set_scratch(1, target_length);
                // §20.2.3.2 step 4 — targetName is a plain observable
                // Get (no HasOwnProperty probe); a non-string result
                // coerces to "" inside the metadata builder.
                let target_name = {
                    let target = roots.target();
                    self.get_property_value_for_call(stack, context, target, "name")?
                };
                let metadata = function_metadata::bound_create_metadata_from_values(
                    &target_name,
                    &roots.scratch(1),
                    roots.args_len(),
                    &self.gc_heap,
                );
                // Reconstitute the owned inline-4 representation only at the
                // bound-function storage boundary. No VM allocation can occur
                // between taking it from the registered state and handing it
                // to the constructor, which roots its exact owned parameters.
                let bound_args: SmallVec<[Value; 4]> = roots.take_args().into_iter().collect();
                let target = roots.target();
                let receiver = roots.receiver_value();
                let mut external_visit = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
                let bound = BoundFunction::new_with_metadata_and_roots(
                    &mut self.gc_heap,
                    target,
                    receiver,
                    bound_args,
                    metadata,
                    &mut external_visit,
                )?;
                let default_proto = self.function_prototype_object().ok().map(Value::object);
                let target_proto = roots.scratch(0);
                if Some(target_proto) != default_proto {
                    bound.set_prototype_override(&mut self.gc_heap, target_proto);
                }
                Ok(Value::bound_function(bound))
            }
            VmIntrinsicFunction::FunctionPrototypeToString => {
                if !self.is_callable_runtime(&roots.target()) {
                    return Err(VmError::NotCallable);
                }
                let display = {
                    let target = roots.target();
                    let owner_bag = self.callable_bag_for_value(&target);
                    let owner_deleted = self.callable_deleted_flags_for_value(&target);
                    let mut ctx = function_metadata::FunctionMetadataContext::new(
                        context,
                        &mut self.gc_heap,
                        owner_bag,
                        &self.function_deleted_metadata,
                    )
                    .with_owner_deleted(owner_deleted);
                    function_metadata::callable_to_string(&mut ctx, &target)
                };
                let s = JsString::from_str(&display, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                Ok(Value::string(s))
            }
            VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance => {
                // §20.2.3.6: Return ? OrdinaryHasInstance(F, V) where
                // F is the `this` value and V is the first argument.
                // <https://tc39.es/ecma262/#sec-function.prototype-@@hasinstance>
                let v = roots
                    .take_args()
                    .into_iter()
                    .next()
                    .unwrap_or(Value::undefined());
                roots.set_receiver(v);
                let target = roots.target();
                let result =
                    self.ordinary_has_instance(stack, context, &target, &roots.receiver_value())?;
                Ok(Value::boolean(result))
            }
        }
    }

    /// ECMA-262 §10.4.3 `OrdinaryHasInstance(C, O)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryhasinstance>
    pub(crate) fn ordinary_has_instance(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        c: &Value,
        o: &Value,
    ) -> Result<bool, VmError> {
        let mut constructor = *c;
        let mut object = *o;
        let mut prototype = Value::undefined();
        let mut bound_target = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: every registered local precedes `roots` and stays stationary
        // through all getter, Proxy, @@hasInstance, and recursive calls below.
        unsafe {
            roots.add_value(&mut constructor);
            roots.add_value(&mut object);
            roots.add_value(&mut prototype);
            roots.add_value(&mut bound_target);
        }

        if !self.is_callable_runtime(&constructor) {
            return Ok(false);
        }
        if let Some(bound) = constructor.as_bound_function() {
            (bound_target, _, _) = bound.parts(&self.gc_heap);
            return self.instanceof_operator(stack, context, &object, &bound_target);
        }
        if !object.is_object_type() {
            return Ok(false);
        }
        let Some(resolved_prototype) =
            self.instanceof_target_prototype(stack, context, &constructor)?
        else {
            return Ok(false);
        };
        prototype = resolved_prototype;
        if !(prototype.is_object_type() || prototype.is_proxy()) {
            return Err(self.err_type(
                ("Function has non-object prototype 'undefined' in instanceof check".to_string())
                    .into(),
            ));
        }
        self.value_has_proxy_aware_prototype(stack, context, object, &prototype)
    }

    /// ECMA-262 §13.10.2 `InstanceofOperator(V, target)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-instanceofoperator>
    pub(crate) fn instanceof_operator(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        v: &Value,
        target: &Value,
    ) -> Result<bool, VmError> {
        let mut value = *v;
        let mut target = *target;
        let mut handler = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: these locals precede `roots` and remain stationary until the
        // full `instanceof` operation, including observable getters/calls,
        // commits or throws.
        unsafe {
            roots.add_value(&mut value);
            roots.add_value(&mut target);
            roots.add_value(&mut handler);
        }

        if !target.is_object_type() {
            return Err(self
                .err_type(("Right-hand side of instanceof is not an object".to_string()).into()));
        }
        let has_instance_sym = self.well_known_symbols.get(symbol::WellKnown::HasInstance);
        let key = VmPropertyKey::Symbol(has_instance_sym);
        handler = match self.ordinary_get_value(stack, context, target, target, &key, 0)? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, target, SmallVec::new())?
            }
        };
        if !handler.is_nullish() {
            if !self.is_callable_runtime(&handler) {
                return Err(self.err_type(("@@hasInstance must be callable".to_string()).into()));
            }
            if let Some(native) = handler.as_native_function()
                && native.is_vm_intrinsic(
                    &self.gc_heap,
                    VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance,
                )
            {
                return self.ordinary_has_instance(stack, context, &target, &value);
            }
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(value);
            let result = self.run_callable_sync_rooted(stack, context, &handler, target, args)?;
            return Ok(result.to_boolean(&self.gc_heap));
        }
        if !self.is_callable_runtime(&target) {
            return Err(
                self.err_type(("Right-hand side of instanceof is not callable".to_string()).into())
            );
        }
        self.ordinary_has_instance(stack, context, &target, &value)
    }

    /// Direct argv read for an untouched `arguments` object.
    ///
    /// Sound exactly when nothing observable diverged from construction: the
    /// current hidden class is still the canonical arity shape (any add,
    /// delete, redefine, or attribute change moves or nulls it), no mapped
    /// parameter aliases exist, and the `length` slot still holds the arity.
    /// Value writes through the untouched shape land in the slab this reads.
    fn arguments_object_direct_list(&self, value: Value) -> Option<SmallVec<[Value; 8]>> {
        let obj = value.as_object()?;
        let (shape, argc) = crate::object::arguments_direct_snapshot(obj, &self.gc_heap)?;
        let canonical = [true, false].into_iter().any(|mapped| {
            self.arguments_shape_cache
                .get(&(argc as u32, mapped))
                .is_some_and(|cached| cached.offset() == shape.offset())
        });
        if !canonical {
            return None;
        }
        let length_slot = u16::try_from(argc).ok()?;
        if crate::object::data_value_at(obj, &self.gc_heap, length_slot)
            != Value::number(crate::NumberValue::from_i32(argc as i32))
        {
            return None;
        }
        Some(
            (0..argc)
                .map(|index| crate::object::data_value_at(obj, &self.gc_heap, index as u16))
                .collect(),
        )
    }

    pub(crate) fn create_list_from_array_like(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<SmallVec<[Value; 8]>, VmError> {
        if let Some(arr) = value.as_array() {
            // §7.3.18 step 4-5 — substitute holes with `undefined`.
            return Ok(crate::array::with_elements(
                arr,
                &self.gc_heap,
                |elements| {
                    elements
                        .iter()
                        .map(|v| if v.is_hole() { Value::undefined() } else { *v })
                        .collect()
                },
            ));
        }
        if let Some(values) = self.arguments_object_direct_list(value) {
            return Ok(values);
        }
        // §7.3.18 — `Type(obj) must be Object`. Cover every shape
        // the VM models as a JS Object: ordinary objects, arrays,
        // proxies, every callable variant (so `Reflect.apply(fn,
        // null, new Function())` reads `.length` and walks indices
        // per spec), plus the exotic objects with their own
        // `[[Get]]` ladder (RegExp, ArrayBuffer, DataView,
        // TypedArray, collections, etc.).
        if !value.is_object_type() {
            return Err(self.err_type(
                ("Function.prototype.apply argument list must be object-like".to_string()).into(),
            ));
        }
        let length = self.get_property_value_for_call(stack, context, value, "length")?;
        // §7.3.18 step 3 — `len = ? ToLength(? Get(obj, "length"))`. The
        // §7.1.4 ToNumber inside ToLength fires a `valueOf` /
        // `@@toPrimitive` hook on an object-valued `length`, so coerce
        // with the execution context here rather than through the
        // infallible primitive-only `to_length` (which would read an
        // object as NaN -> 0).
        let length_num = self.coerce_to_number(stack, context, &length)?;
        let len = to_length(&Value::number(length_num), &self.gc_heap)?;
        let mut values = SmallVec::new();
        for index in 0..len {
            let key = index.to_string();
            values.push(self.get_property_value_for_call(stack, context, value, &key)?);
        }
        Ok(values)
    }

    /// Public `[[Get]]` for host code (e.g. `assert.throws` inspecting a thrown
    /// error's `code`/`name`/`message`): walks the prototype chain and invokes
    /// accessors. Out-of-crate runtime/native bindings use this instead of the
    /// own-only `object::get`.
    ///
    /// # Errors
    /// Propagates any error thrown by an invoked getter.
    pub fn get_property(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        self.get_property_value_for_call(stack, context, receiver, key)
    }

    pub(crate) fn get_property_value_for_call(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        let property_key = VmPropertyKey::String(key);
        match self.ordinary_get_value(stack, context, receiver, receiver, &property_key, 0)? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
            }
        }
    }

    /// §7.3.22 `SpeciesConstructor(O, defaultConstructor)`. Reads
    /// `O.constructor`, then its `@@species`, validating each per the
    /// spec ladder and falling back to `default_ctor` when the
    /// constructor or species hook is absent / nullish. Both reads run
    /// through the `[[Get]]` ladder so user getters fire.
    pub(crate) fn species_constructor_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        obj: &Value,
        default_ctor: &Value,
    ) -> Result<Value, VmError> {
        let c = self.get_property_value_for_call(stack, context, *obj, "constructor")?;
        if c.is_undefined() {
            return Ok(*default_ctor);
        }
        if !c.is_object_type() {
            return Err(self.err_type(("constructor property is not an object".to_string()).into()));
        }
        let species_sym = self
            .well_known_symbols()
            .get(crate::symbol::WellKnown::Species);
        let s = match self.ordinary_get_value(
            stack,
            context,
            c,
            c,
            &VmPropertyKey::Symbol(species_sym),
            0,
        )? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, c, SmallVec::new())?
            }
        };
        if s.is_nullish() {
            return Ok(*default_ctor);
        }
        if abstract_ops::is_constructor(&s, context, &self.gc_heap) {
            return Ok(s);
        }
        Err(self.err_type(("Symbol.species value is not a constructor".to_string()).into()))
    }
    pub(crate) fn callable_bind_metadata_get(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        key: &str,
    ) -> Result<BindMetadataGet, VmError> {
        if let Some(function_id) = target.as_function() {
            return match self.ordinary_function_own_property_descriptor(
                Some(context),
                None,
                function_id,
                key,
            )? {
                Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                None => Ok(BindMetadataGet::Value(Value::undefined())),
            };
        }
        if let Some(closure) = target.as_closure(&self.gc_heap) {
            return match self.ordinary_function_own_property_descriptor(
                Some(context),
                Some(closure),
                closure.cached_function_id,
                key,
            )? {
                Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                None => Ok(BindMetadataGet::Value(Value::undefined())),
            };
        }
        if let Some(native) = target.as_native_function() {
            return match native.own_property_descriptor(&mut self.gc_heap, key)? {
                Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                None => Ok(BindMetadataGet::Value(Value::undefined())),
            };
        }
        if let Some(bound) = target.as_bound_function() {
            return match function_metadata::bound_own_property_descriptor(
                &bound,
                &mut self.gc_heap,
                key,
            )? {
                Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                None => Ok(BindMetadataGet::Value(Value::undefined())),
            };
        }
        if let Some(class) = target.as_class_constructor() {
            let ctor = class.ctor(&self.gc_heap);
            return self.callable_bind_metadata_get(context, &ctor, key);
        }
        if let Some(obj) = target.as_object() {
            if let Some(desc) = object::get_own_descriptor(obj, &self.gc_heap, key) {
                return Ok(bind_metadata_get_from_descriptor(desc));
            }
            if let Some(native) = object::constructor_native(obj, &self.gc_heap)
                && native.is_native_function()
            {
                return self.callable_bind_metadata_get(context, &native, key);
            }
            return Ok(BindMetadataGet::Value(Value::undefined()));
        }
        Ok(BindMetadataGet::Value(Value::undefined()))
    }

    pub(crate) fn coerce_vm_property_key(
        arg: Option<&Value>,
        heap: &otter_gc::GcHeap,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        let Some(value) = arg else {
            return Ok(VmPropertyKey::String("undefined"));
        };
        if let Some(s) = value.as_string(heap) {
            return Ok(VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
        }
        if let Some(n) = value.as_number() {
            return Ok(VmPropertyKey::OwnedString(n.to_display_string()));
        }
        if let Some(b) = value.as_boolean() {
            return Ok(VmPropertyKey::String(if b { "true" } else { "false" }));
        }
        if value.is_null() {
            return Ok(VmPropertyKey::String("null"));
        }
        if value.is_undefined() {
            return Ok(VmPropertyKey::String("undefined"));
        }
        if let Some(sym) = value.as_symbol(heap) {
            return Ok(VmPropertyKey::Symbol(sym));
        }
        Err(VmError::TypeMismatch)
    }

    /// Read a callable's own-property bag without creating one.
    ///
    /// Closures own a per-instance bag in their GC body (so siblings
    /// minted from the same source template do NOT share expandos);
    /// bare interned function values fall back to the template-keyed
    /// [`Self::function_user_props`] side table.
    /// Owner-aware deleted-metadata check: closure instances read their
    /// body flags, bare template functions the global set.
    pub(crate) fn ordinary_metadata_deleted(
        &self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        metadata_key: &'static str,
    ) -> bool {
        match owner {
            Some(c) => c.metadata_deleted(&self.gc_heap, metadata_key),
            None => self
                .function_deleted_metadata
                .contains(&(function_id, metadata_key)),
        }
    }

    pub(crate) fn callable_bag_read(
        &self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
    ) -> Option<JsObject> {
        match owner {
            Some(c) => c.own_props(&self.gc_heap),
            None => self.function_user_props.get(&function_id).copied(),
        }
    }

    /// Resolve a callable value's own-property bag directly (closure →
    /// per-instance body bag; bare function → template side table).
    /// Returns `None` for non-callables or callables with no expandos.
    /// Per-closure-instance deleted-metadata flags for a callable
    /// owner, `None` for bare template functions (global set applies).
    pub(crate) fn callable_deleted_flags(
        &self,
        owner: Option<crate::closure::JsClosure>,
    ) -> Option<(bool, bool)> {
        owner.map(|c| {
            (
                c.metadata_deleted(&self.gc_heap, "name"),
                c.metadata_deleted(&self.gc_heap, "length"),
            )
        })
    }

    /// As [`Self::callable_deleted_flags`] resolved from a value.
    pub(crate) fn callable_deleted_flags_for_value(&self, value: &Value) -> Option<(bool, bool)> {
        self.callable_deleted_flags(value.as_closure(&self.gc_heap))
    }

    pub(crate) fn callable_bag_for_value(&self, value: &Value) -> Option<JsObject> {
        if let Some(c) = value.as_closure(&self.gc_heap) {
            return c.own_props(&self.gc_heap);
        }
        if let Some(fid) = value.as_function() {
            return self.function_user_props.get(&fid).copied();
        }
        None
    }

    pub(crate) fn function_user_bag(
        &mut self,
        stack: &mut ActivationStack,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        value_roots: &[&Value],
    ) -> Result<JsObject, VmError> {
        self.with_handle_scope(|interp, scope| {
            // Park every transient descriptor input in the handle arena instead
            // of materialising another roots snapshot for this allocation.
            for value in value_roots {
                let _ = interp.scoped_value(scope, **value);
            }
            let owner = owner.map(|owner| interp.scoped_value(scope, Value::closure(owner)));
            if let Some(owner) = owner {
                let closure = interp
                    .escape_scoped(owner)
                    .as_closure(&interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                if let Some(bag) = closure.own_props(&interp.gc_heap) {
                    return Ok(bag);
                }
                let bag = interp.alloc_stack_rooted_object_with_extra_roots(stack, &[])?;
                let closure = interp
                    .escape_scoped(owner)
                    .as_closure(&interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                closure.set_own_props(&mut interp.gc_heap, bag);
                return Ok(bag);
            }
            match interp.function_user_props.get(&function_id).copied() {
                Some(bag) => Ok(bag),
                None => {
                    let bag = interp.alloc_stack_rooted_object_with_extra_roots(stack, &[])?;
                    interp.function_user_props.insert(function_id, bag);
                    Ok(bag)
                }
            }
        })
    }

    /// §10.1.3 / §10.1.4 ordinary function `[[Extensible]]`.
    ///
    /// A closure is a distinct function object per evaluation of its
    /// definition, so its flag lives in the closure body. Interned
    /// `Value::function` templates own no body and keep theirs in the
    /// interpreter side table next to their expando bag.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-isextensible>
    /// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-preventextensions>
    pub(crate) fn ordinary_function_is_extensible(
        &self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
    ) -> bool {
        match owner {
            Some(closure) => closure.is_extensible(&self.gc_heap),
            None => !self.function_non_extensible.contains(&function_id),
        }
    }

    pub(crate) fn ordinary_function_prevent_extensions(
        &mut self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
    ) {
        match owner {
            Some(closure) => closure.prevent_extensions(&mut self.gc_heap),
            None => {
                self.function_non_extensible.insert(function_id);
            }
        }
    }

    pub(crate) fn ordinary_function_has_own_string_property_for_extensibility(
        &mut self,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
    ) -> Result<bool, VmError> {
        if self
            .ordinary_function_own_property_descriptor(Some(context), owner, function_id, key)?
            .is_some()
        {
            return Ok(true);
        }
        Ok(key == "prototype"
            && context.function_has_prototype_property(function_id)
            && !self
                .function_deleted_metadata
                .contains(&(function_id, "prototype")))
    }

    pub(crate) fn ordinary_function_has_own_symbol_property_for_extensibility(
        &self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: crate::symbol::JsSymbol,
    ) -> bool {
        self.callable_bag_read(owner, function_id)
            .and_then(|bag| crate::object::get_own_symbol_descriptor(bag, &self.gc_heap, key))
            .is_some()
    }

    /// Own string-keyed property names for an ordinary function
    /// record, in spec creation order.
    ///
    /// Mirrors §10.2.4 OrdinaryFunctionCreate's installed metadata:
    /// `length`, `name`, and (for non-arrow callable shapes)
    /// `prototype` — plus any user-installed own properties that
    /// live in [`Self::function_user_props`]. Each intrinsic key is
    /// suppressed if [`Self::function_deleted_metadata`] records a
    /// matching deletion, so the result agrees with
    /// [`Self::ordinary_function_own_property_descriptor`] and
    /// `hasOwnProperty`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-object.getownpropertynames>
    /// - <https://tc39.es/ecma262/#sec-ordinaryfunctioncreate>
    /// - <https://tc39.es/ecma262/#sec-makeconstructor>
    pub(crate) fn ordinary_function_own_property_keys(
        &self,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
    ) -> Vec<String> {
        let mut keys = Vec::new();
        let has_prototype = context.function_has_prototype_property(function_id);
        let deleted = |key: &'static str| self.ordinary_metadata_deleted(owner, function_id, key);
        if !deleted("length") {
            keys.push("length".to_string());
        }
        if !deleted("name") {
            keys.push("name".to_string());
        }
        let mut bag_has_prototype = false;
        if let Some(bag) = self.callable_bag_read(owner, function_id) {
            crate::object::with_properties(bag, &self.gc_heap, |p| {
                for k in p.keys() {
                    if k == "length" || k == "name" {
                        continue;
                    }
                    if k == "prototype" {
                        bag_has_prototype = true;
                    }
                    keys.push(k.to_string());
                }
            });
        }
        if has_prototype && !bag_has_prototype && !deleted("prototype") {
            keys.push("prototype".to_string());
        }
        keys
    }

    /// Own string-keyed property names for constructor wrappers.
    ///
    /// `Value::ClassConstructor` stores the callable metadata
    /// (`length`, `name`) on the wrapped constructor value, while
    /// static methods live on a separate static-side object. The JS
    /// own-property surface observes both, plus the constructor's
    /// mandated `prototype` property.
    pub(crate) fn class_constructor_own_property_keys(
        &self,
        context: Option<&ExecutionContext>,
        class: ClassConstructor,
    ) -> Result<Vec<String>, VmError> {
        let ctor = class.ctor(&self.gc_heap);
        let function_id = ctor
            .as_function()
            .or_else(|| ctor.as_closure(&self.gc_heap).map(|c| c.cached_function_id));
        let ctor_owner = ctor.as_closure(&self.gc_heap);
        let mut keys = if let Some(function_id) = function_id {
            let Some(context) = context else {
                return Err(VmError::InvalidOperand);
            };
            self.ordinary_function_own_property_keys(context, ctor_owner, function_id)
        } else if let Some(native) = ctor.as_native_function() {
            native.own_property_keys(&self.gc_heap)
        } else if let Some(bound) = ctor.as_bound_function() {
            function_metadata::bound_own_property_keys(&bound, &self.gc_heap)
        } else if let Some(inner) = ctor.as_class_constructor() {
            self.class_constructor_own_property_keys(context, inner)?
        } else {
            Vec::new()
        };

        if !keys.iter().any(|key| key == "prototype") {
            keys.push("prototype".to_string());
        }

        let statics = class.statics(&self.gc_heap);
        // §15.7.14 — a static `name`/`length` member REDEFINES the
        // constructor's implicit property, so it keeps the implicit
        // slot's creation position (length, then name, before
        // `prototype`), not the statics-bag insertion position.
        for key in ["name", "length"] {
            if !keys.iter().any(|existing| existing == key)
                && crate::object::get_own_descriptor(statics, &self.gc_heap, key).is_some()
            {
                // "length" always heads the list; "name" follows any
                // existing "length".
                let pos = usize::from(key == "name" && keys.first().is_some_and(|k| k == "length"));
                keys.insert(pos, key.to_string());
            }
        }
        for key in crate::object::with_properties(statics, &self.gc_heap, |p| {
            p.keys().map(str::to_string).collect::<Vec<_>>()
        }) {
            if !keys.iter().any(|existing| existing == &key) {
                keys.push(key);
            }
        }
        // §10.1.11 OrdinaryOwnPropertyKeys — array-index keys come first
        // in ascending numeric order, then the remaining string keys in
        // creation order. The metadata + statics merge above preserves
        // creation order for the string keys but leaves integer-index
        // static names (e.g. `static [1]() {}`) interleaved, so lift
        // them to the front here.
        let mut indices: Vec<(u32, String)> = Vec::new();
        let mut strings: Vec<String> = Vec::with_capacity(keys.len());
        for key in keys {
            match crate::object::array_index_property_name(&key) {
                Some(idx) => indices.push((idx, key)),
                None => strings.push(key),
            }
        }
        if indices.is_empty() {
            return Ok(strings);
        }
        indices.sort_by_key(|(idx, _)| *idx);
        let mut ordered: Vec<String> = Vec::with_capacity(indices.len() + strings.len());
        ordered.extend(indices.into_iter().map(|(_, key)| key));
        ordered.extend(strings);
        Ok(ordered)
    }

    pub(crate) fn ordinary_function_own_property_descriptor(
        &mut self,
        context: Option<&ExecutionContext>,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        if let Some(bag) = self.callable_bag_read(owner, function_id)
            && let Some(desc) = crate::object::get_own_descriptor(bag, &self.gc_heap, key)
        {
            return Ok(Some(desc));
        }
        let Some(metadata_key) = function_metadata::ordinary_function_metadata_key(key) else {
            return Ok(None);
        };
        if self.ordinary_metadata_deleted(owner, function_id, metadata_key) {
            return Ok(None);
        }
        let Some(context) = context else {
            return Ok(None);
        };
        let owner_bag = self.callable_bag_read(owner, function_id);
        let owner_deleted = self.callable_deleted_flags(owner);
        let mut ctx = function_metadata::FunctionMetadataContext::new(
            context,
            &mut self.gc_heap,
            owner_bag,
            &self.function_deleted_metadata,
        )
        .with_owner_deleted(owner_deleted);
        let value =
            function_metadata::ordinary_function_intrinsic_property(&mut ctx, function_id, key)?;
        Ok(Some(object::PropertyDescriptor::data(
            value, false, false, true,
        )))
    }

    pub(crate) fn ordinary_function_define_own_property(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
        desc_obj: Option<JsObject>,
        descriptor: object::PropertyDescriptor,
    ) -> Result<bool, VmError> {
        let descriptor = match self.ordinary_function_own_property_descriptor(
            context,
            owner,
            function_id,
            key,
        )? {
            Some(existing) => {
                let descriptor = if function_metadata::ordinary_function_metadata_key(key).is_some()
                {
                    match desc_obj {
                        Some(desc_obj) => complete_descriptor_defaults_from_object(
                            desc_obj,
                            &self.gc_heap,
                            descriptor,
                            &existing,
                        ),
                        None => descriptor,
                    }
                } else {
                    descriptor
                };
                match object::validate_descriptor_update(&existing, &descriptor, &self.gc_heap) {
                    Some(merged) => merged,
                    None => return Ok(false),
                }
            }
            None => {
                let has_virtual_prototype = context.is_some_and(|context| {
                    key == "prototype"
                        && context.function_has_prototype_property(function_id)
                        && !self
                            .function_deleted_metadata
                            .contains(&(function_id, "prototype"))
                });
                if !has_virtual_prototype
                    && !self.ordinary_function_is_extensible(owner, function_id)
                {
                    return Ok(false);
                }
                descriptor
            }
        };
        // The bag allocation and the define below can move every carried
        // value, and a shared reference into a stack local is not a root the
        // collector can rewrite. The owner closure and the descriptor's
        // payload values ride iteration-anchor slots and are re-read after
        // each allocating step.
        let owner_slot =
            self.push_iteration_anchor(owner.map(Value::closure).unwrap_or(Value::undefined())) - 1;
        let base = owner_slot;
        let (value_slot, getter_slot, setter_slot) = match &descriptor.kind {
            object::DescriptorKind::Data { value } => {
                (Some(self.push_iteration_anchor(*value) - 1), None, None)
            }
            object::DescriptorKind::Accessor { getter, setter } => (
                None,
                getter.as_ref().map(|g| self.push_iteration_anchor(*g) - 1),
                setter.as_ref().map(|s| self.push_iteration_anchor(*s) - 1),
            ),
        };
        let flags = descriptor.flags;
        let outcome = (|this: &mut Self| {
            let mut bag = this.function_user_bag(stack, owner, function_id, &[])?;
            let kind = match (value_slot, getter_slot, setter_slot) {
                (Some(value_slot), _, _) => object::DescriptorKind::Data {
                    value: this.iteration_anchor(value_slot),
                },
                (None, getter_slot, setter_slot) => object::DescriptorKind::Accessor {
                    getter: getter_slot.map(|slot| this.iteration_anchor(slot)),
                    setter: setter_slot.map(|slot| this.iteration_anchor(slot)),
                },
            };
            let descriptor = object::PropertyDescriptor { kind, flags };
            let ok = crate::object::define_own_property_in_place(
                &mut bag,
                &mut this.gc_heap,
                key,
                descriptor,
            );
            if ok && let Some(metadata_key) = function_metadata::ordinary_function_metadata_key(key)
            {
                match this.iteration_anchor(owner_slot).as_closure(&this.gc_heap) {
                    Some(c) => c.set_metadata_deleted(&mut this.gc_heap, metadata_key, false),
                    None => {
                        this.function_deleted_metadata
                            .remove(&(function_id, metadata_key));
                    }
                }
            }
            Ok(ok)
        })(self);
        self.pop_iteration_anchors_to(base);
        outcome
    }

    pub(crate) fn ordinary_function_delete_own_property(
        &mut self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
        has_prototype_property: bool,
    ) -> bool {
        // §10.2.4 — an ordinary function's implicit `prototype` slot is
        // writable but non-configurable: [[Delete]] refuses it unless a
        // user bag entry (materialized by defineProperty) shadows the
        // virtual slot, in which case the bag's own flags decide.
        if key == "prototype"
            && has_prototype_property
            && self
                .callable_bag_read(owner, function_id)
                .is_none_or(|bag| {
                    crate::object::get_own_descriptor(bag, &self.gc_heap, key).is_none()
                })
        {
            return false;
        }
        let Some(metadata_key) = function_metadata::ordinary_function_metadata_key(key) else {
            return self
                .callable_bag_read(owner, function_id)
                .map(|bag| crate::object::delete(bag, &mut self.gc_heap, key))
                .unwrap_or(true);
        };
        if let Some(bag) = self.callable_bag_read(owner, function_id)
            && crate::object::get_own_descriptor(bag, &self.gc_heap, key).is_some()
            && !crate::object::delete(bag, &mut self.gc_heap, key)
        {
            return false;
        }
        // A closure instance records the deletion on its own body —
        // sibling closures of the same template are unaffected.
        match owner {
            Some(c) => c.set_metadata_deleted(&mut self.gc_heap, metadata_key, true),
            None => {
                self.function_deleted_metadata
                    .insert((function_id, metadata_key));
            }
        }
        true
    }

    pub(crate) fn try_function_object_static_call(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        method: otter_bytecode::method_id::ObjectMethod,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        use otter_bytecode::method_id::ObjectMethod as M;
        let Some(target) = args.first().cloned() else {
            return Ok(None);
        };
        if (target.is_array() || target.is_function() || target.is_closure() || target.is_regexp())
            && matches!(method, M::Freeze | M::Seal | M::IsFrozen | M::IsSealed)
        {
            let Some(context) = context else {
                return Err(VmError::InvalidOperand);
            };
            match method {
                M::Freeze => {
                    if !self.set_integrity_level_value(
                        stack,
                        context,
                        &target,
                        crate::object_internal_ops::ObjectIntegrityLevel::Frozen,
                    )? {
                        return Err(self.err_type(("Object.freeze failed".to_string()).into()));
                    }
                    return Ok(Some(target));
                }
                M::Seal => {
                    if !self.set_integrity_level_value(
                        stack,
                        context,
                        &target,
                        crate::object_internal_ops::ObjectIntegrityLevel::Sealed,
                    )? {
                        return Err(self.err_type(("Object.seal failed".to_string()).into()));
                    }
                    return Ok(Some(target));
                }
                M::IsFrozen => {
                    let frozen = self.test_integrity_level_value(
                        stack,
                        context,
                        &target,
                        crate::object_internal_ops::ObjectIntegrityLevel::Frozen,
                    )?;
                    return Ok(Some(Value::boolean(frozen)));
                }
                M::IsSealed => {
                    let sealed = self.test_integrity_level_value(
                        stack,
                        context,
                        &target,
                        crate::object_internal_ops::ObjectIntegrityLevel::Sealed,
                    )?;
                    return Ok(Some(Value::boolean(sealed)));
                }
                _ => unreachable!("integrity methods are matched above"),
            }
        }
        if (target.is_proxy()
            || target.is_array()
            || target.is_regexp()
            || target.is_function()
            || target.is_closure()
            || target.is_bound_function()
            || target.is_native_function())
            && matches!(
                method,
                M::GetOwnPropertyDescriptor | M::HasOwn | M::Keys | M::GetOwnPropertyNames
            )
        {
            let Some(context) = context else {
                return if target.is_proxy() {
                    Err(VmError::InvalidOperand)
                } else {
                    Ok(None)
                };
            };
            if matches!(method, M::GetOwnPropertyNames) {
                // §20.1.2.12 — `getOwnPropertyNames(O)` returns
                // every own string-keyed property in
                // `[[OwnPropertyKeys]]` order, regardless of
                // enumerability. Route all exotic/function shapes
                // through the shared internal-method implementation
                // so Arrays, string wrappers, functions, and Proxies
                // agree with `Reflect.ownKeys`.
                let values: Vec<Value> = self
                    .own_property_keys_value(stack, context, &target)?
                    .into_iter()
                    .filter(|v| v.is_string())
                    .collect();
                return Ok(Some(Value::array(self.function_static_array_from_values(
                    &*stack,
                    values,
                    &[&target],
                    &[args],
                )?)));
            }
            if matches!(method, M::Keys) {
                // For Proxy targets, route through the full §10.5.11
                // ownKeys path so trap invariants apply, then filter
                // to enumerable strings per §20.1.2.17 Object.keys.
                if target.is_proxy() {
                    let trap_keys = self.own_property_keys_value(stack, context, &target)?;
                    let mut values: Vec<Value> = Vec::with_capacity(trap_keys.len());
                    for key in trap_keys {
                        if !key.is_string() {
                            continue;
                        }
                        let vm_key = if let Some(s) = key.as_string(&self.gc_heap) {
                            VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
                        } else if let Some(sym) = key.as_symbol(&self.gc_heap) {
                            VmPropertyKey::Symbol(sym)
                        } else {
                            return Err(VmError::TypeMismatch);
                        };
                        let desc = self.ordinary_get_own_property_descriptor_value(
                            stack, context, target, &vm_key, 0,
                        )?;
                        if desc.as_ref().is_some_and(|d| d.enumerable()) {
                            values.push(key);
                        }
                    }
                    return Ok(Some(Value::array(self.function_static_array_from_values(
                        &*stack,
                        values,
                        &[&target],
                        &[args],
                    )?)));
                }
                let keys = self.enumerable_own_string_keys_for_value(stack, context, target, 0)?;
                let mut values = Vec::with_capacity(keys.len());
                for key in keys {
                    values.push(Value::string(
                        JsString::from_str(&key, self.gc_heap_mut())
                            .map_err(|_| VmError::TypeMismatch)?,
                    ));
                }
                return Ok(Some(Value::array(self.function_static_array_from_values(
                    &*stack,
                    values,
                    &[&target],
                    &[args],
                )?)));
            }
            let desc =
                self.get_own_property_descriptor_for_value(stack, context, target, args.get(1))?;
            if matches!(method, M::HasOwn) {
                return Ok(Some(Value::boolean(desc.is_some())));
            }
            return match desc {
                Some(desc) => Ok(Some(Value::object(
                    self.function_static_descriptor_to_object(&desc, &[&target], args)?,
                ))),
                None => Ok(Some(Value::undefined())),
            };
        }
        let owner = target.as_closure(&self.gc_heap);
        let function_id = if let Some(id) = target.as_function() {
            Some(id)
        } else if let Some(closure) = owner {
            Some(closure.cached_function_id)
        } else if target.is_bound_function() {
            None
        } else {
            return Ok(None);
        };
        match method {
            M::DefineProperty => {
                // Key coercion, descriptor coercion, and the define below all
                // allocate; the args slice can be an untraced copy on the
                // operands-dispatch path, so the receiver rides an anchor slot.
                let target_slot = self.push_iteration_anchor(target) - 1;
                let outcome = (|this: &mut Self| -> Result<Option<Value>, VmError> {
                    let key = Self::coerce_vm_property_key(args.get(1), &this.gc_heap)?;
                    let desc_obj = args
                        .get(2)
                        .and_then(|v| v.as_object())
                        .ok_or(VmError::TypeMismatch)?;
                    let descriptor =
                        object_statics::coerce_to_descriptor(&desc_obj, &this.gc_heap)?;
                    let completed = descriptor.complete_for_new_property();
                    // The coercions above may have moved the receiver; the
                    // closure handle is re-derived from the anchor.
                    let owner = this.iteration_anchor(target_slot).as_closure(&this.gc_heap);
                    let ok = match (function_id, &key) {
                        (Some(function_id), VmPropertyKey::Symbol(sym)) => {
                            if !this.ordinary_function_has_own_symbol_property_for_extensibility(
                                owner,
                                function_id,
                                *sym,
                            ) && !this.ordinary_function_is_extensible(owner, function_id)
                            {
                                return Err(VmError::TypeMismatch);
                            }
                            let mut bag = this.function_user_bag(stack, owner, function_id, &[])?;
                            crate::object::define_own_symbol_property_partial(
                                &mut bag,
                                &mut this.gc_heap,
                                *sym,
                                descriptor,
                            )
                        }
                        (Some(function_id), _) => this.ordinary_function_define_own_property(
                            stack,
                            context,
                            owner,
                            function_id,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            Some(desc_obj),
                            completed,
                        )?,
                        (None, VmPropertyKey::Symbol(_)) => false,
                        (None, _) => {
                            let target = this.iteration_anchor(target_slot);
                            let Some(bound) = target.as_bound_function() else {
                                return Ok(None);
                            };
                            function_metadata::bound_define_own_property(
                                &bound,
                                &mut this.gc_heap,
                                key.string_name()
                                    .expect("non-symbol key has string spelling"),
                                completed,
                            )
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeMismatch);
                    }
                    Ok(Some(this.iteration_anchor(target_slot)))
                })(self);
                self.pop_iteration_anchors_to(target_slot);
                outcome
            }
            M::GetOwnPropertyDescriptor => {
                let key = Self::coerce_vm_property_key(args.get(1), &self.gc_heap)?;
                let desc = match (function_id, &key) {
                    (Some(function_id), VmPropertyKey::Symbol(sym)) => {
                        let Some(bag) = self.callable_bag_read(owner, function_id) else {
                            return Ok(Some(Value::undefined()));
                        };
                        crate::object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym)
                    }
                    (Some(function_id), _) => self.ordinary_function_own_property_descriptor(
                        context,
                        owner,
                        function_id,
                        key.string_name()
                            .expect("non-symbol key has string spelling"),
                    )?,
                    (None, VmPropertyKey::Symbol(_)) => None,
                    (None, _) => {
                        let Some(bound) = target.as_bound_function() else {
                            return Ok(None);
                        };
                        function_metadata::bound_own_property_descriptor(
                            &bound,
                            &mut self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        )?
                    }
                };
                match desc {
                    Some(desc) => Ok(Some(Value::object(
                        self.function_static_descriptor_to_object(&desc, &[&target], args)?,
                    ))),
                    None => Ok(Some(Value::undefined())),
                }
            }
            M::HasOwn => {
                let key = Self::coerce_vm_property_key(args.get(1), &self.gc_heap)?;
                let present = match (function_id, &key) {
                    (Some(function_id), VmPropertyKey::Symbol(sym)) => self
                        .callable_bag_read(owner, function_id)
                        .map(|bag| crate::object::has_own_symbol(bag, &self.gc_heap, *sym))
                        .unwrap_or(false),
                    (Some(function_id), _) => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        let user_present = self
                            .callable_bag_read(owner, function_id)
                            .map(|bag| {
                                !matches!(
                                    crate::object::lookup_own(bag, &self.gc_heap, key),
                                    object::PropertyLookup::Absent
                                )
                            })
                            .unwrap_or(false);
                        user_present
                            || function_metadata::ordinary_function_metadata_key(key).is_some_and(
                                |metadata_key| {
                                    !self
                                        .function_deleted_metadata
                                        .contains(&(function_id, metadata_key))
                                },
                            )
                    }
                    (None, VmPropertyKey::Symbol(_)) => false,
                    (None, _) => {
                        let Some(bound) = target.as_bound_function() else {
                            return Ok(None);
                        };
                        function_metadata::bound_has_own_property(
                            &bound,
                            &self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        )
                    }
                };
                Ok(Some(Value::boolean(present)))
            }
            // §20.1.2.14 / §20.1.2.18 — ordinary functions keep
            // expando storage outside `ObjectBody`, so handle their
            // `[[Extensible]]` state before the generic static
            // dispatcher. This mirrors §10.1.3/§10.1.4 for the
            // side-table-backed function shape.
            M::IsExtensible => {
                let owner = target.as_closure(&self.gc_heap);
                let function_id = target
                    .as_function()
                    .or_else(|| owner.map(|c| c.cached_function_id));
                match function_id {
                    Some(function_id) => Ok(Some(Value::boolean(
                        self.ordinary_function_is_extensible(owner, function_id),
                    ))),
                    None => Ok(None),
                }
            }
            M::PreventExtensions => {
                let owner = target.as_closure(&self.gc_heap);
                let function_id = target
                    .as_function()
                    .or_else(|| owner.map(|c| c.cached_function_id));
                match function_id {
                    Some(function_id) => {
                        self.ordinary_function_prevent_extensions(owner, function_id);
                        Ok(Some(target))
                    }
                    None => Ok(None),
                }
            }
            // §20.1.2 — only the methods above need the function-as-
            // object fast path; everything else falls through to the
            // ordinary object_statics dispatcher.
            M::Assign
            | M::Create
            | M::DefineProperties
            | M::Entries
            | M::Freeze
            | M::FromEntries
            | M::GetOwnPropertyDescriptors
            | M::GetOwnPropertyNames
            | M::GetOwnPropertySymbols
            | M::IsFrozen
            | M::IsSealed
            | M::Keys
            | M::Seal
            | M::Values
            | M::GroupBy
            | M::ForInKeys => Ok(None),
        }
    }

    fn function_static_array_from_values(
        &mut self,
        stack: &ActivationStack,
        values: Vec<Value>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<array::JsArray, VmError> {
        self.alloc_stack_rooted_array_from_values_with_root_slices(
            stack,
            values,
            value_roots,
            slice_roots,
        )
    }

    fn function_static_descriptor_to_object(
        &mut self,
        desc: &object::PropertyDescriptor,
        value_roots: &[&Value],
        slice_roots: &[Value],
    ) -> Result<JsObject, VmError> {
        self.with_handle_scope(|interp, scope| {
            // Keep the caller-supplied boundary values in the same canonical
            // arena as the descriptor fields. `scoped_descriptor_object`
            // parks its result before the first property write and resolves
            // that slot again for every later write, so a shape/slab
            // allocation cannot strand a copied `JsObject`.
            for value in value_roots {
                let _ = interp.scoped_value(scope, **value);
            }
            for value in slice_roots {
                let _ = interp.scoped_value(scope, *value);
            }
            let result = interp.scoped_descriptor_object(scope, desc)?;
            interp
                .escape_scoped(result)
                .as_object()
                .ok_or(VmError::TypeMismatch)
        })
    }

    /// Preflight dispatcher for `Object.<X>(target)` calls whose
    /// target is a `Value::Proxy`. Routes the spec-mandated internal
    /// methods through the value-level helpers so `Object.isExtensible`
    /// and `Object.preventExtensions` observe proxy traps and the
    /// §10.5 invariants. (`getPrototypeOf` / `setPrototypeOf` go
    /// through dedicated opcodes `Op::GetPrototype` / `Op::SetPrototype`
    /// rather than the `ObjectCall` dispatcher.)
    ///
    /// Returns `Ok(None)` when the method does not need proxy-aware
    /// dispatch, so the caller falls through to the ordinary
    /// `object_statics::call` path.
    pub(crate) fn function_property_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        self.function_property_get_with_receiver(stack, context, owner, function_id, None, name)
    }

    /// SpiderMonkey-legacy magic `fn.caller` / `fn.arguments` read.
    /// `Some(value)` for an eligible sloppy ordinary function
    /// receiver (a live stack walk / arguments snapshot); `None`
    /// falls through to the poisoned %Function.prototype% accessors.
    pub(crate) fn legacy_restricted_property(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        name: &str,
    ) -> Result<Option<Value>, VmError> {
        let fid = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        let Some(fid) = fid else {
            return Ok(None);
        };
        if !self.legacy_function_metadata_eligible(context, fid) {
            return Ok(None);
        }
        match name {
            "caller" => Ok(Some(self.legacy_caller_value(context, stack, receiver))),
            "arguments" => self
                .legacy_arguments_value(stack, context, receiver)
                .map(Some),
            _ => Ok(None),
        }
    }

    /// SpiderMonkey-legacy `fn.caller` / `fn.arguments` eligibility:
    /// only sloppy ordinary functions (not arrows, methods,
    /// generators, async functions) get the magic values; every other
    /// shape falls through to the poisoned %ThrowTypeError%
    /// accessors on %Function.prototype%.
    fn legacy_function_metadata_eligible(
        &self,
        context: &ExecutionContext,
        function_id: u32,
    ) -> bool {
        !context.function_is_strict(function_id)
            && !context.function_is_arrow(function_id)
            && context
                .function(function_id)
                .is_some_and(|f| !f.is_generator && !f.is_async && !f.is_method)
    }

    /// SpiderMonkey-legacy `fn.caller`: the function value of the
    /// nearest frame below `callee`'s innermost activation whose
    /// function is ordinary script code. `<main>` / eval-chunk /
    /// module frames are looked through (legacy `caller` sees past
    /// eval boundaries); strict, generator, and async callers are
    /// censored to `null`; no live activation yields `null`.
    fn legacy_caller_value(
        &self,
        context: &ExecutionContext,
        stack: &ActivationStack,
        callee: Value,
    ) -> Value {
        let mut found = false;
        for frame in stack.iter().rev() {
            if !found {
                found = frame.self_value == callee;
                continue;
            }
            let Some(function) = context.exec_function(frame.function_id) else {
                continue;
            };
            let is_main = context
                .function(frame.function_id)
                .is_none_or(|f| f.name == "<main>");
            if function.is_module || is_main {
                continue;
            }
            if context.function_is_strict(frame.function_id)
                || context
                    .function(frame.function_id)
                    .is_none_or(|f| f.is_generator || f.is_async)
            {
                return Value::null();
            }
            return frame.self_value;
        }
        Value::null()
    }

    /// SpiderMonkey-legacy `fn.arguments`: a fresh snapshot object of
    /// the innermost live activation's arguments (`null` when the
    /// function is not executing). Exact when the frame captured its
    /// incoming argv (`needs_arguments` bodies); otherwise
    /// reconstructed from the parameter registers.
    fn legacy_arguments_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
    ) -> Result<Value, VmError> {
        let mut args: Option<Vec<Value>> = None;
        for frame in stack.iter().rev() {
            if frame.self_value != callee {
                continue;
            }
            if let Some(cold) = self.frame_cold(frame)
                && !cold.incoming_args.is_empty()
            {
                args = Some(cold.incoming_args.to_vec());
                break;
            }
            let param_count = context
                .exec_function(frame.function_id)
                .map(|f| f.param_count as usize)
                .unwrap_or(0);
            let mut collected = Vec::with_capacity(param_count);
            for reg in 0..param_count {
                collected.push(
                    crate::read_register(frame, reg as u16)
                        .copied()
                        .unwrap_or_else(|_| Value::undefined()),
                );
            }
            args = Some(collected);
            break;
        }
        let Some(mut args) = args else {
            return Ok(Value::null());
        };
        let mut callee_root = callee;
        let roots: Vec<&Value> = Vec::new();
        let _ = roots;
        let mut extra: Vec<&Value> = Vec::with_capacity(args.len() + 1);
        extra.push(&callee_root);
        for value in &args {
            extra.push(value);
        }
        let obj = self.alloc_stack_rooted_object_with_extra_roots(stack, &extra)?;
        drop(extra);
        // The object handle and argument values stay valid: the
        // property defines below only allocate property storage, and
        // each define re-reads its value from the rooted vec slot.
        let mut obj = obj;
        {
            let mut scope = otter_gc::RootScope::new(&mut self.gc_heap);
            // SAFETY: locals declared above the scope, stable until drop.
            unsafe {
                scope.add_object(&mut obj);
                scope.add_value(&mut callee_root);
                scope.add_value_vec(&mut args);
            }
            for (index, value) in args.iter().enumerate() {
                let desc = crate::object::PropertyDescriptor::data(*value, true, true, true);
                crate::object::define_own_property(
                    obj,
                    &mut self.gc_heap,
                    index.to_string().as_str(),
                    desc,
                );
            }
            let length = Value::number(crate::NumberValue::from_i32(args.len() as i32));
            let desc = crate::object::PropertyDescriptor::data(length, true, false, true);
            crate::object::define_own_property(obj, &mut self.gc_heap, "length", desc);
            let desc = crate::object::PropertyDescriptor::data(callee_root, true, false, true);
            crate::object::define_own_property(obj, &mut self.gc_heap, "callee", desc);
        }
        Ok(Value::object(obj))
    }

    fn function_property_get_non_prototype(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        if let Some(bag) = self.callable_bag_read(owner, function_id)
            && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
        {
            return Ok(v);
        }
        if crate::interp::helpers::is_restricted_function_property(name) {
            let receiver = owner
                .map(Value::closure)
                .unwrap_or_else(|| Value::function(function_id));
            if let Some(value) = self.legacy_restricted_property(stack, context, receiver, name)? {
                return Ok(value);
            }
        }
        if name == "name" || name == "length" {
            // A deleted own metadata property falls through to the
            // ordinary prototype walk — %Function.prototype%'s own
            // `name`/`length` are themselves deletable and must not
            // resurrect through the intrinsic table.
            let deleted = function_metadata::ordinary_function_metadata_key(name)
                .is_some_and(|key| self.ordinary_metadata_deleted(owner, function_id, key));
            if !deleted {
                let owner_bag = self.callable_bag_read(owner, function_id);
                let owner_deleted = self.callable_deleted_flags(owner);
                let mut ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &mut self.gc_heap,
                    owner_bag,
                    &self.function_deleted_metadata,
                )
                .with_owner_deleted(owner_deleted);
                return function_metadata::ordinary_function_intrinsic_property(
                    &mut ctx,
                    function_id,
                    name,
                );
            }
        }
        // A user-mutated [[Prototype]] replaces the intrinsic chain:
        // continue the ordinary walk from the override.
        if let Some(over) = self.ordinary_function_prototype_override(owner, function_id) {
            if over.is_null() {
                return Ok(Value::undefined());
            }
            let key = VmPropertyKey::OwnedString(name.to_string());
            return match self.ordinary_get_value(stack, context, over, over, &key, 0)? {
                VmGetOutcome::Value(v) => Ok(v),
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync_rooted(stack, context, &getter, over, SmallVec::new())
                }
            };
        }
        if let Some(proto) = self.function_kind_prototype_for(context, function_id)
            && let Some(value) = object::get(proto, &self.gc_heap, name)
        {
            return Ok(value);
        }
        if let Some(value) = self
            .load_function_prototype_method(name)
            .or_else(|| self.load_object_prototype_method(name))
        {
            return Ok(value);
        }
        Ok(Value::undefined())
    }

    /// As [`Self::function_property_get`], but `receiver`
    /// supplies the value written to a freshly materialized
    /// `prototype.constructor`. For a closure the canonical callable is
    /// the closure value itself, not `Value::function(function_id)`, so
    /// callers that hold the closure pass it through to keep
    /// `C.prototype.constructor === C`.
    pub(crate) fn function_property_get_with_receiver(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        receiver: Option<Value>,
        name: &str,
    ) -> Result<Value, VmError> {
        if name != "prototype" {
            return self.function_property_get_non_prototype(
                stack,
                context,
                owner,
                function_id,
                name,
            );
        }
        // A user-installed own `prototype` (data or accessor, e.g. via
        // Object.defineProperty) shadows the implicit one. An accessor
        // must fire its getter with the function as receiver — §7.3.12
        // Get(C, "prototype") in OrdinaryHasInstance — so a poisoned
        // getter propagates instead of reading back `undefined`.
        if let Some(bag) = self.callable_bag_read(owner, function_id) {
            match crate::object::lookup_own(bag, &self.gc_heap, name) {
                crate::object::PropertyLookup::Data { value, .. } => return Ok(value),
                crate::object::PropertyLookup::Accessor { getter, .. } => {
                    return match getter {
                        Some(g) if abstract_ops::is_callable(&g) => {
                            let recv = receiver.unwrap_or_else(|| Value::function(function_id));
                            self.run_callable_sync_rooted(stack, context, &g, recv, SmallVec::new())
                        }
                        _ => Ok(Value::undefined()),
                    };
                }
                crate::object::PropertyLookup::Absent => {}
            }
        }
        // §10.2.5 — arrows, methods, and async (non-generator)
        // functions have no `prototype` property at all, so there is
        // nothing to materialize.
        if !context.function_has_prototype_property(function_id) {
            return Ok(Value::undefined());
        }

        let function_root = Value::function(function_id);
        let constructor_value = receiver.unwrap_or(function_root);
        let bag = self.function_user_bag(
            stack,
            owner,
            function_id,
            &[&function_root, &constructor_value],
        )?;
        if let Some(existing) = crate::object::get(bag, &self.gc_heap, "prototype") {
            return Ok(existing);
        }

        let bag_root = Value::object(bag);
        let mut proto = self.alloc_stack_rooted_object_with_extra_roots(
            stack,
            &[&function_root, &constructor_value, &bag_root],
        )?;
        if let Some(object_proto) = self.realm_intrinsics.object_prototype().or_else(|| {
            crate::object::get(self.global_this, &self.gc_heap, "Object")
                .and_then(|v| v.as_object())
                .and_then(|object_ctor| {
                    crate::object::get(object_ctor, &self.gc_heap, "prototype")
                        .and_then(|v| v.as_object())
                })
        }) {
            crate::object::set_prototype(proto, &mut self.gc_heap, Some(object_proto));
        }
        if context
            .function(function_id)
            .is_some_and(|function| function.is_generator)
        {
            let is_async = context
                .function(function_id)
                .is_some_and(|function| function.is_async_generator);
            if let Some(shared) = self.shared_generator_object_prototype(is_async) {
                // §27.5.1 / §27.6.1 — generator-function `.prototype`
                // objects inherit from the one shared
                // %GeneratorPrototype% / %AsyncGeneratorPrototype%.
                object::set_prototype(proto, &mut self.gc_heap, Some(shared));
            } else {
                let proto_value = Value::object(proto);
                let parent = self.alloc_stack_rooted_object_with_extra_roots(
                    stack,
                    &[&function_root, &bag_root, &proto_value],
                )?;
                self.finish_generator_function_prototype(context, function_id, proto, parent)?;
            }
        }
        // Install `prototype` on the function's property bag first. It is a
        // non-moving define, and routing it before the constructor install means
        // the bag's `prototype` slot already tracks `proto` (the collector
        // forwards live GC slots) when the shape-advancing constructor define
        // below may relocate the heap.
        // `bag` is a bare local from before the `proto` allocation, which can
        // scavenge and relocate it; re-fetch the live handle from its rooted
        // `bag_root` before the define, or it dereferences a moved-from shape cell.
        let bag = bag_root
            .as_object()
            .expect("function bag survives stack rooting");
        let prototype_desc =
            object::PropertyDescriptor::data(Value::object(proto), true, false, false);
        let _ = object::define_own_property(bag, &mut self.gc_heap, "prototype", prototype_desc);
        // §27.5.1 — a generator function's `.prototype` object has NO own
        // properties (no back-pointing `constructor`); ordinary functions get
        // the §10.2.5 MakeConstructor pair. Route the `constructor` install
        // through the hidden-class-advancing define rather than the
        // dictionary-mode `object::define_own_property` (which nulls the shape),
        // so the prototype keeps a fast shape: prototype-style method
        // definitions (`Foo.prototype.m = ...`) then land in shape slots and
        // instance method calls stay inline/direct-call guardable instead of
        // forcing every dispatch through generic method lookup and call
        // machinery. The define
        // allocates a hidden-class child and can move the heap, so the bare
        // `proto` / `bag` locals may be stale afterward.
        if !context
            .function(function_id)
            .is_some_and(|function| function.is_generator)
        {
            let constructor_desc = object::PartialPropertyDescriptor {
                value: Some(constructor_value),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            };
            let _ =
                self.define_own_property_partial(&mut proto, "constructor", constructor_desc)?;
        }
        // Re-acquire the (possibly relocated) prototype through the function's
        // bag, which the collector forwarded; the bare `proto` handle may be
        // stale after the shape allocation above.
        let proto_value = self
            .callable_bag_read(owner, function_id)
            .and_then(|bag| crate::object::get_own(bag, &self.gc_heap, "prototype"))
            .unwrap_or_else(|| Value::object(proto));
        Ok(proto_value)
    }

    fn finish_generator_function_prototype(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        proto: JsObject,
        mut parent: JsObject,
    ) -> Result<(), VmError> {
        if let Some(iterator_proto) = self
            .constructor_prototype_value("Iterator")
            .ok()
            .and_then(|v| v.as_object())
        {
            object::set_prototype(parent, &mut self.gc_heap, Some(iterator_proto));
        }
        let tag_name = match context.function(function_id) {
            Some(function) if function.is_async_generator => "AsyncGenerator",
            _ => "Generator",
        };
        let tag = JsString::from_str(tag_name, self.gc_heap_mut())?;
        let tag_sym = self
            .well_known_symbols()
            .get(symbol::WellKnown::ToStringTag);
        object::define_own_symbol_property_partial(
            &mut parent,
            &mut self.gc_heap,
            tag_sym,
            object::PartialPropertyDescriptor {
                value: Some(Value::string(tag)),
                writable: Some(false),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
        object::set_prototype(proto, &mut self.gc_heap, Some(parent));
        Ok(())
    }

    pub(crate) fn load_global_prototype_method(
        &self,
        constructor_name: &str,
        name: &str,
    ) -> Option<Value> {
        let cached = crate::realm_intrinsics::Intrinsic::from_constructor_name(constructor_name)
            .and_then(|slot| self.realm_intrinsics.get(slot));
        if let Some(prototype_obj) = cached {
            return crate::object::get(prototype_obj, &self.gc_heap, name);
        }
        let constructor_obj = crate::object::get(self.global_this, &self.gc_heap, constructor_name)
            .and_then(|v| v.as_object())?;
        let prototype_obj = crate::object::get(constructor_obj, &self.gc_heap, "prototype")
            .and_then(|v| v.as_object())?;
        crate::object::get(prototype_obj, &self.gc_heap, name)
    }

    pub(crate) fn load_function_prototype_method(&self, name: &str) -> Option<Value> {
        self.load_global_prototype_method("Function", name)
    }

    pub(crate) fn load_object_prototype_method(&self, name: &str) -> Option<Value> {
        self.load_global_prototype_method("Object", name)
    }
}

fn complete_descriptor_defaults_from_object(
    desc_obj: JsObject,
    gc_heap: &otter_gc::GcHeap,
    mut descriptor: object::PropertyDescriptor,
    existing: &object::PropertyDescriptor,
) -> object::PropertyDescriptor {
    let has_value = !matches!(
        object::lookup_own(desc_obj, gc_heap, "value"),
        object::PropertyLookup::Absent
    );
    let has_writable = !matches!(
        object::lookup_own(desc_obj, gc_heap, "writable"),
        object::PropertyLookup::Absent
    );
    let has_enumerable = !matches!(
        object::lookup_own(desc_obj, gc_heap, "enumerable"),
        object::PropertyLookup::Absent
    );
    let has_configurable = !matches!(
        object::lookup_own(desc_obj, gc_heap, "configurable"),
        object::PropertyLookup::Absent
    );

    if !has_value
        && let object::DescriptorKind::Data { value } = &existing.kind
        && let object::DescriptorKind::Data {
            value: descriptor_value,
        } = &mut descriptor.kind
    {
        *descriptor_value = *value;
    }
    if !has_writable {
        descriptor.flags = descriptor.flags.with_writable(existing.writable());
    }
    if !has_enumerable {
        descriptor.flags = descriptor.flags.with_enumerable(existing.enumerable());
    }
    if !has_configurable {
        descriptor.flags = descriptor.flags.with_configurable(existing.configurable());
    }
    descriptor
}

fn bind_metadata_get_from_descriptor(desc: object::PropertyDescriptor) -> BindMetadataGet {
    match desc.kind {
        object::DescriptorKind::Data { value } => BindMetadataGet::Value(value),
        object::DescriptorKind::Accessor { getter, .. } => match getter {
            Some(getter) if abstract_ops::is_callable(&getter) => BindMetadataGet::Getter(getter),
            _ => BindMetadataGet::Value(Value::undefined()),
        },
    }
}

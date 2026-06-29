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
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::Frame`]

use crate::holt_stack::HoltStack;
use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    BoundFunction, ClassConstructor, ExecutionContext, Frame, Interpreter, JsObject, JsString,
    PendingBindFunction, PendingBindStage, UpvalueCell, Value, VmError, VmGetOutcome,
    VmIntrinsicFunction, VmPropertyKey, abstract_ops, array, function_metadata, object,
    object_statics,
    operand_decode::{const_operand, register_operand},
    read_register, symbol, to_length, write_register,
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
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        // §9.1 — a capture-free function created in a frame that carries a
        // direct-eval variable environment still needs the env handle (its
        // free identifiers resolve dynamically), so it materializes as a
        // closure with no upvalues. Otherwise a capture-free function keeps
        // the canonical interned value.
        if function_id != frame.function_id
            && let Some(env) = self.frame_cold(frame).and_then(|cold| cold.eval_env)
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
            write_register(frame, dst, Value::closure(closure))?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §10.2 — the named-function SELF binding (`function_id` is the
        // running function) must resolve to the EXACT instance executing
        // this frame, not a fresh interned bare value: otherwise
        // `this instanceof Self` / `Self.prototype` inside the body would
        // observe a different per-instance `.prototype` than the one the
        // constructor installed on `this`.
        if function_id == frame.function_id
            && let Some(closure) = self.frame_cold(frame).and_then(|cold| cold.callee_closure)
        {
            write_register(frame, dst, Value::closure(closure))?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        write_register(frame, dst, Value::function(function_id))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_make_closure_operands(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let idx = const_operand(operands.get(1))?;
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        // §10.2 — a named function referencing its own binding inside its
        // body (`function_id` is the running function) must resolve to the
        // EXACT instance executing this frame, not a freshly minted one:
        // a fresh instance owns a distinct per-instance `.prototype`, so
        // `this instanceof Self` / `Self.prototype` would diverge from the
        // prototype the constructor installed on `this`.
        if function_id == frame.function_id
            && let Some(closure) = self.frame_cold(frame).and_then(|cold| cold.callee_closure)
        {
            write_register(frame, dst, Value::closure(closure))?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        let count = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let mut cells: Vec<UpvalueCell> = Vec::with_capacity(count);
        for i in 0..count {
            let parent_idx = match operands.get(3 + i) {
                Some(&Operand::Imm32(n)) if n >= 0 => n as usize,
                _ => return Err(VmError::InvalidOperand),
            };
            let cell = *frame
                .upvalues
                .get(parent_idx)
                .ok_or(VmError::InvalidOperand)?;
            cells.push(cell);
        }
        let upvalues = cells;
        // Arrow-closure receivers are bound lexically: every later invocation
        // ignores the call site and uses the enclosing frame's `this`.
        let bound_this = if context.function_is_arrow(function_id) {
            Some(frame.this_value)
        } else {
            None
        };
        let bound_new_target = if context.function_is_arrow(function_id) {
            self.frame_cold(frame).and_then(|cold| cold.new_target)
        } else {
            None
        };
        let bound_derived_this = if context.function_is_arrow(function_id) {
            self.frame_cold(frame)
                .and_then(|cold| cold.derived_this_cell)
        } else {
            None
        };
        // §9.1 — closures made in a frame whose function chain
        // contains a direct eval capture the frame's eval variable
        // environment so eval-introduced vars stay reachable.
        let eval_env = self.frame_cold(frame).and_then(|cold| cold.eval_env);
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
        write_register(frame, dst, Value::closure(closure))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_make_class_regs(
        &mut self,
        stack: &mut HoltStack,
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
            && let Some(function_prototype) = self.realm_intrinsics.function_prototype
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
        let prototype = read_register(&stack[frame_idx], proto_reg)?
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        // §15.7.10 steps 17/20 — the compiler reserves `prototype.constructor`
        // with an `undefined` placeholder before the class elements run, so it
        // precedes the methods in own-key order. A class element whose
        // computed key evaluates to "constructor" (`['constructor']() {}`) is
        // an ordinary method and overrides that slot (step 20 runs after step
        // 17). Only fill the placeholder here: if the slot already holds a real
        // value, a class element claimed it and must not be clobbered.
        let placeholder_pending = crate::object::get_own(prototype, &self.gc_heap, "constructor")
            .is_none_or(|value| value.is_undefined());
        if placeholder_pending {
            let _ = self.define_own_property_partial(prototype, "constructor", constructor_desc)?;
        }
        let frame = &mut stack[frame_idx];
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn drive_bind_function(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
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
            Some(&Operand::ConstIndex(n)) => n as usize,
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
        match self.callable_bind_metadata_get(context, &target, "name")? {
            BindMetadataGet::Value(target_name) => self.continue_bind_function_after_name(
                stack,
                context,
                dst,
                target,
                bound_this,
                bound_args,
                target_name,
            ),
            BindMetadataGet::Getter(getter) => {
                self.frame_ensure_cold(&mut stack[top_idx])
                    .pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    stage: PendingBindStage::Name,
                    target_name: None,
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    pub(crate) fn continue_pending_bind_function(
        &mut self,
        stack: &mut HoltStack,
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
            PendingBindStage::Name => self.continue_bind_function_after_name(
                stack,
                context,
                dst,
                state.target,
                state.bound_this,
                state.bound_args,
                produced,
            ),
            PendingBindStage::Length => {
                let target_name = match state.target_name {
                    Some(value) => value,
                    None => return Some(Err(VmError::InvalidOperand)),
                };
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_bind_function = None;
                }
                self.finish_bind_function(
                    stack,
                    dst,
                    state.target,
                    state.bound_this,
                    state.bound_args,
                    target_name,
                    produced,
                )
            }
        })
    }

    pub(crate) fn continue_bind_function_after_name(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        dst: u16,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        target_name: Value,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        match self.callable_bind_metadata_get(context, &target, "length")? {
            BindMetadataGet::Value(target_length) => {
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_bind_function = None;
                }
                self.finish_bind_function(
                    stack,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    target_name,
                    target_length,
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
                    stage: PendingBindStage::Length,
                    target_name: Some(target_name),
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    fn finish_bind_function(
        &mut self,
        stack: &mut HoltStack,
        dst: u16,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        target_name: Value,
        target_length: Value,
    ) -> Result<(), VmError> {
        let metadata = function_metadata::bound_create_metadata_from_values(
            &target_name,
            &target_length,
            bound_args.len(),
            &self.gc_heap,
        );
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
        };
        let bound = BoundFunction::new_with_metadata_and_roots(
            &mut self.gc_heap,
            target,
            bound_this,
            bound_args,
            metadata,
            &mut external_visit,
        )?;
        let top_idx = stack.len() - 1;
        if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
            cold.pending_bind_function = None;
        }
        write_register(&mut stack[top_idx], dst, Value::bound_function(bound))?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_vm_intrinsic_sync(
        &mut self,
        context: &ExecutionContext,
        intrinsic: VmIntrinsicFunction,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        match intrinsic {
            VmIntrinsicFunction::FunctionPrototypeCall => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let mut iter = args.into_iter();
                let receiver = iter.next().unwrap_or(Value::undefined());
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                self.run_callable_sync(context, &this_value, receiver, forwarded)
            }
            VmIntrinsicFunction::FunctionPrototypeApply => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let mut iter = args.into_iter();
                let receiver = iter.next().unwrap_or(Value::undefined());
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None => SmallVec::new(),
                    Some(v) if v.is_nullish() => SmallVec::new(),
                    Some(arg_array) => self.create_list_from_array_like(context, arg_array)?,
                };
                self.run_callable_sync(context, &this_value, receiver, forwarded)
            }
            VmIntrinsicFunction::FunctionPrototypeBind => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let mut iter = args.into_iter();
                let receiver = iter.next().unwrap_or(Value::undefined());
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let owner_bag = self.callable_bag_for_value(&this_value);
                let mut ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &mut self.gc_heap,
                    owner_bag,
                    &self.function_deleted_metadata,
                );
                let metadata = function_metadata::bound_create_metadata(
                    &mut ctx,
                    &this_value,
                    bound_args.len(),
                )?;
                let this_root = this_value;
                let receiver_root = receiver;
                let bound_args_root = bound_args.clone();
                let roots = self.collect_runtime_roots();
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                    this_root.trace_value_slots(visitor);
                    receiver_root.trace_value_slots(visitor);
                    for arg in &bound_args_root {
                        arg.trace_value_slots(visitor);
                    }
                };
                let bound = BoundFunction::new_with_metadata_and_roots(
                    &mut self.gc_heap,
                    this_value,
                    receiver,
                    bound_args,
                    metadata,
                    &mut external_visit,
                )?;
                Ok(Value::bound_function(bound))
            }
            VmIntrinsicFunction::FunctionPrototypeToString => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let display = {
                    let owner_bag = self.callable_bag_for_value(&this_value);
                    let mut ctx = function_metadata::FunctionMetadataContext::new(
                        context,
                        &mut self.gc_heap,
                        owner_bag,
                        &self.function_deleted_metadata,
                    );
                    function_metadata::callable_to_string(&mut ctx, &this_value)
                };
                let s = JsString::from_str(&display, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                Ok(Value::string(s))
            }
            VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance => {
                // §20.2.3.6: Return ? OrdinaryHasInstance(F, V) where
                // F is the `this` value and V is the first argument.
                // <https://tc39.es/ecma262/#sec-function.prototype-@@hasinstance>
                let v = args.into_iter().next().unwrap_or(Value::undefined());
                let result = self.ordinary_has_instance(context, &this_value, &v)?;
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
        context: &ExecutionContext,
        c: &Value,
        o: &Value,
    ) -> Result<bool, VmError> {
        if !self.is_callable_runtime(c) {
            return Ok(false);
        }
        if let Some(bound) = c.as_bound_function() {
            let (target, _, _) = bound.parts(&self.gc_heap);
            return self.instanceof_operator(context, o, &target);
        }
        if !o.is_object_type() {
            return Ok(false);
        }
        let Some(prototype) = self.instanceof_target_prototype(context, c)? else {
            return Ok(false);
        };
        if !(prototype.is_object_type() || prototype.is_proxy()) {
            return Err(self.err_type(
                ("Function has non-object prototype 'undefined' in instanceof check".to_string())
                    .into(),
            ));
        }
        self.value_has_proxy_aware_prototype(context, *o, &prototype)
    }

    pub(crate) fn ordinary_has_instance_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        c: &Value,
        o: &Value,
    ) -> Result<bool, VmError> {
        if !self.is_callable_runtime(c) {
            return Ok(false);
        }
        if let Some(bound) = c.as_bound_function() {
            let (target, _, _) = bound.parts(&self.gc_heap);
            return self.instanceof_operator_stack_rooted(context, stack, o, &target);
        }
        if !o.is_object_type() {
            return Ok(false);
        }
        let Some(prototype) = self.instanceof_target_prototype_stack_rooted(context, stack, c)?
        else {
            return Ok(false);
        };
        if !(prototype.is_object_type() || prototype.is_proxy()) {
            return Err(self.err_type(
                ("Function has non-object prototype 'undefined' in instanceof check".to_string())
                    .into(),
            ));
        }
        self.value_has_proxy_aware_prototype(context, *o, &prototype)
    }

    /// ECMA-262 §13.10.2 `InstanceofOperator(V, target)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-instanceofoperator>
    pub(crate) fn instanceof_operator(
        &mut self,
        context: &ExecutionContext,
        v: &Value,
        target: &Value,
    ) -> Result<bool, VmError> {
        if !target.is_object_type() {
            return Err(self
                .err_type(("Right-hand side of instanceof is not an object".to_string()).into()));
        }
        let has_instance_sym = self.well_known_symbols.get(symbol::WellKnown::HasInstance);
        let key = VmPropertyKey::Symbol(has_instance_sym);
        let handler = match self.ordinary_get_value(context, *target, *target, &key, 0)? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *target, SmallVec::new())?
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
                return self.ordinary_has_instance(context, target, v);
            }
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(*v);
            let result = self.run_callable_sync(context, &handler, *target, args)?;
            return Ok(result.to_boolean(&self.gc_heap));
        }
        if !self.is_callable_runtime(target) {
            return Err(
                self.err_type(("Right-hand side of instanceof is not callable".to_string()).into())
            );
        }
        self.ordinary_has_instance(context, target, v)
    }

    pub(crate) fn instanceof_operator_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        v: &Value,
        target: &Value,
    ) -> Result<bool, VmError> {
        if !target.is_object_type() {
            return Err(self
                .err_type(("Right-hand side of instanceof is not an object".to_string()).into()));
        }
        let has_instance_sym = self.well_known_symbols.get(symbol::WellKnown::HasInstance);
        let key = VmPropertyKey::Symbol(has_instance_sym);
        let handler = match self.ordinary_get_value(context, *target, *target, &key, 0)? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *target, SmallVec::new())?
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
                return self.ordinary_has_instance_stack_rooted(context, stack, target, v);
            }
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(*v);
            let result = self.run_callable_sync(context, &handler, *target, args)?;
            return Ok(result.to_boolean(&self.gc_heap));
        }
        if !self.is_callable_runtime(target) {
            return Err(
                self.err_type(("Right-hand side of instanceof is not callable".to_string()).into())
            );
        }
        self.ordinary_has_instance_stack_rooted(context, stack, target, v)
    }

    pub(crate) fn create_list_from_array_like(
        &mut self,
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
        let length = self.get_property_value_for_call(context, value, "length")?;
        // §7.3.18 step 3 — `len = ? ToLength(? Get(obj, "length"))`. The
        // §7.1.4 ToNumber inside ToLength fires a `valueOf` /
        // `@@toPrimitive` hook on an object-valued `length`, so coerce
        // with the execution context here rather than through the
        // infallible primitive-only `to_length` (which would read an
        // object as NaN -> 0).
        let length_num = self.coerce_to_number(context, &length)?;
        let len = to_length(&Value::number(length_num), &self.gc_heap)?;
        let mut values = SmallVec::new();
        for index in 0..len {
            let key = index.to_string();
            values.push(self.get_property_value_for_call(context, value, &key)?);
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
        context: &ExecutionContext,
        receiver: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        self.get_property_value_for_call(context, receiver, key)
    }

    pub(crate) fn get_property_value_for_call(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        let property_key = VmPropertyKey::String(key);
        match self.ordinary_get_value(context, receiver, receiver, &property_key, 0)? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, receiver, SmallVec::new())
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
        context: &ExecutionContext,
        obj: &Value,
        default_ctor: &Value,
    ) -> Result<Value, VmError> {
        let c = self.get_property_value_for_call(context, *obj, "constructor")?;
        if c.is_undefined() {
            return Ok(*default_ctor);
        }
        if !c.is_object_type() {
            return Err(self.err_type(("constructor property is not an object".to_string()).into()));
        }
        let species_sym = self
            .well_known_symbols()
            .get(crate::symbol::WellKnown::Species);
        let s =
            match self.ordinary_get_value(context, c, c, &VmPropertyKey::Symbol(species_sym), 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, c, SmallVec::new())?
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
    pub(crate) fn callable_bag_for_value(&self, value: &Value) -> Option<JsObject> {
        if let Some(c) = value.as_closure(&self.gc_heap) {
            return c.own_props(&self.gc_heap);
        }
        if let Some(fid) = value.as_function() {
            return self.function_user_props.get(&fid).copied();
        }
        None
    }

    pub(crate) fn function_user_bag_stack_rooted(
        &mut self,
        stack: &HoltStack,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        value_roots: &[&Value],
    ) -> Result<JsObject, VmError> {
        if let Some(c) = owner {
            if let Some(bag) = c.own_props(&self.gc_heap) {
                return Ok(bag);
            }
            let bag = self.alloc_stack_rooted_object_with_extra_roots(stack, value_roots)?;
            c.set_own_props(&mut self.gc_heap, bag);
            return Ok(bag);
        }
        match self.function_user_props.get(&function_id).copied() {
            Some(bag) => Ok(bag),
            None => {
                let bag = self.alloc_stack_rooted_object_with_extra_roots(stack, value_roots)?;
                self.function_user_props.insert(function_id, bag);
                Ok(bag)
            }
        }
    }

    pub(crate) fn function_user_bag_runtime_rooted(
        &mut self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsObject, VmError> {
        if let Some(c) = owner {
            if let Some(bag) = c.own_props(&self.gc_heap) {
                return Ok(bag);
            }
            let bag = self.alloc_runtime_rooted_object_with_roots(value_roots, slice_roots)?;
            c.set_own_props(&mut self.gc_heap, bag);
            return Ok(bag);
        }
        match self.function_user_props.get(&function_id).copied() {
            Some(bag) => Ok(bag),
            None => {
                let bag = self.alloc_runtime_rooted_object_with_roots(value_roots, slice_roots)?;
                self.function_user_props.insert(function_id, bag);
                Ok(bag)
            }
        }
    }

    /// §10.1.3 / §10.1.4 ordinary function `[[Extensible]]`.
    ///
    /// Function objects store user expandos in an interpreter side
    /// table, so their per-instance extensibility flag lives next
    /// to that table rather than in a materialised ordinary object
    /// bag.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-isextensible>
    /// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-preventextensions>
    pub(crate) fn ordinary_function_is_extensible(&self, function_id: u32) -> bool {
        !self.function_non_extensible.contains(&function_id)
    }

    pub(crate) fn ordinary_function_prevent_extensions(&mut self, function_id: u32) {
        self.function_non_extensible.insert(function_id);
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
        let deleted =
            |key: &'static str| self.function_deleted_metadata.contains(&(function_id, key));
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
        if self
            .function_deleted_metadata
            .contains(&(function_id, metadata_key))
        {
            return Ok(None);
        }
        let Some(context) = context else {
            return Ok(None);
        };
        let owner_bag = self.callable_bag_read(owner, function_id);
        let mut ctx = function_metadata::FunctionMetadataContext::new(
            context,
            &mut self.gc_heap,
            owner_bag,
            &self.function_deleted_metadata,
        );
        let value =
            function_metadata::ordinary_function_intrinsic_property(&mut ctx, function_id, key)?;
        Ok(Some(object::PropertyDescriptor::data(
            value, false, false, true,
        )))
    }

    pub(crate) fn ordinary_function_define_own_property(
        &mut self,
        context: Option<&ExecutionContext>,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
        desc_obj: Option<JsObject>,
        descriptor: object::PropertyDescriptor,
    ) -> Result<bool, VmError> {
        self.ordinary_function_define_own_property_with_roots(
            context,
            owner,
            function_id,
            key,
            desc_obj,
            descriptor,
            None,
            &[],
        )
    }

    fn ordinary_function_define_own_property_with_roots(
        &mut self,
        context: Option<&ExecutionContext>,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
        desc_obj: Option<JsObject>,
        descriptor: object::PropertyDescriptor,
        stack_roots: Option<&HoltStack>,
        value_roots: &[&Value],
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
                if !has_virtual_prototype && !self.ordinary_function_is_extensible(function_id) {
                    return Ok(false);
                }
                descriptor
            }
        };
        let mut roots = Vec::with_capacity(value_roots.len() + 3);
        roots.extend_from_slice(value_roots);
        let desc_obj_root = desc_obj.map(Value::object);
        if let Some(value) = &desc_obj_root {
            roots.push(value);
        }
        match &descriptor.kind {
            object::DescriptorKind::Data { value } => roots.push(value),
            object::DescriptorKind::Accessor { getter, setter } => {
                if let Some(getter) = getter {
                    roots.push(getter);
                }
                if let Some(setter) = setter {
                    roots.push(setter);
                }
            }
        }
        let bag = match stack_roots {
            Some(stack) => {
                self.function_user_bag_stack_rooted(stack, owner, function_id, &roots)?
            }
            None => self.function_user_bag_runtime_rooted(owner, function_id, &roots, &[])?,
        };
        let ok = crate::object::define_own_property(bag, &mut self.gc_heap, key, descriptor);
        if ok && let Some(metadata_key) = function_metadata::ordinary_function_metadata_key(key) {
            self.function_deleted_metadata
                .remove(&(function_id, metadata_key));
        }
        Ok(ok)
    }

    pub(crate) fn ordinary_function_delete_own_property(
        &mut self,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        key: &str,
    ) -> bool {
        let Some(metadata_key) = function_metadata::ordinary_function_metadata_key(key) else {
            return self
                .callable_bag_read(owner, function_id)
                .map(|bag| crate::object::delete(bag, &mut self.gc_heap, key))
                .unwrap_or(true);
        };
        if let Some(bag) = self.callable_bag_read(owner, function_id)
            && crate::object::get_own_descriptor(bag, &self.gc_heap, key).is_some()
        {
            if !crate::object::delete(bag, &mut self.gc_heap, key) {
                return false;
            }
            self.function_deleted_metadata
                .insert((function_id, metadata_key));
            return true;
        }
        self.function_deleted_metadata
            .insert((function_id, metadata_key));
        true
    }

    pub(crate) fn try_function_object_static_call(
        &mut self,
        context: Option<&ExecutionContext>,
        stack_roots: Option<&HoltStack>,
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
                        context,
                        &target,
                        crate::object_internal_ops::ObjectIntegrityLevel::Frozen,
                    )?;
                    return Ok(Some(Value::boolean(frozen)));
                }
                M::IsSealed => {
                    let sealed = self.test_integrity_level_value(
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
                    .own_property_keys_value(context, &target)?
                    .into_iter()
                    .filter(|v| v.is_string())
                    .collect();
                return Ok(Some(Value::array(self.function_static_array_from_values(
                    stack_roots,
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
                    let trap_keys = self.own_property_keys_value(context, &target)?;
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
                        let desc = match stack_roots {
                            Some(stack) => self
                                .ordinary_get_own_property_descriptor_value_stack_rooted(
                                    context, stack, target, &vm_key, 0,
                                )?,
                            None => self
                                .ordinary_get_own_property_descriptor_value_runtime_rooted(
                                    context,
                                    target,
                                    &vm_key,
                                    0,
                                    &[&target],
                                    &[args],
                                )?,
                        };
                        if desc.as_ref().is_some_and(|d| d.enumerable()) {
                            values.push(key);
                        }
                    }
                    return Ok(Some(Value::array(self.function_static_array_from_values(
                        stack_roots,
                        values,
                        &[&target],
                        &[args],
                    )?)));
                }
                let keys = self.enumerable_own_string_keys_for_value(context, target, 0)?;
                let mut values = Vec::with_capacity(keys.len());
                for key in keys {
                    values.push(Value::string(
                        JsString::from_str(&key, self.gc_heap_mut())
                            .map_err(|_| VmError::TypeMismatch)?,
                    ));
                }
                return Ok(Some(Value::array(self.function_static_array_from_values(
                    stack_roots,
                    values,
                    &[&target],
                    &[args],
                )?)));
            }
            let desc = self.get_own_property_descriptor_for_value(context, target, args.get(1))?;
            if matches!(method, M::HasOwn) {
                return Ok(Some(Value::boolean(desc.is_some())));
            }
            return match desc {
                Some(desc) => Ok(Some(Value::object(
                    self.function_static_descriptor_to_object(
                        stack_roots,
                        &desc,
                        &[&target],
                        args,
                    )?,
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
                let key = Self::coerce_vm_property_key(args.get(1), &self.gc_heap)?;
                let desc_obj = args
                    .get(2)
                    .and_then(|v| v.as_object())
                    .ok_or(VmError::TypeMismatch)?;
                let descriptor = object_statics::coerce_to_descriptor(&desc_obj, &self.gc_heap)?;
                let completed = descriptor.complete_for_new_property();
                let ok = match (function_id, &key) {
                    (Some(function_id), VmPropertyKey::Symbol(sym)) => {
                        if !self.ordinary_function_has_own_symbol_property_for_extensibility(
                            owner,
                            function_id,
                            *sym,
                        ) && !self.ordinary_function_is_extensible(function_id)
                        {
                            return Err(VmError::TypeMismatch);
                        }
                        let bag = match stack_roots {
                            Some(stack) => self.function_user_bag_stack_rooted(
                                stack,
                                owner,
                                function_id,
                                &[&target],
                            )?,
                            None => self.function_user_bag_runtime_rooted(
                                owner,
                                function_id,
                                &[&target],
                                &[args],
                            )?,
                        };
                        crate::object::define_own_symbol_property_partial(
                            bag,
                            &mut self.gc_heap,
                            *sym,
                            descriptor,
                        )
                    }
                    (Some(function_id), _) => self
                        .ordinary_function_define_own_property_with_roots(
                            context,
                            owner,
                            function_id,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            Some(desc_obj),
                            completed,
                            stack_roots,
                            &[&target],
                        )?,
                    (None, VmPropertyKey::Symbol(_)) => false,
                    (None, _) => {
                        let Some(bound) = target.as_bound_function() else {
                            return Ok(None);
                        };
                        function_metadata::bound_define_own_property(
                            &bound,
                            &mut self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            completed,
                        )
                    }
                };
                if !ok {
                    return Err(VmError::TypeMismatch);
                }
                Ok(Some(target))
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
                        self.function_static_descriptor_to_object(
                            stack_roots,
                            &desc,
                            &[&target],
                            args,
                        )?,
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
                let function_id = target.as_function().or_else(|| {
                    target
                        .as_closure(&self.gc_heap)
                        .map(|c| c.cached_function_id)
                });
                match function_id {
                    Some(function_id) => Ok(Some(Value::boolean(
                        self.ordinary_function_is_extensible(function_id),
                    ))),
                    None => Ok(None),
                }
            }
            M::PreventExtensions => {
                let function_id = target.as_function().or_else(|| {
                    target
                        .as_closure(&self.gc_heap)
                        .map(|c| c.cached_function_id)
                });
                match function_id {
                    Some(function_id) => {
                        self.ordinary_function_prevent_extensions(function_id);
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
        stack_roots: Option<&HoltStack>,
        values: Vec<Value>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<array::JsArray, VmError> {
        match stack_roots {
            Some(stack) => self.alloc_stack_rooted_array_from_values_with_root_slices(
                stack,
                values,
                value_roots,
                slice_roots,
            ),
            None => self.alloc_runtime_rooted_array_from_values(values, value_roots, slice_roots),
        }
    }

    fn function_static_descriptor_to_object(
        &mut self,
        stack_roots: Option<&HoltStack>,
        desc: &object::PropertyDescriptor,
        value_roots: &[&Value],
        slice_roots: &[Value],
    ) -> Result<JsObject, VmError> {
        let object_proto = self.constructor_prototype_value("Object").ok();
        let mut roots = Vec::with_capacity(value_roots.len() + 3);
        roots.extend_from_slice(value_roots);
        if let Some(proto) = object_proto.as_ref() {
            roots.push(proto);
        }
        match &desc.kind {
            object::DescriptorKind::Data { value } => roots.push(value),
            object::DescriptorKind::Accessor { getter, setter } => {
                if let Some(getter) = getter {
                    roots.push(getter);
                }
                if let Some(setter) = setter {
                    roots.push(setter);
                }
            }
        }
        let result = match stack_roots {
            Some(stack) => self.alloc_stack_rooted_object_with_value_roots(
                stack,
                roots.as_slice(),
                slice_roots,
            )?,
            None => {
                self.alloc_runtime_rooted_object_with_roots(roots.as_slice(), &[slice_roots])?
            }
        };
        if let Some(proto_obj) = object_proto.and_then(|v| v.as_object()) {
            object::set_prototype(result, &mut self.gc_heap, Some(proto_obj));
        }
        match &desc.kind {
            object::DescriptorKind::Data { value } => {
                self.set_property(result, "value", *value)?;
                self.set_property(result, "writable", Value::boolean(desc.writable()))?;
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                self.set_property(result, "get", (*getter).unwrap_or(Value::undefined()))?;
                self.set_property(result, "set", (*setter).unwrap_or(Value::undefined()))?;
            }
        }
        self.set_property(result, "enumerable", Value::boolean(desc.enumerable()))?;
        self.set_property(result, "configurable", Value::boolean(desc.configurable()))?;
        Ok(result)
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
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        if name == "prototype" {
            return self.function_property_get_runtime_rooted(
                context,
                owner,
                function_id,
                name,
                &[],
                &[],
            );
        }
        self.function_property_get_non_prototype(context, owner, function_id, name)
    }

    fn function_property_get_non_prototype(
        &mut self,
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
        if name == "name" || name == "length" {
            let owner_bag = self.callable_bag_read(owner, function_id);
            let mut ctx = function_metadata::FunctionMetadataContext::new(
                context,
                &mut self.gc_heap,
                owner_bag,
                &self.function_deleted_metadata,
            );
            return function_metadata::ordinary_function_intrinsic_property(
                &mut ctx,
                function_id,
                name,
            );
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

    pub(crate) fn function_property_get_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        self.function_property_get_stack_rooted_with_receiver(
            context,
            stack,
            owner,
            function_id,
            None,
            name,
        )
    }

    /// As [`Self::function_property_get_stack_rooted`], but `receiver`
    /// supplies the value written to a freshly materialized
    /// `prototype.constructor`. For a closure the canonical callable is
    /// the closure value itself, not `Value::function(function_id)`, so
    /// callers that hold the closure pass it through to keep
    /// `C.prototype.constructor === C`.
    pub(crate) fn function_property_get_stack_rooted_with_receiver(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        receiver: Option<Value>,
        name: &str,
    ) -> Result<Value, VmError> {
        if name != "prototype" {
            return self.function_property_get_non_prototype(context, owner, function_id, name);
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
                            self.run_callable_sync(context, &g, recv, SmallVec::new())
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
        let bag = self.function_user_bag_stack_rooted(
            stack,
            owner,
            function_id,
            &[&function_root, &constructor_value],
        )?;
        if let Some(existing) = crate::object::get(bag, &self.gc_heap, "prototype") {
            return Ok(existing);
        }

        let bag_root = Value::object(bag);
        let proto = self.alloc_stack_rooted_object_with_extra_roots(
            stack,
            &[&function_root, &constructor_value, &bag_root],
        )?;
        if let Some(object_proto) = self.realm_intrinsics.object_prototype.or_else(|| {
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
        // forcing every dispatch through the generic method bridge. The define
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
            let _ = self.define_own_property_partial(proto, "constructor", constructor_desc)?;
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

    pub(crate) fn function_property_get_runtime_rooted(
        &mut self,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        name: &str,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        self.function_property_get_runtime_rooted_with_receiver(
            context,
            owner,
            function_id,
            None,
            name,
            value_roots,
            slice_roots,
        )
    }

    /// As [`Self::function_property_get_runtime_rooted`], but `receiver`
    /// supplies the value written to a freshly materialized
    /// `prototype.constructor` (the closure value for a closure; see
    /// [`Self::function_property_get_stack_rooted_with_receiver`]).
    pub(crate) fn function_property_get_runtime_rooted_with_receiver(
        &mut self,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        receiver: Option<Value>,
        name: &str,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if name != "prototype" {
            return self.function_property_get_non_prototype(context, owner, function_id, name);
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
                            self.run_callable_sync(context, &g, recv, SmallVec::new())
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
        let mut bag_roots = Vec::with_capacity(value_roots.len() + 2);
        bag_roots.push(&function_root);
        bag_roots.push(&constructor_value);
        bag_roots.extend_from_slice(value_roots);
        let bag =
            self.function_user_bag_runtime_rooted(owner, function_id, &bag_roots, slice_roots)?;
        if let Some(existing) = crate::object::get(bag, &self.gc_heap, "prototype") {
            return Ok(existing);
        }

        let bag_root = Value::object(bag);
        let mut proto_roots = Vec::with_capacity(value_roots.len() + 3);
        proto_roots.push(&function_root);
        proto_roots.push(&constructor_value);
        proto_roots.push(&bag_root);
        proto_roots.extend_from_slice(value_roots);
        let proto = self.alloc_runtime_rooted_object_with_roots(&proto_roots, slice_roots)?;
        if let Some(object_proto) = self.realm_intrinsics.object_prototype.or_else(|| {
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
                // §27.5.1 / §27.6.1 — inherit from the one shared
                // %GeneratorPrototype% / %AsyncGeneratorPrototype%.
                object::set_prototype(proto, &mut self.gc_heap, Some(shared));
            } else {
                let proto_value = Value::object(proto);
                let mut parent_roots = Vec::with_capacity(value_roots.len() + 3);
                parent_roots.push(&function_root);
                parent_roots.push(&bag_root);
                parent_roots.push(&proto_value);
                parent_roots.extend_from_slice(value_roots);
                let parent =
                    self.alloc_runtime_rooted_object_with_roots(&parent_roots, slice_roots)?;
                self.finish_generator_function_prototype(context, function_id, proto, parent)?;
            }
        }
        // Install `prototype` on the function's property bag first. It is a
        // non-moving define, and routing it before the constructor install means
        // the bag's `prototype` slot already tracks `proto` (the collector
        // forwards live GC slots) when the shape-advancing constructor define
        // below may relocate the heap.
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
        // forcing every dispatch through the generic method bridge. The define
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
            let _ = self.define_own_property_partial(proto, "constructor", constructor_desc)?;
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
        parent: JsObject,
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
            parent,
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
        let cached = match constructor_name {
            "Object" => self.realm_intrinsics.object_prototype,
            "Function" => self.realm_intrinsics.function_prototype,
            "Array" => self.realm_intrinsics.array_prototype,
            _ => None,
        };
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

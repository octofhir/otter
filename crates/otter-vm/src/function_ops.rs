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

enum BindMetadataGet {
    Value(Value),
    Getter(Value),
}

impl Interpreter {
    pub(crate) fn run_make_function_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, Value::Function { function_id })?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_make_closure_operands(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let idx = const_operand(operands.get(1))?;
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
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
        let upvalues: std::rc::Rc<[UpvalueCell]> = std::rc::Rc::from(cells);
        // Arrow-closure receivers are bound lexically: every later invocation
        // ignores the call site and uses the enclosing frame's `this`.
        let bound_this = if context.function_is_arrow(function_id) {
            Some(Box::new(frame.this_value.clone()))
        } else {
            None
        };
        write_register(
            frame,
            dst,
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            },
        )?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_make_class_regs(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        frame_idx: usize,
        dst: u16,
        ctor_reg: u16,
        proto_reg: u16,
        statics_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[frame_idx];
        let ctor = read_register(frame, ctor_reg)?.clone();
        if !self.is_callable_runtime(&ctor) {
            return Err(VmError::NotCallable);
        }
        let prototype = match read_register(frame, proto_reg)? {
            Value::Object(o) => *o,
            _ => return Err(VmError::TypeMismatch),
        };
        let statics = match read_register(frame, statics_reg)? {
            Value::Object(o) => *o,
            _ => return Err(VmError::TypeMismatch),
        };
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
        // §15.7.10 ClassDefinitionEvaluation step 24 — install
        // `C.prototype.constructor = C` so reflective probes
        // (`new Sub(...).constructor === Sub`) walk to the
        // class constructor itself rather than to the inherited
        // parent class's `constructor` slot.
        let constructor_desc = object::PropertyDescriptor::data(
            Value::ClassConstructor(class),
            /* writable */ true,
            /* enumerable */ false,
            /* configurable */ true,
        );
        let _ = object::define_own_property(
            prototype,
            &mut self.gc_heap,
            "constructor",
            constructor_desc,
        );
        let frame = &mut stack[frame_idx];
        write_register(frame, dst, Value::ClassConstructor(class))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn drive_bind_function(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        let pending = stack[top_idx]
            .pending_bind_function
            .as_ref()
            .filter(|state| state.pc == pc && state.dst == dst)
            .cloned();
        if let Some(state) = pending {
            let produced = read_register(&stack[top_idx], dst)?.clone();
            return match state.stage {
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
                    let target_name = state.target_name.ok_or(VmError::InvalidOperand)?;
                    stack[top_idx].pending_bind_function = None;
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
            };
        }

        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let target = read_register(&stack[top_idx], callee_reg)?.clone();
        if !self.is_callable_runtime(&target) {
            return Err(VmError::NotCallable);
        }
        let bound_this = read_register(&stack[top_idx], this_reg)?.clone();
        let mut bound_args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            bound_args.push(read_register(&stack[top_idx], r)?.clone());
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
                stack[top_idx].pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target: target.clone(),
                    bound_this,
                    bound_args,
                    stage: PendingBindStage::Name,
                    target_name: None,
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    fn continue_bind_function_after_name(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
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
                stack[top_idx].pending_bind_function = None;
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
                stack[top_idx].pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target: target.clone(),
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
        stack: &mut SmallVec<[Frame; 8]>,
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
        );
        let target_root = target.clone();
        let bound_this_root = bound_this.clone();
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
        stack[top_idx].pending_bind_function = None;
        write_register(&mut stack[top_idx], dst, Value::BoundFunction(bound))?;
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
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
                let receiver = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                self.run_callable_sync(context, &this_value, receiver, forwarded)
            }
            VmIntrinsicFunction::FunctionPrototypeApply => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let mut iter = args.into_iter();
                let receiver = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                    Some(arg_array) => self.create_list_from_array_like(context, arg_array)?,
                };
                self.run_callable_sync(context, &this_value, receiver, forwarded)
            }
            VmIntrinsicFunction::FunctionPrototypeBind => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let mut iter = args.into_iter();
                let receiver = iter.next().unwrap_or(Value::Undefined);
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &self.gc_heap,
                    &self.string_heap,
                    &self.function_user_props,
                    &self.function_deleted_metadata,
                );
                let metadata =
                    function_metadata::bound_create_metadata(&ctx, &this_value, bound_args.len())?;
                let this_root = this_value.clone();
                let receiver_root = receiver.clone();
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
                Ok(Value::BoundFunction(bound))
            }
            VmIntrinsicFunction::FunctionPrototypeToString => {
                if !self.is_callable_runtime(&this_value) {
                    return Err(VmError::NotCallable);
                }
                let ctx = function_metadata::FunctionMetadataContext::new(
                    context,
                    &self.gc_heap,
                    &self.string_heap,
                    &self.function_user_props,
                    &self.function_deleted_metadata,
                );
                let display = function_metadata::callable_to_string(&ctx, &this_value);
                let s = JsString::from_str(&display, &self.string_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                Ok(Value::String(s))
            }
            VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance => {
                // §20.2.3.6: Return ? OrdinaryHasInstance(F, V) where
                // F is the `this` value and V is the first argument.
                // <https://tc39.es/ecma262/#sec-function.prototype-@@hasinstance>
                let v = args.into_iter().next().unwrap_or(Value::Undefined);
                let result = self.ordinary_has_instance(context, &this_value, &v)?;
                Ok(Value::Boolean(result))
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
        if let Value::BoundFunction(bound) = c {
            let (target, _, _) = bound.parts(&self.gc_heap);
            return self.instanceof_operator(context, o, &target);
        }
        if !matches!(
            o,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Array(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
                | Value::RegExp(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Promise(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_)
        ) {
            return Ok(false);
        }
        let Some(prototype) = self.instanceof_target_prototype(context, c)? else {
            return Ok(false);
        };
        if !matches!(prototype, Value::Object(_) | Value::Proxy(_)) {
            return Err(VmError::TypeError {
                message: "Function has non-object prototype 'undefined' in instanceof check"
                    .to_string(),
            });
        }
        self.value_has_proxy_aware_prototype(context, o.clone(), &prototype)
    }

    pub(crate) fn ordinary_has_instance_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        c: &Value,
        o: &Value,
    ) -> Result<bool, VmError> {
        if !self.is_callable_runtime(c) {
            return Ok(false);
        }
        if let Value::BoundFunction(bound) = c {
            let (target, _, _) = bound.parts(&self.gc_heap);
            return self.instanceof_operator_stack_rooted(context, stack, o, &target);
        }
        if !matches!(
            o,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Array(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
                | Value::RegExp(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Promise(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_)
        ) {
            return Ok(false);
        }
        let Some(prototype) = self.instanceof_target_prototype_stack_rooted(context, stack, c)?
        else {
            return Ok(false);
        };
        if !matches!(prototype, Value::Object(_) | Value::Proxy(_)) {
            return Err(VmError::TypeError {
                message: "Function has non-object prototype 'undefined' in instanceof check"
                    .to_string(),
            });
        }
        self.value_has_proxy_aware_prototype(context, o.clone(), &prototype)
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
        if !matches!(
            target,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
        ) {
            return Err(VmError::TypeError {
                message: "Right-hand side of instanceof is not an object".to_string(),
            });
        }
        let has_instance_sym = self.well_known_symbols.get(symbol::WellKnown::HasInstance);
        let key = VmPropertyKey::Symbol(has_instance_sym);
        let handler =
            match self.ordinary_get_value(context, target.clone(), target.clone(), &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, target.clone(), SmallVec::new())?
                }
            };
        if !matches!(handler, Value::Undefined | Value::Null) {
            if !self.is_callable_runtime(&handler) {
                return Err(VmError::TypeError {
                    message: "@@hasInstance must be callable".to_string(),
                });
            }
            if let Value::NativeFunction(native) = &handler
                && native.is_vm_intrinsic(
                    &self.gc_heap,
                    VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance,
                )
            {
                return self.ordinary_has_instance(context, target, v);
            }
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(v.clone());
            let result = self.run_callable_sync(context, &handler, target.clone(), args)?;
            return Ok(result.to_boolean());
        }
        if !self.is_callable_runtime(target) {
            return Err(VmError::TypeError {
                message: "Right-hand side of instanceof is not callable".to_string(),
            });
        }
        self.ordinary_has_instance(context, target, v)
    }

    pub(crate) fn instanceof_operator_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        v: &Value,
        target: &Value,
    ) -> Result<bool, VmError> {
        if !matches!(
            target,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
        ) {
            return Err(VmError::TypeError {
                message: "Right-hand side of instanceof is not an object".to_string(),
            });
        }
        let has_instance_sym = self.well_known_symbols.get(symbol::WellKnown::HasInstance);
        let key = VmPropertyKey::Symbol(has_instance_sym);
        let handler =
            match self.ordinary_get_value(context, target.clone(), target.clone(), &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, target.clone(), SmallVec::new())?
                }
            };
        if !matches!(handler, Value::Undefined | Value::Null) {
            if !self.is_callable_runtime(&handler) {
                return Err(VmError::TypeError {
                    message: "@@hasInstance must be callable".to_string(),
                });
            }
            if let Value::NativeFunction(native) = &handler
                && native.is_vm_intrinsic(
                    &self.gc_heap,
                    VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance,
                )
            {
                return self.ordinary_has_instance_stack_rooted(context, stack, target, v);
            }
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(v.clone());
            let result = self.run_callable_sync(context, &handler, target.clone(), args)?;
            return Ok(result.to_boolean());
        }
        if !self.is_callable_runtime(target) {
            return Err(VmError::TypeError {
                message: "Right-hand side of instanceof is not callable".to_string(),
            });
        }
        self.ordinary_has_instance_stack_rooted(context, stack, target, v)
    }

    pub(crate) fn create_list_from_array_like(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<SmallVec<[Value; 8]>, VmError> {
        // §7.3.18 — `Type(obj) must be Object`. Cover every shape
        // the VM models as a JS Object: ordinary objects, arrays,
        // proxies, every callable variant (so `Reflect.apply(fn,
        // null, new Function())` reads `.length` and walks indices
        // per spec), plus the exotic objects with their own
        // `[[Get]]` ladder (RegExp, ArrayBuffer, DataView,
        // TypedArray, collections, etc.).
        if !matches!(
            value,
            Value::Object(_)
                | Value::Array(_)
                | Value::Proxy(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
                | Value::RegExp(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_)
                | Value::Promise(_)
        ) {
            return Err(VmError::TypeError {
                message: "Function.prototype.apply argument list must be object-like".to_string(),
            });
        }
        let length = self.get_property_value_for_call(context, value.clone(), "length")?;
        let len = to_length(&length)?;
        let mut values = SmallVec::new();
        for index in 0..len {
            let key = index.to_string();
            values.push(self.get_property_value_for_call(context, value.clone(), &key)?);
        }
        Ok(values)
    }

    pub(crate) fn get_property_value_for_call(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        let property_key = VmPropertyKey::String(key);
        match self.ordinary_get_value(
            context,
            receiver.clone(),
            receiver.clone(),
            &property_key,
            0,
        )? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, receiver, SmallVec::new())
            }
        }
    }
    fn callable_bind_metadata_get(
        &self,
        context: &ExecutionContext,
        target: &Value,
        key: &str,
    ) -> Result<BindMetadataGet, VmError> {
        match target {
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    *function_id,
                    key,
                )? {
                    Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                    None => Ok(BindMetadataGet::Value(Value::Undefined)),
                }
            }
            Value::NativeFunction(native) => {
                match native.own_property_descriptor(&self.gc_heap, &self.string_heap, key)? {
                    Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                    None => Ok(BindMetadataGet::Value(Value::Undefined)),
                }
            }
            Value::BoundFunction(bound) => {
                match function_metadata::bound_own_property_descriptor(
                    bound,
                    &self.gc_heap,
                    &self.string_heap,
                    key,
                )? {
                    Some(desc) => Ok(bind_metadata_get_from_descriptor(desc)),
                    None => Ok(BindMetadataGet::Value(Value::Undefined)),
                }
            }
            Value::ClassConstructor(class) => {
                self.callable_bind_metadata_get(context, &class.ctor(&self.gc_heap), key)
            }
            Value::Object(obj) => {
                if let Some(desc) = object::get_own_descriptor(*obj, &self.gc_heap, key) {
                    return Ok(bind_metadata_get_from_descriptor(desc));
                }
                match object::constructor_native(*obj, &self.gc_heap) {
                    Some(native @ Value::NativeFunction(_)) => {
                        self.callable_bind_metadata_get(context, &native, key)
                    }
                    _ => Ok(BindMetadataGet::Value(Value::Undefined)),
                }
            }
            _ => Ok(BindMetadataGet::Value(Value::Undefined)),
        }
    }

    pub(crate) fn coerce_vm_property_key(
        arg: Option<&Value>,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        match arg {
            Some(Value::String(s)) => Ok(VmPropertyKey::OwnedString(s.to_lossy_string())),
            Some(Value::Number(n)) => Ok(VmPropertyKey::OwnedString(n.to_display_string())),
            Some(Value::Boolean(b)) => Ok(VmPropertyKey::String(if *b { "true" } else { "false" })),
            Some(Value::Null) => Ok(VmPropertyKey::String("null")),
            Some(Value::Undefined) | None => Ok(VmPropertyKey::String("undefined")),
            Some(Value::Symbol(sym)) => Ok(VmPropertyKey::Symbol(sym.clone())),
            _ => Err(VmError::TypeMismatch),
        }
    }

    pub(crate) fn function_user_bag_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        function_id: u32,
        value_roots: &[&Value],
    ) -> Result<JsObject, VmError> {
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
        function_id: u32,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsObject, VmError> {
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
        &self,
        context: &ExecutionContext,
        function_id: u32,
        key: &str,
    ) -> Result<bool, VmError> {
        if self
            .ordinary_function_own_property_descriptor(Some(context), function_id, key)?
            .is_some()
        {
            return Ok(true);
        }
        Ok(key == "prototype"
            && !context.function_is_arrow(function_id)
            && !self
                .function_deleted_metadata
                .contains(&(function_id, "prototype")))
    }

    pub(crate) fn ordinary_function_has_own_symbol_property_for_extensibility(
        &self,
        function_id: u32,
        key: &crate::symbol::JsSymbol,
    ) -> bool {
        self.function_user_props
            .get(&function_id)
            .copied()
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
        function_id: u32,
    ) -> Vec<String> {
        let mut keys = Vec::new();
        let is_arrow = context.function_is_arrow(function_id);
        let deleted =
            |key: &'static str| self.function_deleted_metadata.contains(&(function_id, key));
        if !deleted("length") {
            keys.push("length".to_string());
        }
        if !deleted("name") {
            keys.push("name".to_string());
        }
        let mut bag_has_prototype = false;
        if let Some(bag) = self.function_user_props.get(&function_id).copied() {
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
        if !is_arrow && !bag_has_prototype {
            keys.push("prototype".to_string());
        }
        keys
    }

    pub(crate) fn ordinary_function_own_property_descriptor(
        &self,
        context: Option<&ExecutionContext>,
        function_id: u32,
        key: &str,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        if let Some(bag) = self.function_user_props.get(&function_id).copied()
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
        let ctx = function_metadata::FunctionMetadataContext::new(
            context,
            &self.gc_heap,
            &self.string_heap,
            &self.function_user_props,
            &self.function_deleted_metadata,
        );
        let value =
            function_metadata::ordinary_function_intrinsic_property(&ctx, function_id, key)?;
        Ok(Some(object::PropertyDescriptor::data(
            value, false, false, true,
        )))
    }

    pub(crate) fn ordinary_function_define_own_property(
        &mut self,
        context: Option<&ExecutionContext>,
        function_id: u32,
        key: &str,
        desc_obj: Option<JsObject>,
        descriptor: object::PropertyDescriptor,
    ) -> Result<bool, VmError> {
        self.ordinary_function_define_own_property_with_roots(
            context,
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
        function_id: u32,
        key: &str,
        desc_obj: Option<JsObject>,
        descriptor: object::PropertyDescriptor,
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
        value_roots: &[&Value],
    ) -> Result<bool, VmError> {
        let descriptor =
            match self.ordinary_function_own_property_descriptor(context, function_id, key)? {
                Some(existing) => {
                    let descriptor =
                        if function_metadata::ordinary_function_metadata_key(key).is_some() {
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
                    match object::validate_descriptor_update(&existing, &descriptor) {
                        Some(merged) => merged,
                        None => return Ok(false),
                    }
                }
                None => {
                    let has_virtual_prototype = context.is_some_and(|context| {
                        key == "prototype"
                            && !context.function_is_arrow(function_id)
                            && !self
                                .function_deleted_metadata
                                .contains(&(function_id, "prototype"))
                    });
                    if !has_virtual_prototype && !self.ordinary_function_is_extensible(function_id)
                    {
                        return Ok(false);
                    }
                    descriptor
                }
            };
        let mut roots = Vec::with_capacity(value_roots.len() + 3);
        roots.extend_from_slice(value_roots);
        let desc_obj_root = desc_obj.map(Value::Object);
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
            Some(stack) => self.function_user_bag_stack_rooted(stack, function_id, &roots)?,
            None => self.function_user_bag_runtime_rooted(function_id, &roots, &[])?,
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
        function_id: u32,
        key: &str,
    ) -> bool {
        let Some(metadata_key) = function_metadata::ordinary_function_metadata_key(key) else {
            return self
                .function_user_props
                .get(&function_id)
                .copied()
                .map(|bag| crate::object::delete(bag, &mut self.gc_heap, key))
                .unwrap_or(true);
        };
        if let Some(bag) = self.function_user_props.get(&function_id).copied()
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
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
        method: otter_bytecode::method_id::ObjectMethod,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        use otter_bytecode::method_id::ObjectMethod as M;
        let Some(target) = args.first().cloned() else {
            return Ok(None);
        };
        if matches!(
            target,
            Value::Proxy(_)
                | Value::Array(_)
                | Value::RegExp(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::BoundFunction(_)
                | Value::NativeFunction(_)
        ) && matches!(
            method,
            M::GetOwnPropertyDescriptor | M::HasOwn | M::Keys | M::GetOwnPropertyNames
        ) {
            let Some(context) = context else {
                return if matches!(target, Value::Proxy(_)) {
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
                let string_heap = self.string_heap.clone();
                let values: Vec<Value> = self
                    .own_property_keys_value(context, &target, &string_heap)?
                    .into_iter()
                    .filter(|v| matches!(v, Value::String(_)))
                    .collect();
                return Ok(Some(Value::Array(self.function_static_array_from_values(
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
                if matches!(target, Value::Proxy(_)) {
                    let string_heap = self.string_heap.clone();
                    let trap_keys = self.own_property_keys_value(context, &target, &string_heap)?;
                    let mut values: Vec<Value> = Vec::with_capacity(trap_keys.len());
                    for key in trap_keys {
                        let Value::String(_) = &key else { continue };
                        let vm_key = match &key {
                            Value::String(s) => VmPropertyKey::OwnedString(s.to_lossy_string()),
                            Value::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
                            _ => return Err(VmError::TypeMismatch),
                        };
                        let desc = match stack_roots {
                            Some(stack) => self
                                .ordinary_get_own_property_descriptor_value_stack_rooted(
                                    context,
                                    stack,
                                    target.clone(),
                                    &vm_key,
                                    0,
                                )?,
                            None => self
                                .ordinary_get_own_property_descriptor_value_runtime_rooted(
                                    context,
                                    target.clone(),
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
                    return Ok(Some(Value::Array(self.function_static_array_from_values(
                        stack_roots,
                        values,
                        &[&target],
                        &[args],
                    )?)));
                }
                let keys = self.enumerable_own_string_keys_for_value(context, target.clone(), 0)?;
                let mut values = Vec::with_capacity(keys.len());
                for key in keys {
                    values.push(Value::String(
                        JsString::from_str(&key, &self.string_heap)
                            .map_err(|_| VmError::TypeMismatch)?,
                    ));
                }
                return Ok(Some(Value::Array(self.function_static_array_from_values(
                    stack_roots,
                    values,
                    &[&target],
                    &[args],
                )?)));
            }
            let desc =
                self.get_own_property_descriptor_for_value(context, target.clone(), args.get(1))?;
            if matches!(method, M::HasOwn) {
                return Ok(Some(Value::Boolean(desc.is_some())));
            }
            return match desc {
                Some(desc) => Ok(Some(Value::Object(
                    self.function_static_descriptor_to_object(
                        stack_roots,
                        &desc,
                        &[&target],
                        args,
                    )?,
                ))),
                None => Ok(Some(Value::Undefined)),
            };
        }
        let function_id = match &target {
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                Some(*function_id)
            }
            Value::BoundFunction(_) => None,
            _ => return Ok(None),
        };
        match method {
            M::DefineProperty => {
                let key = Self::coerce_vm_property_key(args.get(1))?;
                let desc_obj = match args.get(2) {
                    Some(Value::Object(obj)) => *obj,
                    _ => return Err(VmError::TypeMismatch),
                };
                let descriptor = object_statics::coerce_to_descriptor(&desc_obj, &self.gc_heap)?;
                let completed = descriptor.complete_for_new_property();
                let ok = match (&target, function_id, &key) {
                    (_, Some(function_id), VmPropertyKey::Symbol(sym)) => {
                        if !self.ordinary_function_has_own_symbol_property_for_extensibility(
                            function_id,
                            sym,
                        ) && !self.ordinary_function_is_extensible(function_id)
                        {
                            return Err(VmError::TypeMismatch);
                        }
                        let bag = match stack_roots {
                            Some(stack) => {
                                self.function_user_bag_stack_rooted(stack, function_id, &[&target])?
                            }
                            None => self.function_user_bag_runtime_rooted(
                                function_id,
                                &[&target],
                                &[args],
                            )?,
                        };
                        crate::object::define_own_symbol_property_partial(
                            bag,
                            &mut self.gc_heap,
                            sym,
                            descriptor,
                        )
                    }
                    (_, Some(function_id), _) => self
                        .ordinary_function_define_own_property_with_roots(
                            context,
                            function_id,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            Some(desc_obj),
                            completed,
                            stack_roots,
                            &[&target],
                        )?,
                    (Value::BoundFunction(_), None, VmPropertyKey::Symbol(_)) => false,
                    (Value::BoundFunction(bound), None, _) => {
                        function_metadata::bound_define_own_property(
                            bound,
                            &mut self.gc_heap,
                            &self.string_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            completed,
                        )
                    }
                    _ => return Ok(None),
                };
                if !ok {
                    return Err(VmError::TypeMismatch);
                }
                Ok(Some(target))
            }
            M::GetOwnPropertyDescriptor => {
                let key = Self::coerce_vm_property_key(args.get(1))?;
                let desc = match (&target, function_id, &key) {
                    (_, Some(function_id), VmPropertyKey::Symbol(sym)) => {
                        let Some(bag) = self.function_user_props.get(&function_id).copied() else {
                            return Ok(Some(Value::Undefined));
                        };
                        crate::object::get_own_symbol_descriptor(bag, &self.gc_heap, sym)
                    }
                    (_, Some(function_id), _) => self.ordinary_function_own_property_descriptor(
                        context,
                        function_id,
                        key.string_name()
                            .expect("non-symbol key has string spelling"),
                    )?,
                    (Value::BoundFunction(_), None, VmPropertyKey::Symbol(_)) => None,
                    (Value::BoundFunction(bound), None, _) => {
                        function_metadata::bound_own_property_descriptor(
                            bound,
                            &self.gc_heap,
                            &self.string_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        )?
                    }
                    _ => return Ok(None),
                };
                match desc {
                    Some(desc) => Ok(Some(Value::Object(
                        self.function_static_descriptor_to_object(
                            stack_roots,
                            &desc,
                            &[&target],
                            args,
                        )?,
                    ))),
                    None => Ok(Some(Value::Undefined)),
                }
            }
            M::HasOwn => {
                let key = Self::coerce_vm_property_key(args.get(1))?;
                let present = match (&target, function_id, &key) {
                    (_, Some(function_id), VmPropertyKey::Symbol(sym)) => self
                        .function_user_props
                        .get(&function_id)
                        .copied()
                        .map(|bag| crate::object::has_own_symbol(bag, &self.gc_heap, sym))
                        .unwrap_or(false),
                    (_, Some(function_id), _) => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        let user_present = self
                            .function_user_props
                            .get(&function_id)
                            .copied()
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
                    (Value::BoundFunction(_), None, VmPropertyKey::Symbol(_)) => false,
                    (Value::BoundFunction(bound), None, _) => {
                        function_metadata::bound_has_own_property(
                            bound,
                            &self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        )
                    }
                    _ => return Ok(None),
                };
                Ok(Some(Value::Boolean(present)))
            }
            // §20.1.2.14 / §20.1.2.18 — ordinary functions keep
            // expando storage outside `ObjectBody`, so handle their
            // `[[Extensible]]` state before the generic static
            // dispatcher. This mirrors §10.1.3/§10.1.4 for the
            // side-table-backed function shape.
            M::IsExtensible => match target {
                Value::Function { function_id } | Value::Closure { function_id, .. } => Ok(Some(
                    Value::Boolean(self.ordinary_function_is_extensible(function_id)),
                )),
                _ => Ok(None),
            },
            M::PreventExtensions => match target {
                Value::Function { function_id } | Value::Closure { function_id, .. } => {
                    self.ordinary_function_prevent_extensions(function_id);
                    Ok(Some(target))
                }
                _ => Ok(None),
            },
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
            | M::GroupBy => Ok(None),
        }
    }

    fn function_static_array_from_values(
        &mut self,
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
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
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
        desc: &object::PropertyDescriptor,
        value_roots: &[&Value],
        slice_roots: &[Value],
    ) -> Result<JsObject, VmError> {
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.extend_from_slice(value_roots);
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
        match &desc.kind {
            object::DescriptorKind::Data { value } => {
                self.set_property(result, "value", value.clone())?;
                self.set_property(result, "writable", Value::Boolean(desc.writable()))?;
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                self.set_property(result, "get", getter.clone().unwrap_or(Value::Undefined))?;
                self.set_property(result, "set", setter.clone().unwrap_or(Value::Undefined))?;
            }
        }
        self.set_property(result, "enumerable", Value::Boolean(desc.enumerable()))?;
        self.set_property(result, "configurable", Value::Boolean(desc.configurable()))?;
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
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        if name == "prototype" {
            return self.function_property_get_runtime_rooted(context, function_id, name, &[], &[]);
        }
        self.function_property_get_non_prototype(context, function_id, name)
    }

    fn function_property_get_non_prototype(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        if let Some(bag) = self.function_user_props.get(&function_id).copied()
            && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
        {
            return Ok(v);
        }
        if name == "name" || name == "length" {
            let ctx = function_metadata::FunctionMetadataContext::new(
                context,
                &self.gc_heap,
                &self.string_heap,
                &self.function_user_props,
                &self.function_deleted_metadata,
            );
            return function_metadata::ordinary_function_intrinsic_property(
                &ctx,
                function_id,
                name,
            );
        }
        if let Some(value) = self
            .load_function_prototype_method(name)
            .or_else(|| self.load_object_prototype_method(name))
        {
            return Ok(value);
        }
        Ok(Value::Undefined)
    }

    pub(crate) fn function_property_get_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        if name != "prototype" {
            return self.function_property_get_non_prototype(context, function_id, name);
        }
        if let Some(bag) = self.function_user_props.get(&function_id).copied()
            && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
        {
            return Ok(v);
        }

        let function_root = Value::Function { function_id };
        let bag = self.function_user_bag_stack_rooted(stack, function_id, &[&function_root])?;
        if let Some(existing) = crate::object::get(bag, &self.gc_heap, "prototype") {
            return Ok(existing);
        }

        let bag_root = Value::Object(bag);
        let proto =
            self.alloc_stack_rooted_object_with_extra_roots(stack, &[&function_root, &bag_root])?;
        if let Some(Value::Object(object_ctor)) =
            crate::object::get(self.global_this, &self.gc_heap, "Object")
            && let Some(Value::Object(object_proto)) =
                crate::object::get(object_ctor, &self.gc_heap, "prototype")
        {
            crate::object::set_prototype(proto, &mut self.gc_heap, Some(object_proto));
        }
        let proto_value = Value::Object(proto);
        let constructor = object::PropertyDescriptor::data(function_root, true, false, true);
        let _ = object::define_own_property(proto, &mut self.gc_heap, "constructor", constructor);
        let prototype_desc =
            object::PropertyDescriptor::data(proto_value.clone(), true, false, false);
        let _ = object::define_own_property(bag, &mut self.gc_heap, "prototype", prototype_desc);
        Ok(proto_value)
    }

    pub(crate) fn function_property_get_runtime_rooted(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name: &str,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if name != "prototype" {
            return self.function_property_get_non_prototype(context, function_id, name);
        }
        if let Some(bag) = self.function_user_props.get(&function_id).copied()
            && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
        {
            return Ok(v);
        }

        let function_root = Value::Function { function_id };
        let mut bag_roots = Vec::with_capacity(value_roots.len() + 1);
        bag_roots.push(&function_root);
        bag_roots.extend_from_slice(value_roots);
        let bag = self.function_user_bag_runtime_rooted(function_id, &bag_roots, slice_roots)?;
        if let Some(existing) = crate::object::get(bag, &self.gc_heap, "prototype") {
            return Ok(existing);
        }

        let bag_root = Value::Object(bag);
        let mut proto_roots = Vec::with_capacity(value_roots.len() + 2);
        proto_roots.push(&function_root);
        proto_roots.push(&bag_root);
        proto_roots.extend_from_slice(value_roots);
        let proto = self.alloc_runtime_rooted_object_with_roots(&proto_roots, slice_roots)?;
        if let Some(Value::Object(object_ctor)) =
            crate::object::get(self.global_this, &self.gc_heap, "Object")
            && let Some(Value::Object(object_proto)) =
                crate::object::get(object_ctor, &self.gc_heap, "prototype")
        {
            crate::object::set_prototype(proto, &mut self.gc_heap, Some(object_proto));
        }
        let proto_value = Value::Object(proto);
        let constructor = object::PropertyDescriptor::data(function_root, true, false, true);
        let _ = object::define_own_property(proto, &mut self.gc_heap, "constructor", constructor);
        let prototype_desc =
            object::PropertyDescriptor::data(proto_value.clone(), true, false, false);
        let _ = object::define_own_property(bag, &mut self.gc_heap, "prototype", prototype_desc);
        Ok(proto_value)
    }

    pub(crate) fn load_global_prototype_method(
        &self,
        constructor_name: &str,
        name: &str,
    ) -> Option<Value> {
        let constructor = crate::object::get(self.global_this, &self.gc_heap, constructor_name)?;
        let Value::Object(constructor_obj) = constructor else {
            return None;
        };
        let prototype = crate::object::get(constructor_obj, &self.gc_heap, "prototype")?;
        let Value::Object(prototype_obj) = prototype else {
            return None;
        };
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
        *descriptor_value = value.clone();
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
            _ => BindMetadataGet::Value(Value::Undefined),
        },
    }
}

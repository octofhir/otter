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
        frame: &mut Frame,
        dst: u16,
        ctor_reg: u16,
        proto_reg: u16,
        statics_reg: u16,
    ) -> Result<(), VmError> {
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
        let class = ClassConstructor::new(&mut self.gc_heap, ctor, prototype, statics)?;
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
        let bound = BoundFunction::new_with_metadata(
            &mut self.gc_heap,
            target,
            bound_this,
            bound_args,
            metadata,
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
                let bound = BoundFunction::new_with_metadata(
                    &mut self.gc_heap,
                    this_value,
                    receiver,
                    bound_args,
                    metadata,
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

    pub(crate) fn create_list_from_array_like(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<SmallVec<[Value; 8]>, VmError> {
        if !matches!(value, Value::Object(_) | Value::Array(_) | Value::Proxy(_)) {
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
        let property_key = VmPropertyKey::String(key.to_string());
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

    pub(crate) fn coerce_vm_property_key(arg: Option<&Value>) -> Result<VmPropertyKey, VmError> {
        match arg {
            Some(Value::String(s)) => Ok(VmPropertyKey::String(s.to_lossy_string())),
            Some(Value::Number(n)) => Ok(VmPropertyKey::String(n.to_display_string())),
            Some(Value::Boolean(b)) => Ok(VmPropertyKey::String(
                (if *b { "true" } else { "false" }).to_string(),
            )),
            Some(Value::Null) => Ok(VmPropertyKey::String("null".to_string())),
            Some(Value::Undefined) | None => Ok(VmPropertyKey::String("undefined".to_string())),
            Some(Value::Symbol(sym)) => Ok(VmPropertyKey::Symbol(sym.clone())),
            _ => Err(VmError::TypeMismatch),
        }
    }

    pub(crate) fn function_user_bag(&mut self, function_id: u32) -> Result<JsObject, VmError> {
        match self.function_user_props.get(&function_id).copied() {
            Some(bag) => Ok(bag),
            None => {
                let bag = crate::object::alloc_object(&mut self.gc_heap)?;
                self.function_user_props.insert(function_id, bag);
                Ok(bag)
            }
        }
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
                None => descriptor,
            };
        let bag = self.function_user_bag(function_id)?;
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
        ) && matches!(method, M::GetOwnPropertyDescriptor | M::HasOwn | M::Keys)
        {
            let Some(context) = context else {
                return if matches!(target, Value::Proxy(_)) {
                    Err(VmError::InvalidOperand)
                } else {
                    Ok(None)
                };
            };
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
                            Value::String(s) => VmPropertyKey::String(s.to_lossy_string()),
                            Value::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
                            _ => return Err(VmError::TypeMismatch),
                        };
                        let desc = self.ordinary_get_own_property_descriptor_value(
                            context,
                            target.clone(),
                            &vm_key,
                            0,
                        )?;
                        if desc.as_ref().is_some_and(|d| d.enumerable()) {
                            values.push(key);
                        }
                    }
                    return Ok(Some(Value::Array(array::from_elements(
                        &mut self.gc_heap,
                        values,
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
                return Ok(Some(Value::Array(array::from_elements(
                    &mut self.gc_heap,
                    values,
                )?)));
            }
            let desc =
                self.get_own_property_descriptor_for_value(context, target.clone(), args.get(1))?;
            if matches!(method, M::HasOwn) {
                return Ok(Some(Value::Boolean(desc.is_some())));
            }
            return match desc {
                Some(desc) => Ok(Some(Value::Object(object_statics::descriptor_to_object(
                    &desc,
                    &mut self.gc_heap,
                )?))),
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
                let ok = match (&target, function_id, key) {
                    (_, Some(function_id), VmPropertyKey::String(key)) => self
                        .ordinary_function_define_own_property(
                            context,
                            function_id,
                            &key,
                            Some(desc_obj),
                            completed,
                        )?,
                    (_, Some(function_id), VmPropertyKey::Symbol(sym)) => {
                        let bag = self.function_user_bag(function_id)?;
                        crate::object::define_own_symbol_property_partial(
                            bag,
                            &mut self.gc_heap,
                            &sym,
                            descriptor,
                        )
                    }
                    (Value::BoundFunction(bound), None, VmPropertyKey::String(key)) => {
                        function_metadata::bound_define_own_property(
                            bound,
                            &mut self.gc_heap,
                            &self.string_heap,
                            &key,
                            completed,
                        )
                    }
                    (Value::BoundFunction(_), None, VmPropertyKey::Symbol(_)) => false,
                    _ => return Ok(None),
                };
                if !ok {
                    return Err(VmError::TypeMismatch);
                }
                Ok(Some(target))
            }
            M::GetOwnPropertyDescriptor => {
                let key = Self::coerce_vm_property_key(args.get(1))?;
                let desc = match (&target, function_id, key) {
                    (_, Some(function_id), VmPropertyKey::String(key)) => {
                        self.ordinary_function_own_property_descriptor(context, function_id, &key)?
                    }
                    (_, Some(function_id), VmPropertyKey::Symbol(sym)) => {
                        let Some(bag) = self.function_user_props.get(&function_id).copied() else {
                            return Ok(Some(Value::Undefined));
                        };
                        crate::object::get_own_symbol_descriptor(bag, &self.gc_heap, &sym)
                    }
                    (Value::BoundFunction(bound), None, VmPropertyKey::String(key)) => {
                        function_metadata::bound_own_property_descriptor(
                            bound,
                            &self.gc_heap,
                            &self.string_heap,
                            &key,
                        )?
                    }
                    (Value::BoundFunction(_), None, VmPropertyKey::Symbol(_)) => None,
                    _ => return Ok(None),
                };
                match desc {
                    Some(desc) => Ok(Some(Value::Object(object_statics::descriptor_to_object(
                        &desc,
                        &mut self.gc_heap,
                    )?))),
                    None => Ok(Some(Value::Undefined)),
                }
            }
            M::HasOwn => {
                let key = Self::coerce_vm_property_key(args.get(1))?;
                let present = match (&target, function_id, key) {
                    (_, Some(function_id), VmPropertyKey::String(key)) => {
                        let user_present = self
                            .function_user_props
                            .get(&function_id)
                            .copied()
                            .map(|bag| {
                                !matches!(
                                    crate::object::lookup_own(bag, &self.gc_heap, &key),
                                    object::PropertyLookup::Absent
                                )
                            })
                            .unwrap_or(false);
                        user_present
                            || function_metadata::ordinary_function_metadata_key(&key).is_some_and(
                                |metadata_key| {
                                    !self
                                        .function_deleted_metadata
                                        .contains(&(function_id, metadata_key))
                                },
                            )
                    }
                    (_, Some(function_id), VmPropertyKey::Symbol(sym)) => self
                        .function_user_props
                        .get(&function_id)
                        .copied()
                        .map(|bag| crate::object::has_own_symbol(bag, &self.gc_heap, &sym))
                        .unwrap_or(false),
                    (Value::BoundFunction(bound), None, VmPropertyKey::String(key)) => {
                        function_metadata::bound_has_own_property(bound, &self.gc_heap, &key)
                    }
                    (Value::BoundFunction(_), None, VmPropertyKey::Symbol(_)) => false,
                    _ => return Ok(None),
                };
                Ok(Some(Value::Boolean(present)))
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
            | M::IsExtensible
            | M::IsFrozen
            | M::IsSealed
            | M::Keys
            | M::PreventExtensions
            | M::Seal
            | M::Values => Ok(None),
        }
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
        if let Some(bag) = self.function_user_props.get(&function_id).copied()
            && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
        {
            return Ok(v);
        }
        if name == "prototype" {
            // §9.2.10 — function instances expose a writable,
            // non-configurable `.prototype` that auto-allocates as
            // a fresh ordinary object on first access. The fresh
            // prototype owns the standard non-enumerable
            // `constructor` data property pointing back at the
            // function object.
            let bag = match self.function_user_props.get(&function_id).copied() {
                Some(b) => b,
                None => {
                    let new_bag = crate::object::alloc_object(&mut self.gc_heap)?;
                    self.function_user_props.insert(function_id, new_bag);
                    new_bag
                }
            };
            if let Some(existing) = crate::object::get(bag, &self.gc_heap, "prototype") {
                return Ok(existing);
            }
            let proto = crate::object::alloc_object(&mut self.gc_heap)?;
            if let Some(Value::Object(object_ctor)) =
                crate::object::get(self.global_this, &self.gc_heap, "Object")
                && let Some(Value::Object(object_proto)) =
                    crate::object::get(object_ctor, &self.gc_heap, "prototype")
            {
                crate::object::set_prototype(proto, &mut self.gc_heap, Some(object_proto));
            }
            let proto_value = Value::Object(proto);
            let constructor = object::PropertyDescriptor::data(
                Value::Function { function_id },
                true,
                false,
                true,
            );
            let _ =
                object::define_own_property(proto, &mut self.gc_heap, "constructor", constructor);
            let prototype_desc =
                object::PropertyDescriptor::data(proto_value.clone(), true, false, false);
            let _ =
                object::define_own_property(bag, &mut self.gc_heap, "prototype", prototype_desc);
            return Ok(proto_value);
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

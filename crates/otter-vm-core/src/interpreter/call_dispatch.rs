use super::*;
use crate::context::DispatchAction;

impl Interpreter {
    /// Call a native function with depth tracking to prevent Rust stack overflow.
    ///
    /// This method tracks the native call depth and returns an error if it exceeds
    /// the maximum. This prevents JS code that calls native functions recursively
    /// from overflowing the Rust stack.
    #[inline]
    pub(super) fn call_native_fn(
        &self,
        ctx: &mut VmContext,
        native_fn: &crate::value::NativeFn,
        this_value: &Value,
        args: &[Value],
    ) -> VmResult<Value> {
        self.call_native_fn_with_realm(ctx, native_fn, this_value, args, None, false)
    }

    pub(super) fn call_native_fn_with_realm(
        &self,
        ctx: &mut VmContext,
        native_fn: &crate::value::NativeFn,
        this_value: &Value,
        args: &[Value],
        target_realm: Option<RealmId>,
        as_construct: bool,
    ) -> VmResult<Value> {
        let previous_realm = ctx.realm_id();
        if let Some(realm_id) = target_realm
            && realm_id != previous_realm
        {
            ctx.switch_realm(realm_id);
        }

        ctx.enter_native_call()?;
        // Take pending NewTarget before creating NativeContext (avoids double borrow)
        let pending_nt = ctx.take_pending_new_target();
        let result = {
            let mut ncx = if as_construct {
                crate::context::NativeContext::new_construct(ctx, self)
            } else {
                crate::context::NativeContext::new(ctx, self)
            };
            // Propagate pending NewTarget from Reflect.construct
            if let Some(nt) = pending_nt {
                ncx.set_new_target(nt);
            }
            native_fn(this_value, args, &mut ncx)
        };
        ctx.exit_native_call();

        let result = match result {
            Err(VmError::TypeError(message)) => Err(VmError::exception(self.make_error(
                ctx,
                "TypeError",
                &message,
            ))),
            Err(VmError::RangeError(message)) => Err(VmError::exception(self.make_error(
                ctx,
                "RangeError",
                &message,
            ))),
            Err(VmError::ReferenceError(message)) => Err(VmError::exception(self.make_error(
                ctx,
                "ReferenceError",
                &message,
            ))),
            Err(VmError::SyntaxError(message)) => Err(VmError::exception(self.make_error(
                ctx,
                "SyntaxError",
                &message,
            ))),
            other => other,
        };

        if ctx.realm_id() != previous_realm {
            ctx.switch_realm(previous_realm);
        }

        result
    }

    /// Call a function value as a constructor (native or closure).
    ///
    /// This sets the construct flag so `return` uses the constructed `this`
    /// when the constructor returns a non-object.
    pub fn call_function_construct(
        &self,
        ctx: &mut VmContext,
        func: &Value,
        this_value: Value,
        args: &[Value],
    ) -> VmResult<Value> {
        // Check __non_constructor flag (ES2023 §17: built-in methods are not constructors)
        if let Some(func_obj) = func.as_object()
            && let Some(crate::object::PropertyDescriptor::Data { value, .. }) = func_obj
                .get_own_property_descriptor(&crate::object::PropertyKey::string(
                    "__non_constructor",
                ))
            && value.as_boolean() == Some(true)
        {
            return Err(VmError::type_error("not a constructor"));
        }

        // Check if it's a native function
        if let Some(native_fn) = func.as_native_function() {
            let realm_id = self.realm_id_for_function(ctx, func);

            // If caller already provided a `this` object (e.g. Reflect.construct),
            // use it. Otherwise create a new `this` with the constructor's prototype.
            let new_obj_value = if this_value.is_object() {
                this_value
            } else {
                let ctor_proto = func
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object())
                    .or_else(|| self.default_object_prototype_for_constructor(ctx, func));
                let new_obj = GcRef::new(JsObject::new(
                    ctor_proto.map(Value::object).unwrap_or_else(Value::null),
                ));
                Value::object(new_obj)
            };

            let result = self.call_native_fn_with_realm(
                ctx,
                native_fn,
                &new_obj_value,
                args,
                Some(realm_id),
                true,
            )?;

            // Per spec: if constructor returns an object, use it; otherwise use `this`
            if result.is_object()
                || result.is_data_view()
                || result.is_array_buffer()
                || result.is_typed_array()
            {
                return Ok(result);
            }
            return Ok(new_obj_value);
        }

        // Regular closure call
        let closure = func
            .as_function()
            .ok_or_else(|| VmError::type_error("not a function"))?;

        // Save current state
        let was_running = ctx.is_running();
        let prev_stack_depth = ctx.stack_depth();

        // Get function info
        let func_info = closure
            .module
            .function(closure.function_index)
            .ok_or_else(|| VmError::internal("function not found"))?;

        // Set up the call — handle rest parameters
        let mut call_args: SmallVec<[Value; 8]> = SmallVec::from_slice(args);
        if func_info.flags.has_rest {
            let param_count = func_info.param_count as usize;
            let rest_args: Vec<Value> = if call_args.len() > param_count {
                call_args.drain(param_count..).collect()
            } else {
                Vec::new()
            };
            let rest_arr = crate::gc::GcRef::new(crate::object::JsObject::array(rest_args.len()));
            if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                && let Some(array_proto) = array_obj
                    .get(&crate::object::PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
            {
                rest_arr.set_prototype(Value::object(array_proto));
            }
            for (i, arg) in rest_args.into_iter().enumerate() {
                let _ = rest_arr.set(crate::object::PropertyKey::Index(i as u32), arg);
            }
            call_args.push(Value::object(rest_arr));
        }

        let argc = call_args.len() as u16;
        ctx.set_pending_args(call_args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(closure.upvalues.clone());
        // Propagate home_object from closure to the new call frame
        if let Some(ref ho) = closure.home_object {
            ctx.set_pending_home_object(*ho);
        }

        let realm_id = self.realm_id_for_function(ctx, func);
        ctx.set_pending_realm_id(realm_id);
        // Store callee value for arguments.callee
        ctx.set_pending_callee_value(*func);
        ctx.register_module(&closure.module);
        ctx.push_frame(
            closure.function_index,
            closure.module.module_id,
            func_info.local_count,
            Some(0), // Return register (unused, we get result from Return)
            true,    // Construct call
            closure.is_async,
            argc,
        )?;
        ctx.set_running(true);

        // Execute until this call returns
        let result = loop {
            let frame = match ctx.current_frame() {
                Some(f) => f,
                None => return Err(VmError::internal("no frame")),
            };

            let current_module = Arc::clone(ctx.module_table.get(frame.module_id));
            let construct_func_index = frame.function_index;
            let func = match current_module.function(construct_func_index) {
                Some(f) => f,
                None => return Err(VmError::internal("function not found")),
            };

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.read().len() {
                // Check if we've returned to the original depth
                if ctx.stack_depth() <= prev_stack_depth {
                    break Value::undefined();
                }
                ctx.pop_frame_discard();
                continue;
            }

            let instruction = &func.instructions.read()[frame.pc];

            match self.execute_instruction(instruction, &current_module, ctx) {
                Ok(()) => {}
                Err(e) => {
                    // Pop the frame we pushed and unwind to original depth
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame_discard();
                    }
                    ctx.set_running(was_running);
                    return Err(e);
                }
            }

            if let Some(action) = ctx.take_dispatch_action() {
                match action {
                    DispatchAction::Jump(offset) => {
                        if offset < 0 {
                            let newly_hot = func.record_back_edge();
                            if newly_hot {
                                func.mark_hot();
                            }
                        }
                        ctx.jump(offset);
                    }
                    DispatchAction::Return(value) => {
                        let (return_reg, is_construct, construct_this, is_async) = {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            (
                                frame.return_register,
                                frame.flags.is_construct(),
                                frame.this_value,
                                frame.flags.is_async(),
                            )
                        };
                        let value = if is_construct && !value.is_object() {
                            construct_this
                        } else if is_async {
                            self.create_js_promise(ctx, JsPromise::resolved(value))
                        } else {
                            value
                        };
                        // Check if we've returned to the original depth
                        if ctx.stack_depth() <= prev_stack_depth + 1 {
                            ctx.pop_frame_discard();
                            break value;
                        }
                        // Handle return from nested call
                        ctx.pop_frame_discard();
                        if let Some(reg) = return_reg {
                            ctx.set_register(reg, value);
                        } else {
                            ctx.set_register(0, value);
                        }
                    }
                    DispatchAction::Call {
                        func_index,
                        module_id,
                        argc,
                        return_reg,
                        is_construct,
                        is_async,
                        upvalues,
                    } => {
                        self.dispatch_call(
                            ctx,
                            func_index,
                            module_id,
                            argc,
                            return_reg,
                            is_construct,
                            is_async,
                            upvalues,
                        )?;
                    }
                    DispatchAction::TailCall {
                        func_index,
                        module_id,
                        argc,
                        return_reg,
                        is_async,
                        upvalues,
                    } => {
                        ctx.pop_frame_discard();
                        self.dispatch_tail_call(
                            ctx, func_index, module_id, argc, return_reg, is_async, upvalues,
                        )?;
                    }
                    DispatchAction::Suspend { .. } => {
                        // Can't handle suspension in direct call, return undefined
                        break Value::undefined();
                    }
                    DispatchAction::Yield { .. } => {
                        // Can't handle yield in direct call, return undefined
                        break Value::undefined();
                    }
                    DispatchAction::Throw(error) => {
                        // Pop the frame we pushed and unwind to original depth
                        while ctx.stack_depth() > prev_stack_depth {
                            ctx.pop_frame_discard();
                        }
                        ctx.set_running(was_running);
                        return Err(VmError::exception(error));
                    }
                }
            } else {
                ctx.advance_pc();
            }
        };

        ctx.set_running(was_running);
        Ok(result)
    }

    /// Call a native function as a constructor (via `new`).
    /// Sets `NativeContext::is_construct()` to true.
    pub(super) fn call_native_fn_construct(
        &self,
        ctx: &mut VmContext,
        native_fn: &crate::value::NativeFn,
        this_value: &Value,
        args: &[Value],
    ) -> VmResult<Value> {
        self.call_native_fn_with_realm(ctx, native_fn, this_value, args, None, true)
    }

    /// Handle a function call value (native or closure)
    pub(super) fn try_fast_path_array_method(
        &self,
        ctx: &mut VmContext,
        method_value: &Value,
        receiver: &Value,
        argc: u16,
        args_start_reg: u16,
        dst_reg: u16,
    ) -> Result<bool, VmError> {
        if let Some(fn_obj) = method_value.native_function_object() {
            let flags = fn_obj.flags.borrow();
            if flags.is_array_push {
                if let Some(receiver_obj) = receiver.as_object()
                    && receiver_obj.is_array()
                    && !receiver_obj.is_dictionary_mode()
                {
                    let mut last_len = receiver_obj.array_length();
                    for i in 0..argc {
                        let arg = *ctx.get_register(args_start_reg + i);
                        last_len = receiver_obj.array_push(arg);
                    }
                    ctx.set_register(dst_reg, Value::number(last_len as f64));
                    return Ok(true);
                }
            } else if flags.is_array_pop
                && let Some(receiver_obj) = receiver.as_object()
                && receiver_obj.is_array()
                && !receiver_obj.is_dictionary_mode()
            {
                let val = receiver_obj.array_pop();
                ctx.set_register(dst_reg, val);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn handle_call_value(
        &self,
        ctx: &mut VmContext,
        func_value: &Value,
        this_value: Value,
        args: Vec<Value>,
        return_reg: u16,
    ) -> VmResult<()> {
        let mut current_func = *func_value;
        let mut current_this = this_value;
        let mut current_args = args;

        // 1. Unwrap all nested bound functions
        while let Some(obj) = current_func.as_object() {
            if let Some(bound_fn) = obj.get(&PropertyKey::string("__boundFunction__")) {
                let raw_this_arg = obj
                    .get(&PropertyKey::string("__boundThis__"))
                    .unwrap_or_else(Value::undefined);
                if raw_this_arg.is_null() || raw_this_arg.is_undefined() {
                    current_this = Value::object(ctx.global());
                } else {
                    current_this = raw_this_arg;
                };

                if let Some(bound_args_val) = obj.get(&PropertyKey::string("__boundArgs__"))
                    && let Some(args_obj) = bound_args_val.as_object()
                {
                    let len = if let Some(len_val) = args_obj.get(&PropertyKey::string("length")) {
                        len_val.as_int32().unwrap_or(0) as usize
                    } else {
                        0
                    };
                    let mut new_args = Vec::with_capacity(len + current_args.len());
                    for i in 0..len {
                        new_args.push(
                            args_obj
                                .get(&PropertyKey::Index(i as u32))
                                .unwrap_or_else(Value::undefined),
                        );
                    }
                    new_args.extend(current_args);
                    current_args = new_args;
                }
                current_func = bound_fn;
            } else {
                break;
            }
        }

        // 2. Handle native functions
        if let Some(native_fn) = current_func.as_native_function() {
            let realm_id = self.realm_id_for_function(ctx, &current_func);
            // Native function execution
            match self.call_native_fn_with_realm(
                ctx,
                native_fn,
                &current_this,
                &current_args,
                Some(realm_id),
                false,
            ) {
                Ok(result) => {
                    ctx.set_register(return_reg, result);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }

        // 3. Handle closures
        if let Some(closure) = current_func.as_function() {
            if closure.is_generator {
                // Use generator prototype from the function's realm, not the caller's.
                let realm_id = closure
                    .object
                    .get(&PropertyKey::string("__realm_id__"))
                    .and_then(|v| v.as_int32())
                    .map(|id| id as u32)
                    .unwrap_or_else(|| ctx.realm_id());
                let proto = ctx
                    .realm_intrinsics(realm_id)
                    .map(|intrinsics| {
                        if closure.is_async {
                            intrinsics.async_generator_prototype
                        } else {
                            intrinsics.generator_prototype
                        }
                    })
                    .or_else(|| {
                        if closure.is_async {
                            ctx.async_generator_prototype_intrinsic()
                        } else {
                            ctx.generator_prototype_intrinsic()
                        }
                    });

                // Create the generator's internal object
                let gen_obj = GcRef::new(JsObject::new(
                    proto.map(Value::object).unwrap_or_else(Value::null),
                ));

                let generator = JsGenerator::new(
                    closure.function_index,
                    Arc::clone(&closure.module),
                    closure.upvalues.clone(),
                    current_args,
                    current_this,
                    false, // is_construct
                    closure.is_async,
                    realm_id,
                    gen_obj,
                );
                // Store callee value for arguments.callee in sloppy mode generators
                generator.set_callee_value(current_func);
                ctx.set_register(return_reg, Value::generator(generator));
                return Ok(());
            }

            let argc = current_args.len() as u8;
            let realm_id = self.realm_id_for_function(ctx, &current_func);
            ctx.set_pending_realm_id(realm_id);
            ctx.set_pending_this(current_this);
            ctx.set_pending_args_from_vec(current_args);
            // Propagate home_object from closure to the new call frame
            if let Some(ref ho) = closure.home_object {
                ctx.set_pending_home_object(*ho);
            }
            // Store callee value for arguments.callee
            ctx.set_pending_callee_value(current_func);
            ctx.dispatch_action = Some(DispatchAction::Call {
                func_index: closure.function_index,
                module_id: closure.module.module_id,
                argc,
                return_reg,
                is_construct: false,
                is_async: closure.is_async,
                upvalues: closure.upvalues.clone(),
            });
            return Ok(());
        }

        Err(VmError::type_error("Value is not a function"))
    }

    /// Observe the type of a value for type feedback collection
    #[inline]
    pub(crate) fn observe_value_type(type_flags: &mut TypeFlags, value: &Value) {
        if value.is_undefined() {
            type_flags.observe_undefined();
        } else if value.is_null() {
            type_flags.observe_null();
        } else if value.is_boolean() {
            type_flags.observe_boolean();
        } else if value.is_int32() {
            type_flags.observe_int32();
        } else if value.is_number() {
            type_flags.observe_number();
        } else if value.is_string() {
            type_flags.observe_string();
        } else if value.is_function() {
            type_flags.observe_function();
        } else if value.is_object() {
            type_flags.observe_object();
        }
    }

    /// Add operation (handles string concatenation)
    pub(crate) fn op_add(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
    ) -> VmResult<Value> {
        let left_prim = self.to_primitive(ctx, left, PreferredType::Default)?;
        let right_prim = self.to_primitive(ctx, right, PreferredType::Default)?;

        // String concatenation
        if left_prim.is_string() || right_prim.is_string() {
            let l_js_str = if let Some(s) = left_prim.as_string() {
                s
            } else {
                let s = self.to_string_value(ctx, &left_prim)?;
                JsString::intern(&s)
            };

            let r_js_str = if let Some(s) = right_prim.as_string() {
                s
            } else {
                let s = self.to_string_value(ctx, &right_prim)?;
                JsString::intern(&s)
            };

            return Ok(Value::string(JsString::concat_gc(l_js_str, r_js_str)));
        }

        let left_bigint = self.bigint_value(&left_prim)?;
        let right_bigint = self.bigint_value(&right_prim)?;
        if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
            let result = left_bigint + right_bigint;
            return Ok(Value::bigint(result.to_string()));
        }

        if left_prim.is_bigint() || right_prim.is_bigint() {
            return Err(VmError::type_error("Cannot mix BigInt and other types"));
        }

        // Numeric addition
        let left_num = self.to_number_value(ctx, &left_prim)?;
        let right_num = self.to_number_value(ctx, &right_prim)?;
        Ok(Value::number(left_num + right_num))
    }

    pub(crate) fn op_sub(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
    ) -> VmResult<Value> {
        let left_num = self.to_numeric(ctx, left)?;
        let right_num = self.to_numeric(ctx, right)?;
        match (left_num, right_num) {
            (Numeric::BigInt(l), Numeric::BigInt(r)) => Ok(Value::bigint((l - r).to_string())),
            (Numeric::Number(l), Numeric::Number(r)) => Ok(Value::number(l - r)),
            _ => Err(VmError::type_error("Cannot mix BigInt and other types")),
        }
    }

    pub(crate) fn op_mul(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
    ) -> VmResult<Value> {
        let left_num = self.to_numeric(ctx, left)?;
        let right_num = self.to_numeric(ctx, right)?;
        match (left_num, right_num) {
            (Numeric::BigInt(l), Numeric::BigInt(r)) => Ok(Value::bigint((l * r).to_string())),
            (Numeric::Number(l), Numeric::Number(r)) => Ok(Value::number(l * r)),
            _ => Err(VmError::type_error("Cannot mix BigInt and other types")),
        }
    }

    pub(crate) fn op_div(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
    ) -> VmResult<Value> {
        let left_num = self.to_numeric(ctx, left)?;
        let right_num = self.to_numeric(ctx, right)?;
        match (left_num, right_num) {
            (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                if r.is_zero() {
                    return Err(VmError::range_error("Division by zero"));
                }
                Ok(Value::bigint((l / r).to_string()))
            }
            (Numeric::Number(l), Numeric::Number(r)) => Ok(Value::number(l / r)),
            _ => Err(VmError::type_error("Cannot mix BigInt and other types")),
        }
    }

    /// Internal method dispatch helper for spread
    pub(super) fn dispatch_method_spread(
        &self,
        ctx: &mut VmContext,
        method_value: &Value,
        receiver: Value,
        spread_arr: &Value,
        return_reg: u16,
    ) -> VmResult<()> {
        // Collect all arguments from the spread array
        let mut args = Vec::new();
        if let Some(obj) = spread_arr.as_object() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_int32())
                .unwrap_or(0);
            for i in 0..len {
                args.push(
                    obj.get(&PropertyKey::Index(i as u32))
                        .unwrap_or_else(Value::undefined),
                );
            }
        }

        if let Some(native_fn) = method_value.as_native_function() {
            let result = self.call_native_fn(ctx, native_fn, &receiver, &args)?;
            ctx.set_register(return_reg, result);
            return Ok(());
        }

        if let Some(closure) = method_value.as_function() {
            let argc = args.len() as u8;
            ctx.set_pending_args_from_vec(args);
            ctx.set_pending_this(receiver);

            ctx.dispatch_action = Some(DispatchAction::Call {
                func_index: closure.function_index,
                module_id: closure.module.module_id,
                argc,
                return_reg,
                is_construct: false,
                is_async: closure.is_async,
                upvalues: closure.upvalues.clone(),
            });
            return Ok(());
        }

        Err(VmError::type_error("method is not a function"))
    }

    /// Convert value to string
    pub(super) fn to_string(&self, value: &Value) -> String {
        match value.type_of() {
            "undefined" => "undefined".to_string(),
            "null" => "null".to_string(),
            "boolean" => {
                if value.to_boolean() {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            "number" => {
                if let Some(n) = value.as_number() {
                    crate::globals::js_number_to_string(n)
                } else {
                    "NaN".to_string()
                }
            }
            "string" => {
                if let Some(s) = value.as_string() {
                    s.as_str().to_string()
                } else {
                    String::new()
                }
            }
            "bigint" => {
                if let Some(b) = value.as_bigint() {
                    b.value.clone()
                } else {
                    "0".to_string()
                }
            }
            _ => {
                if let Some(obj) = value.as_object() {
                    let name = obj
                        .get(&PropertyKey::string("name"))
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string());
                    let message = obj
                        .get(&PropertyKey::string("message"))
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string());

                    match (name, message) {
                        (Some(n), Some(m)) => format!("{}: {}", n, m),
                        (Some(n), None) => n,
                        (None, Some(m)) => m,
                        (None, None) => "[object Object]".to_string(),
                    }
                } else if value.is_function() || value.is_native_function() {
                    // Functions: toString should return source or "function X() { [native code] }"
                    "function () { [native code] }".to_string()
                } else {
                    "[object Object]".to_string()
                }
            }
        }
    }
}

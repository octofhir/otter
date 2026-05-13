//! Call and construct opcode helpers.
//!
//! Stack-modifying call bytecodes decode variadic executable operands, prepare
//! frames, and may immediately invoke native/proxy/constructor paths. Keeping
//! that machinery here lets `lib.rs` stay closer to a dispatch map.
//!
//! # Contents
//! - Ordinary call entry and shared callable invocation.
//! - Constructor call entry and receiver/prototype setup.
//! - Spread and explicit-`this` call forms.
//!
//! # Invariants
//! - Call-site helpers advance the caller PC before pushing or synchronously
//!   invoking another frame.
//! - `invoke` remains the shared call path for bytecode, closures, native
//!   callables, bound functions, class constructors, and proxies.
//! - Constructor dispatch preserves `new.target` and receiver substitution
//!   invariants used by `pop_frame`.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    AsyncFrameState, ExecutionContext, Frame, Interpreter, NativeCallInfo, NativeCtx, Value,
    VmError, abstract_ops, constructor_return_is_object, is_constructor_runtime,
    native_to_vm_error, operand_decode::register_operand, promise_dispatch, read_register,
    write_register,
};

impl Interpreter {
    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots and `this` bound
    /// to `Value::Undefined` (foundation strict default).
    pub(crate) fn do_call(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };

        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, context, &callee, Value::Undefined, args, dst)
    }

    /// Invoke `callee` with the explicit receiver `this_value` and
    /// the given argument list. Centralizes the BoundFunction
    /// unwrapping, closure `bound_this` override, and frame push so
    /// every call opcode (`Op::Call`, `Op::CallWithThis`,
    /// `Op::CallMethodValue`) shares one path.
    ///
    /// `dst` is the **caller's** register that should receive the
    /// completion value when the callee returns. `caller_pc` must
    /// already be advanced before this call so the post-pop
    /// dispatch resumes after the originating instruction.
    pub(crate) fn invoke(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        // Walk through any number of `bind` layers, accumulating
        // their bound arguments and overriding `this_value` with
        // the innermost `bound_this`. The loop bound matches the
        // JS-call stack-depth limit so a pathological self-bound
        // chain still surfaces as `StackOverflow` rather than
        // unbounded recursion.
        let mut current = callee.clone();
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            match current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound_args.len() + effective_args.len());
                    combined.extend(bound_args);
                    combined.extend(effective_args);
                    effective_this = bound_this;
                    effective_args = combined;
                    current = target;
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor(&self.gc_heap).clone();
                }
                _ => break,
            }
        }
        // Native callables short-circuit the frame push: invoke
        // the closure inline, write the result into the caller's
        // dst, and advance pc on the caller frame. No stack frame
        // is created — the closure cannot itself push frames.
        if let Value::Object(obj) = &current
            && let Some(Value::NativeFunction(native)) =
                crate::object::call_native(*obj, &self.gc_heap)
        {
            let call = native.call_target(&self.gc_heap);
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call_info = NativeCallInfo::call(effective_this.clone());
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        if let Value::NativeFunction(native) = &current {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                let result =
                    self.run_vm_intrinsic_sync(context, intrinsic, effective_this, effective_args)?;
                let top_idx = stack.len() - 1;
                write_register(&mut stack[top_idx], dst, result)?;
                return Ok(());
            }
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call_info = NativeCallInfo::call(effective_this.clone());
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // §28.2.4.13 Proxy.[[Call]] — delegate to the `apply`
        // trap when present; otherwise call through to the
        // target as a function.
        if let Value::Proxy(p) = &current {
            let proxy = p.clone();
            let argv_array =
                crate::array::from_elements(&mut self.gc_heap, effective_args.iter().cloned())?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                effective_this.clone(),
                Value::Array(argv_array),
            ];
            let result = match self.invoke_proxy_trap(context, &proxy, "apply", trap_args)? {
                Some(v) => v,
                None => {
                    // Fall through to the target's [[Call]] —
                    // `proxy.target()` returns the original Value,
                    // which may be a callable directly.
                    let underlying = proxy.target();
                    self.run_callable_sync(context, &underlying, effective_this, effective_args)?
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        let (function_id, parent_upvalues, this_for_callee) = match current {
            Value::Function { function_id } => {
                (function_id, std::rc::Rc::from(Vec::new()), effective_this)
            }
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            } => {
                let this_value = match bound_this {
                    Some(t) => *t,
                    None => effective_this,
                };
                (function_id, upvalues, this_value)
            }
            _ => return Err(VmError::NotCallable),
        };

        if stack.len() as u32 >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // Async-call entry path (spec §27.7.5.1): synthesise a
        // fresh pending result promise, write it into the caller's
        // `dst` register *now* so the call expression's value is
        // visible synchronously, and park the new frame with
        // `return_register = None` so its eventual completion
        // settles the promise instead of writing back.
        let (return_register, async_state) = if function.is_async {
            let result_promise = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending(&mut self.gc_heap)?;
            let promise_value = Value::Promise(result_promise);
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, promise_value)?;
            (None, Some(AsyncFrameState { result_promise }))
        } else {
            (Some(dst), None)
        };
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call(function, this_for_callee)?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_for_callee,
        );
        new_frame.async_state = async_state;
        // Bind parameters: extra args are dropped, missing args
        // stay `Value::Undefined` (matches JS semantics).
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        // Snapshot the full argv when the callee body references
        // `arguments`. Cloning is cheap because effective_args is a
        // SmallVec; the snapshot is consumed exactly once by
        // `Op::CollectArguments`.
        if function.needs_arguments {
            new_frame.incoming_args = effective_args.iter().cloned().collect();
        }
        let mut iter = effective_args.into_iter();
        for i in 0..bind_count {
            let value = iter.next().expect("bind_count <= len");
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        // Stash the trailing args for `Op::CollectRest`. Only the
        // rest-aware callees pay the allocation; everyone else
        // leaves `rest_args` empty as initialised.
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = iter.collect();
        }
        // §27.5 Generator-call entry: instead of pushing the frame
        // onto the dispatch stack, hand the caller a paused
        // [`Value::Generator`] handle that owns the prepared frame.
        // The body only runs when `.next()` resumes it.
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let gen_handle = crate::generator::JsGenerator::new(&mut self.gc_heap, new_frame)?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            // Backlink the generator into the frame so `Op::Yield`
            // can find its owner once execution starts.
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, Value::Generator(gen_handle))?;
            return Ok(());
        }
        stack.push(new_frame);
        Ok(())
    }

    /// Handle `Op::New`: allocate a fresh receiver, set its
    /// `[[Prototype]]` to `callee.prototype` (when present), and
    /// invoke the callee with `this = receiver`. The caller's `dst`
    /// register receives either the constructor's returned object
    /// or the freshly allocated receiver — `pop_frame` performs
    /// that swap so the unwind path is uniform across call shapes.
    pub(crate) fn do_construct(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    pub(crate) fn do_construct_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let args_value = read_register(&stack[top_idx], args_reg)?.clone();
        let arr = match args_value {
            Value::Array(a) => a,
            _ => return Err(VmError::TypeMismatch),
        };
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(arr, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    pub(crate) fn dispatch_construct(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let mut callee = callee;
        let mut new_target = callee.clone();
        let mut args = args;
        let mut hops: u32 = 0;
        while let Value::BoundFunction(bound) = &callee {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            hops += 1;
            let (target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
            let mut combined: SmallVec<[Value; 8]> =
                SmallVec::with_capacity(bound_args.len() + args.len());
            combined.extend(bound_args);
            combined.extend(args);
            if abstract_ops::same_value(&callee, &new_target) {
                new_target = target.clone();
            }
            callee = target;
            args = combined;
        }
        // §28.2.4.14 Proxy.[[Construct]] — `new <proxy>(args)`
        // routes through the `construct` trap when present;
        // otherwise delegates to the target.
        if let Value::Proxy(p) = &callee {
            let proxy = p.clone();
            let argv_array = crate::array::from_elements(&mut self.gc_heap, args.iter().cloned())?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                Value::Array(argv_array),
                Value::Proxy(proxy.clone()),
            ];
            let result = match self.invoke_proxy_trap(context, &proxy, "construct", trap_args)? {
                Some(v) => {
                    // §10.5.13 step 9 — trap result must be an Object;
                    // primitive returns surface as TypeError.
                    if !constructor_return_is_object(&v) {
                        return Err(VmError::TypeError {
                            message: "Proxy construct trap returned non-object".to_string(),
                        });
                    }
                    v
                }
                None => {
                    // Fall through to [[Construct]] on the underlying
                    // target via `run_construct_sync`, which honours
                    // bound/proxy/native paths and re-checks the
                    // constructor-return invariants.
                    self.run_construct_sync(context, &proxy.target(), callee.clone(), args)?
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // Allocate receiver and link its prototype before pushing
        // the new frame. The constructor might mutate the receiver
        // immediately, so the prototype link must already be in
        // place.
        let proto = self.construct_prototype_for_callee(context, &new_target)?;
        let receiver = crate::object::alloc_object(&mut self.gc_heap)?;
        if let Some(proto) = proto {
            crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        }
        let this_value = Value::Object(receiver);
        // Built-in constructor objects (`Number`, `Boolean`, …)
        // surface as a `Value::Object` with an internal native
        // constructor slot. Promote to the native-function construct
        // path so the JS-visible callee can also carry own
        // properties (statics + `prototype`) without leaking the
        // implementation slot through reflection.
        if let Value::Object(obj) = &callee
            && let Some(Value::NativeFunction(native)) =
                crate::object::constructor_native(*obj, &self.gc_heap)
        {
            let argv: Vec<Value> = args.into_iter().collect();
            let call = native.call_target(&self.gc_heap);
            let call_info = NativeCallInfo::construct(this_value.clone(), Some(new_target.clone()));
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            let constructed = if constructor_return_is_object(&result) {
                // Spec §10.1.13 step 5 — non-undefined "object-like"
                // returns are honoured. Builtin constructors such as
                // `Array` produce a `Value::Array` (still an object
                // per ECMA-262), so the foundation also forwards it.
                result
            } else {
                this_value
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        // `Value::NativeFunction` carries `[[Construct]]` whenever
        // the runtime needs the callable to behave as a constructor
        // (e.g. `new Number(x)`). The native callback inspects
        // `NativeCtx::is_construct_call()` to differentiate the
        // call shape.
        if let Value::NativeFunction(native) = &callee {
            let argv: Vec<Value> = args.into_iter().collect();
            let call = native.call_target(&self.gc_heap);
            let call_info = NativeCallInfo::construct(this_value.clone(), Some(new_target.clone()));
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            let constructed = if constructor_return_is_object(&result) {
                // Spec §10.1.13 step 5 — non-undefined "object-like"
                // returns are honoured. Builtin constructors such as
                // `Array` produce a `Value::Array` (still an object
                // per ECMA-262), so the foundation also forwards it.
                result
            } else {
                this_value
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        if let Value::ClassConstructor(class) = &callee
            && let Value::NativeFunction(native) = &class.ctor(&self.gc_heap)
        {
            let argv: Vec<Value> = args.into_iter().collect();
            let call = native.call_target(&self.gc_heap);
            let call_info = NativeCallInfo::construct(this_value.clone(), Some(new_target.clone()));
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            let constructed = if constructor_return_is_object(&result) {
                // Spec §10.1.13 step 5 — non-undefined "object-like"
                // returns are honoured. Builtin constructors such as
                // `Array` produce a `Value::Array` (still an object
                // per ECMA-262), so the foundation also forwards it.
                result
            } else {
                this_value
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        self.invoke(stack, context, &callee, this_value, args, dst)?;
        // The pushed frame is now on top; mark it so `pop_frame`
        // can substitute the receiver for any non-object return.
        if let Some(top) = stack.last_mut() {
            top.construct_target = Some(receiver);
            top.new_target = Some(new_target);
        }
        Ok(())
    }

    pub(crate) fn construct_prototype_for_callee(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        match callee {
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                match self.function_property_get(context, *function_id, "prototype")? {
                    proto if constructor_return_is_object(&proto) => Ok(Some(proto)),
                    _ => Ok(None),
                }
            }
            Value::ClassConstructor(c) => Ok(Some(Value::Object(c.prototype(&self.gc_heap)))),
            Value::Object(obj) => Ok(match crate::object::get(*obj, &self.gc_heap, "prototype") {
                Some(proto) if constructor_return_is_object(&proto) => Some(proto),
                _ => None,
            }),
            Value::BoundFunction(b) => {
                let (target, _, _) = b.parts(&self.gc_heap);
                self.construct_prototype_for_callee(context, &target)
            }
            Value::NativeFunction(_) => Ok(None),
            _ => Ok(None),
        }
    }

    /// Handle `Op::CallSpread`: read the args array, fan it out
    /// into the standard call path. The receiver register holds
    /// the explicit `this` value (foundation lowers free spread
    /// calls with `this = undefined`).
    pub(crate) fn do_call_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let this_value = read_register(&stack[top_idx], this_reg)?.clone();
        let args_array = match read_register(&stack[top_idx], args_reg)? {
            Value::Array(a) => *a,
            _ => return Err(VmError::TypeMismatch),
        };
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(args_array, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, context, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallWithThis`: same as `do_call` but the call
    /// site supplies an explicit `this` register. Used by
    /// `Function.prototype.call` lowering and the array-literal
    /// path of `Function.prototype.apply`.
    pub(crate) fn do_call_with_this(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let this_value = read_register(&stack[top_idx], this_reg)?.clone();
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(4 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, context, &callee, this_value, args, dst)
    }
    /// Synchronously invoke `callee(args)` with the given `this` and
    /// return the completion value.
    ///
    /// # Algorithm
    /// 1. NativeFunction callees run inline — the foundation native
    ///    surface is `Fn`, so calling them here is just a function
    ///    pointer hop with `&mut self` access.
    /// 2. BoundFunction layers are unwrapped iteratively, prepending
    ///    bound args and replacing `this_value` with `bound_this`.
    /// 3. Bytecode / closure callees push a frame whose
    ///    `return_register` is `None`, which makes
    ///    [`Self::dispatch_loop`] return the completion value when
    ///    the frame pops.
    ///
    /// Used by collection `forEach` and other host-driven iteration
    /// helpers.
    pub fn run_callable_sync(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let mut current = callee.clone();
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            match current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound_args.len() + effective_args.len());
                    combined.extend(bound_args);
                    combined.extend(effective_args);
                    effective_this = bound_this;
                    effective_args = combined;
                    current = target;
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor(&self.gc_heap).clone();
                }
                // §10.5.12 Proxy [[Call]] — dispatch `apply` trap or
                // fall through to target.[[Call]] when the trap is
                // absent. Target may itself be a Proxy, hence the
                // surrounding loop. §10.5.1 revocation check.
                // <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-call-thisargument-argumentslist>
                Value::Proxy(proxy) => {
                    if proxy.is_revoked() {
                        return Err(VmError::TypeError {
                            message: "Cannot perform 'apply' on a proxy that has been revoked"
                                .to_string(),
                        });
                    }
                    hops += 1;
                    let handler = proxy.handler();
                    let trap_value = crate::object::get(handler, &self.gc_heap, "apply");
                    match trap_value {
                        Some(trap) if self.is_callable_runtime(&trap) => {
                            let argv_array = crate::array::from_elements(
                                &mut self.gc_heap,
                                effective_args.iter().cloned(),
                            )?;
                            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                                proxy.target(),
                                effective_this.clone(),
                                Value::Array(argv_array),
                            ];
                            return self.run_callable_sync(
                                context,
                                &trap,
                                Value::Object(handler),
                                trap_args,
                            );
                        }
                        Some(Value::Undefined) | Some(Value::Null) | None => {
                            current = proxy.target();
                        }
                        Some(_) => {
                            return Err(VmError::TypeError {
                                message: "Proxy apply trap is not callable".to_string(),
                            });
                        }
                    }
                }
                _ => break,
            }
        }
        if let Value::Object(obj) = &current
            && let Some(Value::NativeFunction(native)) =
                crate::object::call_native(*obj, &self.gc_heap)
        {
            let call = native.call_target(&self.gc_heap);
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call_info = NativeCallInfo::call(effective_this.clone());
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            return call.invoke(&mut ctx, &argv).map_err(native_to_vm_error);
        }
        if let Value::NativeFunction(native) = &current {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                return self.run_vm_intrinsic_sync(
                    context,
                    intrinsic,
                    effective_this,
                    effective_args,
                );
            }
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call_info = NativeCallInfo::call(effective_this.clone());
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            return call.invoke(&mut ctx, &argv).map_err(native_to_vm_error);
        }
        let (function_id, parent_upvalues, this_for_callee) = match current {
            Value::Function { function_id } => {
                (function_id, std::rc::Rc::from(Vec::new()), effective_this)
            }
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            } => {
                let this_value = match bound_this {
                    Some(t) => *t,
                    None => effective_this,
                };
                (function_id, upvalues, this_value)
            }
            _ => return Err(VmError::NotCallable),
        };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call(function, this_for_callee)?;
        let mut inner: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut new_frame =
            Frame::with_exec_return_upvalues_and_this(function, None, upvalues, this_for_callee);
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        if function.needs_arguments {
            new_frame.incoming_args = effective_args.iter().cloned().collect();
        }
        let mut arg_iter = effective_args.into_iter();
        for i in 0..bind_count {
            let v = arg_iter.next().expect("bind_count <= len");
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = v;
        }
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = arg_iter.collect();
        }
        inner.push(new_frame);
        self.dispatch_loop(context, &mut inner)
    }

    /// Synchronously perform `Construct(target, args, newTarget)`.
    ///
    /// This mirrors the `Op::New` user-function entry path but
    /// returns the completion directly for builtins such as
    /// `Reflect.construct`. Bound functions are unwrapped with the
    /// ECMA-262 `[[Construct]]` newTarget rewrite: constructing a
    /// bound function as itself exposes the bound target as
    /// `new.target` inside the target body.
    pub(crate) fn run_construct_sync(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let mut current = target.clone();
        let mut effective_new_target = new_target;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            match &current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let (next_target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound_args.len() + effective_args.len());
                    combined.extend(bound_args);
                    combined.extend(effective_args);
                    if abstract_ops::same_value(&current, &effective_new_target) {
                        effective_new_target = next_target.clone();
                    }
                    current = next_target;
                    effective_args = combined;
                }
                // §10.5.13 Proxy [[Construct]] — dispatch `construct`
                // trap or fall through to target.[[Construct]]. Target
                // may be another Proxy, hence the loop.
                // <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-construct-argumentslist-newtarget>
                Value::Proxy(proxy) => {
                    if proxy.is_revoked() {
                        return Err(VmError::TypeError {
                            message: "Cannot perform 'construct' on a proxy that has been revoked"
                                .to_string(),
                        });
                    }
                    hops += 1;
                    let handler = proxy.handler();
                    let trap_value = crate::object::get(handler, &self.gc_heap, "construct");
                    match trap_value {
                        Some(trap) if self.is_callable_runtime(&trap) => {
                            let target_value = proxy.target();
                            let argv_array = crate::array::from_elements(
                                &mut self.gc_heap,
                                effective_args.iter().cloned(),
                            )?;
                            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                                target_value,
                                Value::Array(argv_array),
                                effective_new_target.clone(),
                            ];
                            let result = self.run_callable_sync(
                                context,
                                &trap,
                                Value::Object(handler),
                                trap_args,
                            )?;
                            if !constructor_return_is_object(&result) {
                                return Err(VmError::TypeError {
                                    message: "Proxy construct trap returned non-object".to_string(),
                                });
                            }
                            return Ok(result);
                        }
                        Some(Value::Undefined) | Some(Value::Null) | None => {
                            current = proxy.target();
                        }
                        Some(_) => {
                            return Err(VmError::TypeError {
                                message: "Proxy construct trap is not callable".to_string(),
                            });
                        }
                    }
                }
                _ => break,
            }
        }

        let proto = self.construct_prototype_for_callee(context, &effective_new_target)?;
        let receiver = crate::object::alloc_object(&mut self.gc_heap)?;
        if let Some(proto) = proto {
            crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        }
        let this_value = Value::Object(receiver);

        if let Value::Object(obj) = &current
            && let Some(Value::NativeFunction(native)) =
                crate::object::constructor_native(*obj, &self.gc_heap)
        {
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call = native.call_target(&self.gc_heap);
            let call_info =
                NativeCallInfo::construct(this_value.clone(), Some(effective_new_target));
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            return Ok(if constructor_return_is_object(&result) {
                result
            } else {
                this_value
            });
        }
        if let Value::NativeFunction(native) = &current {
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call = native.call_target(&self.gc_heap);
            let call_info =
                NativeCallInfo::construct(this_value.clone(), Some(effective_new_target));
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            return Ok(if constructor_return_is_object(&result) {
                result
            } else {
                this_value
            });
        }
        if let Value::ClassConstructor(class) = &current
            && let Value::NativeFunction(native) = &class.ctor(&self.gc_heap)
        {
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let call = native.call_target(&self.gc_heap);
            let call_info =
                NativeCallInfo::construct(this_value.clone(), Some(effective_new_target));
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            let result = call.invoke(&mut ctx, &argv).map_err(native_to_vm_error)?;
            return Ok(if constructor_return_is_object(&result) {
                result
            } else {
                this_value
            });
        }
        if let Value::ClassConstructor(class) = &current {
            current = class.ctor(&self.gc_heap).clone();
        }

        let (function_id, parent_upvalues) = match current {
            Value::Function { function_id } => (function_id, std::rc::Rc::from(Vec::new())),
            Value::Closure {
                function_id,
                upvalues,
                ..
            } => (function_id, upvalues),
            _ => return Err(VmError::NotCallable),
        };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let mut new_frame =
            Frame::with_exec_return_upvalues_and_this(function, None, upvalues, this_value);
        new_frame.construct_target = Some(receiver);
        new_frame.new_target = Some(effective_new_target);
        if function.needs_arguments {
            new_frame.incoming_args = effective_args.iter().cloned().collect();
        }
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        let mut arg_iter = effective_args.into_iter();
        for i in 0..bind_count {
            let v = arg_iter.next().expect("bind_count <= len");
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = v;
        }
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = arg_iter.collect();
        }
        let mut inner: SmallVec<[Frame; 8]> = SmallVec::new();
        inner.push(new_frame);
        self.dispatch_loop(context, &mut inner)
    }
}

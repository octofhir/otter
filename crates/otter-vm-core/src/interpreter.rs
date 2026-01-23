//! Bytecode interpreter
//!
//! Executes bytecode instructions.

use otter_vm_bytecode::{Instruction, Module};

use crate::context::VmContext;
use crate::error::{VmError, VmResult};
use crate::generator::JsGenerator;
use crate::object::{JsObject, PropertyKey};
use crate::promise::{JsPromise, PromiseState};
use crate::string::JsString;
use crate::value::{Closure, Value};

use std::sync::Arc;

/// The bytecode interpreter
pub struct Interpreter {
    /// Current module being executed
    #[allow(dead_code)]
    current_module: Option<Arc<Module>>,
}

impl Interpreter {
    /// Create a new interpreter
    pub fn new() -> Self {
        Self {
            current_module: None,
        }
    }

    /// Execute a module
    pub fn execute(&mut self, module: &Module, ctx: &mut VmContext) -> VmResult<Value> {
        // Get entry function
        let entry_func = module
            .entry_function()
            .ok_or_else(|| VmError::internal("no entry function"))?;

        // Push initial frame
        ctx.push_frame(module.entry_point, entry_func.local_count, None)?;
        ctx.set_running(true);

        // Execute loop
        let result = self.run_loop(module, ctx);

        ctx.set_running(false);
        result
    }

    /// Main execution loop
    fn run_loop(&mut self, module: &Module, ctx: &mut VmContext) -> VmResult<Value> {
        loop {
            let frame = ctx
                .current_frame()
                .ok_or_else(|| VmError::internal("no frame"))?;
            let func = module
                .function(frame.function_index)
                .ok_or_else(|| VmError::internal("function not found"))?;

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.len() {
                // Implicit return undefined
                if ctx.stack_depth() == 1 {
                    return Ok(Value::undefined());
                }
                ctx.pop_frame();
                continue;
            }

            let instruction = &func.instructions[frame.pc];

            // Execute the instruction
            match self.execute_instruction(instruction, module, ctx)? {
                InstructionResult::Continue => {
                    ctx.advance_pc();
                }
                InstructionResult::Jump(offset) => {
                    ctx.jump(offset);
                }
                InstructionResult::Return(value) => {
                    if ctx.stack_depth() == 1 {
                        return Ok(value);
                    }

                    let return_reg = ctx.current_frame().and_then(|f| f.return_register);
                    ctx.pop_frame();

                    if let Some(reg) = return_reg {
                        ctx.set_register(reg, value);
                    }
                }
                InstructionResult::Call {
                    func_index,
                    argc: _,
                    return_reg,
                } => {
                    ctx.advance_pc(); // Advance before pushing new frame

                    let callee = module
                        .function(func_index)
                        .ok_or_else(|| VmError::internal("callee not found"))?;

                    // Handle rest parameters
                    if callee.flags.has_rest {
                        let mut args = ctx.take_pending_args();
                        let param_count = callee.param_count as usize;

                        // Collect extra arguments into rest array
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };

                        // Create rest array
                        let rest_arr = Arc::new(JsObject::array(rest_args.len()));
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }

                        // Append rest array to args
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.push_frame(func_index, callee.local_count, Some(return_reg))?;
                }
                InstructionResult::Suspend {
                    promise,
                    resume_reg,
                } => {
                    // Store the pending promise state for later resumption
                    // For now, we poll the promise - a real implementation would
                    // integrate with the event loop for true async suspension
                    ctx.advance_pc();

                    // Busy-wait poll the promise (temporary - will integrate with event loop)
                    // In a real implementation, this would yield to the event loop
                    match promise.state() {
                        PromiseState::Fulfilled(value) => {
                            ctx.set_register(resume_reg, value);
                        }
                        PromiseState::Rejected(error) => {
                            return Err(VmError::type_error(format!(
                                "Promise rejected: {:?}",
                                error
                            )));
                        }
                        PromiseState::Pending => {
                            // Promise still pending - return a pending promise as result
                            // In a real async runtime, we'd suspend and resume later
                            return Ok(Value::promise(promise));
                        }
                    }
                }
                InstructionResult::Yield { value } => {
                    // Generator yielded a value
                    // Create an iterator result object { value, done: false }
                    let result = Arc::new(JsObject::new(None));
                    result.set(PropertyKey::string("value"), value);
                    result.set(PropertyKey::string("done"), Value::boolean(false));
                    ctx.advance_pc();
                    return Ok(Value::object(result));
                }
            }
        }
    }

    /// Execute a single instruction
    fn execute_instruction(
        &mut self,
        instruction: &Instruction,
        module: &Module,
        ctx: &mut VmContext,
    ) -> VmResult<InstructionResult> {
        match instruction {
            // ==================== Constants ====================
            Instruction::LoadUndefined { dst } => {
                ctx.set_register(dst.0, Value::undefined());
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadNull { dst } => {
                ctx.set_register(dst.0, Value::null());
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadTrue { dst } => {
                ctx.set_register(dst.0, Value::boolean(true));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadFalse { dst } => {
                ctx.set_register(dst.0, Value::boolean(false));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadInt8 { dst, value } => {
                ctx.set_register(dst.0, Value::int32(*value as i32));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadInt32 { dst, value } => {
                ctx.set_register(dst.0, Value::int32(*value));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadConst { dst, idx } => {
                let constant = module
                    .constants
                    .get(idx.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let value = self.constant_to_value(constant)?;
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            // ==================== Variables ====================
            Instruction::GetLocal { dst, idx } => {
                let value = ctx.get_local(idx.0)?.clone();
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetLocal { idx, src } => {
                let value = ctx.get_register(src.0).clone();
                ctx.set_local(idx.0, value)?;
                Ok(InstructionResult::Continue)
            }

            Instruction::GetGlobal { dst, name } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                let value = ctx.get_global(name_str).unwrap_or_else(Value::undefined);
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetGlobal { name, src } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                let value = ctx.get_register(src.0).clone();
                ctx.set_global(name_str, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadThis { dst } => {
                let this_value = ctx.this_value();
                ctx.set_register(dst.0, this_value);
                Ok(InstructionResult::Continue)
            }

            // ==================== Arithmetic ====================
            Instruction::Add { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = self.op_add(left, right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::Sub { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(left - right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Inc { dst, src } => {
                let val = ctx
                    .get_register(src.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(val + 1.0));
                Ok(InstructionResult::Continue)
            }

            Instruction::Dec { dst, src } => {
                let val = ctx
                    .get_register(src.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(val - 1.0));
                Ok(InstructionResult::Continue)
            }

            Instruction::Mul { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(left * right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Div { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(left / right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Mod { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(left % right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Neg { dst, src } => {
                let value = ctx
                    .get_register(src.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::number(-value));
                Ok(InstructionResult::Continue)
            }

            // ==================== Comparison ====================
            Instruction::Eq { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = self.abstract_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Ne { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = !self.abstract_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::StrictEq { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = self.strict_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::StrictNe { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = !self.strict_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Lt { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::boolean(left < right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Le { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::boolean(left <= right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Gt { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::boolean(left > right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Ge { dst, lhs, rhs } => {
                let left = ctx
                    .get_register(lhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
                let right = ctx
                    .get_register(rhs.0)
                    .as_number()
                    .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

                ctx.set_register(dst.0, Value::boolean(left >= right));
                Ok(InstructionResult::Continue)
            }

            // ==================== Logical ====================
            Instruction::Not { dst, src } => {
                let value = ctx.get_register(src.0).to_boolean();
                ctx.set_register(dst.0, Value::boolean(!value));
                Ok(InstructionResult::Continue)
            }

            // ==================== Type Operations ====================
            Instruction::TypeOf { dst, src } => {
                let type_name = ctx.get_register(src.0).type_of();
                let str_value = Value::string(Arc::new(JsString::new(type_name)));
                ctx.set_register(dst.0, str_value);
                Ok(InstructionResult::Continue)
            }

            // ==================== Control Flow ====================
            Instruction::Jump { offset } => Ok(InstructionResult::Jump(offset.0)),

            Instruction::JumpIfTrue { cond, offset } => {
                if ctx.get_register(cond.0).to_boolean() {
                    Ok(InstructionResult::Jump(offset.0))
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::JumpIfFalse { cond, offset } => {
                if !ctx.get_register(cond.0).to_boolean() {
                    Ok(InstructionResult::Jump(offset.0))
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::JumpIfNullish { src, offset } => {
                if ctx.get_register(src.0).is_nullish() {
                    Ok(InstructionResult::Jump(offset.0))
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            // ==================== Functions ====================
            Instruction::Closure { dst, func } => {
                let closure = Arc::new(Closure {
                    function_index: func.0,
                    upvalues: Vec::new(),
                    is_async: false,
                });
                ctx.set_register(dst.0, Value::function(closure));
                Ok(InstructionResult::Continue)
            }

            Instruction::AsyncClosure { dst, func } => {
                let closure = Arc::new(Closure {
                    function_index: func.0,
                    upvalues: Vec::new(),
                    is_async: true,
                });
                ctx.set_register(dst.0, Value::function(closure));
                Ok(InstructionResult::Continue)
            }

            Instruction::GeneratorClosure { dst, func } => {
                // Create a generator function - when called, it creates a generator object
                let generator_fn = JsGenerator::new(func.0, Vec::new());
                ctx.set_register(dst.0, Value::generator(generator_fn));
                Ok(InstructionResult::Continue)
            }

            Instruction::Call { dst, func, argc } => {
                let func_value = ctx.get_register(func.0).clone();

                // Check if it's a native function first
                if let Some(native_fn) = func_value.as_native_function() {
                    // Collect arguments
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..*argc {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    // Call the native function directly
                    let result = native_fn(&args).map_err(VmError::type_error)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Regular closure call
                let closure = func_value
                    .as_function()
                    .ok_or_else(|| VmError::type_error("not a function"))?;

                // Copy arguments from caller registers (func+1, func+2, ...)
                // to prepare for the new frame
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..*argc {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                // Store args in context for new frame to pick up
                ctx.set_pending_args(args);

                Ok(InstructionResult::Call {
                    func_index: closure.function_index,
                    argc: *argc,
                    return_reg: dst.0,
                })
            }

            Instruction::Construct { dst, func, argc } => {
                let func_value = ctx.get_register(func.0).clone();

                // Check if it's a callable constructor
                if let Some(closure) = func_value.as_function() {
                    // Create a new empty object
                    let new_obj = Arc::new(JsObject::new(None));
                    let new_obj_value = Value::object(new_obj.clone());

                    // Copy arguments from caller registers
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..*argc {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    // Store args and the new object (as `this`) for new frame
                    ctx.set_pending_args(args);

                    // For simplicity, return the new object directly for now
                    // A proper implementation would call the constructor and return `this`
                    ctx.set_register(dst.0, new_obj_value);

                    Ok(InstructionResult::Call {
                        func_index: closure.function_index,
                        argc: *argc,
                        return_reg: dst.0,
                    })
                } else {
                    // Not a function - return error
                    Err(VmError::type_error("not a constructor"))
                }
            }

            Instruction::CallMethod {
                dst,
                obj,
                method,
                argc,
            } => {
                let object = ctx.get_register(obj.0).clone();
                let method_const = module
                    .constants
                    .get(method.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let method_name = method_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                // Get the method from the object
                let method_value = if let Some(obj_ref) = object.as_object() {
                    obj_ref
                        .get(&PropertyKey::string(method_name))
                        .unwrap_or_else(Value::undefined)
                } else {
                    return Err(VmError::type_error("Cannot read property of non-object"));
                };

                // Check if it's a native function
                if let Some(native_fn) = method_value.as_native_function() {
                    // Collect arguments
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..*argc {
                        let arg = ctx.get_register(obj.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    // Call the native function directly
                    let result = native_fn(&args).map_err(VmError::type_error)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Regular closure call with `this` binding
                let closure = method_value
                    .as_function()
                    .ok_or_else(|| VmError::type_error("not a function"))?;

                // Copy arguments from caller registers
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..*argc {
                    let arg = ctx.get_register(obj.0 + 1 + i).clone();
                    args.push(arg);
                }

                // Store args and `this` value in context for new frame
                ctx.set_pending_args(args);
                ctx.set_pending_this(object);

                Ok(InstructionResult::Call {
                    func_index: closure.function_index,
                    argc: *argc,
                    return_reg: dst.0,
                })
            }

            Instruction::Return { src } => {
                let value = ctx.get_register(src.0).clone();
                Ok(InstructionResult::Return(value))
            }

            Instruction::ReturnUndefined => Ok(InstructionResult::Return(Value::undefined())),

            Instruction::CallSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let func_value = ctx.get_register(func.0).clone();
                let spread_arr = ctx.get_register(spread.0).clone();

                // Collect regular arguments first
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..*argc {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                // Spread the array into args
                if let Some(arr_obj) = spread_arr.as_object() {
                    let len = arr_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    for i in 0..len {
                        if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                            args.push(elem);
                        } else {
                            args.push(Value::undefined());
                        }
                    }
                }

                // Check if it's a native function first
                if let Some(native_fn) = func_value.as_native_function() {
                    // Call the native function directly
                    let result = native_fn(&args).map_err(VmError::type_error)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Regular closure call
                let closure = func_value
                    .as_function()
                    .ok_or_else(|| VmError::type_error("not a function"))?;

                // Store args in context for new frame to pick up
                ctx.set_pending_args(args.clone());

                Ok(InstructionResult::Call {
                    func_index: closure.function_index,
                    argc: args.len() as u8,
                    return_reg: dst.0,
                })
            }

            // ==================== Async/Await ====================
            Instruction::Await { dst, src } => {
                let value = ctx.get_register(src.0).clone();

                // Check if the value is a Promise
                if let Some(promise) = value.as_promise() {
                    match promise.state() {
                        PromiseState::Fulfilled(resolved) => {
                            // Promise already resolved, use the value
                            ctx.set_register(dst.0, resolved);
                            Ok(InstructionResult::Continue)
                        }
                        PromiseState::Rejected(error) => {
                            // Promise rejected, propagate the error
                            Err(VmError::type_error(format!(
                                "Promise rejected: {:?}",
                                error
                            )))
                        }
                        PromiseState::Pending => {
                            // Promise is pending, suspend execution
                            Ok(InstructionResult::Suspend {
                                promise: Arc::clone(promise),
                                resume_reg: dst.0,
                            })
                        }
                    }
                } else {
                    // Not a Promise, wrap in resolved promise and return immediately
                    // Per JS spec: await non-promise returns the value directly
                    ctx.set_register(dst.0, value);
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::Yield { dst, src } => {
                let value = ctx.get_register(src.0).clone();

                // Yield suspends the generator and returns the value
                // The dst register will receive the value sent to next() on resumption
                ctx.set_register(dst.0, Value::undefined()); // Will be set on resume

                // Return a yield result
                Ok(InstructionResult::Yield { value })
            }

            // ==================== Objects ====================
            Instruction::NewObject { dst } => {
                let obj = Arc::new(JsObject::new(None));
                ctx.set_register(dst.0, Value::object(obj));
                Ok(InstructionResult::Continue)
            }

            Instruction::GetPropConst { dst, obj, name } => {
                let object = ctx.get_register(obj.0);
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                let value = if let Some(obj) = object.as_object() {
                    obj.get(&PropertyKey::string(name_str))
                        .unwrap_or_else(Value::undefined)
                } else {
                    Value::undefined()
                };

                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetPropConst { obj, name, val } => {
                let object = ctx.get_register(obj.0);
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;
                let value = ctx.get_register(val.0).clone();

                if let Some(obj) = object.as_object() {
                    obj.set(PropertyKey::string(name_str), value);
                }

                Ok(InstructionResult::Continue)
            }

            Instruction::GetProp { dst, obj, key } => {
                let object = ctx.get_register(obj.0);
                let key_value = ctx.get_register(key.0);

                let value = if let Some(obj) = object.as_object() {
                    // Convert key to property key
                    if let Some(n) = key_value.as_int32() {
                        obj.get(&PropertyKey::Index(n as u32))
                            .unwrap_or_else(Value::undefined)
                    } else if let Some(s) = key_value.as_string() {
                        obj.get(&PropertyKey::string(s.as_str()))
                            .unwrap_or_else(Value::undefined)
                    } else {
                        let key_str = self.to_string(key_value);
                        obj.get(&PropertyKey::string(&key_str))
                            .unwrap_or_else(Value::undefined)
                    }
                } else {
                    Value::undefined()
                };

                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetProp { obj, key, val } => {
                let object = ctx.get_register(obj.0);
                let key_value = ctx.get_register(key.0);
                let value = ctx.get_register(val.0).clone();

                if let Some(obj) = object.as_object() {
                    if let Some(n) = key_value.as_int32() {
                        obj.set(PropertyKey::Index(n as u32), value);
                    } else if let Some(s) = key_value.as_string() {
                        obj.set(PropertyKey::string(s.as_str()), value);
                    } else {
                        let key_str = self.to_string(key_value);
                        obj.set(PropertyKey::string(&key_str), value);
                    }
                }

                Ok(InstructionResult::Continue)
            }

            // ==================== Arrays ====================
            Instruction::NewArray { dst, len } => {
                let arr = Arc::new(JsObject::array(*len as usize));
                ctx.set_register(dst.0, Value::object(arr));
                Ok(InstructionResult::Continue)
            }

            Instruction::GetElem { dst, arr, idx } => {
                let array = ctx.get_register(arr.0);
                let index = ctx.get_register(idx.0);

                let value = if let Some(obj) = array.as_object() {
                    if let Some(n) = index.as_int32() {
                        obj.get(&PropertyKey::Index(n as u32))
                            .unwrap_or_else(Value::undefined)
                    } else {
                        let idx_str = self.to_string(index);
                        obj.get(&PropertyKey::string(&idx_str))
                            .unwrap_or_else(Value::undefined)
                    }
                } else {
                    Value::undefined()
                };

                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetElem { arr, idx, val } => {
                let array = ctx.get_register(arr.0);
                let index = ctx.get_register(idx.0);
                let value = ctx.get_register(val.0).clone();

                if let Some(obj) = array.as_object() {
                    if let Some(n) = index.as_int32() {
                        obj.set(PropertyKey::Index(n as u32), value);
                    } else {
                        let idx_str = self.to_string(index);
                        obj.set(PropertyKey::string(&idx_str), value);
                    }
                }

                Ok(InstructionResult::Continue)
            }

            Instruction::Spread { dst, src } => {
                // Spread elements from src array into dst array
                let dst_arr = ctx.get_register(dst.0);
                let src_arr = ctx.get_register(src.0);

                if let (Some(dst_obj), Some(src_obj)) = (dst_arr.as_object(), src_arr.as_object()) {
                    // Get current length of dst array
                    let dst_len = dst_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    // Get length of src array
                    let src_len = src_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    // Copy elements from src to dst
                    for i in 0..src_len {
                        let elem = src_obj
                            .get(&PropertyKey::Index(i))
                            .unwrap_or_else(Value::undefined);
                        dst_obj.set(PropertyKey::Index(dst_len + i), elem);
                    }

                    // Update dst length
                    dst_obj.set(
                        PropertyKey::string("length"),
                        Value::int32((dst_len + src_len) as i32),
                    );
                }

                Ok(InstructionResult::Continue)
            }

            // ==================== Misc ====================
            Instruction::Move { dst, src } => {
                let value = ctx.get_register(src.0).clone();
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::Nop => Ok(InstructionResult::Continue),

            Instruction::Debugger => {
                // TODO: Implement debugger hook
                Ok(InstructionResult::Continue)
            }

            // ==================== Iteration ====================
            Instruction::GetIterator { dst, src } => {
                use crate::value::HeapRef;

                let obj = ctx.get_register(src.0).clone();

                // Get Symbol.iterator method using well-known symbol ID (1)
                const SYMBOL_ITERATOR_ID: u64 = 1;
                let iterator_method = match obj.heap_ref() {
                    Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                        o.get(&PropertyKey::Symbol(SYMBOL_ITERATOR_ID))
                    }
                    _ => None,
                };

                let iterator_fn =
                    iterator_method.ok_or_else(|| VmError::type_error("Object is not iterable"))?;

                // Call the iterator method with obj as `this`
                let iterator = if let Some(native_fn) = iterator_fn.as_native_function() {
                    native_fn(std::slice::from_ref(&obj)).map_err(VmError::type_error)?
                } else {
                    return Err(VmError::type_error("Symbol.iterator is not a function"));
                };

                ctx.set_register(dst.0, iterator);
                Ok(InstructionResult::Continue)
            }

            Instruction::IteratorNext { dst, done, iter } => {
                let iterator = ctx.get_register(iter.0).clone();

                // Get the next method
                let next_method = if let Some(obj) = iterator.as_object() {
                    obj.get(&PropertyKey::string("next"))
                } else {
                    None
                };

                let next_fn = next_method
                    .ok_or_else(|| VmError::type_error("Iterator has no next method"))?;

                // Call next()
                let result = if let Some(native_fn) = next_fn.as_native_function() {
                    native_fn(std::slice::from_ref(&iterator)).map_err(VmError::type_error)?
                } else {
                    return Err(VmError::type_error("next is not a function"));
                };

                // Extract done and value from result object
                let result_obj = result
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;

                let done_value = result_obj
                    .get(&PropertyKey::string("done"))
                    .unwrap_or_else(|| Value::boolean(false));
                let value = result_obj
                    .get(&PropertyKey::string("value"))
                    .unwrap_or_else(Value::undefined);

                ctx.set_register(dst.0, value);
                ctx.set_register(done.0, done_value);
                Ok(InstructionResult::Continue)
            }

            // Catch-all for unimplemented instructions
            _ => Err(VmError::internal(format!(
                "Unimplemented instruction: {:?}",
                instruction
            ))),
        }
    }

    /// Convert a bytecode constant to a Value
    fn constant_to_value(&self, constant: &otter_vm_bytecode::Constant) -> VmResult<Value> {
        use otter_vm_bytecode::Constant;

        match constant {
            Constant::Number(n) => Ok(Value::number(*n)),
            Constant::String(s) => {
                let js_str = Arc::new(JsString::new(s.clone()));
                Ok(Value::string(js_str))
            }
            Constant::BigInt(_) => Err(VmError::internal("BigInt not yet supported")),
            Constant::RegExp { .. } => Err(VmError::internal("RegExp not yet supported")),
            Constant::TemplateLiteral(_) => {
                Err(VmError::internal("Template literals not yet supported"))
            }
        }
    }

    /// Add operation (handles string concatenation)
    fn op_add(&self, left: &Value, right: &Value) -> VmResult<Value> {
        // String concatenation
        if left.is_string() || right.is_string() {
            let left_str = self.to_string(left);
            let right_str = self.to_string(right);
            let result = format!("{}{}", left_str, right_str);
            let js_str = Arc::new(JsString::new(result));
            return Ok(Value::string(js_str));
        }

        // Numeric addition
        let left_num = left
            .as_number()
            .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;
        let right_num = right
            .as_number()
            .ok_or_else(|| VmError::type_error("Cannot convert to number"))?;

        Ok(Value::number(left_num + right_num))
    }

    /// Convert value to string
    fn to_string(&self, value: &Value) -> String {
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
                    if n.is_nan() {
                        "NaN".to_string()
                    } else if n.is_infinite() {
                        if n > 0.0 {
                            "Infinity".to_string()
                        } else {
                            "-Infinity".to_string()
                        }
                    } else if n.fract() == 0.0 {
                        format!("{}", n as i64)
                    } else {
                        format!("{}", n)
                    }
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
            _ => "[object Object]".to_string(),
        }
    }

    /// Abstract equality comparison (==)
    fn abstract_equal(&self, left: &Value, right: &Value) -> bool {
        // Same type: use strict equality
        if left.type_of() == right.type_of() {
            return self.strict_equal(left, right);
        }

        // null == undefined
        if left.is_null() && right.is_undefined() {
            return true;
        }
        if left.is_undefined() && right.is_null() {
            return true;
        }

        // Number comparisons
        if let (Some(a), Some(b)) = (left.as_number(), right.as_number()) {
            return a == b;
        }

        // TODO: More coercion rules
        false
    }

    /// Strict equality comparison (===)
    fn strict_equal(&self, left: &Value, right: &Value) -> bool {
        // Different types are never strictly equal
        if left.type_of() != right.type_of() {
            return false;
        }

        // Use Value's PartialEq
        left == right
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of executing an instruction
#[allow(dead_code)]
enum InstructionResult {
    /// Continue to next instruction
    Continue,
    /// Jump by offset
    Jump(i32),
    /// Return from function
    Return(Value),
    /// Call a function
    Call {
        func_index: u32,
        argc: u8,
        return_reg: u8,
    },
    /// Suspend execution waiting for Promise
    Suspend {
        promise: Arc<JsPromise>,
        resume_reg: u8,
    },
    /// Yield from generator
    Yield {
        value: Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::Register;
    use otter_vm_bytecode::{Function, Module};

    fn create_test_context() -> VmContext {
        let global = Arc::new(JsObject::new(None));
        VmContext::new(global)
    }

    #[test]
    fn test_load_constants() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_arithmetic() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_number(), Some(15.0));
    }

    #[test]
    fn test_comparison() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(1),
                rhs: Register(0),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_object_prop_const() {
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            // NewObject r0
            .instruction(Instruction::NewObject { dst: Register(0) })
            // LoadInt32 r1, 42
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            // SetPropConst r0, "x", r1
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0),
                val: Register(1),
            })
            // GetPropConst r2, r0, "x"
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
            })
            // Return r2
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_array_elem() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            // NewArray r0, 3
            .instruction(Instruction::NewArray {
                dst: Register(0),
                len: 3,
            })
            // LoadInt32 r1, 10
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            })
            // LoadInt32 r2, 0
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 0,
            })
            // SetElem r0, r2, r1
            .instruction(Instruction::SetElem {
                arr: Register(0),
                idx: Register(2),
                val: Register(1),
            })
            // GetElem r3, r0, r2
            .instruction(Instruction::GetElem {
                dst: Register(3),
                arr: Register(0),
                idx: Register(2),
            })
            // Return r3
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(10));
    }

    #[test]
    fn test_object_prop_computed() {
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("foo");

        let func = Function::builder()
            .name("main")
            // NewObject r0
            .instruction(Instruction::NewObject { dst: Register(0) })
            // LoadInt32 r1, 99
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 99,
            })
            // LoadConst r2, "foo"
            .instruction(Instruction::LoadConst {
                dst: Register(2),
                idx: ConstantIndex(0),
            })
            // SetProp r0, r2, r1
            .instruction(Instruction::SetProp {
                obj: Register(0),
                key: Register(2),
                val: Register(1),
            })
            // GetProp r3, r0, r2
            .instruction(Instruction::GetProp {
                dst: Register(3),
                obj: Register(0),
                key: Register(2),
            })
            // Return r3
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(99));
    }

    #[test]
    fn test_closure_creation() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main function: creates closure and returns it
        let main = Function::builder()
            .name("main")
            // Closure r0, func#1
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            // TypeOf r1, r0
            .instruction(Instruction::TypeOf {
                dst: Register(1),
                src: Register(0),
            })
            // Return r1
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        // Function at index 1 (not called in this test)
        let helper = Function::builder()
            .name("helper")
            .instruction(Instruction::ReturnUndefined)
            .build();

        builder.add_function(main);
        builder.add_function(helper);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        // typeof function === "function"
        assert_eq!(result.as_string().map(|s| s.as_str()), Some("function"));
    }

    #[test]
    fn test_function_call_simple() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main function:
        //   Closure r0, func#1 (double)
        //   LoadInt32 r1, 5     (argument)
        //   Call r2, r0, 1      (result = double(5))
        //   Return r2
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Call {
                dst: Register(2),
                func: Register(0),
                argc: 1,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        // double(x): returns x + x
        //   local[0] = x (argument)
        //   GetLocal r0, 0
        //   Add r1, r0, r0
        //   Return r1
        let double = Function::builder()
            .name("double")
            .local_count(1)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::Add {
                dst: Register(1),
                lhs: Register(0),
                rhs: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        builder.add_function(main);
        builder.add_function(double);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_number(), Some(10.0)); // 5 + 5 = 10
    }

    #[test]
    fn test_function_call_multiple_args() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main: call add(3, 7)
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 7,
            })
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(0),
                argc: 2,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        // add(a, b): returns a + b
        let add = Function::builder()
            .name("add")
            .local_count(2)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: otter_vm_bytecode::LocalIndex(1),
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(main);
        builder.add_function(add);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_number(), Some(10.0)); // 3 + 7 = 10
    }

    #[test]
    fn test_nested_function_calls() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main: call outer(2), which calls inner(2) and returns inner(2) * 2
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::Call {
                dst: Register(2),
                func: Register(0),
                argc: 1,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        // outer(x): returns inner(x) * 2
        let outer = Function::builder()
            .name("outer")
            .local_count(1)
            // Get argument x
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            // Create closure for inner
            .instruction(Instruction::Closure {
                dst: Register(1),
                func: FunctionIndex(2),
            })
            // Call inner(x)
            .instruction(Instruction::Move {
                dst: Register(2),
                src: Register(0),
            })
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(1),
                argc: 1,
            })
            // Multiply by 2
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 2,
            })
            .instruction(Instruction::Mul {
                dst: Register(5),
                lhs: Register(3),
                rhs: Register(4),
            })
            .instruction(Instruction::Return { src: Register(5) })
            .build();

        // inner(x): returns x * x
        let inner = Function::builder()
            .name("inner")
            .local_count(1)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::Mul {
                dst: Register(1),
                lhs: Register(0),
                rhs: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        builder.add_function(main);
        builder.add_function(outer);
        builder.add_function(inner);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        // outer(2) = inner(2) * 2 = (2*2) * 2 = 8
        assert_eq!(result.as_number(), Some(8.0));
    }
}
